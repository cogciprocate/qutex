extern crate qutex;
extern crate futures;

use std::thread;
use futures::{executor, FutureExt};
use qutex::Qutex;

fn main() {
    let thread_count = 100;
    let mut threads = Vec::with_capacity(thread_count);
    let start_val = 0;
    let qutex = Qutex::new(start_val);

    for _ in 0..thread_count {
        let future_val = qutex.clone().lock();

        let future_add = future_val.and_then(|mut val| {
            *val += 1;
            Ok(())
        });

        threads.push(thread::spawn(|| {
            executor::block_on(future_add).unwrap();
        }));
    }

    for thread in threads {
        thread.join().unwrap();
    }

    let val = executor::block_on(qutex.lock()).unwrap();
    assert_eq!(*val, start_val + thread_count);
    println!("Qutex final value: {}", *val);
}