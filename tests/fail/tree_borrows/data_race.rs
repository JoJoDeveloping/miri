use std::sync::*;
use std::thread;

fn main() {
    // create a Mutex
    let data = Box::leak(Box::new(Mutex::new(42)));
    fn foo(x: &'static mut Mutex<i32>) {
        // we get the Mutex as a mutable reference. This means we
        // 1) get it with a protector
        // 2) can use non-atomic accesses
        *Mutex::get_mut(x).unwrap() = 21;
        // we have now written to the Mutex, non-atomically.
        // the protector will now protect us from _all_ foreign accesses,
        // and since we would really like to add/insert/reorder writes to mutable references,
        // there will be a protector end semantics write later.

        // create a shared reference to the mutex, which does not reborrow (due to interior mutability)
        let x = &*x;
        thread::spawn(move || {
            // we move the shared reference to another thread
            let x = x;
            // yield, to ensure the protector ends "first"
            std::thread::yield_now();
            // now lock the mutex and use it for something
            let mut xl = x.lock().unwrap();
            *xl = *xl + 1;
        });
        // here, the protector end access happens
        // if we count this as a data race, it will race with the mutex above.
    }
    thread::spawn(move || {
        foo(data);
    })
    .join()
    .unwrap();
}
