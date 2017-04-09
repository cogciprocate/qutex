//! A collection of locking data structures, both thread-safe and
//! single-thread-optimized, which use Rust futures instead of
//! thread-blocking.
//!
//! [![](https://img.shields.io/badge/github-qutex-blue.svg)][repo] [![](http://meritbadge.herokuapp.com/qutex)](https://crates.io/crates/qutex)
//!
//! [repo]: https://github.com/cogciprocate/qutex

extern crate crossbeam;
extern crate futures;

mod qutex;
mod qrw_lock;

pub use self::qutex::{Guard, FutureGuard, Request, Qutex};
pub use self::qrw_lock::{ReadGuard, WriteGuard, FutureReadGuard, FutureWriteGuard, 
	QrwRequest, RequestKind, QrwLock};