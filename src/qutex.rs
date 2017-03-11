//! A mutex-like lock which can be shared between threads.
//!

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
    lock: Qutex<T>,
}

impl<T> Deref for Guard<T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.lock.inner.cell.get() }
    }
}

impl<T> DerefMut for Guard<T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.inner.cell.get() }
    }
}

impl<T> Drop for Guard<T> {
    fn drop(&mut self) {
        self.lock.unlock();
    }
}


pub struct FutureGuard<T> {
    lock: Option<Qutex<T>>,
    rx: oneshot::Receiver<()>,
}

impl<T> FutureGuard<T> {
    pub fn wait(self) -> Result<Guard<T>, Canceled> {
        <Self as Future>::wait(self)
    }
}

impl<T> Future for FutureGuard<T> {
    type Item = Guard<T>;
    type Error = Canceled;

    #[inline]
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if self.lock.is_some() {
            self.lock.as_ref().unwrap().process_queue();

            match self.rx.poll() {
                Ok(status) => Ok(status.map(|_| {
                    Guard { lock: self.lock.take().unwrap() }
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


////// [BRING ME BACK ONE DAY USING GENERICS]:
// /// Like a `FutureGuard` but additionally waits on an OpenCL event.
// pub struct PendingGuard<T> {
//     lock: Option<Qutex<T>>,
//     rx: oneshot::Receiver<()>,
//     wait_event: Event,
//     trigger_event: Event,
//     // rx_is_complete: bool,
//     // ev_is_complete: bool,
// }

// impl<T> PendingGuard<T> {
//     fn new(lock: Option<Qutex<T>>, rx: oneshot::Receiver<()>, wait_event: Event) -> OclResult<PendingGuard<T>> {
//         let trigger_event = Event::user(&wait_event.context()?)?;

//         Ok(PendingGuard {
//             lock: lock,
//             rx: rx,
//             wait_event: wait_event,
//             trigger_event: trigger_event,
//             // rx_is_complete: false,
//             // ev_is_complete: false,
//         })
//     }

//     pub fn trigger_event(&self) -> &Event {
//         &self.trigger_event
//     }

//     pub fn wait(self) -> AsyncResult<Guard<T>> {
//         <Self as Future>::wait(self)
//     }

//     pub unsafe fn cell_mut(&self) -> Option<&mut T> {
//         self.lock.as_ref().map(|l| &mut *l.inner.cell.get())
//     }
// }

// impl<T> Future for PendingGuard<T> {
//     type Item = Guard<T>;
//     type Error = AsyncError;

//     #[inline]
//     fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
//         if self.lock.is_some() {
//             self.lock.as_ref().unwrap().process_queue();

//             if !self.wait_event.is_complete()? {
//                 let task_ptr = standard::box_raw_void(task::park());
//                     unsafe { self.wait_event.set_callback(standard::_unpark_task, task_ptr)?; };
//                     return Ok(Async::NotReady);
//             }

//             match self.rx.poll() {
//                 Ok(status) => Ok(status.map(|_| {
//                     Guard { lock: self.lock.take().unwrap() }
//                 })),
//                 Err(e) => return Err(e.into()),
//             }
//         } else {
//             Err("PendingGuard::poll: Task already completed.".into())
//         }
//     }
// }


struct Request {
    tx: oneshot::Sender<()>,
    // wait_event: Option<Event>,
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

    /// Pops the next lock request in the queue if this lock is unlocked.
    fn process_queue(&self) {
        match self.inner.state.compare_and_swap(0, 1, SeqCst) {
            // Unlocked:
            0 => {
                if let Some(req) = self.inner.queue.try_pop() {
                    req.tx.complete(());
                } else {
                    self.inner.state.store(0, SeqCst);
                }
            },
            // Already locked, leave it alone:
            1 => (),
            // Something else:
            n => panic!("Qutex::process_queue: inner.state: {}.", n),
        }
    }

    /// Returns a new `FutureGuard` which can be used as a future and will
    /// resolve into a `Guard`.
    pub fn lock(&self) -> FutureGuard<T> {
        let (tx, rx) = oneshot::channel();
        // self.inner.queue.push(Request { tx: tx, wait_event: None });
        self.inner.queue.push(Request { tx: tx });
        FutureGuard { lock: Some((*self).clone()), rx: rx }
    }

    /// Unlocks this lock and wakes up the next task in the queue.
    fn unlock(&self) {
        // TODO: Consider using `Ordering::Release`.
        self.inner.state.store(0, SeqCst);
        self.process_queue();
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
}

impl<T> From<T> for Qutex<T> {
    #[inline]
    fn from(val: T) -> Qutex<T> {
        Qutex {
            inner: Arc::new(Inner::from(val))
        }
    }
}

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
            let future_guard = val.lock();
            let guard = future_guard.wait().unwrap();
            println!("val: {}", *guard);
        }

        println!("Storing new val...");
        {
            let future_guard = val.lock();
            let mut guard = future_guard.wait().unwrap();
            *guard = 5;
        }

        println!("Reading val...");
        {
            let future_guard = val.lock();
            let guard = future_guard.wait().unwrap();
            println!("val: {}", *guard);
        }
    }

    #[test]
    // TODO: INCOMPLETE: Actually make this concurrent.
    fn concurrent() {
        let val = Qutex::from(10000i32);

        let fg0 = val.lock();
        let fg1 = val.lock();
        let fg2 = val.lock();

        println!("Reading val 0...");
        {
            let guard = fg0.wait().unwrap();
            println!("val: {}", *guard);
        }

        println!("Reading val 1...");
        {
            let guard = fg1.wait().unwrap();
            println!("val: {}", *guard);
        }

        println!("Reading val 2...");
        {
            let guard = fg2.wait().unwrap();
            println!("val: {}", *guard);
        }
    }
}