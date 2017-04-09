# Qutex [![](http://meritbadge.herokuapp.com/qutex)](https://crates.io/crates/qutex) [![](https://docs.rs/qutex/badge.svg)](https://docs.rs/qutex)

Non-thread-blocking queue-backed data locks based on Rust futures.

#### [Documentation](https://docs.rs/qutex)


## Example

`Cargo.toml`:

```rust
[dependencies] 
qutex = "0.0"
```

`main.rs`:

```rust
extern crate qutex;
extern crate futures;

use std::thread;
use futures::Future;
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

```