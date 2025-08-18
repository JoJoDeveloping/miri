#![feature(vec_into_raw_parts)]
use std::mem::{ManuallyDrop, MaybeUninit};

// run with
// MIRIFLAGS="-Zmiri-ownership"
// to support ownership tracking

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
/// Calling this with a zero-sized T is a memory no-op, but it still constructs a T, so if T is uninhabited it might be UB.
pub fn owned<T>(t: *const T) -> T {
    let t = t as *mut T;
    let mut tmu = MaybeUninit::<T>::uninit();
    unsafe {
        miri_owned_raw(tmu.as_mut_ptr() as *mut _, t as *mut _, size_of::<T>());
        tmu.assume_init()
    }
}

/// Similar to owned, the `block` assertion asserts that the block of memory starting at `t` has size `block_size` (in units of `T`),
/// and that we have the **unique, exclusive** right to free that memory. This does not mean that we have ownership
/// (as in `owned`) over all the bytes in that block. But freeing the block requires all the bytes, plus this special
/// `block` ownership. The point is that this forces us to specify how large our allocated memory blocks are.
pub fn block<T>(t: *const T, block_size: usize) {
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

pub trait Ownable {
    type Rep;
    fn to_rep(self: &Self) -> Self::Rep;
}

pub trait OwnableHL: Ownable {
    type HlRep: Clone;
    fn to_hl_rep_inner(ll_rep: Self::Rep) -> Self::HlRep;
    fn to_hl_rep(&self) -> Self::HlRep {
        Self::to_hl_rep_inner(self.to_rep())
    }
}

impl Ownable for i32 {
    type Rep = i32;
    fn to_rep(&self) -> i32 {
        *self
    }
}

impl OwnableHL for i32 {
    type HlRep = i32;
    fn to_hl_rep_inner(x: i32) -> i32 {
        x
    }
}

impl<'a, T: Ownable> Ownable for &'a mut T {
    type Rep = T::Rep;
    fn to_rep(&self) -> T::Rep {
        let x = ManuallyDrop::new(owned(&**self));
        let res = x.to_rep();
        res
    }
}

impl<'a, T: OwnableHL> OwnableHL for &'a mut T {
    type HlRep = T::HlRep;
    fn to_hl_rep_inner(x: T::Rep) -> T::HlRep {
        T::to_hl_rep_inner(x)
    }
}

impl Ownable for () {
    type Rep = ();
    fn to_rep(&self) -> () {
        *self
    }
}

impl OwnableHL for () {
    type HlRep = ();
    fn to_hl_rep_inner(x: ()) -> () {
        x
    }
}

impl<T> Ownable for Vec<T> {
    type Rep = (*const T, usize, usize);
    fn to_rep(&self) -> Self::Rep {
        (self.as_ptr(), self.capacity(), self.len())
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

    pub fn nil() -> List<T> {
        Box::new(Self::Nil)
    }

    pub fn cons(hd: T, tl: List<T>) -> List<T> {
        Box::new(Self::Cons(hd, tl))
    }
}

/// This trait defines the high-level (logical) abstraction behind a vector.
/// In computing this abstraction, it asserts all necessary ownership
impl<T: OwnableHL> OwnableHL for Vec<T> {
    // A vector is a list of the underlying type's logical abstraction
    type HlRep = List<T::HlRep>;
    fn to_hl_rep_inner((ptr, cap, len): Self::Rep) -> Self::HlRep {
        assert!(len <= cap);
        // Then, we compute the high-level list representation
        let mut res = ListB::nil();
        if size_of::<T>() == 0 {
            // ZST vectors are special
            assert_eq!(cap, usize::MAX);
            for _ in (0..len).rev() {
                // the ptr value does not matter, as it's zero-sized
                // this asserts that `T` is not uninhabited, as it should for length > 0
                res = ListB::cons(owned(ptr).to_hl_rep(), res);
            }
        } else {
            // If `cap` is 0, then the pointer is dangling, and we don't actually own anything else.
            if cap > 0 {
                // If `cap > 0`, then we own the block of memory which has the given size (in units of `T`).
                block(ptr, cap);
                // We also own a `T` for each offset in the vector up to `len`
                for i in (0..len).rev() {
                    res = ListB::cons(owned(ptr.wrapping_add(i)).to_hl_rep(), res);
                }
                // For the ones beyond `len`, we own them as well, but we don't care what is written there.
                for i in len..cap {
                    owned(ptr.wrapping_add(i) as *mut MaybeUninit<T>);
                }
            }
        }
        res
    }
}

/// A wrapper around Vec::new that tests the contract.
// #[requires(true)]
// #[ensures(move |result| {result.to_hl_rep() == []})]
fn vec_new<T: OwnableHL>() -> Vec<T> {
    {
        // This precondition always holds, because nothing is asserted.
        // We only call `precond()` so that we can call `postcond()` later.
        precond();
    }
    let res = Vec::new();
    {
        postcond();
        // The spec says that the resulting vector's high-level value is the empty list.
        let lst = res.to_hl_rep();
        assert!(matches!(&*lst, ListB::Nil))
    }
    res
}

/// A wrapper around Vec::push that tests the contract.
// #[requires(let pre_list = vec.to_hl_rep() && let t = topush.to_hl_rep() && true)]
// #[ensures(move |result| {vec.to_hl_rep() == pre_list ++ [t] })]
fn vec_push<T: OwnableHL>(vec: &mut Vec<T>, topush: T)
where
    <T as OwnableHL>::HlRep: Eq,
{
    // The spec will relate the values at the start to the values at the end.
    // At the start, we just know that we have some vector, and some `T`.
    let (pre_list, pre_t) = {
        precond();
        (vec.to_hl_rep(), topush.to_hl_rep())
    };
    Vec::push(vec, topush);
    {
        postcond();
        // In the postcond, we compare the final vector to the initial vector.
        // More specifically, the list representing the vector at the end is the list from the beginning, but with `pre_t` appended.
        let post_list = vec.to_hl_rep();
        // With some fancier notation, we would write `post_list == pre_list ++ [pre_t]`
        assert!(post_list == pre_list.append(ListB::cons(pre_t, ListB::nil())));
        // We don't use assert_eq since that requires a `Debug` implementation.
    }
}

#[derive(Clone)]
enum Empty {}

impl Ownable for Empty {
    type Rep = Empty;
    fn to_rep(&self) -> Empty {
        match *self {}
    }
}

impl OwnableHL for Empty {
    type HlRep = Empty;
    fn to_hl_rep_inner(x: Empty) -> Empty {
        match x {}
    }
}

fn main() {
    // We call our functions on a test case.
    // This checks that the specifications hold at various places throughout the program.
    let mut v1 = vec_new::<i32>();
    vec_push(&mut v1, 42);
    vec_push(&mut v1, 43);
    vec_push(&mut v1, 44);

    let mut v2 = vec_new::<()>();
    vec_push(&mut v2, ());
    vec_push(&mut v2, ());
    vec_push(&mut v2, ());
    assert_eq!(v1.len(), v2.len());

    let _v3 = vec_new::<Empty>();
    println!("Everything worked!");
}
