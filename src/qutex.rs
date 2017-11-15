//! A queue-backed exclusive data lock.
//!
//
// * It is unclear how many of the unsafe methods within need actually remain
//   unsafe.

use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::SeqCst;
use std::cell::UnsafeCell;
use futures::{Future, Poll, Canceled, Async};
use futures::sync::oneshot::{self, Sender, Receiver};
use crossbeam::sync::SegQueue;


/// Allows access to the data contained within a lock just like a mutex guard.
#[derive(Debug)]
pub struct Guard<T> {
    qutex: Qutex<T>,
}

impl<T> Guard<T> {
    /// Releases the lock held by a `Guard` and returns the original `Qutex`.
    pub fn unlock(guard: Guard<T>) -> Qutex<T> {
        // guard.qutex.clone()
        let qutex = unsafe { ::std::ptr::read(&guard.qutex) };
        ::std::mem::forget(guard);
        qutex
    }
}

impl<T> Deref for Guard<T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.qutex.inner.cell.get() }
    }
}

impl<T> DerefMut for Guard<T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.qutex.inner.cell.get() }
    }
}

impl<T> Drop for Guard<T> {
    fn drop(&mut self) {
        // unsafe { self.qutex.direct_unlock().expect("Error dropping Guard") };
        unsafe { self.qutex.direct_unlock() }
    }
}


/// A future which resolves to a `Guard`.
#[must_use = "futures do nothing unless polled"]
#[derive(Debug)]
pub struct FutureGuard<T> {
    qutex: Option<Qutex<T>>,
    rx: Receiver<()>,
}

impl<T> FutureGuard<T> {
    /// Returns a new `FutureGuard`.
    fn new(qutex: Qutex<T>, rx: Receiver<()>) -> FutureGuard<T> {
        FutureGuard {
            qutex: Some(qutex),
            rx: rx,
        }
    }

    /// Blocks the current thread until this future resolves.
    #[inline]
    pub fn wait(self) -> Result<Guard<T>, Canceled> {
        <Self as Future>::wait(self)
    }
}

impl<T> Future for FutureGuard<T> {
    type Item = Guard<T>;
    type Error = Canceled;

    #[inline]
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if self.qutex.is_some() {
            unsafe {
                self.qutex.as_ref().unwrap().process_queue()
                    // .expect("Error polling FutureGuard");
            }

            match self.rx.poll() {
                Ok(status) => Ok(status.map(|_| {
                    // fence(SeqCst);
                    Guard { qutex: self.qutex.take().unwrap() }
                })),
                Err(e) => Err(e.into()),
            }
        } else {
            panic!("FutureGuard::poll: Task already completed.");
        }
    }
}

impl<T> Drop for FutureGuard<T> {
    /// Gracefully unlock if this guard has a lock acquired.
    fn drop(&mut self) {
        if let Some(qutex) = self.qutex.take() {
            self.rx.close();

            match self.rx.poll() {
                Ok(status) => {
                    match status {
                        Async::Ready(_) => {
                            unsafe { qutex.direct_unlock(); }
                        },
                        Async::NotReady => (),
                    }
                },
                Err(_) => (),
            }
        }
    }
}


/// A request to lock the qutex for exclusive access.
#[derive(Debug)]
pub struct Request {
    tx: Sender<()>,
}

impl Request {
    /// Returns a new `Request`.
    pub fn new(tx: Sender<()>) -> Request {
        Request { tx: tx }
    }
}

#[derive(Debug)]
struct Inner<T> {
    // TODO: Convert to `AtomicBool` if no additional states are needed:
    state: AtomicUsize,
    cell: UnsafeCell<T>,
    queue: SegQueue<Request>,
}

impl<T> From<T> for Inner<T> {
    #[inline]
    fn from(val: T) -> Inner<T> {
        Inner {
            state: AtomicUsize::new(0),
            cell: UnsafeCell::new(val),
            queue: SegQueue::new(),
        }
    }
}

unsafe impl<T: Send> Send for Inner<T> {}
unsafe impl<T: Send> Sync for Inner<T> {}


/// A lock-free-queue-backed exclusive data lock.
#[derive(Debug)]
pub struct Qutex<T> {
    inner: Arc<Inner<T>>,
}

impl<T> Qutex<T> {
    /// Creates and returns a new `Qutex`.
    #[inline]
    pub fn new(val: T) -> Qutex<T> {
        Qutex {
            inner: Arc::new(Inner::from(val)),
        }
    }

    /// Returns a new `FutureGuard` which can be used as a future and will
    /// resolve into a `Guard`.
    pub fn lock(self) -> FutureGuard<T> {
        let (tx, rx) = oneshot::channel();
        unsafe { self.push_request(Request::new(tx)); }
        FutureGuard::new(self, rx)
    }

    /// Pushes a lock request onto the queue.
    ///
    //
    // TODO: Evaluate unsafe-ness.
    //
    #[inline]
    pub unsafe fn push_request(&self, req: Request) {
        self.inner.queue.push(req);
    }

    /// Returns a mutable reference to the inner `Vec` if there are currently
    /// no other copies of this `Qutex`.
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
    #[inline]
    pub fn as_ptr(&self) -> *const T {
        self.inner.cell.get()
    }

    /// Returns a mutable reference to the inner value.
    ///
    #[inline]
    pub fn as_mut_ptr(&self) -> *mut T {
        self.inner.cell.get()
    }

    /// Pops the next lock request in the queue if this (the caller's) lock is
    /// unlocked.
    //
    // TODO:
    // * This is currently public due to 'derivers' (aka. sub-types). Evaluate.
    // * Consider removing unsafe qualifier.
    // * Return proper error type.
    //
    // pub unsafe fn process_queue(&self) -> Result<(), ()> {
    pub unsafe fn process_queue(&self) {
        match self.inner.state.compare_and_swap(0, 1, SeqCst) {
            // Unlocked:
            0 => {
                loop {
                    if let Some(req) = self.inner.queue.try_pop() {
                        // If there is a send error, a requester has dropped
                        // its receiver so just go to the next.
                        if req.tx.send(()).is_err() {
                            continue;
                        } else {
                            // return Ok(())
                            break;
                        }
                    } else {
                        self.inner.state.store(0, SeqCst);
                        // return Ok(());
                        break;
                    }
                }
            },
            // Already locked, leave it alone:
            // 1 => Ok(()),
            1 => (),
            // Something else:
            n => panic!("Qutex::process_queue: inner.state: {}.", n),
        }
    }

    /// Unlocks this (the caller's) lock and wakes up the next task in the
    /// queue.
    //
    // TODO:
    // * Evaluate unsafe-ness.
    // * Return proper error type
    // pub unsafe fn direct_unlock(&self) -> Result<(), ()> {
    pub unsafe fn direct_unlock(&self) {
        // TODO: Consider using `Ordering::Release`.
        self.inner.state.store(0, SeqCst);
        // fence(SeqCst);
        self.process_queue()
    }
}

impl<T> From<T> for Qutex<T> {
    #[inline]
    fn from(val: T) -> Qutex<T> {
        Qutex::new(val)
    }
}

// Avoids needing `T: Clone`.
impl<T> Clone for Qutex<T> {
    #[inline]
    fn clone(&self) -> Qutex<T> {
        Qutex {
            inner: self.inner.clone(),
        }
    }
}


#[cfg(test)]
// Woefully incomplete:
mod tests {
    use super::*;

    #[test]
    fn simple() {
        let val = Qutex::from(999i32);

        println!("Reading val...");
        {
            let future_guard = val.clone().lock();
            let guard = future_guard.wait().unwrap();
            println!("val: {}", *guard);
        }

        println!("Storing new val...");
        {
            let future_guard = val.clone().lock();
            let mut guard = future_guard.wait().unwrap();
            *guard = 5;
        }

        println!("Reading val...");
        {
            let future_guard = val.clone().lock();
            let guard = future_guard.wait().unwrap();
            println!("val: {}", *guard);
        }
    }

    #[test]
    fn concurrent() {
        use std::thread;

        let thread_count = 20;
        let mut threads = Vec::with_capacity(thread_count);
        let start_val = 0i32;
        let qutex = Qutex::new(start_val);

        for i in 0..thread_count {
            let future_guard = qutex.clone().lock();

            let future_write = future_guard.and_then(|mut guard| {
                *guard += 1;
                Ok(())
            });

            threads.push(thread::Builder::new().name(format!("test_thread_{}", i)).spawn(|| {
                future_write.wait().unwrap();
            }).unwrap());

        }

        for i in 0..thread_count {
            let future_guard = qutex.clone().lock();

            threads.push(thread::Builder::new().name(format!("test_thread_{}", i + thread_count)).spawn(|| {
                let mut guard = future_guard.wait().unwrap();
                *guard -= 1;
            }).unwrap())
        }

        for thread in threads {
            thread.join().unwrap();
        }

        let guard = qutex.clone().lock().wait().unwrap();
        assert_eq!(*guard, start_val);
    }
}