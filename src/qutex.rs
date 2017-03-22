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
use futures::{Future, Poll, Canceled};
use futures::sync::oneshot;
use crossbeam::sync::SegQueue;


 // Allows access to the data contained within a lock just like a mutex guard.
pub struct Guard<T> {
    qutex: Qutex<T>,
}

impl<T> Guard<T> {
    /// Releases the lock held by this `Guard` and returns the original `Qutex`.
    //
    // NOTE: This increments the `Qutex` reference count before immediately
    // decrementing it. Is it worth avoiding this by wrapping it in an Option
    // or using unsafe hackery? It would make uglier code for the measly
    // savings of two atomic stores...
    pub fn unlock(self) -> Qutex<T> {
        self.qutex.clone()
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
        unsafe { self.qutex.direct_unlock().expect("Error dropping Guard") };
    }
}


/// A future which resolves to a `Guard`.
pub struct FutureGuard<T> {
    qutex: Option<Qutex<T>>,
    rx: oneshot::Receiver<()>,
}

impl<T> FutureGuard<T> {
    /// Returns a new `FutureGuard`.
    fn new(qutex: Qutex<T>, rx: oneshot::Receiver<()>) -> FutureGuard<T> {
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
            unsafe { self.qutex.as_ref().unwrap().process_queue()
                .expect("Error polling FutureGuard"); }

            match self.rx.poll() {
                Ok(status) => Ok(status.map(|_| {
                    Guard { qutex: self.qutex.take().unwrap() }
                })),
                Err(e) => Err(e.into()),
            }
        } else {
            ///// [KEEPME]:
            // Err("FutureGuard::poll: Task already completed.".into())
            panic!("FutureGuard::poll: Task already completed.");
        }
    }
}


/// A request to lock the qutex for exclusive access.
#[derive(Debug)]
pub struct Request {
    tx: oneshot::Sender<()>,
    // wait_event: Option<Event>,
}

impl Request {
    /// Returns a new `Request`.
    pub fn new(tx: oneshot::Sender<()>) -> Request {
        Request { tx: tx }
    }
}


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
    pub fn request_lock(self) -> FutureGuard<T> {
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

    /// Pops the next lock request in the queue if this lock is unlocked.
    //
    // TODO: 
    // * This is currently public due to 'derivers' (aka. sub-types). Evaluate.
    // * Consider removing unsafe qualifier.
    // * Return proper error type.
    //
    pub unsafe fn process_queue(&self) -> Result<(), &'static str> {
        match self.inner.state.compare_and_swap(0, 1, SeqCst) {
            // Unlocked:
            0 => {
                if let Some(req) = self.inner.queue.try_pop() {
                    // println!("Qutex::process_queue: Fulfilling lock request.");
                    req.tx.send(()).map_err(|_| "Qutex queue has been dropped")
                } else {
                    // println!("Qutex::process_queue: Queue is empty.");
                    self.inner.state.store(0, SeqCst);
                    Ok(())
                }
            },
            // Already locked, leave it alone:
            1 => Ok(()),
            // Something else:
            n => panic!("Qutex::process_queue: inner.state: {}.", n),
        }
    }

    /// Unlocks this lock and wakes up the next task in the queue.
    //
    // TODO: 
    // * Evaluate unsafe-ness.
    // * Return proper error type
    pub unsafe fn direct_unlock(&self) -> Result<(), &'static str> {
        // TODO: Consider using `Ordering::Release`.
        self.inner.state.store(0, SeqCst);
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
    #![allow(unused_variables, unused_imports, dead_code)]
    use super::*;

    #[test]
    fn simple() {
        let val = Qutex::from(999i32);

        println!("Reading val...");
        {
            let future_guard = val.clone().request_lock();
            let guard = future_guard.wait().unwrap();
            println!("val: {}", *guard);
        }

        println!("Storing new val...");
        {
            let future_guard = val.clone().request_lock();
            let mut guard = future_guard.wait().unwrap();
            *guard = 5;
        }

        println!("Reading val...");
        {
            let future_guard = val.clone().request_lock();
            let guard = future_guard.wait().unwrap();
            println!("val: {}", *guard);
        }
    }
    
    #[test]
    fn concurrent() {
        use std::thread;

        let start_val = 10000i32;
        let thread_count = 50;
        let qutex = Qutex::new(start_val);
        let mut threads = Vec::with_capacity(thread_count);

        for i in 0..thread_count {
            let future_guard = qutex.clone().request_lock();

            threads.push(thread::spawn(|| {
                let mut guard = future_guard.wait().unwrap();
                *guard += 1
            }))            
        }

        for thread in threads {
            thread.join().unwrap();
        }

        let guard = qutex.clone().request_lock().wait().unwrap();
        assert!(*guard == start_val + thread_count as i32);
    }
}