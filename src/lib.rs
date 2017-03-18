//! A collection of locking data structures, both thread-safe and
//! single-thread-optimized, which use Rust futures instead of
//! thread-blocking.
//!
//!

extern crate crossbeam;
extern crate futures;

pub mod qutex;

pub use self::qutex::{Request, Guard, FutureGuard, Qutex};