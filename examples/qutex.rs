extern crate futures;
extern crate qutex;

use futures::Future;
use qutex::Qutex;
use std::thread;

fn main() {
    let thread_count = 100;
    let mut threads = Vec::with_capacity(thread_count);
    let start_val = 0;

    // Create a `Qutex` protecting a start value of zero.
    let qutex = Qutex::new(start_val);

    // Spawn several threads, each adding 1 to the protected value.
    for _ in 0..thread_count {
        // Obtain a 'guard' (akin to a `std::sync::MutexGuard`).
        let future_val = qutex.clone().lock();

        // Add 1 to the protected value. `future_val` is a `FutureGuard` which
        // will resolve to a `Guard` providing mutable access to the protected
        // value. The guard can be passed between futures combinators and will
        // unlock the `Qutex` when dropped.
        let future_add = future_val.map(|mut val| {
            *val += 1;
        });

        // Spawn a thread which blocks upon completion of the above lock and
        // add operations.
        threads.push(thread::spawn(|| {
            future_add.wait().unwrap();
        }));
    }

    for thread in threads {
        thread.join().unwrap();
    }

    let val = qutex.lock().wait().unwrap();
    assert_eq!(*val, start_val + thread_count);
    println!("Qutex final value: {}", *val);
}
