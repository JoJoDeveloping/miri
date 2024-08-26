//! In this file we handle the "Tree" part of Tree Borrows, i.e. all tree
//! traversal functions, optimizations to trim branches, and keeping track of
//! the relative position of the access to each node being updated. This of course
//! also includes the definition of the tree structure.
//!
//! Functions here manipulate permissions but are oblivious to them: as
//! the internals of `Permission` are private, the update process is a black
//! box. All we need to know here are
//! - the fact that updates depend only on the old state, the status of protectors,
//!   and the relative position of the access;
//! - idempotency properties asserted in `perms.rs` (for optimizations)

use std::{fmt, mem};

use smallvec::SmallVec;

use rustc_data_structures::fx::FxHashSet;
use rustc_span::Span;
use rustc_target::abi::Size;

use crate::borrow_tracker::tree_borrows::{
    diagnostics::{self, NodeDebugInfo, TbError, TransitionError},
    perms::PermTransition,
    unimap::{UniEntry, UniIndex, UniKeyMap, UniValMap},
    Permission,
};
use crate::borrow_tracker::{GlobalState, ProtectorKind};
use crate::*;

mod tests;

/// Data for a single *location*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct LocationState {
    /// A location is initialized when it is child-accessed for the first time (and the initial
    /// retag initializes the location for the range covered by the type), and it then stays
    /// initialized forever.
    /// For initialized locations, "permission" is the current permission. However, for
    /// uninitialized locations, we still need to track the "future initial permission": this will
    /// start out to be `default_initial_perm`, but foreign accesses need to be taken into account.
    /// Crucially however, while transitions to `Disabled` would usually be UB if this location is
    /// protected, that is *not* the case for uninitialized locations. Instead we just have a latent
    /// "future initial permission" of `Disabled`, causing UB only if an access is ever actually
    /// performed.
    /// Note that the tree root is also always initialized, as if the allocation was a write access.
    initialized: bool,
    /// This pointer's current permission / future initial permission.
    permission: Permission,
    /// Strongest foreign access whose effects have already been applied to
    /// this node and all its children since the last child access.
    /// This is `None` if the most recent access is a child access,
    /// `Some(Write)` if at least one foreign write access has been applied
    /// since the previous child access, and `Some(Read)` if at least one
    /// foreign read and no foreign write have occurred since the last child access.
    latest_foreign_access: Option<AccessKind>,
}

impl LocationState {
    /// Constructs a new initial state. It has neither been accessed, nor been subjected
    /// to any foreign access yet.
    /// The permission is not allowed to be `Active`.
    fn new_uninit(permission: Permission) -> Self {
        assert!(permission.is_initial() || permission.is_disabled());
        Self { permission, initialized: false, latest_foreign_access: None }
    }

    /// Constructs a new initial state. It has not yet been subjected
    /// to any foreign access. However, it is already marked as having been accessed.
    fn new_init(permission: Permission) -> Self {
        Self { permission, initialized: true, latest_foreign_access: None }
    }

    /// Check if the location has been initialized, i.e. if it has
    /// ever been accessed through a child pointer.
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Check if the state can exist as the initial permission of a pointer.
    ///
    /// Do not confuse with `is_initialized`, the two are almost orthogonal
    /// as apart from `Active` which is not initial and must be initialized,
    /// any other permission can have an arbitrary combination of being
    /// initial/initialized.
    /// FIXME: when the corresponding `assert` in `tree_borrows/mod.rs` finally
    /// passes and can be uncommented, remove this `#[allow(dead_code)]`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_initial(&self) -> bool {
        self.permission.is_initial()
    }

    pub fn permission(&self) -> Permission {
        self.permission
    }

    /// Apply the effect of an access to one location, including
    /// - applying `Permission::perform_access` to the inner `Permission`,
    /// - emitting protector UB if the location is initialized,
    /// - updating the initialized status (child accesses produce initialized locations).
    fn perform_access(
        &mut self,
        access_kind: AccessKind,
        rel_pos: AccessRelatedness,
        protected: bool,
    ) -> Result<PermTransition, TransitionError> {
        let old_perm = self.permission;
        let transition = Permission::perform_access(access_kind, rel_pos, old_perm, protected)
            .ok_or(TransitionError::ChildAccessForbidden(old_perm))?;
        self.initialized |= !rel_pos.is_foreign();
        self.permission = transition.applied(old_perm).unwrap();
        // Why do only initialized locations cause protector errors?
        // Consider two mutable references `x`, `y` into disjoint parts of
        // the same allocation. A priori, these may actually both be used to
        // access the entire allocation, as long as only reads occur. However,
        // a write to `y` needs to somehow record that `x` can no longer be used
        // on that location at all. For these uninitialized locations (i.e., locations
        // that haven't been accessed with `x` yet), we track the "future initial state":
        // it defaults to whatever the initial state of the tag is,
        // but the access to `y` moves that "future initial state" of `x` to `Disabled`.
        // However, usually a `Reserved -> Disabled` transition would be UB due to the protector!
        // So clearly protectors shouldn't fire for such "future initial state" transitions.
        //
        // See the test `two_mut_protected_same_alloc` in `tests/pass/tree_borrows/tree-borrows.rs`
        // for an example of safe code that would be UB if we forgot to check `self.initialized`.
        if protected && self.initialized && transition.produces_disabled() {
            return Err(TransitionError::ProtectedDisabled(old_perm));
        }
        Ok(transition)
    }

    /// Like `perform_access`, but ignores the diagnostics, and also is pure.
    /// As such, it returns `Some(x)` if the transition succeeded, or `None`
    /// if there was an error.
    #[allow(unused)]
    fn perform_access_no_fluff(
        mut self,
        access_kind: AccessKind,
        rel_pos: AccessRelatedness,
        protected: bool,
    ) -> Option<Self> {
        match self.perform_access(access_kind, rel_pos, protected) {
            Ok(_) => Some(self),
            Err(_) => None,
        }
    }

    // Helper to optimize the tree traversal.
    // The optimization here consists of observing thanks to the tests
    // `foreign_read_is_noop_after_foreign_write` and `all_transitions_idempotent`,
    // that there are actually just three possible sequences of events that can occur
    // in between two child accesses that produce different results.
    //
    // Indeed,
    // - applying any number of foreign read accesses is the same as applying
    //   exactly one foreign read,
    // - applying any number of foreign read or write accesses is the same
    //   as applying exactly one foreign write.
    // therefore the three sequences of events that can produce different
    // outcomes are
    // - an empty sequence (`self.latest_foreign_access = None`)
    // - a nonempty read-only sequence (`self.latest_foreign_access = Some(Read)`)
    // - a nonempty sequence with at least one write (`self.latest_foreign_access = Some(Write)`)
    //
    // This function not only determines if skipping the propagation right now
    // is possible, it also updates the internal state to keep track of whether
    // the propagation can be skipped next time.
    // It is a performance loss not to call this function when a foreign access occurs.
    fn skip_if_known_noop(
        &self,
        access_kind: AccessKind,
        rel_pos: AccessRelatedness,
    ) -> ContinueTraversal {
        if rel_pos.is_foreign() {
            let mut new_access_noop = match (self.latest_foreign_access, access_kind) {
                // Previously applied transition makes the new one a guaranteed
                // noop in the two following cases:
                // (1) justified by `foreign_read_is_noop_after_foreign_write`
                (Some(AccessKind::Write), AccessKind::Read) => true,
                // (2) justified by `all_transitions_idempotent`
                (Some(old), new) if old == new => true,
                // In all other cases there has been a recent enough
                // child access that the effects of the new foreign access
                // need to be applied to this subtree.
                _ => false,
            };
            if self.permission.is_disabled() {
                // A foreign access to a `Disabled` tag will have almost no observable effect.
                // It's a theorem that `Disabled` node have no protected initialized children,
                // and so this foreign access will never trigger any protector.
                // Further, the children will never be able to read or write again, since they
                // have a `Disabled` parents. Even further, all children of `Disabled` are one
                // of `ReservedIM`, `Disabled`, or a not-yet-accessed "lazy" permission thing.
                // The two former are already invariant under all foreign accesses, and for
                // the latter it does not really matter, since they can not be used/initialized
                // due to having a protected parent. So this only affects diagnostics, but the
                // blocking write will still be identified directly, just at a different tag.
                new_access_noop = true;
            }
            if self.permission.is_frozen() && access_kind == AccessKind::Read {
                // A foreign read to a `Frozen` tag will have almost no observable effect.
                // It's a theorem that `Frozen` nodes have no active children, so all children
                // already survive foreign reads. Foreign reads in general have almost no
                // effect, the only further thing they could do is make protected `Reserved`
                // nodes become conflicted, i.e. make them reject child writes for the further
                // duration of their protector. But such a child write is already rejected
                // because this node is frozen. So this only affects diagnostics, but the
                // blocking read will still be identified directly, just at a different tag.
                new_access_noop = true;
            }
            if new_access_noop {
                // Abort traversal if the new transition is indeed guaranteed
                // to be noop.
                // No need to update `self.latest_foreign_access`,
                // the type of the current streak among nonempty read-only
                // or nonempty with at least one write has not changed.
                ContinueTraversal::SkipSelfAndChildren
            } else {
                // Otherwise propagate this time, and also record the
                // access that just occurred so that we can skip the propagation
                // next time.
                ContinueTraversal::Recurse
            }
        } else {
            // A child access occurred, this breaks the streak of foreign
            // accesses in a row and the sequence since the previous child access
            // is now empty.
            ContinueTraversal::Recurse
        }
    }

    /// Records a new access, so that future access can potentially be skipped
    /// by `skip_if_known_noop`.
    /// The invariants for this function are closely coupled to the function above:
    /// It MUST be called on child accesses, and on foreign accesses MUST be called
    /// when `skip_if_know_noop` returns `Recurse`, and MUST NOT be called otherwise.
    fn record_new_access(&mut self, access_kind: AccessKind, rel_pos: AccessRelatedness) {
        if rel_pos.is_foreign() {
            self.latest_foreign_access = Some(access_kind);
        } else {
            self.latest_foreign_access = None;
        }
    }
}

impl fmt::Display for LocationState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.permission)?;
        if !self.initialized {
            write!(f, "?")?;
        }
        Ok(())
    }
}

/// Tree structure with both parents and children since we want to be
/// able to traverse the tree efficiently in both directions.
#[derive(Clone, Debug)]
pub struct Tree {
    /// Mapping from tags to keys. The key obtained can then be used in
    /// any of the `UniValMap` relative to this allocation, i.e. both the
    /// `nodes` and `rperms` of the same `Tree`.
    /// The parent-child relationship in `Node` is encoded in terms of these same
    /// keys, so traversing the entire tree needs exactly one access to
    /// `tag_mapping`.
    pub(super) tag_mapping: UniKeyMap<BorTag>,
    /// All nodes of this tree.
    pub(super) nodes: UniValMap<Node>,
    /// Maps a tag and a location to a perm, with possible lazy
    /// initialization.
    ///
    /// NOTE: not all tags registered in `nodes` are necessarily in all
    /// ranges of `rperms`, because `rperms` is in part lazily initialized.
    /// Just because `nodes.get(key)` is `Some(_)` does not mean you can safely
    /// `unwrap` any `perm.get(key)`.
    ///
    /// We do uphold the fact that `keys(perms)` is a subset of `keys(nodes)`
    pub(super) rperms: RangeMap<UniValMap<LocationState>>,
    /// The index of the root node.
    pub(super) root: UniIndex,
}

/// A node in the borrow tree. Each node is uniquely identified by a tag via
/// the `nodes` map of `Tree`.
#[derive(Clone, Debug)]
pub(super) struct Node {
    /// The tag of this node.
    pub tag: BorTag,
    /// All tags except the root have a parent tag.
    pub parent: Option<UniIndex>,
    /// If the pointer was reborrowed, it has children.
    // FIXME: bench to compare this to FxHashSet and to other SmallVec sizes
    pub children: SmallVec<[UniIndex; 4]>,
    /// Either `Reserved`,  `Frozen`, or `Disabled`, it is the permission this tag will
    /// lazily be initialized to on the first access.
    /// It is only ever `Disabled` for a tree root, since the root is initialized to `Active` by
    /// its own separate mechanism.
    default_initial_perm: Permission,
    /// Some extra information useful only for debugging purposes
    pub debug_info: NodeDebugInfo,
}

/// Data given to the transition function
struct NodeAppArgs<'node> {
    /// Node on which the transition is currently being applied
    node: &'node mut Node,
    /// Mutable access to its permissions
    perm: UniEntry<'node, LocationState>,
    /// Relative position of the access
    rel_pos: AccessRelatedness,
}
/// Data given to the error handler
struct ErrHandlerArgs<'node, InErr> {
    /// Kind of error that occurred
    error_kind: InErr,
    /// Tag that triggered the error (not the tag that was accessed,
    /// rather the parent tag that had insufficient permissions or the
    /// non-parent tag that had a protector).
    conflicting_info: &'node NodeDebugInfo,
    /// Information about the tag that was accessed just before the
    /// error was triggered.
    accessed_info: &'node NodeDebugInfo,
}
/// Internal contents of `Tree` with the minimum of mutable access for
/// the purposes of the tree traversal functions: the permissions (`perms`) can be
/// updated but not the tree structure (`tag_mapping` and `nodes`)
struct TreeVisitor<'tree> {
    tag_mapping: &'tree UniKeyMap<BorTag>,
    nodes: &'tree mut UniValMap<Node>,
    perms: &'tree mut UniValMap<LocationState>,
}

/// Whether to continue exploring the children recursively or not.
enum ContinueTraversal {
    Recurse,
    SkipSelfAndChildren,
}

/// Stack of nodes left to explore in a tree traversal.
struct TreeVisitorStack<NodeContinue, NodeApp, ErrHandler> {
    /// Identifier of the original access.
    initial: UniIndex,
    /// Function describing whether to continue at a tag.
    /// This is only invoked for foreign accesses.
    f_continue: NodeContinue,
    /// Function to apply to each tag.
    f_propagate: NodeApp,
    /// Handler to add the required context to diagnostics.
    err_builder: ErrHandler,
    /// Mutable state of the visit: the tags left to handle.
    /// Every tag pushed should eventually be handled,
    /// and the precise order is relevant for diagnostics.
    /// Since the traversal is bottom-up, we need to remember
    /// whether we're here initially (false) or after visiting all
    /// children (true). The bool indicates this.
    stack: Vec<(UniIndex, AccessRelatedness, bool)>,
}

impl<NodeContinue, NodeApp, InnErr, OutErr, ErrHandler>
    TreeVisitorStack<NodeContinue, NodeApp, ErrHandler>
where
    NodeContinue: Fn(&NodeAppArgs<'_>) -> ContinueTraversal,
    NodeApp: Fn(NodeAppArgs<'_>) -> Result<(), InnErr>,
    ErrHandler: Fn(ErrHandlerArgs<'_, InnErr>) -> OutErr,
{
    fn should_continue_at(
        &self,
        this: &mut TreeVisitor<'_>,
        idx: UniIndex,
        rel_pos: AccessRelatedness,
    ) -> ContinueTraversal {
        let node = this.nodes.get_mut(idx).unwrap();
        let args = NodeAppArgs { node, perm: this.perms.entry(idx), rel_pos };
        (self.f_continue)(&args)
    }

    fn propagate_at(
        &mut self,
        this: &mut TreeVisitor<'_>,
        idx: UniIndex,
        rel_pos: AccessRelatedness,
    ) -> Result<(), OutErr> {
        let node = this.nodes.get_mut(idx).unwrap();
        (self.f_propagate)(NodeAppArgs { node, perm: this.perms.entry(idx), rel_pos }).map_err(
            |error_kind| {
                (self.err_builder)(ErrHandlerArgs {
                    error_kind,
                    conflicting_info: &this.nodes.get(idx).unwrap().debug_info,
                    accessed_info: &this.nodes.get(self.initial).unwrap().debug_info,
                })
            },
        )
    }

    fn go_upwards_from_accessed(
        &mut self,
        this: &mut TreeVisitor<'_>,
        accessed_node: UniIndex,
        push_children_of_accessed: bool,
    ) -> Result<(), OutErr> {
        assert!(self.stack.is_empty());
        // First, handle accessed node. A bunch of things need to
        // be handled differently here compared to the further parents
        // of `accesssed_node`.
        {
            self.propagate_at(this, accessed_node, AccessRelatedness::This)?;
            if push_children_of_accessed {
                let accessed_node = this.nodes.get(accessed_node).unwrap();
                for &child in accessed_node.children.iter().rev() {
                    self.stack.push((child, AccessRelatedness::AncestorAccess, false));
                }
            }
        }
        // Then, handle the accessed node's parent. Here, we need to
        // make sure we only mark the "cousin" subtrees for later visitation,
        // not the subtree that contains the accessed node.
        let mut last_node = accessed_node;
        while let Some(current) = this.nodes.get(last_node).unwrap().parent {
            self.propagate_at(this, current, AccessRelatedness::StrictChildAccess)?;
            let node = this.nodes.get(current).unwrap();
            for &child in node.children.iter().rev() {
                if last_node == child {
                    continue;
                }
                self.stack.push((child, AccessRelatedness::DistantAccess, false));
            }
            last_node = current;
        }
        self.stack.reverse();
        Ok(())
    }

    fn finish_foreign_accesses(&mut self, this: &mut TreeVisitor<'_>) -> Result<(), OutErr> {
        while let Some((idx, rel_pos, is_final)) = self.stack.last_mut() {
            let idx = *idx;
            let rel_pos = *rel_pos;
            if *is_final {
                self.stack.pop();
                self.propagate_at(this, idx, rel_pos)?;
            } else {
                *is_final = true;
                let handle_children = self.should_continue_at(this, idx, rel_pos);
                match handle_children {
                    ContinueTraversal::Recurse => {
                        // add all children, and also leave the node itself
                        // on the stack so that it can be visited later.
                        let node = this.nodes.get(idx).unwrap();
                        for &child in node.children.iter() {
                            self.stack.push((child, rel_pos, false));
                        }
                    }
                    ContinueTraversal::SkipSelfAndChildren => {
                        // skip self
                        self.stack.pop();
                        continue;
                    }
                }
            }
        }
        Ok(())
    }

    fn new(
        initial: UniIndex,
        f_continue: NodeContinue,
        f_propagate: NodeApp,
        err_builder: ErrHandler,
    ) -> Self {
        Self { initial, f_continue, f_propagate, err_builder, stack: Vec::new() }
    }
}

impl<'tree> TreeVisitor<'tree> {
    // Applies `f_propagate` to every vertex of the tree bottom-up in the following order: first
    // all ancestors of `start` (starting with `start` itself), then children of `start`, then the rest,
    // always going bottom-up.
    // This ensures that errors are triggered in the following order
    // - first invalid accesses with insufficient permissions, closest to the accessed node first,
    // - then protector violations, bottom-up, starting with the children of the accessed node, and then
    //   going upwards and outwards.
    //
    // `f_propagate` should follow the following format: for a given `Node` it updates its
    // `Permission` depending on the position relative to `start` (given by an
    // `AccessRelatedness`).
    // `f_continue` is called before on foreign nodes, and describes whether to continue
    // with the subtree at that node.
    fn traverse_this_parents_children_other<InnErr, OutErr>(
        mut self,
        start: BorTag,
        f_continue: impl Fn(&NodeAppArgs<'_>) -> ContinueTraversal,
        f_propagate: impl Fn(NodeAppArgs<'_>) -> Result<(), InnErr>,
        err_builder: impl Fn(ErrHandlerArgs<'_, InnErr>) -> OutErr,
    ) -> Result<(), OutErr> {
        let start_idx = self.tag_mapping.get(&start).unwrap();
        let mut stack = TreeVisitorStack::new(start_idx, f_continue, f_propagate, err_builder);
        // Visits the accessed node itself, and all its parents, i.e. all nodes
        // undergoing a child access. Also pushes the children and the other
        // cousin nodes (i.e. all nodes undergoing a foreign access) to the stack
        // to be processed later.
        stack.go_upwards_from_accessed(&mut self, start_idx, true)?;
        // Now visit all the foreign nodes we remembered earlier.
        // For this we go bottom-up, but also allow f_continue to skip entire
        // subtrees from being visited if it would be a NOP.
        stack.finish_foreign_accesses(&mut self)
    }

    // Like `traverse_this_parents_children_other`, but skips the children of `start`.
    fn traverse_nonchildren<InnErr, OutErr>(
        mut self,
        start: BorTag,
        f_continue: impl Fn(&NodeAppArgs<'_>) -> ContinueTraversal,
        f_propagate: impl Fn(NodeAppArgs<'_>) -> Result<(), InnErr>,
        err_builder: impl Fn(ErrHandlerArgs<'_, InnErr>) -> OutErr,
    ) -> Result<(), OutErr> {
        let start_idx = self.tag_mapping.get(&start).unwrap();
        let mut stack = TreeVisitorStack::new(start_idx, f_continue, f_propagate, err_builder);
        // Visits the accessed node itself, and all its parents, i.e. all nodes
        // undergoing a child access. Also pushes the other cousin nodes to the
        // stack, but not the children of the accessed node.
        stack.go_upwards_from_accessed(&mut self, start_idx, false)?;
        // Now visit all the foreign nodes we remembered earlier.
        // For this we go bottom-up, but also allow f_continue to skip entire
        // subtrees from being visited if it would be a NOP.
        stack.finish_foreign_accesses(&mut self)
    }
}

impl Tree {
    /// Create a new tree, with only a root pointer.
    pub fn new(root_tag: BorTag, size: Size, span: Span) -> Self {
        // The root has `Disabled` as the default permission,
        // so that any access out of bounds is invalid.
        let root_default_perm = Permission::new_disabled();
        let mut tag_mapping = UniKeyMap::default();
        let root_idx = tag_mapping.insert(root_tag);
        let nodes = {
            let mut nodes = UniValMap::<Node>::default();
            let mut debug_info = NodeDebugInfo::new(root_tag, root_default_perm, span);
            // name the root so that all allocations contain one named pointer
            debug_info.add_name("root of the allocation");
            nodes.insert(
                root_idx,
                Node {
                    tag: root_tag,
                    parent: None,
                    children: SmallVec::default(),
                    default_initial_perm: root_default_perm,
                    debug_info,
                },
            );
            nodes
        };
        let rperms = {
            let mut perms = UniValMap::default();
            // We manually set it to `Active` on all in-bounds positions.
            // We also ensure that it is initialized, so that no `Active` but
            // not yet initialized nodes exist. Essentially, we pretend there
            // was a write that initialized these to `Active`.
            perms.insert(root_idx, LocationState::new_init(Permission::new_active()));
            RangeMap::new(size, perms)
        };
        Self { root: root_idx, nodes, rperms, tag_mapping }
    }
}

impl<'tcx> Tree {
    /// Insert a new tag in the tree
    pub fn new_child(
        &mut self,
        parent_tag: BorTag,
        new_tag: BorTag,
        default_initial_perm: Permission,
        reborrow_range: AllocRange,
        span: Span,
    ) -> InterpResult<'tcx> {
        assert!(!self.tag_mapping.contains_key(&new_tag));
        let idx = self.tag_mapping.insert(new_tag);
        let parent_idx = self.tag_mapping.get(&parent_tag).unwrap();
        // Create the node
        self.nodes.insert(
            idx,
            Node {
                tag: new_tag,
                parent: Some(parent_idx),
                children: SmallVec::default(),
                default_initial_perm,
                debug_info: NodeDebugInfo::new(new_tag, default_initial_perm, span),
            },
        );
        // Register new_tag as a child of parent_tag
        self.nodes.get_mut(parent_idx).unwrap().children.push(idx);
        // Initialize perms
        let perm = LocationState::new_init(default_initial_perm);
        for (_perms_range, perms) in self.rperms.iter_mut(reborrow_range.start, reborrow_range.size)
        {
            perms.insert(idx, perm);
        }
        Ok(())
    }

    /// Deallocation requires
    /// - a pointer that permits write accesses
    /// - the absence of Strong Protectors anywhere in the allocation
    pub fn dealloc(
        &mut self,
        tag: BorTag,
        access_range: AllocRange,
        global: &GlobalState,
        alloc_id: AllocId, // diagnostics
        span: Span,        // diagnostics
    ) -> InterpResult<'tcx> {
        self.perform_access(
            tag,
            Some((access_range, AccessKind::Write, diagnostics::AccessCause::Dealloc)),
            global,
            alloc_id,
            span,
        )?;
        for (perms_range, perms) in self.rperms.iter_mut(access_range.start, access_range.size) {
            TreeVisitor { nodes: &mut self.nodes, tag_mapping: &self.tag_mapping, perms }
                .traverse_this_parents_children_other(
                    tag,
                    // visit all children, skipping none
                    |_| ContinueTraversal::Recurse,
                    |args: NodeAppArgs<'_>| -> Result<(), TransitionError> {
                        let NodeAppArgs { node, .. } = args;
                        if global.borrow().protected_tags.get(&node.tag)
                            == Some(&ProtectorKind::StrongProtector)
                        {
                            Err(TransitionError::ProtectedDealloc)
                        } else {
                            Ok(())
                        }
                    },
                    |args: ErrHandlerArgs<'_, TransitionError>| -> InterpError<'tcx> {
                        let ErrHandlerArgs { error_kind, conflicting_info, accessed_info } = args;
                        TbError {
                            conflicting_info,
                            access_cause: diagnostics::AccessCause::Dealloc,
                            alloc_id,
                            error_offset: perms_range.start,
                            error_kind,
                            accessed_info,
                        }
                        .build()
                    },
                )?;
        }
        Ok(())
    }

    /// Map the per-node and per-location `LocationState::perform_access`
    /// to each location of the first component of `access_range_and_kind`,
    /// on every tag of the allocation.
    ///
    /// If `access_range_and_kind` is `None`, this is interpreted as the special
    /// access that is applied on protector release:
    /// - the access will be applied only to initialized locations of the allocation,
    /// - it will not be visible to children,
    /// - it will be recorded as a `FnExit` diagnostic access
    /// - and it will be a read except if the location is `Active`, i.e. has been written to,
    ///   in which case it will be a write.
    ///
    /// `LocationState::perform_access` will take care of raising transition
    /// errors and updating the `initialized` status of each location,
    /// this traversal adds to that:
    /// - inserting into the map locations that do not exist yet,
    /// - trimming the traversal,
    /// - recording the history.
    pub fn perform_access(
        &mut self,
        tag: BorTag,
        access_range_and_kind: Option<(AllocRange, AccessKind, diagnostics::AccessCause)>,
        global: &GlobalState,
        alloc_id: AllocId, // diagnostics
        span: Span,        // diagnostics
    ) -> InterpResult<'tcx> {
        use std::ops::Range;
        // Performs the per-node work:
        // - insert the permission if it does not exist
        // - perform the access
        // - record the transition
        // to which some optimizations are added:
        // - skip the traversal of the children in some cases
        // - do not record noop transitions
        //
        // `perms_range` is only for diagnostics (it is the range of
        // the `RangeMap` on which we are currently working).
        let node_skipper = |access_kind: AccessKind, args: &NodeAppArgs<'_>| -> ContinueTraversal {
            let NodeAppArgs { node, perm, rel_pos } = args;

            let old_state = perm
                .get()
                .copied()
                .unwrap_or_else(|| LocationState::new_uninit(node.default_initial_perm));
            old_state.skip_if_known_noop(access_kind, *rel_pos)
        };
        let node_app = |perms_range: Range<u64>,
                        access_kind: AccessKind,
                        access_cause: diagnostics::AccessCause,
                        args: NodeAppArgs<'_>|
         -> Result<(), TransitionError> {
            let NodeAppArgs { node, mut perm, rel_pos } = args;

            let old_state = perm.or_insert(LocationState::new_uninit(node.default_initial_perm));

            old_state.record_new_access(access_kind, rel_pos);

            let protected = global.borrow().protected_tags.contains_key(&node.tag);
            let transition = old_state.perform_access(access_kind, rel_pos, protected)?;
            // Record the event as part of the history
            if !transition.is_noop() {
                node.debug_info.history.push(diagnostics::Event {
                    transition,
                    is_foreign: rel_pos.is_foreign(),
                    access_cause,
                    access_range: access_range_and_kind.map(|x| x.0),
                    transition_range: perms_range,
                    span,
                });
            }
            Ok(())
        };

        // Error handler in case `node_app` goes wrong.
        // Wraps the faulty transition in more context for diagnostics.
        let err_handler = |perms_range: Range<u64>,
                           access_cause: diagnostics::AccessCause,
                           args: ErrHandlerArgs<'_, TransitionError>|
         -> InterpError<'tcx> {
            let ErrHandlerArgs { error_kind, conflicting_info, accessed_info } = args;
            TbError {
                conflicting_info,
                access_cause,
                alloc_id,
                error_offset: perms_range.start,
                error_kind,
                accessed_info,
            }
            .build()
        };

        if let Some((access_range, access_kind, access_cause)) = access_range_and_kind {
            // Default branch: this is a "normal" access through a known range.
            // We iterate over affected locations and traverse the tree for each of them.
            for (perms_range, perms) in self.rperms.iter_mut(access_range.start, access_range.size)
            {
                TreeVisitor { nodes: &mut self.nodes, tag_mapping: &self.tag_mapping, perms }
                    .traverse_this_parents_children_other(
                        tag,
                        |args| node_skipper(access_kind, args),
                        |args| node_app(perms_range.clone(), access_kind, access_cause, args),
                        |args| err_handler(perms_range.clone(), access_cause, args),
                    )?;
            }
        } else {
            // This is a special access through the entire allocation.
            // It actually only affects `initialized` locations, so we need
            // to filter on those before initiating the traversal.
            //
            // In addition this implicit access should not be visible to children,
            // thus the use of `traverse_nonchildren`.
            // See the test case `returned_mut_is_usable` from
            // `tests/pass/tree_borrows/tree-borrows.rs` for an example of
            // why this is important.
            for (perms_range, perms) in self.rperms.iter_mut_all() {
                let idx = self.tag_mapping.get(&tag).unwrap();
                // Only visit initialized permissions
                if let Some(p) = perms.get(idx)
                    && p.initialized
                {
                    let access_kind =
                        if p.permission.is_active() { AccessKind::Write } else { AccessKind::Read };
                    let access_cause = diagnostics::AccessCause::FnExit(access_kind);
                    TreeVisitor { nodes: &mut self.nodes, tag_mapping: &self.tag_mapping, perms }
                        .traverse_nonchildren(
                        tag,
                        |args| node_skipper(access_kind, args),
                        |args| node_app(perms_range.clone(), access_kind, access_cause, args),
                        |args| err_handler(perms_range.clone(), access_cause, args),
                    )?;
                }
            }
        }
        Ok(())
    }
}

/// Integration with the BorTag garbage collector
impl Tree {
    pub fn remove_unreachable_tags(&mut self, live_tags: &FxHashSet<BorTag>) {
        self.remove_useless_children(self.root, live_tags);
        // Right after the GC runs is a good moment to check if we can
        // merge some adjacent ranges that were made equal by the removal of some
        // tags (this does not necessarily mean that they have identical internal representations,
        // see the `PartialEq` impl for `UniValMap`)
        self.rperms.merge_adjacent_thorough();
    }

    /// Checks if a node is useless and should be GC'ed.
    /// A node is useless if it has no children and also the tag is no longer live.
    fn is_useless(&self, idx: UniIndex, live: &FxHashSet<BorTag>) -> bool {
        let node = self.nodes.get(idx).unwrap();
        node.children.is_empty() && !live.contains(&node.tag)
    }

    /// Checks whether a node can be replaced by its only child.
    /// If so, returns the index of said only child.
    /// If not, returns none.
    fn can_be_replaced_by_single_child(
        &self,
        idx: UniIndex,
        live: &FxHashSet<BorTag>,
    ) -> Option<UniIndex> {
        let node = self.nodes.get(idx).unwrap();
        if node.children.len() != 1 || live.contains(&node.tag) || node.parent.is_none() {
            return None;
        }
        // Since protected nodes are never GC'd (see `borrow_tracker::GlobalStateInner::visit_provenance`),
        // we know that `node` is not protected because otherwise `live` would
        // have contained `node.tag`.
        let child_idx = node.children[0];
        let child = self.nodes.get(child_idx).unwrap();
        for (_, data) in self.rperms.iter_all() {
            let parent_perm =
                data.get(idx).map(|x| x.permission).unwrap_or_else(|| node.default_initial_perm);
            let child_perm = data
                .get(child_idx)
                .map(|x| x.permission)
                .unwrap_or_else(|| child.default_initial_perm);
            if !parent_perm.can_be_replaced_by_child(child_perm) {
                return None;
            }
        }

        Some(child_idx)
    }

    /// Properly removes a node.
    /// The node to be removed should not otherwise be usable. It also
    /// should have no children, but this is not checked, so that nodes
    /// whose children were rotated somewhere else can be deleted without
    /// having to first modify them to clear that array.
    /// otherwise (i.e. the GC should have marked it as removable).
    fn remove_useless_node(&mut self, this: UniIndex) {
        // Due to the API of UniMap we must make sure to call
        // `UniValMap::remove` for the key of this node on *all* maps that used it
        // (which are `self.nodes` and every range of `self.rperms`)
        // before we can safely apply `UniKeyMap::remove` to truly remove
        // this tag from the `tag_mapping`.
        let node = self.nodes.remove(this).unwrap();
        for (_perms_range, perms) in self.rperms.iter_mut_all() {
            perms.remove(this);
        }
        self.tag_mapping.remove(&node.tag);
    }

    /// Traverses the entire tree looking for useless tags.
    /// Removes from the tree all useless child nodes of root.
    /// It will not delete the root itself.
    ///
    /// NOTE: This leaves in the middle of the tree tags that are unreachable but have
    /// reachable children. There is a potential for compacting the tree by reassigning
    /// children of dead tags to the nearest live parent, but it must be done with care
    /// not to remove UB.
    ///
    /// Example: Consider the tree `root - parent - child`, with `parent: Frozen` and
    /// `child: Reserved`. This tree can exist. If we blindly delete `parent` and reassign
    /// `child` to be a direct child of `root` then Writes to `child` are now permitted
    /// whereas they were not when `parent` was still there.
    fn remove_useless_children(&mut self, root: UniIndex, live: &FxHashSet<BorTag>) {
        // To avoid stack overflows, we roll our own stack.
        // Each element in the stack consists of the current tag, and the number of the
        // next child to be processed.

        // The other functions are written using the `TreeVisitorStack`, but that does not work here
        // since we need to 1) do a post-traversal and 2) remove nodes from the tree.
        // Since we do a post-traversal (by deleting nodes only after handling all children),
        // we also need to be a bit smarter than "pop node, push all children."
        let mut stack = vec![(root, 0)];
        while let Some((tag, nth_child)) = stack.last_mut() {
            let node = self.nodes.get(*tag).unwrap();
            if *nth_child < node.children.len() {
                // Visit the child by pushing it to the stack.
                // Also increase `nth_child` so that when we come back to the `tag` node, we
                // look at the next child.
                let next_child = node.children[*nth_child];
                *nth_child += 1;
                stack.push((next_child, 0));
                continue;
            } else {
                // We have processed all children of `node`, so now it is time to process `node` itself.
                // First, get the current children of `node`. To appease the borrow checker,
                // we have to temporarily move the list out of the node, and then put the
                // list of remaining children back in.
                let mut children_of_node =
                    mem::take(&mut self.nodes.get_mut(*tag).unwrap().children);
                // Remove all useless children.
                children_of_node.retain_mut(|idx| {
                    if self.is_useless(*idx, live) {
                        // delete it everywhere else
                        self.remove_useless_node(*idx);
                        // and delete it from children_of_node
                        false
                    } else {
                        if let Some(nextchild) = self.can_be_replaced_by_single_child(*idx, live) {
                            // delete the in-between child
                            self.remove_useless_node(*idx);
                            // set the new child's parent
                            self.nodes.get_mut(nextchild).unwrap().parent = Some(*tag);
                            // save the new child in children_of_node
                            *idx = nextchild;
                        }
                        // retain it
                        true
                    }
                });
                // Put back the now-filtered vector.
                self.nodes.get_mut(*tag).unwrap().children = children_of_node;

                // We are done, the parent can continue.
                stack.pop();
                continue;
            }
        }
    }
}

impl VisitProvenance for Tree {
    fn visit_provenance(&self, visit: &mut VisitWith<'_>) {
        // To ensure that the root never gets removed, we visit it
        // (the `root` node of `Tree` is not an `Option<_>`)
        visit(None, Some(self.nodes.get(self.root).unwrap().tag))
    }
}

/// Relative position of the access
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessRelatedness {
    /// The accessed pointer is the current one
    This,
    /// The accessed pointer is a (transitive) child of the current one.
    // Current pointer is excluded (unlike in some other places of this module
    // where "child" is inclusive).
    StrictChildAccess,
    /// The accessed pointer is a (transitive) parent of the current one.
    // Current pointer is excluded.
    AncestorAccess,
    /// The accessed pointer is neither of the above.
    // It's a cousin/uncle/etc., something in a side branch.
    // FIXME: find a better name ?
    DistantAccess,
}

impl AccessRelatedness {
    /// Check that access is either Ancestor or Distant, i.e. not
    /// a transitive child (initial pointer included).
    pub fn is_foreign(self) -> bool {
        matches!(self, AccessRelatedness::AncestorAccess | AccessRelatedness::DistantAccess)
    }

    /// Given the AccessRelatedness for the parent node, compute the AccessRelatedness
    /// for the child node. This function assumes that we propagate away from the initial
    /// access.
    pub fn for_child(self) -> Self {
        use AccessRelatedness::*;
        match self {
            AncestorAccess | This => AncestorAccess,
            StrictChildAccess | DistantAccess => DistantAccess,
        }
    }
}
