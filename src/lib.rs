//! A collection of locking data structures, both thread-safe and
//! single-thread-optimized, which use Rust futures instead of
//! thread-blocking.
//!
//!

extern crate crossbeam;
extern crate futures;

mod qutex;
mod qrw_lock;

pub use self::qutex::{Guard, FutureGuard, Request, Qutex};
pub use self::qrw_lock::{ReadGuard, WriteGuard, FutureReadGuard, FutureWriteGuard, 
	QrwRequest, QrwLock};