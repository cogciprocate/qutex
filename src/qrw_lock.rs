//! A queue-backed exclusive data lock.
//!
//
// * It is unclear how many of the unsafe methods within need actually remain
//   unsafe.

use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::{SeqCst, Acquire};
use std::cell::UnsafeCell;
use futures::{Future, Poll, Canceled};
use futures::sync::oneshot;
use crossbeam::sync::SegQueue;


const READ_COUNT_MASK: usize = 0x00FFFFFF;
const WRITE_LOCKED: usize = 1 << 24;
const PROCESSING: usize = 1 << 25;

const PRINT_DEBUG: bool = false;

/// Allows read-only access to the data contained within a lock.
pub struct ReadGuard<T> {
    qutex: QrwLock<T>,
}

impl<T> ReadGuard<T> {
    /// Releases the lock held by this `ReadGuard` and returns the original `QrwLock`.
    pub fn unlock(self) -> QrwLock<T> {
        unsafe { 
            // All of this stuff simply saves us two unnecessary atomic stores
            // (the reference count of qutex going up then down):
            self.qutex.release_reader();
            let mut qutex = ::std::mem::uninitialized::<QrwLock<T>>();
            ::std::ptr::copy_nonoverlapping(&self.qutex, &mut qutex, 1);            
            ::std::mem::forget(self);
            qutex
        }
    }
}

impl<T> Deref for ReadGuard<T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.qutex.inner.cell.get() }
    }
}

impl<T> Drop for ReadGuard<T> {
    fn drop(&mut self) {
        // unsafe { self.qutex.direct_unlock().expect("Error dropping ReadGuard") };
        unsafe { self.qutex.release_reader() }
    }
}


/// Allows read or write access to the data contained within a lock.
pub struct WriteGuard<T> {
    qutex: QrwLock<T>,
}

impl<T> WriteGuard<T> {
    /// Releases the lock held by this `WriteGuard` and returns the original `QrwLock`.
    pub fn unlock(self) -> QrwLock<T> {
        unsafe { 
            // All of this stuff simply saves us two unnecessary atomic stores
            // (the reference count of qutex going up then down):
            self.qutex.release_writer();
            let mut qutex = ::std::mem::uninitialized::<QrwLock<T>>();
            ::std::ptr::copy_nonoverlapping(&self.qutex, &mut qutex, 1);            
            ::std::mem::forget(self);
            qutex
        }
    }
}

impl<T> Deref for WriteGuard<T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.qutex.inner.cell.get() }
    }
}

impl<T> DerefMut for WriteGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.qutex.inner.cell.get() }
    }
}

impl<T> Drop for WriteGuard<T> {
    fn drop(&mut self) {
        // unsafe { self.qutex.direct_unlock().expect("Error dropping WriteGuard") };
        unsafe { self.qutex.release_writer() }
    }
}


/// A future which resolves to a `ReadGuard`.
pub struct FutureReadGuard<T> {
    qutex: Option<QrwLock<T>>,
    rx: oneshot::Receiver<()>,
}

impl<T> FutureReadGuard<T> {
    /// Returns a new `FutureReadGuard`.
    fn new(qutex: QrwLock<T>, rx: oneshot::Receiver<()>) -> FutureReadGuard<T> {
        FutureReadGuard {
            qutex: Some(qutex),
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
        if self.qutex.is_some() {
            unsafe { self.qutex.as_ref().unwrap().process_queue() }
            self.rx.poll().map(|res| res.map(|_| ReadGuard { qutex: self.qutex.take().unwrap() }))
        } else {
            panic!("FutureReadGuard::poll: Task already completed.");
        }
    }
}

/// A future which resolves to a `WriteGuard`.
pub struct FutureWriteGuard<T> {
    qutex: Option<QrwLock<T>>,
    rx: oneshot::Receiver<()>,
}

impl<T> FutureWriteGuard<T> {
    /// Returns a new `FutureWriteGuard`.
    fn new(qutex: QrwLock<T>, rx: oneshot::Receiver<()>) -> FutureWriteGuard<T> {
        FutureWriteGuard {
            qutex: Some(qutex),
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
        if self.qutex.is_some() {
            unsafe { self.qutex.as_ref().unwrap().process_queue() }
            self.rx.poll().map(|res| res.map(|_| WriteGuard { qutex: self.qutex.take().unwrap() }))
        } else {
            panic!("FutureWriteGuard::poll: Task already completed.");
        }
    }
}


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Read,
    Write,
}


/// A request to lock the qutex for either read or write access.
#[derive(Debug)]
pub struct QrwRequest {
    tx: oneshot::Sender<()>,
    kind: RequestKind,
}

impl QrwRequest {
    /// Returns a new `QrwRequest`.
    pub fn new(tx: oneshot::Sender<()>, kind: RequestKind) -> QrwRequest {
        QrwRequest { 
            tx: tx,
            kind: kind,
        }
    }
}


struct Inner<T> {
    // TODO: Convert to `AtomicBool` if no additional states are needed:
    state: AtomicUsize,
    cell: UnsafeCell<T>,
    queue: SegQueue<QrwRequest>,
    tip: UnsafeCell<Option<QrwRequest>>,
}

impl<T> From<T> for Inner<T> {
    #[inline]
    fn from(val: T) -> Inner<T> {
        Inner {
            state: AtomicUsize::new(0),
            cell: UnsafeCell::new(val),
            queue: SegQueue::new(),
            tip: UnsafeCell::new(None),
        }
    }
}

unsafe impl<T: Send> Send for Inner<T> {}
unsafe impl<T: Send> Sync for Inner<T> {}


/// A lock-free queue-backed read/write data lock.
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
    pub fn request_read(self) -> FutureReadGuard<T> {
        if PRINT_DEBUG { println!("Requesting read lock."); }
        let (tx, rx) = oneshot::channel();
        unsafe { self.push_request(QrwRequest::new(tx, RequestKind::Read)); }
        FutureReadGuard::new(self, rx)
    }

    /// Returns a new `FutureWriteGuard` which can be used as a future and will
    /// resolve into a `WriteGuard`.
    pub fn request_write(self) -> FutureWriteGuard<T> {
        if PRINT_DEBUG { println!("Requesting write lock."); }
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
    /// This is frought with potential peril.
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
    fn pop_request(&self) -> Option<QrwRequest> {
        debug_assert_eq!(self.inner.state.load(Acquire) & PROCESSING, PROCESSING);

        // unsafe { ::std::mem::replace(&mut *self.inner.tip.get(), self.inner.queue.try_pop()) }      

        unsafe { 
            // match ::std::mem::replace(&mut *self.inner.tip.get(), self.inner.queue.try_pop()) {
            //     Some(req) => Some(req),
            //     None => {                    
            //         // let pop = self.inner.queue.try_pop();
            //         // *self.inner.tip.get() = self.inner.queue.try_pop();
            //         // pop

            //         if (*self.inner.tip.get()).is_some() {
            //             self.pop_request()
            //         } else {
            //             None
            //         }
            //     },
            // }

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
    fn peek_request_kind(&self) -> Option<RequestKind> {
        debug_assert_eq!(self.inner.state.load(Acquire) & PROCESSING, PROCESSING);
        unsafe { (*self.inner.tip.get()).as_ref().map(|req| req.kind) }
    }

    fn fulfill_request(&self, mut state: usize) -> usize {
        loop {
            debug_assert_eq!(self.inner.state.load(Acquire) & PROCESSING, PROCESSING);
            debug_assert_eq!(self.inner.state.load(Acquire) & WRITE_LOCKED, 0);

            if let Some(req) = self.pop_request() {
                // If there is a send error, a requester has dropped its
                // receiver so just go to the next, otherwise process.
                if req.tx.send(()).is_ok() {
                    debug_assert_eq!(self.inner.state.load(Acquire) & WRITE_LOCKED, 0);

                    match req.kind {
                        RequestKind::Read => {
                            state += 1;
                            if PRINT_DEBUG { println!("Locked for reading (state: {}).", state); }

                            if let Some(RequestKind::Read) = self.peek_request_kind() {                                
                                continue;
                            } else {
                                break;
                            }
                        },
                        RequestKind::Write => {
                            // self.inner.state.store(WRITE_LOCKED, SeqCst);
                            // break;
                            debug_assert_eq!(state, 0); 
                            state = WRITE_LOCKED;
                            if PRINT_DEBUG { println!("Locked for writing (state: {}).", state); }
                            break;
                        },
                    }
                }
            } else {
                // self.inner.state.store(0, SeqCst);

                break;
            }
        }

        state
    }

    /// Pops the next lock request in the queue if possible.
    ///
    /// If this lock is unlocked, read or write-locks this lock and unparks
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
    // * Consider removing unsafe qualifier.
    // * Return proper error type.
    //
    // pub unsafe fn process_queue(&self) -> Result<(), ()> {
    pub unsafe fn process_queue(&self) {
        // fn fulfill_request(state: &AtomicUsize, kind: RequestKind) {
        //     debug_assert_eq!(state.load(Acquire) & WRITE_LOCKED, 0);

        //     match kind {
        //         RequestKind::Read => {
                    
        //         },
        //         RequestKind::Write => {
        //             state.store(WRITE_LOCKED, SeqCst)
        //         },
        //     }
        // }

        if PRINT_DEBUG { println!("Processing queue..."); }

        loop {
            // match self.inner.state.load(SeqCst) {
            match self.inner.state.fetch_or(PROCESSING, SeqCst) {
                // Unlocked:
                0 => {
                    if PRINT_DEBUG { println!("Processing queue: Unlocked"); }
                    let new_state = self.fulfill_request(0);
                    self.inner.state.store(new_state, SeqCst);
                    break;
                },

                // Write locked, unset PROCESSING flag:
                WRITE_LOCKED => {
                    if PRINT_DEBUG { println!("Processing queue: Write Locked"); }
                    self.inner.state.store(WRITE_LOCKED, SeqCst);
                    break;
                },

                // Either read locked or already being processed:
                state => {
                    if PRINT_DEBUG { println!("Processing queue: Other {{ state: {}, peek: {:?} }}", 
                        state, self.peek_request_kind()); }

                    // Ensure that if WRITE_LOCKED is set, PROCESSING is also set.
                    debug_assert!(
                        (state & PROCESSING == PROCESSING) == (state & WRITE_LOCKED == WRITE_LOCKED) ||
                        (state & PROCESSING == PROCESSING) && !(state & WRITE_LOCKED == WRITE_LOCKED)
                    );

                    // If not already being processed, check for additional readers:
                    if state & PROCESSING != PROCESSING {
                        debug_assert!(state <= READ_COUNT_MASK);

                        if self.peek_request_kind() == Some(RequestKind::Read) {
                            // We are read locked and the next request is a read.
                            let new_state = self.fulfill_request(state);
                            self.inner.state.store(new_state, SeqCst);
                        } else {
                            // Either the next request is empty or a write and
                            // we are already read locked. Leave the request
                            // there and restore our original state, removing
                            // the PROCESSING flag.
                            self.inner.state.store(state, SeqCst);
                        }

                        break;
                    }
                    // If already being processed or the next request is a write
                    // request, do nothing.
                },
            }
        }
    }

    /// Decreases the reader count by one and unparks the next requester task
    /// in the queue if possible.
    //
    // TODO: Consider using `Ordering::Release`.
    pub unsafe fn release_reader(&self) {
        if PRINT_DEBUG { println!("Releasing read lock..."); }
        // Ensure we are read locked and not processing or write locked:
        debug_assert!(self.inner.state.load(SeqCst) & READ_COUNT_MASK != 0);
        debug_assert_eq!(self.inner.state.load(SeqCst) & WRITE_LOCKED, 0);

        // self.inner.state.fetch_sub(1, SeqCst);

        loop {
            match self.inner.state.fetch_or(PROCESSING, SeqCst) {
                0 => unreachable!(),
                WRITE_LOCKED => unreachable!(),
                state => {
                    if state & PROCESSING != PROCESSING {
                        debug_assert!(state > 0 && state <= READ_COUNT_MASK);
                        self.inner.state.store(state - 1, SeqCst);
                        break;                   
                    }
                }
            }
        }

        self.process_queue()
    }

    /// Unlocks this lock and unparks the next requester task in the queue if
    /// possible.
    //
    // TODO: Consider using `Ordering::Release`.
    pub unsafe fn release_writer(&self) {
        if PRINT_DEBUG { println!("Releasing write lock..."); }
        // Ensure we are write locked and are not processing or read locked:
        debug_assert_eq!(self.inner.state.load(SeqCst) & WRITE_LOCKED, WRITE_LOCKED);
        debug_assert!(self.inner.state.load(SeqCst) & READ_COUNT_MASK == 0);

        loop {
            match self.inner.state.fetch_or(PROCESSING, SeqCst) {
                0 => unreachable!(),
                WRITE_LOCKED => {
                    self.inner.state.store(0, SeqCst);
                    break;
                },
                state => debug_assert_eq!(state, PROCESSING),
            }
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
// Woefully incomplete:
mod tests {
    use std::thread;
    use super::*;    

    #[test]
    fn simple() {
        let qutex = QrwLock::from(0i32);

        let (future_r0_a, future_r0_b) = (qutex.clone().request_read(), qutex.clone().request_read());
        let future_r0 = qutex.clone().request_read()
            .and_then(|guard| {
                assert_eq!(*guard, 0);
                println!("val[r0]: {}", *guard);
                guard.unlock();

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
            });

        let future_w0 = qutex.clone().request_write()
            .and_then(|mut guard| {
                *guard = 5;
                println!("val is now: {}", *guard);
                Ok(())
            });

        let future_r1 = qutex.clone().request_read()
            .and_then(|guard| {
                assert_eq!(*guard, 5);
                println!("val[r1]: {}", *guard);
                Ok(())
            });

        let future_r2 = qutex.clone().request_read()
            .and_then(|guard| {
                assert_eq!(*guard, 5);
                println!("val[r2]: {}", *guard);
                Ok(())
            });

        future_r0.join4(future_w0, future_r1, future_r2).wait().unwrap();

        let future_guard = qutex.clone().request_read();
        let guard = future_guard.wait().unwrap();
        assert_eq!(*guard, 5);
    }
    
    #[test]
    fn concurrent() {
        let start_val = 0i32;
        let qutex = QrwLock::new(start_val);
        let thread_count = 20;        
        let mut threads = Vec::with_capacity(thread_count);

        for _i in 0..thread_count {
            let future_write_guard = qutex.clone().request_write();
            let future_read_guard = qutex.clone().request_read();

            let future_write = future_write_guard.and_then(|mut guard| {
                *guard += 1;
                Ok(())
            });

            let future_read = future_read_guard.and_then(move |_guard| {
                // println!("Value for thread '{}' is: {}", _i, *_guard);
                Ok(())
            });

            threads.push(thread::spawn(|| {
                future_write.join(future_read).wait().unwrap();
            }));
        
        }

        for i in 0..thread_count {
            let future_write_guard = qutex.clone().request_write();

            threads.push(thread::Builder::new().name(format!("test_thread_{}", i)).spawn(|| {
                let mut guard = future_write_guard.wait().unwrap();
                *guard -= 1
            }).unwrap());

            let future_read_guard = qutex.clone().request_read();

            threads.push(thread::Builder::new().name(format!("test_thread_{}", i)).spawn(|| {
                let _val = *future_read_guard.wait().unwrap();
            }).unwrap());
        }

        for thread in threads {
            thread.join().unwrap();
        }

        let guard = qutex.clone().request_read().wait().unwrap();
        assert_eq!(*guard, start_val);
    }
}