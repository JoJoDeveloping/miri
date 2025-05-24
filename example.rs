#![feature(vec_into_raw_parts)]
use std::mem::{ManuallyDrop, MaybeUninit};

unsafe extern "Rust" {
    pub fn miri_start_ownership_frame(parent_nr: usize);
    pub fn miri_start_ownership_postcondition();
    pub fn miri_owned_raw(dest_ptr: *mut u8, owned_at_ptr: *mut u8, owned_size: usize) -> *mut u8;
    pub fn miri_owned_block_token(block_ptr: *mut u8, block_size: usize);
}

// return ManuallyDrop to ensure this is not dropped.
// the value is supposed to be "ghost" so let's try to avoid accidentally freeing it
pub fn owned<T>(t: *const T) -> ManuallyDrop<T> {
    let t = t as *mut T;
    let mut tmu = MaybeUninit::<T>::uninit();
    unsafe {
        miri_owned_raw(tmu.as_mut_ptr() as *mut _, t as *mut _, size_of::<T>());
        ManuallyDrop::new(tmu.assume_init())
    }
}

pub fn block<T>(t: *mut T, block_size: usize) {
    unsafe {
        miri_owned_block_token(t as *mut _, block_size);
    }
}

pub fn precond() {
    unsafe {
        miri_start_ownership_frame(2);
    }
}

pub fn postcond() {
    unsafe {
        miri_start_ownership_postcondition();
    }
}

// a "ghost" immutable / pure list
#[derive(PartialEq, Eq, Clone)]
pub enum ListB<T> {
    Nil,
    Cons(T, List<T>),
}
pub type List<T> = Box<ListB<T>>;

impl<T> ListB<T> {
    // appends two lists
    // ghost code
    pub fn append(mut self: Box<Self>, other: Box<Self>) -> Box<Self> {
        let mut me = &mut self;
        loop {
            if matches!(&mut **me, ListB::Nil) {
                *me = other;
                break;
            } else {
                match &mut **me {
                    ListB::Nil => unreachable!(),
                    ListB::Cons(_, list_b) => me = list_b,
                }
            }
        }
        self
    }
}

// what does it mean to own a vec?
// this specifies the correctness invariant of a vector
pub fn owned_vec<T>(t: *const Vec<T>) -> List<ManuallyDrop<T>> {
    // zero-sized types are compicated, this is a simplified example ignoring them
    assert_ne!(size_of::<T>(), 0);
    let vec = owned(t);
    let (ptr, len, cap) = ManuallyDrop::into_inner(vec).into_raw_parts();
    let mut res = Box::new(ListB::Nil);
    if cap > 0 {
        // we own the block of memory backed by the vector, it has this size
        block(ptr, cap * size_of::<T>());
        for i in (0..len).rev() {
            res = Box::new(ListB::Cons(owned(ptr.wrapping_add(i)), res));
        }
        for i in len..cap {
            // we own the memory, but make no assertion about it
            owned(ptr.wrapping_add(i) as *mut MaybeUninit<T>);
        }
    }
    res
}

fn vec_new<T>() -> Vec<T> {
    {
        precond();
    }
    let mut res = Vec::new();
    {
        postcond();
        let lst = owned_vec(&raw mut res);
        assert!(matches!(&*lst, ListB::Nil))
    }
    res
}

fn vec_push<T: Eq>(vec: &mut Vec<T>, topush: T) {
    // this saves the initial values
    let (pre_list, pre_t) = {
        precond();
        (owned_vec(&raw mut *vec), owned(&raw const topush))
    };
    Vec::push(vec, topush);
    {
        postcond();
        // in the postcond, we compare ourselves to the inital values
        let post_list = owned_vec(&raw mut *vec);
        assert!(post_list == pre_list.append(Box::new(ListB::Cons(pre_t, Box::new(ListB::Nil)))));
    }
}

fn main() {
    let mut v1 = vec_new::<i32>();
    vec_push(&mut v1, 42);
    vec_push(&mut v1, 43);
    vec_push(&mut v1, 44);
}
