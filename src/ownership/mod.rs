use std::cell::{Cell, RefCell};
use std::collections::hash_map::Entry;
use std::mem;

use either::Either;
use rustc_abi::Size;
use rustc_const_eval::interpret::{
    AllocId, AllocRange, InterpCx, InterpResult, Pointer, interp_ok,
};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};
use rustc_middle::{throw_ub_format, throw_unsup_format};

use self::fraction::Fraction;
use crate::concurrency::thread::EvalContextExt;
use crate::machine::Provenance;
use crate::{MiriMachine, OpTy, ThreadId};

pub mod fraction;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Ownable {
    BlockToken(AllocId),
    AllocedByte { alloc_id: AllocId, offset: Size },
}

#[derive(Debug)]
enum FractionalityAssertion {
    Fully,
    Partially,
}

enum FractionalitySplit {
    All,
    AtLeast(Fraction),
    #[allow(unused)]
    Partially,
}

#[derive(Debug)]
pub struct FunctionFrame {
    owned: FxHashMap<Ownable, Fraction>,
    /// This is `None` for the dummy base frame at the start of each thread.
    /// For "main" this contains all the statics (TODO),
    /// and for other threads this contains everything that will be given to the `join`ing thread.
    real_call_stack_idx: Option<usize>,
    current_mode: TransferMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferMode {
    IntoChild,
    IntoParent,
}

pub struct ThreadState {
    /// The stack of functions that have an ownership annotation.
    /// (If non-ownership-annotated funcs are called, we treat it as if they were inlined)
    /// It is an invariant that this vec is not empty, the 0th element is always a `thread_base` frame where
    /// `real_call_stack_idx == None`. All others above have `real_call_stack_idx == Some(_)`.
    call_stack: Vec<FunctionFrame>,
}

pub struct GlobalStateInner {
    thread_state: FxHashMap<ThreadId, ThreadState>,
    magic_skipped_ids: FxHashSet<AllocId>,
    skippidy: Cell<bool>,
}

pub type GlobalState = RefCell<GlobalStateInner>;

impl FunctionFrame {
    fn assert(&self, ownable: &Ownable, mode: &FractionalityAssertion) -> bool {
        let Some(frac) = self.owned.get(ownable) else {
            // println!("Assertion that _ is {mode:?} failed immediately!");
            return false;
        };
        // println!("Assertion that {frac} is {mode:?}");
        match mode {
            FractionalityAssertion::Fully => frac.is_one(),
            FractionalityAssertion::Partially => !frac.is_zero(),
        }
    }
    fn remove(&mut self, ownable: &Ownable, mode: &FractionalitySplit) -> Option<Fraction> {
        let Some(frac) = self.owned.get_mut(ownable) else {
            return None;
        };
        match mode {
            FractionalitySplit::All => self.owned.remove(ownable),
            FractionalitySplit::AtLeast(wanted) if &*frac >= &wanted => {
                *frac -= wanted.clone();
                if frac.is_zero() {
                    self.owned.remove(ownable);
                } else {
                    // println!("After removing {wanted}, left with {frac}!")
                }
                Some(wanted.clone())
            }
            FractionalitySplit::Partially => Some(frac.halve_in_place()),
            _ => None,
        }
    }
    fn insert(&mut self, ownable: Ownable, amount: Fraction) {
        match self.owned.entry(ownable) {
            Entry::Occupied(mut occupied_entry) => {
                let oe = occupied_entry.get_mut();
                *oe += amount;
            }
            Entry::Vacant(vacant_entry) => {
                vacant_entry.insert(amount);
            }
        }
    }

    pub fn handle_alloc(&mut self, alloc_id: AllocId, size: Size) {
        for off in 0..size.bytes() {
            self.insert(
                Ownable::AllocedByte { alloc_id, offset: Size::from_bytes(off) },
                Fraction::one(),
            );
        }
        self.insert(Ownable::BlockToken(alloc_id), Fraction::one());
    }

    pub fn handle_access(&mut self, alloc_id: AllocId, range: AllocRange, is_write: bool) -> bool {
        let mode = if is_write {
            FractionalityAssertion::Fully
        } else {
            FractionalityAssertion::Partially
        };
        for off in range.start.bytes()..range.end().bytes() {
            // println!("Access to {alloc_id:?} at offset {off}");
            if !self
                .assert(&Ownable::AllocedByte { alloc_id, offset: Size::from_bytes(off) }, &mode)
            {
                return false;
            }
        }
        return true;
    }

    pub fn handle_dealloc(&mut self, alloc_id: AllocId, size: Size) -> bool {
        for off in 0..size.bytes() {
            if let Some(fr) = self.remove(
                &Ownable::AllocedByte { alloc_id, offset: Size::from_bytes(off) },
                &FractionalitySplit::All,
            ) && fr.is_one()
            {
                continue;
            }
            return false;
        }
        if let Some(fr) = self.remove(&Ownable::BlockToken(alloc_id), &FractionalitySplit::All)
            && fr.is_one()
        {
            return true;
        }
        return false;
    }

    pub fn new_thread_base() -> Self {
        Self {
            owned: FxHashMap::default(),
            real_call_stack_idx: None,
            current_mode: TransferMode::IntoChild,
        }
    }

    pub fn new(idx: usize) -> Self {
        Self {
            owned: FxHashMap::default(),
            real_call_stack_idx: Some(idx),
            current_mode: TransferMode::IntoChild,
        }
    }
}

impl ThreadState {
    fn new() -> Self {
        Self { call_stack: vec![FunctionFrame::new_thread_base()] }
    }

    fn transfer(&mut self, ownable: Ownable, mode: FractionalitySplit) -> bool {
        assert!(self.call_stack.len() >= 2);
        let mut iter = self.call_stack.iter_mut().rev();
        let stack_top = iter.next().unwrap();
        let stack_top_parent = iter.next().unwrap();
        let (into, from) = match stack_top.current_mode {
            TransferMode::IntoChild => (stack_top, stack_top_parent),
            TransferMode::IntoParent => (stack_top_parent, stack_top),
        };
        let Some(taken) = from.remove(&ownable, &mode) else {
            return false;
        };
        into.insert(ownable, taken);
        return true;
    }

    fn start_frame(&mut self, idx: usize) {
        self.call_stack.push(FunctionFrame::new(idx));
    }

    fn for_topmost_frame<T, F: for<'a> FnOnce(&'a mut FunctionFrame) -> T>(&mut self, f: F) -> T {
        f(self.call_stack.last_mut().unwrap())
    }
}

fn for_all_locals_in_current_thread<'tcx, F: FnMut(AllocId)>(
    this: &InterpCx<'tcx, MiriMachine<'tcx>>,
    offset: usize,
    include_return_place: bool,
    mut f: F,
) {
    let curfunc = &this.active_thread_stack()[offset];
    for x in &curfunc.locals {
        match x.as_mplace_or_imm() {
            Some(Either::Left((ptr, _))) => {
                if let Ok((alloc_id, _, _)) = this.ptr_try_get_alloc_id(ptr, 0) {
                    f(alloc_id)
                }
            }
            _ => continue,
        }
    }
    if include_return_place
        && let Ok((alloc_id, _, _)) = this.ptr_try_get_alloc_id(curfunc.return_place.ptr(), 0)
    {
        f(alloc_id);
    }
}

fn is_alloc_local_in_active_thread_stack_frame<'tcx>(
    this: &InterpCx<'tcx, MiriMachine<'tcx>>,
    offset: usize,
    id: AllocId,
) -> bool {
    let mut res = false;
    {
        let res = &mut res;
        for_all_locals_in_current_thread(this, offset, false, |aid| {
            if id == aid {
                // println!("Found alloc {id:?} at offset {offset:?}");
            }
            *res |= id == aid;
        });
    }
    res
}

fn is_alloc_local_in_included_stack_frames<'tcx>(
    this: &InterpCx<'tcx, MiriMachine<'tcx>>,
    topmost_offset: Option<usize>,
    id: AllocId,
) -> bool {
    for offset in (topmost_offset.unwrap_or(0))..=this.frame_idx() {
        if is_alloc_local_in_active_thread_stack_frame(this, offset, id) {
            return true;
        }
    }
    return false;
}

impl GlobalStateInner {
    pub fn new(magic_skipped_ids: FxHashSet<AllocId>) -> Self {
        let mut thread_state = FxHashMap::default();
        thread_state.insert(ThreadId::MAIN_THREAD, ThreadState::new());
        Self { thread_state, magic_skipped_ids, skippidy: Cell::new(false) }
    }

    pub fn new_thread(&mut self, tid: ThreadId) {
        let x = self.thread_state.insert(tid, ThreadState::new());
        //TODO: we probably don't clean up all threads when we should.
        // This assert will at least tell us when we don't
        assert!(x.is_none());
    }

    pub fn join_thread(&mut self, this_thread: ThreadId, joining_thread: ThreadId) {
        assert_ne!(this_thread, joining_thread);
        let Some(ot) = self.thread_state.remove(&joining_thread) else {
            return;
        };
        let Some(tt) = self.thread_state.get_mut(&this_thread) else {
            return;
        };
        assert!(ot.call_stack.len() == 1 && ot.call_stack[0].real_call_stack_idx.is_none());
        let last_frame = ot.call_stack.into_iter().next().unwrap();
        let ttframe = tt.call_stack.last_mut().unwrap();
        for (ownable, amount) in last_frame.owned {
            ttframe.insert(ownable, amount);
        }
    }

    pub fn on_stack_pop(&mut self, tid: ThreadId, popped_id: usize) {
        let Some(ts) = self.thread_state.get_mut(&tid) else {
            return;
        };
        if ts.call_stack.last().unwrap().real_call_stack_idx == Some(popped_id) {
            // println!("Ending ownership frame, offset: {}", ts.call_stack.len() - 1);
            let popped = ts.call_stack.pop().unwrap();
            let last = ts.call_stack.last_mut().unwrap();
            for (ownable, amount) in popped.owned {
                // println!("transferring {amount} ownership of {ownable:?} into parent");
                last.insert(ownable, amount);
            }
        }
    }

    fn for_topmost_frame<'tcx, T, F: for<'a> FnOnce(&'a mut FunctionFrame) -> T>(
        &mut self,
        this: &MiriMachine<'tcx>,
        f: F,
    ) -> T {
        self.thread_state.get_mut(&this.threads.active_thread()).unwrap().for_topmost_frame(f)
    }

    pub fn handle_memory_alloc<'tcx>(
        &mut self,
        this: &MiriMachine<'tcx>,
        alloc_id: AllocId,
        size: Size,
    ) -> InterpResult<'tcx, ()> {
        if self.skippidy.get() {
            return interp_ok(());
        }
        if self.magic_skipped_ids.contains(&alloc_id) {
            return interp_ok(());
        }
        // println!("Allocing {alloc_id:?} of size {size:?}");
        self.for_topmost_frame(this, |f| f.handle_alloc(alloc_id, size));
        interp_ok(())
    }

    pub fn handle_memory_access<'tcx>(
        &mut self,
        this: &MiriMachine<'tcx>,
        alloc_id: AllocId,
        range: AllocRange,
        is_write: bool,
    ) -> InterpResult<'tcx, ()> {
        if self.skippidy.get() {
            return interp_ok(());
        }
        if self.magic_skipped_ids.contains(&alloc_id) {
            return interp_ok(());
        }
        if !self.for_topmost_frame(this, |f| f.handle_access(alloc_id, range, is_write)) {
            throw_ub_format!("Insufficient permissions for access {alloc_id:?} {range:?}!");
        }
        interp_ok(())
    }

    pub fn handle_memory_dealloc<'tcx>(
        &mut self,
        this: &MiriMachine<'tcx>,
        alloc_id: AllocId,
        size: Size,
    ) -> InterpResult<'tcx, ()> {
        if self.skippidy.get() {
            return interp_ok(());
        }
        if self.magic_skipped_ids.contains(&alloc_id) {
            return interp_ok(());
        }
        // println!("Freeing {alloc_id:?} of size {size:?}");
        if !self.for_topmost_frame(this, |f| f.handle_dealloc(alloc_id, size)) {
            throw_ub_format!("Insufficient permissions for deallocation of {alloc_id:?}!");
        }
        interp_ok(())
    }

    pub fn handle_start_frame<'tcx>(
        &mut self,
        this: &InterpCx<'tcx, MiriMachine<'tcx>>,
        parent_nr: &OpTy<'tcx>,
    ) -> InterpResult<'tcx, ()> {
        let parent_nr: u64 = this.read_target_usize(parent_nr)?;
        let thread_data = self.thread_state.get_mut(&this.active_thread()).unwrap();
        let mut current_stack_size = this.active_thread_stack().len();
        if parent_nr >= current_stack_size as u64 {
            throw_ub_format!("No {parent_nr}th parent of the current function found!");
        }
        // this can not overflow, due to the check above
        current_stack_size -= parent_nr as usize;
        thread_data.start_frame(current_stack_size);
        // println!("Started ownership frame, offset: {}", thread_data.call_stack.len() - 1);
        for offset in current_stack_size..=this.frame_idx() {
            for_all_locals_in_current_thread(
                this,
                offset,
                offset == current_stack_size,
                |alloc_id| {
                    // println!("  transferring {alloc_id:?} into new frame");
                    let info = this.get_alloc_info(alloc_id);
                    for offset in 0..info.size.bytes() {
                        thread_data.transfer(
                            Ownable::AllocedByte {
                                alloc_id: alloc_id,
                                offset: Size::from_bytes(offset),
                            },
                            FractionalitySplit::All,
                        );
                    }
                    thread_data.transfer(Ownable::BlockToken(alloc_id), FractionalitySplit::All);
                },
            );
        }
        // let x = &this.active_thread_stack()[current_stack_size];
        // println!(
        //     "Corresponds to the following span: {:?}, and a function named {:?}",
        //     x.current_span(),
        //     x.instance()
        // );
        interp_ok(())
    }
    pub fn handle_start_postcondition<'tcx>(
        &mut self,
        this: &InterpCx<'tcx, MiriMachine<'tcx>>,
    ) -> InterpResult<'tcx, ()> {
        // println!(
        //     "Started postcondition in frame {}",
        //     self.thread_state.get(&this.active_thread()).unwrap().call_stack.len() - 1
        // );
        if !self.for_topmost_frame(&this.machine, |top| {
            if top.current_mode != TransferMode::IntoChild {
                return false;
            }
            top.current_mode = TransferMode::IntoParent;
            return true;
        }) {
            throw_ub_format!("`miri_start_ownership_postcondition` called twice!")
        }
        interp_ok(())
    }

    fn handle_owned_raw_inner<'tcx>(
        &mut self,
        this: &InterpCx<'tcx, MiriMachine<'tcx>>,
        owned_at_ptr: Pointer<Option<Provenance>>,
        owned_size: u64,
    ) -> InterpResult<'tcx, ()> {
        let thread_data = self.thread_state.get_mut(&this.active_thread()).unwrap();
        let Ok(owned_size_s) = owned_size.try_into() else {
            throw_unsup_format!(
                "`miri_owned_raw`: can not own {owned_size} many bytes, this is too large!"
            );
        };
        let (alloc_id, alloc_offset, _) = this.ptr_get_alloc_id(owned_at_ptr, owned_size_s)?;
        // println!(
        //     "Got Owned() call at {alloc_id:?}:{alloc_offset:?} for {owned_size:?} many bytes! (extra info: top frame's real frame is {:?})",
        //     thread_data.for_topmost_frame(|x| x.real_call_stack_idx)
        // );
        if self.magic_skipped_ids.contains(&alloc_id) {
            // println!("  Skipping cause it's ignored!");
            return interp_ok(());
        }
        if is_alloc_local_in_included_stack_frames(
            this,
            thread_data.for_topmost_frame(|x| x.real_call_stack_idx),
            alloc_id,
        ) {
            // println!("  Skipping cause it's local!");
            // hack alert
            // if we are owning locals of the current function, abort.
            // this is because we want to e.g. call owned_vec() on an argument, but we already own the local backing the storage
            return interp_ok(());
        }
        for off in 0..owned_size {
            if !thread_data.transfer(
                Ownable::AllocedByte {
                    alloc_id,
                    offset: Size::from_bytes(alloc_offset.bytes().checked_add(off).unwrap()),
                },
                FractionalitySplit::AtLeast(Fraction::one()),
            ) {
                throw_ub_format!("not (enough) ownership when calling `miri_owned_raw`!")
            }
        }
        interp_ok(())
    }

    pub fn handle_owned_raw<'tcx>(
        this: &mut InterpCx<'tcx, MiriMachine<'tcx>>,
        dest_ptr: &OpTy<'tcx>,
        owned_at_ptr: &OpTy<'tcx>,
        owned_size: &OpTy<'tcx>,
    ) -> InterpResult<'tcx, Pointer<Option<Provenance>>> {
        let dest_ptr = this.read_pointer(dest_ptr)?;
        let owned_at_ptr = this.read_pointer(owned_at_ptr)?;
        let owned_size = this.read_target_usize(owned_size)?;

        let Some(me) = this.machine.ownership.as_ref() else {
            throw_unsup_format!(
                "`miri_owned_raw` requires that ownership tracking is enabled in Miri"
            );
        };

        me.borrow_mut().handle_owned_raw_inner(this, owned_at_ptr, owned_size)?;

        me.borrow().skippidy.set(true);
        // this is supposed to be called with a "ghost" buffer that owns an "actual" buffer
        // they should never overlap, that does not make sense
        this.mem_copy(owned_at_ptr, dest_ptr, Size::from_bytes(owned_size), true)?;
        this.machine.ownership.as_ref().unwrap().borrow().skippidy.set(false);
        interp_ok(dest_ptr)
    }
    pub fn handle_owned_block_token<'tcx>(
        &mut self,
        this: &InterpCx<'tcx, MiriMachine<'tcx>>,
        block: &OpTy<'tcx>,
        size: &OpTy<'tcx>,
    ) -> InterpResult<'tcx, ()> {
        let block = this.read_pointer(block)?;
        let size = this.read_target_usize(size)?;
        let thread_data = self.thread_state.get_mut(&this.active_thread()).unwrap();
        let Ok(size_s) = size.try_into() else {
            throw_unsup_format!(
                "`miri_owned_block_token`: can not own {size} many bytes, this is too large!"
            );
        };
        let (alloc_id, alloc_offset, _) = this.ptr_get_alloc_id(block, size_s)?;
        if self.magic_skipped_ids.contains(&alloc_id) {
            // println!("  Skipping cause it's ignored!");
            return interp_ok(());
        }
        let alloc_info = this.get_alloc_info(alloc_id);
        if alloc_offset.bytes() != 0 {
            throw_ub_format!(
                "`miri_owned_block_token`: first argument does not point to beginning of block!"
            );
        };
        if size != alloc_info.size.bytes() {
            throw_ub_format!(
                "`miri_owned_block_token`: block size is {}, not {size}!",
                alloc_info.size.bytes()
            );
        };
        if !thread_data
            .transfer(Ownable::BlockToken(alloc_id), FractionalitySplit::AtLeast(Fraction::one()))
        {
            throw_ub_format!("not (enough) ownership when calling `miri_owned_block_token`!")
        }
        interp_ok(())
    }
}
