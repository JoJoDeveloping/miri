#![feature(vec_into_raw_parts)]
use std::mem::{ManuallyDrop, MaybeUninit};

// run with
// MIRIFLAGS="-Zmiri-ownership -Zmiri-ownership-ignore=5,6,401,403"
// the latter ignores are for consts accessed by ghost code
// which are there for various reasons unknown to me

// declare the miri helpers we need
unsafe extern "Rust" {
    pub fn miri_start_ownership_frame(parent_nr: usize);
    pub fn miri_start_ownership_postcondition();
    pub fn miri_owned_raw(dest_ptr: *mut u8, owned_at_ptr: *mut u8, owned_size: usize) -> *mut u8;
    pub fn miri_owned_block_token(block_ptr: *mut u8, block_size: usize);
}

/// The `owned` function asserts that we have exclusive ownership of the memory pointed at by `T`.
/// This is very much like a separation logic points-to. Think of it like an assertion, but you can only assert it once,
/// because whoever asserts it is guaranteed unique ownership.
/// The function returns the object pointed at by T. Miri will complain if this is an invalid value (i.e. uninitialized),
/// which is intended.
/// Since this is supposed to be used in specifications, which are *not* supposed to change program state, we return everything
/// wrapped in `ManuallyDrop`, so that one does not accidentally trigger `drop` glue after calling `owned` on e.g. a `Box`.
pub fn owned<T>(t: *const T) -> ManuallyDrop<T> {
    let t = t as *mut T;
    let mut tmu = MaybeUninit::<T>::uninit();
    unsafe {
        miri_owned_raw(tmu.as_mut_ptr() as *mut _, t as *mut _, size_of::<T>());
        ManuallyDrop::new(tmu.assume_init())
    }
}

/// Similar to owned, the `block` assertion asserts that the block of memory starting at `t` has size `block_size` (in units of `T`),
/// and that we have the **unique, exclusive** right to free that memory. This does not mean that we have ownership
/// (as in `owned`) over all the bytes in that block. But freeing the block requires all the bytes, plus this special
/// `block` ownership. The point is that this forces us to specify how large our allocated memory blocks are.
pub fn block<T>(t: *mut T, block_size: usize) {
    unsafe {
        miri_owned_block_token(t as *mut _, size_of::<T>() * block_size);
    }
}

/// This needs to be called before starting a precondition block.
pub fn precond() {
    unsafe {
        miri_start_ownership_frame(2);
    }
}

/// This needs to be called before starting a postcondition block.
pub fn postcond() {
    unsafe {
        miri_start_ownership_postcondition();
    }
}

/// A simple linked list for use in specifications.
/// Ignore the `Box`, this is supposed to represent a functional/mathematical value
#[derive(PartialEq, Eq, Clone, Debug)]
pub enum ListB<T> {
    Nil,
    Cons(T, List<T>),
}
pub type List<T> = Box<ListB<T>>;

impl<T> ListB<T> {
    /// Appends two list, returning the result.
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

/// This function says that we own a the `Vec` pointed at by `T`.
/// It computes a mathematical abstraction, i.e. a list.
/// It also asserts all the necessary ownership.
pub fn owned_vec<T>(t: *const Vec<T>) -> List<ManuallyDrop<T>> {
    // Zero-sized types are compicated, this is a simplified example ignoring them
    assert_ne!(size_of::<T>(), 0);
    // First, we actually own the "immediate" `Vec` data.
    let vec = owned(t);
    let (ptr, len, cap) = ManuallyDrop::into_inner(vec).into_raw_parts();
    // Then, we compute the high-level list representation
    let mut res = Box::new(ListB::Nil);
    // If `cap` is 0, then the pointer is dangling, and we don't actually own anything else.
    if cap > 0 {
        // If `cap > 0`, then we own the block of memory which has the given size (in units of `T`).
        block(ptr, cap);
        // We also own a `T` for each offset in the vector up to `len`
        for i in (0..len).rev() {
            res = Box::new(ListB::Cons(owned(ptr.wrapping_add(i)), res));
        }
        // For the ones beyond `len`, we own them as well, but we don't care what is written there.
        for i in len..cap {
            owned(ptr.wrapping_add(i) as *mut MaybeUninit<T>);
        }
    }
    res
}

/// A wrapper around Vec::new that tests the contract.
fn vec_new<T>() -> Vec<T> {
    {
        // The spec says we can always call this.
        precond();
    }
    let mut res = Vec::new();
    {
        postcond();
        // The spec says that the resulting vector's high-level value is the empty list.
        let lst = owned_vec(&raw mut res);
        assert!(matches!(&*lst, ListB::Nil))
    }
    res
}

/// A wrapper around Vec::push that tests the contract.
fn vec_push<T: Eq>(vec: &mut Vec<T>, topush: T) {
    // The spec will relate the values at the start to the values at the end.
    // At the start, we just know that we have some vector, and some `T`.
    let (pre_list, pre_t) = {
        precond();
        (owned_vec(&raw mut *vec), owned(&raw const topush))
    };
    Vec::push(vec, topush);
    {
        postcond();
        // In the postcond, we compare the final vector to the initial vector.
        // More specifically, the list representing the vector at the end is the list from the beginning, but with `pre_t` appended.
        let post_list = owned_vec(&raw mut *vec);
        // With some fancier notation, we would write `post_list == pre_list ++ [pre_t]`
        assert!(post_list == pre_list.append(Box::new(ListB::Cons(pre_t, Box::new(ListB::Nil)))));
        // We don't use assert_eq since that requires a `Debug` implementation.
    }
}

fn main() {
    // We call our functions on a test case.
    // This checks that the specifications hold at various places throughout the program.
    let mut v1 = vec_new::<i32>();
    vec_push(&mut v1, 42);
    vec_push(&mut v1, 43);
    vec_push(&mut v1, 44);
}
