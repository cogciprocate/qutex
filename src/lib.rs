//! A collection of locking data structures, both thread-safe and
//! single-thread-optimized, which use Rust futures instead of
//! thread-blocking.
//!
//! [![](https://img.shields.io/badge/github-qutex-blue.svg)][repo] [![](http://meritbadge.herokuapp.com/qutex)](https://crates.io/crates/qutex)
//!
//! [repo]: https://github.com/cogciprocate/qutex

extern crate crossbeam;
extern crate futures;

#[cfg(feature = "async_await")]
mod async_await;

mod qrw_lock;
mod qutex;

pub use self::qrw_lock::{
    FutureReadGuard, FutureWriteGuard, QrwLock, QrwRequest, ReadGuard, RequestKind, WriteGuard,
};
pub use self::qutex::{FutureGuard, Guard, Qutex, Request};

#[cfg(feature = "async_await")]
pub use async_await::*;
