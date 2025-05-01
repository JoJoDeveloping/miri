use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};
use std::thread;

extern "Rust" {
    pub fn miri_write_to_stdout(bytes: &[u8]);
    pub fn miri_get_alloc_id(ptr: *const ()) -> u64;
    pub fn miri_print_borrow_state(alloc_id: u64, show_unnamed: bool);
}

fn print_state<T>(x: *const T) {
    unsafe {
        miri_print_borrow_state(miri_get_alloc_id(x as *const ()), true);
    }
}

// Miri magic to println!() without synchronization
fn println(data: &str) {
    unsafe {
        miri_write_to_stdout(data.as_bytes());
        miri_write_to_stdout(b"\n");
    }
}

static ATOMIC: AtomicU8 = AtomicU8::new(0u8);

struct SyncPtr<T>(T);
unsafe impl<T> Sync for SyncPtr<T> {}
unsafe impl<T> Send for SyncPtr<T> {}

fn unknown_function() {
    thread::yield_now();
    thread::yield_now();
    println("Thread 1: releasing store");
    ATOMIC.store(1u8, Ordering::Release);
    for _ in 0..1000 {
        thread::yield_now();
    }
}

fn client(x: &mut u32) {
    println("Thread 1: writing non-atomically");
    *x = 0xDEADBEEF;

    unknown_function();

    println("Thread 1: place where we would be performing write");
    // uncomment for data race
    *x = 0xDEADBEEF;
    println("Thread 1: returning from client");
}

fn first_thread(m: *mut Mutex<u32>) {
    unsafe {
        // print_state(m);
        let m = &mut *m;
        client(m.get_mut().unwrap());
    }
    println("Thread 1: relaxed store, since thread 1 is about to finish");
    ATOMIC.store(2u8, Ordering::Relaxed);
}

fn evil_other_thread(x: *mut Mutex<u32>) {
    let xraw = SyncPtr(x);
    let _ = thread::spawn(|| {
        let xraw = xraw;
        first_thread(xraw.0)
    });

    loop {
        let yy = ATOMIC.load(Ordering::Acquire);
        println(&format!("Thread 2: waiting for acquire: loaded {yy}"));
        if yy >= 1 {
            break;
        }
        thread::yield_now();
    }
    println("Thread 2: waiting for relaxed");
    while ATOMIC.load(Ordering::Relaxed) < 2 {
        thread::yield_now();
    }
    println("Thread 2: accessing mutex");
    let x: &Mutex<u32> = unsafe { &*x };
    *x.lock().unwrap() += 1;
}

fn main() {
    let m = Box::leak(Box::new(Mutex::new(0u32)));
    let mp = m as *mut _;

    evil_other_thread(mp);

    let _ = unsafe { Box::from_raw(mp) };
}
