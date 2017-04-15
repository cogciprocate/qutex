//! A queue-backed read/write data lock.
//!
//! As with any queue-backed system, deadlocks must be carefully avoided when
//! interoperating with other queues.
//!
//
// * It is unclear how many of the unsafe methods within need actually remain
//   unsafe.
// * Virtually every aspect of each of the types in this module would benefit
//   from simplifying refactoring.
//   - Locking for processing, looping, and resetting the processing flag all
//     need to be standardized (factored out).
// * Evaluate whether or not sleeping when the lock is contended (`CONTENDED`
//   bit) is the best approach. This may be slower than it needs to be when
//   multiple cores are concurrently attempting to access. Use
//   `thread::yield_now()` instead? Spin a few times first? Whatever. [UPDATE:
//   Doing some spinning now]
//


use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::sync::atomic::{fence, AtomicUsize};
use std::sync::atomic::Ordering::{SeqCst, Acquire, /*Release*/};
use std::cell::UnsafeCell;
use std::thread;
use futures::{Future, Poll, Canceled, Async};
use futures::sync::oneshot::{self, Sender, Receiver};
use crossbeam::sync::SegQueue;


const READ_COUNT_MASK: usize = 0x00FFFFFF;
const WRITE_LOCKED: usize = 1 << 24;
const CONTENDED: usize = 1 << 25;

const PRINT_DEBUG: bool = false;


/// Our currently favored thread 'chill out' method used when multiple threads
/// are attempting to contend concurrently.
#[inline]
fn chill_out() {
    // thread::sleep(::std::time::Duration::new(0, 1));
    thread::yield_now();

    // NOTE: It's possible that sleeping or yielding here prolongs the time it
    // takes to process the queue to an unreasonable degree. There may be an
    // efficiency vs. duration balance to strike here (compared to spinning).
}


/// Extracts a `QrwLock` from a guard of either type.
//
// This saves us two unnecessary atomic stores (the reference count of lock
// going up then down when releasing or up/downgrading) which would occur if
// we were to clone then drop.
//
// QUESTION: Is there a more elegant way to do this?
unsafe fn extract_lock<T, G: Guard<T>>(guard: G) -> QrwLock<T> {
    let lock = ::std::ptr::read(guard.lock());
    ::std::mem::forget(guard);
    lock
}


/// Very forgettable guards.
trait Guard<T> where Self: ::std::marker::Sized {
    fn lock(&self) -> &QrwLock<T>;

    unsafe fn forget(self) {
        ::std::mem::forget(self);
    }
}


/// Allows read-only access to the data contained within a lock.
#[derive(Debug)]
pub struct ReadGuard<T> {
    lock: QrwLock<T>,
}

impl<T> ReadGuard<T> {
    pub fn upgrade(guard: ReadGuard<T>) -> FutureUpgrade<T> {
        debug_assert!(guard.lock.read_count().unwrap() > 0);

        match unsafe { guard.lock.upgrade_read_lock() } {
            Ok(_) => {
                if PRINT_DEBUG { println!("Read lock is now upgraded (thread: {}) ...",
                    thread::current().name().unwrap_or("<unnamed>")); }
                unsafe { FutureUpgrade::new(extract_lock(guard), None) }
            },
            Err(rx) => {
                if PRINT_DEBUG { println!("Waiting for the read count to reach 1 (thread: {}) ...",
                    thread::current().name().unwrap_or("<unnamed>")); }

                unsafe { FutureUpgrade::new(extract_lock(guard), Some(rx)) }
            }
        }
    }

    /// Releases the lock held by this `ReadGuard` and returns the original `QrwLock`.
    pub fn release(guard: ReadGuard<T>) -> QrwLock<T> {
        unsafe {
            guard.lock.release_read_lock();
            extract_lock(guard)
        }
    }
}

impl<T> Deref for ReadGuard<T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.lock.inner.cell.get() }
    }
}

impl<T> Drop for ReadGuard<T> {
    fn drop(&mut self) {
        unsafe { self.lock.release_read_lock() }
    }
}

impl<T> Guard<T> for ReadGuard<T> {
    fn lock(&self) -> &QrwLock<T> {
        &self.lock
    }
}


/// Allows read or write access to the data contained within a lock.
#[derive(Debug)]
pub struct WriteGuard<T> {
    lock: QrwLock<T>,
}

impl<T> WriteGuard<T> {
    /// Converts this `WriteGuard` into a `ReadGuard` and fulfills any other
    /// pending read requests.
    pub fn downgrade(guard: WriteGuard<T>) -> ReadGuard<T> {
        unsafe {
            guard.lock.downgrade_write_lock();
            ReadGuard { lock: extract_lock(guard) }
        }
    }

    /// Releases the lock held by this `WriteGuard` and returns the original
    /// `QrwLock`.
    //
    // * TODO: Create a test that ensures the write lock is released.
    //   Commenting out the `release_write_lock()' line appears to have no
    //   effect on the outcome of the current tests.
    pub fn release(guard: WriteGuard<T>) -> QrwLock<T> {
        unsafe {
            guard.lock.release_write_lock();
            extract_lock(guard)
        }
    }
}

impl<T> Deref for WriteGuard<T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.lock.inner.cell.get() }
    }
}

impl<T> DerefMut for WriteGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.inner.cell.get() }
    }
}

impl<T> Drop for WriteGuard<T> {
    fn drop(&mut self) {
        unsafe { self.lock.release_write_lock() }
    }
}

impl<T> Guard<T> for WriteGuard<T> {
    fn lock(&self) -> &QrwLock<T> {
        &self.lock
    }
}


/// A precursor to a `WriteGuard`.
#[must_use = "futures do nothing unless polled"]
#[derive(Debug)]
pub struct FutureUpgrade<T> {
    lock: Option<QrwLock<T>>,
    // Designates whether or not to resolve immediately:
    rx: Option<Receiver<()>>,
}

impl<T> FutureUpgrade<T> {
    /// Returns a new `FutureUpgrade`.
    fn new(lock: QrwLock<T>, rx: Option<Receiver<()>>) -> FutureUpgrade<T> {
        FutureUpgrade {
            lock: Some(lock),
            rx: rx,
        }
    }

    /// Blocks the current thread until this future resolves.
    #[inline]
    pub fn wait(self) -> Result<WriteGuard<T>, Canceled> {
        <Self as Future>::wait(self)
    }
}

impl<T> Future for FutureUpgrade<T> {
    type Item = WriteGuard<T>;
    type Error = Canceled;

    #[inline]
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if self.lock.is_some() {
            if PRINT_DEBUG { println!("Polling FutureUpgrade (thread: {}) ...",
                thread::current().name().unwrap_or("<unnamed>")); }

            if self.rx.is_none() {
                Ok(Async::Ready(WriteGuard { lock: self.lock.take().unwrap() }))
            } else {
                unsafe { self.lock.as_ref().unwrap().process_queue() }
                self.rx.poll().map(|res| res.map(|_| WriteGuard { lock: self.lock.take().unwrap() }))
            }
        } else {
            panic!("FutureUpgrade::poll: Task already completed.");
        }
    }
}


/// A future which resolves to a `ReadGuard`.
#[must_use = "futures do nothing unless polled"]
#[derive(Debug)]
pub struct FutureReadGuard<T> {
    lock: Option<QrwLock<T>>,
    rx: Receiver<()>,
}

impl<T> FutureReadGuard<T> {
    /// Returns a new `FutureReadGuard`.
    fn new(lock: QrwLock<T>, rx: Receiver<()>) -> FutureReadGuard<T> {
        FutureReadGuard {
            lock: Some(lock),
            rx: rx,
        }
    }

    /// Blocks the current thread until this future resolves.
    #[inline]
    pub fn wait(self) -> Result<ReadGuard<T>, Canceled> {
        <Self as Future>::wait(self)
    }
}

impl<T> Future for FutureReadGuard<T> {
    type Item = ReadGuard<T>;
    type Error = Canceled;

    #[inline]
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if self.lock.is_some() {
            unsafe { self.lock.as_ref().unwrap().process_queue() }
            self.rx.poll().map(|res| res.map(|_| ReadGuard { lock: self.lock.take().unwrap() }))
        } else {
            panic!("FutureReadGuard::poll: Task already completed.");
        }
    }
}


/// A future which resolves to a `WriteGuard`.
#[must_use = "futures do nothing unless polled"]
#[derive(Debug)]
pub struct FutureWriteGuard<T> {
    lock: Option<QrwLock<T>>,
    rx: Receiver<()>,
}

impl<T> FutureWriteGuard<T> {
    /// Returns a new `FutureWriteGuard`.
    fn new(lock: QrwLock<T>, rx: Receiver<()>) -> FutureWriteGuard<T> {
        FutureWriteGuard {
            lock: Some(lock),
            rx: rx,
        }
    }

    /// Blocks the current thread until this future resolves.
    #[inline]
    pub fn wait(self) -> Result<WriteGuard<T>, Canceled> {
        <Self as Future>::wait(self)
    }
}

impl<T> Future for FutureWriteGuard<T> {
    type Item = WriteGuard<T>;
    type Error = Canceled;

    #[inline]
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if self.lock.is_some() {
            unsafe { self.lock.as_ref().unwrap().process_queue() }
            self.rx.poll().map(|res| res.map(|_| WriteGuard { lock: self.lock.take().unwrap() }))
        } else {
            panic!("FutureWriteGuard::poll: Task already completed.");
        }
    }
}


/// Specifies whether a `QrwRequest` is a read or write request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Read,
    Write,
}


/// A request to lock the lock for either read or write access.
#[derive(Debug)]
pub struct QrwRequest {
    tx: Sender<()>,
    kind: RequestKind,
}

impl QrwRequest {
    /// Returns a new `QrwRequest`.
    pub fn new(tx: Sender<()>, kind: RequestKind) -> QrwRequest {
        QrwRequest {
            tx: tx,
            kind: kind,
        }
    }
}


/// The guts of a `QrwLock`.
#[derive(Debug)]
struct Inner<T> {
    // TODO: Convert to `AtomicBool` if no additional states are needed:
    state: AtomicUsize,
    cell: UnsafeCell<T>,
    queue: SegQueue<QrwRequest>,
    tip: UnsafeCell<Option<QrwRequest>>,
    upgrade_tx: UnsafeCell<Option<Sender<()>>>,
}

impl<T> From<T> for Inner<T> {
    #[inline]
    fn from(val: T) -> Inner<T> {
        Inner {
            state: AtomicUsize::new(0),
            cell: UnsafeCell::new(val),
            queue: SegQueue::new(),
            tip: UnsafeCell::new(None),
            upgrade_tx: UnsafeCell::new(None),
        }
    }
}

unsafe impl<T: Send> Send for Inner<T> {}
unsafe impl<T: Send> Sync for Inner<T> {}


/// A queue-backed read/write data lock.
///
/// As with any queue-backed system, deadlocks must be carefully avoided when
/// interoperating with other queues.
///
#[derive(Debug)]
pub struct QrwLock<T> {
    inner: Arc<Inner<T>>,
}

impl<T> QrwLock<T> {
    /// Creates and returns a new `QrwLock`.
    #[inline]
    pub fn new(val: T) -> QrwLock<T> {
        QrwLock {
            inner: Arc::new(Inner::from(val)),
        }
    }

    /// Returns a new `FutureReadGuard` which can be used as a future and will
    /// resolve into a `ReadGuard`.
    #[inline]
    #[deprecated(since="0.0.9", note="please use `::read` instead")]
    pub fn request_read(self) -> FutureReadGuard<T> {
        self.read()
    }

    /// Returns a new `FutureWriteGuard` which can be used as a future and will
    /// resolve into a `WriteGuard`.
    #[inline]
    #[deprecated(since="0.0.9", note="please use `::write` instead")]
    pub fn request_write(self) -> FutureWriteGuard<T> {
        self.write()
    }

    /// Returns a new `FutureReadGuard` which can be used as a future and will
    /// resolve into a `ReadGuard`.
    #[inline]
    pub fn read(self) -> FutureReadGuard<T> {
        if PRINT_DEBUG { println!("Requesting read lock.(thread: {}) ...",
            thread::current().name().unwrap_or("<unnamed>")); }
        let (tx, rx) = oneshot::channel();
        unsafe { self.push_request(QrwRequest::new(tx, RequestKind::Read)); }
        FutureReadGuard::new(self, rx)
    }

    /// Returns a new `FutureWriteGuard` which can be used as a future and will
    /// resolve into a `WriteGuard`.
    #[inline]
    pub fn write(self) -> FutureWriteGuard<T> {
        if PRINT_DEBUG { println!("Requesting write lock.(thread: {}) ...",
            thread::current().name().unwrap_or("<unnamed>")); }
        let (tx, rx) = oneshot::channel();
        unsafe { self.push_request(QrwRequest::new(tx, RequestKind::Write)); }
        FutureWriteGuard::new(self, rx)
    }

    /// Pushes a lock request onto the queue.
    ///
    //
    // TODO: Evaluate unsafe-ness (appears unlikely this can be misused except
    // to deadlock the queue which is fine).
    //
    #[inline]
    pub unsafe fn push_request(&self, req: QrwRequest) {
        self.inner.queue.push(req);
    }

    /// Returns a mutable reference to the inner `Vec` if there are currently
    /// no other copies of this `QrwLock`.
    ///
    /// Since this call borrows the inner lock mutably, no actual locking needs to
    /// take place---the mutable borrow statically guarantees no locks exist.
    ///
    #[inline]
    pub fn get_mut(&mut self) -> Option<&mut T> {
        Arc::get_mut(&mut self.inner).map(|inn| unsafe { &mut *inn.cell.get() })
    }

    /// Returns a reference to the inner value.
    ///
    /// This is fraught with potential peril.
    ///
    #[inline]
    pub fn as_ptr(&self) -> *const T {
        self.inner.cell.get()
    }

    /// Returns a mutable reference to the inner value.
    ///
    /// Drinking water from the tap in 1850's London would be safer.
    ///
    #[inline]
    pub fn as_mut_ptr(&self) -> *mut T {
        self.inner.cell.get()
    }

    /// Pops the next read or write lock request and returns it or `None` if the queue is empty.
    #[inline]
    fn pop_request(&self) -> Option<QrwRequest> {
        debug_assert_eq!(self.inner.state.load(Acquire) & CONTENDED, CONTENDED);

        unsafe {
            // Pop twice if the tip was `None` but the queue was not empty.
            ::std::mem::replace(&mut *self.inner.tip.get(), self.inner.queue.try_pop())
                .or_else(|| {
                    if (*self.inner.tip.get()).is_some() {
                        self.pop_request()
                    } else {
                        None
                    }
                })
        }
    }

    /// Returns the `RequestKind` for the next pending read or write lock request.
    #[inline]
    fn peek_request_kind(&self) -> Option<RequestKind> {
        debug_assert_eq!(self.inner.state.load(Acquire) & CONTENDED, CONTENDED);

        unsafe {
            if (*self.inner.tip.get()).is_none() {
                ::std::mem::replace(&mut *self.inner.tip.get(), self.inner.queue.try_pop());
            }
            (*self.inner.tip.get()).as_ref().map(|req| req.kind)
        }
    }

    /// Fulfill a request if possible.
    #[inline]
    fn fulfill_request(&self, mut state: usize) -> usize {
        loop {
            debug_assert_eq!(self.inner.state.load(Acquire) & CONTENDED, CONTENDED);
            debug_assert_eq!(self.inner.state.load(Acquire) & WRITE_LOCKED, 0);

            if let Some(req) = self.pop_request() {
                // If there is a send error, a requester has dropped its
                // receiver so just go to the next, otherwise process.
                if req.tx.send(()).is_ok() {
                    debug_assert_eq!(self.inner.state.load(Acquire) & WRITE_LOCKED, 0);

                    match req.kind {
                        RequestKind::Read => {
                            state += 1;
                            if PRINT_DEBUG { println!("Locked for reading (state: {}) \
                                (thread: {}) ...", state,
                                thread::current().name().unwrap_or("<unnamed>")); }

                            if let Some(RequestKind::Read) = self.peek_request_kind() {
                                continue;
                            } else {
                                break;
                            }
                        },
                        RequestKind::Write => {
                            debug_assert_eq!(state, 0);
                            state = WRITE_LOCKED;
                            if PRINT_DEBUG { println!("Locked for writing (state: {}) \
                                (thread: {}) ...", state,
                                thread::current().name().unwrap_or("<unnamed>")); }
                            break;
                        },
                    }
                }
            } else {
                break;
            }
        }

        state
    }

    /// Returns the current number of read locks.
    ///
    /// Currently used for debug purposes only.
    #[inline]
    fn read_count(&self) -> Option<u32> {
        let state = self.inner.state.load(Acquire);
        let read_count = state & READ_COUNT_MASK;

        if state & READ_COUNT_MASK == read_count {
            if PRINT_DEBUG { println!("Read count: {} (thread: {}) ...", read_count,
                thread::current().name().unwrap_or("<unnamed>")); }
            Some(read_count as u32)
        } else {
            None
        }
    }

    /// Acquires exclusive access to the lock state and returns it.
    #[inline(always)]
    fn contend(&self) -> usize {
        if PRINT_DEBUG { println!("Processing state (thread: {}) ...",
            thread::current().name().unwrap_or("<unnamed>")); }
        let mut spins: u32 = 0;

        loop {
            let state = self.inner.state.fetch_or(CONTENDED, SeqCst);
            if state & CONTENDED != 0 {
                if spins >= 16 {
                    chill_out();
                } else {
                    for _ in 0..(2 << spins) { fence(SeqCst); }
                }
                spins += 1;
            } else {
                return state;
            }
        }
    }

    /// Pops the next lock request in the queue if possible.
    ///
    /// If this lock is released, read or write-locks this lock and unparks
    /// the next requester task in the queue.
    ///
    /// If this lock is write-locked, this function does nothing.
    ///
    /// If this lock is read-locked and the next request or consecutive
    /// requests in the queue are read requests, those requests will be
    /// fulfilled, unparking their respective tasks and incrementing the
    /// read-lock count appropriately.
    ///
    //
    // TODO:
    // * This is currently public due to 'derivers' (aka. sub-types). Evaluate.
    // * Consider removing unsafe qualifier (should be fine, this fn assumes
    //   no particular state).
    // * Return proper error type.
    //
    // pub unsafe fn process_queue(&self) -> Result<(), ()> {
    #[inline]
    pub unsafe fn process_queue(&self) {
        if PRINT_DEBUG { println!("Processing queue (thread: {}) ...",
                thread::current().name().unwrap_or("<unnamed>")); }

        // Do nothing if an upgrade is pending.
        if (*self.inner.upgrade_tx.get()).is_some() { return; }

        match self.contend() {
            // Unlocked:
            0 => {
                if PRINT_DEBUG { println!("Processing queue: Unlocked (thread: {}) ...",
                    thread::current().name().unwrap_or("<unnamed>")); }
                let new_state = self.fulfill_request(0);
                self.inner.state.store(new_state, SeqCst);
            },

            // Write locked, unset CONTENDED flag:
            WRITE_LOCKED => {
                if PRINT_DEBUG { println!("Processing queue: Write Locked (thread: {}) ...",
                    thread::current().name().unwrap_or("<unnamed>")); }
                self.inner.state.store(WRITE_LOCKED, SeqCst);
            },

            // Either read locked or already being processed:
            state => {
                debug_assert!(self.inner.state.load(SeqCst) & CONTENDED != 0);
                debug_assert!(state <= READ_COUNT_MASK);

                if PRINT_DEBUG { println!("Processing queue: Other {{ state: {}, peek: {:?} }} \
                    (thread: {}) ...", state, self.peek_request_kind(),
                    thread::current().name().unwrap_or("<unnamed>")); }

                if self.peek_request_kind() == Some(RequestKind::Read) {
                    // We are read locked and the next request is a read.
                    let new_state = self.fulfill_request(state);
                    self.inner.state.store(new_state, SeqCst);
                } else {
                    // Either the next request is empty or a write and
                    // we are already read locked. Leave the request
                    // there and restore our original state, removing
                    // the CONTENDED flag.
                    self.inner.state.store(state, SeqCst);
                }
            },
        }
    }

    /// Converts a single read lock (read count of '1') into a write lock.
    ///
    /// Returns an error containing a oneshot receiver if there is currently
    /// more than one read lock. When the read count reaches one, the receiver
    /// channel will be completed (i.e. poll it).
    ///
    /// Panics if there are no read locks.
    ///
    /// Do not call this method directly unless you are using a custom guard
    /// or are otherwise managing the lock state manually. Use
    /// `ReadGuard::upgrade` instead.
    #[inline]
    pub unsafe fn upgrade_read_lock(&self) -> Result<(), Receiver<()>> {
        if PRINT_DEBUG { println!("Upgrading reader to writer: (thread: {}) ...",
            thread::current().name().unwrap_or("<unnamed>")); }

        match self.contend() {
            0 => panic!("Unable to upgrade this QrwLock: no read locks."),
            WRITE_LOCKED => panic!("Unable to upgrade this QrwLock: already write locked."),
            state => {
                debug_assert!(self.inner.state.load(SeqCst) & CONTENDED != 0);
                debug_assert_eq!(state, self.read_count().unwrap() as usize);

                if state == 1 {
                    self.inner.state.store(WRITE_LOCKED, SeqCst);
                    Ok(())
                } else {
                    let (tx, rx) = oneshot::channel();
                    *self.inner.upgrade_tx.get() = Some(tx);
                    self.inner.state.store(state, SeqCst);
                    Err(rx)
                }
            },
        }
    }

    /// Converts a write lock into a read lock then processes the queue,
    /// allowing additional read requests to acquire locks.
    ///
    /// Use `WriteGuard::downgrade` rather than calling this directly.
    #[inline]
    pub unsafe fn downgrade_write_lock(&self) {
        debug_assert_eq!(self.inner.state.load(SeqCst) & WRITE_LOCKED, WRITE_LOCKED);

        match self.contend() {
            0 => unreachable!(),
            WRITE_LOCKED => {
                self.inner.state.store(1, SeqCst);
            },
            _state => unreachable!(),
        }

        self.process_queue();
    }

    /// Decreases the reader count by one and unparks the next requester task
    /// in the queue if possible.
    ///
    /// If a reader is waiting to be upgraded and the read lock count reaches
    /// 1, the upgrade sender will be completed.
    //
    // TODO: Consider using `Ordering::Release`.
    // TODO: Wire up upgrade checking (if reader count == 1, complete `upgrade_tx`).
    #[inline]
    pub unsafe fn release_read_lock(&self) {
        if PRINT_DEBUG { println!("Releasing read lock...(thread: {}) ...",
            thread::current().name().unwrap_or("<unnamed>")); }
        // Ensure we are read locked and not processing or write locked:
        debug_assert!(self.inner.state.load(SeqCst) & READ_COUNT_MASK != 0);
        debug_assert_eq!(self.inner.state.load(SeqCst) & WRITE_LOCKED, 0);

        match self.contend() {
            0 => unreachable!(),
            WRITE_LOCKED => unreachable!(),
            state => {
                debug_assert!(self.inner.state.load(SeqCst) & CONTENDED != 0);
                assert!(state > 0 && state <= READ_COUNT_MASK);
                let new_state = state - 1;

                // If an upgrade (read lock -> write lock) is pending
                // and we have just reached a read lock count of 1:
                if new_state == 1 && (*self.inner.upgrade_tx.get()).is_some() {
                    self.inner.state.store(WRITE_LOCKED, SeqCst);
                    // Send the upgrade signal. Ignore any errors (if
                    // the upgrade receiver has dropped):
                    let tx = (*self.inner.upgrade_tx.get()).take().unwrap();
                    tx.send(()).ok();
                } else {
                    self.inner.state.store(new_state, SeqCst);
                }
            }
        }

        self.process_queue()
    }

    /// Unlocks this lock and unparks the next requester task in the queue if
    /// possible.
    //
    // TODO: Consider using `Ordering::Release`.
    #[inline]
    pub unsafe fn release_write_lock(&self) {
        if PRINT_DEBUG { println!("Releasing write lock...(thread: {}) ...",
            thread::current().name().unwrap_or("<unnamed>")); }

        // If we are not WRITE_LOCKED, we must be CONTENDED (and will soon be
        // write locked).
        debug_assert!({
            let state = self.inner.state.load(SeqCst);
            (state & CONTENDED == CONTENDED) == (state & WRITE_LOCKED != WRITE_LOCKED) ||
            (state & CONTENDED == CONTENDED) && (state & WRITE_LOCKED == WRITE_LOCKED)
        });

        // Ensure we are not read locked.
        debug_assert!(self.inner.state.load(SeqCst) & READ_COUNT_MASK == 0);

        match self.contend() {
            0 => unreachable!(),
            WRITE_LOCKED => {
                self.inner.state.store(0, SeqCst);
            },
            _state => unreachable!(),
        }

        self.process_queue()
    }
}

impl<T> From<T> for QrwLock<T> {
    #[inline]
    fn from(val: T) -> QrwLock<T> {
        QrwLock::new(val)
    }
}

// Avoids needing `T: Clone`.
impl<T> Clone for QrwLock<T> {
    #[inline]
    fn clone(&self) -> QrwLock<T> {
        QrwLock {
            inner: self.inner.clone(),
        }
    }
}


#[cfg(test)]
// Woefully incomplete.
mod tests {
    use std::thread;
    use futures::future;
    use super::*;

    #[test]
    fn simple() {
        let lock = QrwLock::from(0i32);

        let (future_r0_a, future_r0_b) = (lock.clone().read(), lock.clone().read());
        let future_r0 = lock.clone().read().and_then(|guard| {
            assert_eq!(*guard, 0);
            println!("val[r0]: {}", *guard);
            ReadGuard::release(guard);

            future_r0_a.and_then(|guard| {
                    assert_eq!(*guard, 0);
                    println!("val[r0a]: {}", *guard);
                    Ok(())
                }).wait().unwrap();

            future_r0_b.and_then(|guard| {
                    assert_eq!(*guard, 0);
                    println!("val[r0b]: {}", *guard);
                    Ok(())
                }).wait().unwrap();

            Ok(())
        }).boxed();

        let future_w0 = lock.clone().write().and_then(|mut guard| {
            *guard = 5;
            println!("val is now: {}", *guard);
            Ok(())
        }).boxed();

        let future_r1 = lock.clone().read().and_then(|guard| {
            assert_eq!(*guard, 5);
            println!("val[r1]: {}", *guard);
            Ok(())
        }).boxed();

        let future_r2 = lock.clone().read().and_then(|guard| {
            assert_eq!(*guard, 5);
            println!("val[r2]: {}", *guard);
            Ok(())
        }).boxed();

        let future_u0 = lock.clone().read().and_then(|read_guard| {
            println!("Upgrading read guard...");
            ReadGuard::upgrade(read_guard).and_then(|mut write_guard| {
                println!("Read guard upgraded.");
                *write_guard = 6;
                Ok(())
            })
        }).boxed();

        // This read will take place before the above read lock can be
        // upgraded because read requests are processed in a chained fashion:
        let future_r3 = lock.clone().read().and_then(|guard| {
            // Value should not yet be affected by the events following the
            // above write guard upgrade.
            assert_eq!(*guard, 5);
            println!("val[r3]: {}", *guard);
            Ok(())
        }).boxed();

        // future_r0.join4(future_w0, future_r1, future_r2).wait().unwrap();

        let futures = vec![future_r0, future_w0, future_r1, future_r2, future_u0, future_r3];
        future::join_all(futures).wait().unwrap();

        let future_guard = lock.clone().read();
        let guard = future_guard.wait().unwrap();
        assert_eq!(*guard, 6);
    }

    // This doesn't really prove much...
    //
    // * TODO: *Actually* determine whether or not the lock acquisition order is
    //   upheld.
    //
    #[test]
    fn concurrent() {
        let start_val = 0i32;
        let lock = QrwLock::new(start_val);
        let thread_count = 20;
        let mut threads = Vec::with_capacity(thread_count);

        for _i in 0..thread_count {
            let future_write_guard = lock.clone().write();
            let future_read_guard = lock.clone().read();

            let future_write = future_write_guard.and_then(|mut guard| {
                *guard += 1;
                WriteGuard::downgrade(guard);
                Ok(())
            });

            let future_read = future_read_guard.and_then(move |guard| {
                // println!("Value for thread '{}' is: {}", _i, *_guard);
                Ok(guard)
            });

            threads.push(thread::spawn(|| {
                future_write.join(future_read).wait().unwrap();
            }));
        }

        for i in 0..thread_count {
            let future_write_guard = lock.clone().write();

            threads.push(thread::Builder::new().name(format!("test_thread_{}", i)).spawn(|| {
                let mut guard = future_write_guard.wait().unwrap();
                *guard -= 1
            }).unwrap());

            let future_read_guard = lock.clone().read();

            threads.push(thread::Builder::new().name(format!("test_thread_{}", i)).spawn(|| {
                let _val = *future_read_guard.wait().unwrap();
            }).unwrap());
        }

        for thread in threads {
            thread.join().unwrap();
        }

        let guard = lock.clone().read().wait().unwrap();
        assert_eq!(*guard, start_val);
    }
}