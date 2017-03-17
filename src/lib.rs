extern crate crossbeam;
extern crate futures;

pub mod qutex;

pub use self::qutex::{Request, Guard, FutureGuard, Qutex};