//! The runtime half of the host-function embedding API — see
//! `plans/EMBEDDING.md`.
//!
//! A host (Rust) function compiled to a shim callable from JIT code has the
//! uniform signature `extern "C" fn(ctx: *mut FrogCtx, args: *const i64, out:
//! *mut i64)`. `FrogCtx` is that shim's only way to touch the heap — it
//! carries the executing `FrogState`'s `GcHeap` plus the struct/union
//! layout tables codegen itself uses, so a shim can build a `Str`/`List`/
//! struct/union value without any layout baked into it at compile time.
//!
//! GC safety follows the same rule as every other allocating runtime
//! function (`gc.rs`, "if a runtime function takes a GC pointer and can
//! collect, it holds its pointer arguments for its whole body") — see
//! `HostRootScope`.

use std::cell::Cell;

use crate::frontend::typeck::{StructDefs, UnionDefs};
use crate::runtime::gc::GcHeap;

thread_local! {
    /// The `FrogCtx` for the `FrogState` currently executing on this
    /// thread, published by `FrogState::call_jit` around each JIT call —
    /// mirrors `gc::ACTIVE_HEAP`, kept separate because it carries strictly
    /// more (and shorter-lived: valid only inside one `call_jit`, not for
    /// the `FrogState`'s whole lifetime) than a heap pointer.
    static ACTIVE_CTX: Cell<*mut FrogCtx> = const { Cell::new(std::ptr::null_mut()) };
}

/// Publish `ctx` as the active one for the duration of `f`. Used by
/// `FrogState::call_jit`; nests correctly with re-entrant JIT calls the
/// same way `gc::JitFrameGuard` does, by saving and restoring rather than
/// clearing.
pub fn with_active_ctx<R>(ctx: &mut FrogCtx, f: impl FnOnce() -> R) -> R {
    let previous = ACTIVE_CTX.with(|c| c.replace(ctx as *mut FrogCtx));
    let result = f();
    ACTIVE_CTX.with(|c| c.set(previous));
    result
}

/// The runtime import a host-call site fetches its `ctx` argument from
/// (registered in `Codegen::new_with_hosts`, aliased to the frog-invisible
/// name `frog_ctx_current`). Never bakes a host address into JIT code —
/// see `plans/EMBEDDING.md`'s "Getting the ctx pointer without baking an
/// address".
#[no_mangle]
pub extern "C" fn frog_ctx_current() -> i64 {
    ACTIVE_CTX.with(|c| c.get() as i64)
}

/// What a host shim uses to read its arguments, build heap values, and
/// write its result. One instance lives on `FrogState::call_jit`'s stack
/// for the duration of one JIT entry — every host call made during that
/// entry sees the same `FrogCtx`, not a fresh one per call.
#[repr(C)]
pub struct FrogCtx {
    heap:    *mut GcHeap,
    structs: *const StructDefs,
    unions:  *const UnionDefs,
}

impl FrogCtx {
    /// # Safety
    /// `heap` must be a valid, exclusively-owned `GcHeap` for the duration
    /// this `FrogCtx` is published, and `structs`/`unions` must outlive it
    /// (both hold for `FrogState::call_jit`, which builds this from `self`
    /// and never lets it escape the call).
    pub unsafe fn new(heap: *mut GcHeap, structs: *const StructDefs, unions: *const UnionDefs) -> Self {
        FrogCtx { heap, structs, unions }
    }

    fn heap(&mut self) -> &mut GcHeap {
        unsafe { &mut *self.heap }
    }

    pub fn structs(&self) -> &StructDefs {
        unsafe { &*self.structs }
    }

    pub fn unions(&self) -> &UnionDefs {
        unsafe { &*self.unions }
    }

    /// Open a root scope: every value this `FrogCtx` allocates after this
    /// call is rooted immediately (so a second allocation can't collect it
    /// out from under a Rust-side local holding it), and released when the
    /// returned guard drops. A `#[frog_fn]`-generated shim opens exactly
    /// one, for its whole body — by the time it drops (the shim's return),
    /// every value that's still wanted has already been written into `out`,
    /// which is enough to keep it alive: `compile_call`'s host-call arm
    /// `declare_gc_leaves`s the loaded-back `out` slots before this
    /// function's own stack map would otherwise consider them dead.
    pub fn scope(&mut self) -> HostRootScope {
        let mark = self.heap().roots_len();
        HostRootScope { heap: self.heap, mark }
    }

    /// Allocate a `Str` from `s` and root it for the enclosing `scope()`.
    pub fn alloc_str(&mut self, s: &str) -> i64 {
        let ptr = {
            let heap = self.heap();
            heap.maybe_collect();
            heap.alloc_str(s.as_bytes()) as i64
        };
        self.heap().push_root(ptr, true);
        ptr
    }

    /// Borrow a `Str` argument's bytes. `w` must be a live `FrogStr`
    /// pointer — true of any `Str`-typed argument slot, since the caller's
    /// stack map (or this ctx's own `scope()`) keeps it alive for the
    /// call's duration.
    ///
    /// # Safety
    /// `w` must be a valid, live `FrogStr` pointer.
    pub unsafe fn str_of<'a>(&self, w: i64) -> &'a str {
        crate::runtime::gc::frog_str_as_str(w as *const crate::runtime::gc::FrogStr)
    }

    /// Allocate a `List<T>` of `stride`-wide elements (`stride == 1` for
    /// every non-struct `T` — see `FrogList`'s doc comment in `gc.rs`) from
    /// a flat, already-wire-formatted `i64` buffer, and root the result.
    pub fn alloc_list(&mut self, elems: &[i64], stride: usize, ptr_mask: u64) -> i64 {
        let elem_count = if stride == 0 { 0 } else { elems.len() / stride.max(1) };
        let ptr = {
            let heap = self.heap();
            heap.maybe_collect();
            let list = heap.alloc_list(elem_count, stride, ptr_mask);
            unsafe {
                (*list).len = elems.len() as u32;
                if !elems.is_empty() {
                    std::ptr::copy_nonoverlapping(elems.as_ptr(), (*list).data, elems.len());
                }
            }
            list as i64
        };
        self.heap().push_root(ptr, true);
        ptr
    }

    /// Read a `List`'s element count and one flattened element's slots
    /// (`stride`-wide, matching `alloc_list`'s convention) — a thin wrapper
    /// over the same layout `ffi::frog_list_get`/`frog_list_len` use,
    /// exposed here so a shim doesn't need a second way to reach the heap.
    pub fn list_len(&self, w: i64, stride: usize) -> usize {
        let list_ptr = w as *const crate::runtime::gc::FrogList;
        let raw_len = unsafe { (*list_ptr).len as usize };
        raw_len / stride.max(1)
    }

    pub fn list_elem(&self, w: i64, idx: usize, stride: usize, slot: usize) -> i64 {
        let list_ptr = w as *const crate::runtime::gc::FrogList;
        unsafe { *(*list_ptr).data.add(idx * stride.max(1) + slot) }
    }

    /// Allocate a `Dict<K, V>` from `entries` — a flat, already-wire-
    /// formatted buffer of `kstride + vstride`-wide `(key, value)` blocks,
    /// in the insertion order the resulting `Dict` should report — and
    /// root the result. `key_kind` is a `runtime::dict::KeyKind`
    /// discriminant. Builds the hash index in one pass (`rebuild`, the
    /// same one `GcHeap::clone_obj`'s `Dict` arm uses) rather than
    /// inserting one entry at a time.
    ///
    /// A duplicate key in `entries` is **not** deduplicated here — the
    /// caller (a `ToFrog` impl driven by a Rust collection that is itself
    /// already key-unique, e.g. `HashMap`) never produces one; passing
    /// one anyway leaves both entries in `entries` but only the *last*
    /// reachable through the index, the same "last write wins" a repeated
    /// key in a source-level Dict literal gets.
    pub fn alloc_dict(&mut self, entries: &[i64], kstride: usize, vstride: usize, ptr_mask: u64, key_kind: u32) -> i64 {
        use crate::runtime::dict::{hash_key, dict_index_mut, KeyKind};
        let stride = (kstride + vstride).max(1);
        let elem_count = if stride == 0 { 0 } else { entries.len() / stride };
        let ptr = {
            let heap = self.heap();
            heap.maybe_collect();
            let dict = heap.alloc_dict(elem_count, kstride, vstride, ptr_mask, key_kind);
            unsafe {
                (*dict).len = elem_count as u32;
                if !entries.is_empty() {
                    std::ptr::copy_nonoverlapping(entries.as_ptr(), (*dict).entries, entries.len());
                }
                let kind = KeyKind::from_u32(key_kind);
                let dict_entries = (*dict).entries;
                let mut pairs = (0..elem_count).map(|i| {
                    let key = *dict_entries.add(i * stride);
                    (hash_key(kind, key), i as u32)
                });
                dict_index_mut(dict).rebuild(&mut pairs);
            }
            dict as i64
        };
        self.heap().push_root(ptr, true);
        ptr
    }

    /// Read a `Dict`'s entry count and one entry's flattened slots
    /// (`kstride + vstride`-wide, `alloc_dict`'s convention), in whatever
    /// order the entries buffer holds them — insertion order (`FrogDict`'s
    /// doc comment). Thin wrapper mirroring `list_len`/`list_elem`.
    pub fn dict_len(&self, w: i64) -> usize {
        let dict_ptr = w as *const crate::runtime::gc::FrogDict;
        unsafe { (*dict_ptr).len as usize }
    }

    pub fn dict_entry_slot(&self, w: i64, idx: usize, kstride: usize, vstride: usize, slot: usize) -> i64 {
        let stride = (kstride + vstride).max(1);
        let dict_ptr = w as *const crate::runtime::gc::FrogDict;
        unsafe { *(*dict_ptr).entries.add(idx * stride + slot) }
    }
}

/// Releases every root `FrogCtx` pushed since the scope opened. See
/// `FrogCtx::scope`.
pub struct HostRootScope {
    heap: *mut GcHeap,
    mark: usize,
}

impl Drop for HostRootScope {
    fn drop(&mut self) {
        let heap = unsafe { &mut *self.heap };
        let held = heap.roots_len().saturating_sub(self.mark);
        heap.pop_roots(held);
    }
}
