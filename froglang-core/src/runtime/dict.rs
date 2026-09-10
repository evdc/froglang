//! `Dict<K, V>`'s runtime — hashing/equality for the four supported key
//! types (`Trait::Hash` in `frontend::typeck`), and a swappable index
//! backend the host embedder can replace (`plans/DATA.md`'s Dict design;
//! `FrogStateBuilder::dict_backend`). See `runtime::gc::FrogDict` for the
//! object layout this operates on.
//!
//! The backend never sees a froglang value: it maps a hash to candidate
//! entry indices, and equality between candidate and query key is a
//! closure this module supplies (`key_eq`) — so a replacement backend
//! needs no knowledge of the value representation, the word encoding, or
//! the GC.

use super::gc::{frog_str_as_str, FrogDict, FrogStr, GcHeap, RuntimeRoots};
use super::ffi::{with_heap, frog_abort};

// ── Key hashing / equality ──────────────────────────────────────────────────

/// A `Dict` key's runtime kind — the four types `Trait::Hash` admits today.
/// Stored as a plain `u32` on `FrogDict` (`key_kind`) since the GC layer
/// doesn't otherwise know about `frontend::typeck::Type`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum KeyKind { Int = 0, Float = 1, Bool = 2, Str = 3 }

impl KeyKind {
    pub fn from_u32(v: u32) -> KeyKind {
        match v {
            0 => KeyKind::Int,
            1 => KeyKind::Float,
            2 => KeyKind::Bool,
            3 => KeyKind::Str,
            _ => unreachable!("invalid KeyKind discriminant {}", v),
        }
    }
}

/// Hash one key word. `Float` normalizes `-0.0` to `0.0` so they hash (and
/// therefore look up) identically, matching `==`'s own `-0.0 == 0.0`. A
/// `NaN` key hashes to some value like any other bit pattern, but
/// `key_eq` never reports it equal to anything (including itself) — same
/// as `==` — so a `NaN` key is always a miss on lookup, and duplicate
/// `NaN` keys can coexist in one dict.
pub fn hash_key(kind: KeyKind, w: i64) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    match kind {
        KeyKind::Int | KeyKind::Bool => w.hash(&mut h),
        KeyKind::Float => {
            let f = f64::from_bits(w as u64);
            let norm = if f == 0.0 { 0.0 } else { f };
            norm.to_bits().hash(&mut h);
        }
        KeyKind::Str => {
            let s = unsafe { frog_str_as_str(w as *const FrogStr) };
            s.hash(&mut h);
        }
    }
    h.finish()
}

/// Are keys `a` and `b` (both already known to be `kind`) equal?
pub fn key_eq(kind: KeyKind, a: i64, b: i64) -> bool {
    match kind {
        KeyKind::Int | KeyKind::Bool => a == b,
        KeyKind::Float => {
            let (fa, fb) = (f64::from_bits(a as u64), f64::from_bits(b as u64));
            // Not `fa == fb` alone: that already treats `-0.0 == 0.0` as
            // true (matching `hash_key`'s normalization) and `NaN == NaN`
            // as false, which is exactly the behavior wanted here — but
            // spelled out because it's load-bearing, not incidental.
            fa == fb
        }
        KeyKind::Str => {
            let (sa, sb) = unsafe {
                (frog_str_as_str(a as *const FrogStr), frog_str_as_str(b as *const FrogStr))
            };
            sa == sb
        }
    }
}

// ── Swappable index backend ─────────────────────────────────────────────────

/// One `FrogDict`'s hash index: hash -> candidate entry indices. Never
/// sees a key's actual value — only its precomputed hash and an
/// externally-supplied equality closure over entry indices.
pub trait DictIndex: Send {
    /// Find the entry index whose hash is `hash` and which `eq` accepts,
    /// if any. `eq` is only ever called on entries this index actually
    /// stored at `hash` (or a colliding hash), never speculatively.
    fn find(&self, hash: u64, eq: &mut dyn FnMut(u32) -> bool) -> Option<u32>;
    /// Record that `entry` is stored at `hash`. Caller's responsibility
    /// that no entry already at `hash` compares equal — i.e. call `find`
    /// first and only `insert` on a miss.
    fn insert(&mut self, hash: u64, entry: u32);
    /// Discard everything and rebuild from `entries` (an iterator of
    /// `(hash, entry_index)` pairs) — used after `frog_dict_remove`
    /// compacts the entries buffer (every index after the removed one
    /// shifts) and by `GcHeap::clone_obj`.
    fn rebuild(&mut self, entries: &mut dyn Iterator<Item = (u64, u32)>);
}

/// Builds the index every `FrogDict` a `GcHeap` allocates uses. The seam
/// an embedder swaps via `FrogStateBuilder::dict_backend` (`state.rs`) —
/// a replacement needs only these two methods, never touching the GC,
/// the word encoding, or `hash_key`/`key_eq` above.
pub trait DictBackend: Send + Sync {
    fn new_index(&self, cap: usize) -> Box<dyn DictIndex>;
}

/// The default backend: `hashbrown::HashTable`, storing `(hash, entry)`
/// pairs so a resize never needs to recompute a hash from the entry
/// alone (which would require reaching back into `FrogDict::entries`,
/// exactly the coupling this module exists to avoid).
pub struct HashbrownBackend;

impl DictBackend for HashbrownBackend {
    fn new_index(&self, cap: usize) -> Box<dyn DictIndex> {
        Box::new(HashbrownIndex(hashbrown::HashTable::with_capacity(cap)))
    }
}

struct HashbrownIndex(hashbrown::HashTable<(u64, u32)>);

impl DictIndex for HashbrownIndex {
    fn find(&self, hash: u64, eq: &mut dyn FnMut(u32) -> bool) -> Option<u32> {
        self.0.find(hash, |&(h, e)| h == hash && eq(e)).map(|&(_, e)| e)
    }
    fn insert(&mut self, hash: u64, entry: u32) {
        self.0.insert_unique(hash, (hash, entry), |&(h, _)| h);
    }
    fn rebuild(&mut self, entries: &mut dyn Iterator<Item = (u64, u32)>) {
        self.0.clear();
        for (h, e) in entries {
            self.0.insert_unique(h, (h, e), |&(hh, _)| hh);
        }
    }
}

/// The only sound way to read or write a `FrogDict`'s double-boxed index
/// — see `FrogDict`'s doc comment for why it's double-boxed.
///
/// # Safety
/// `dict` must be a live, fully-initialized `FrogDict` pointer.
pub unsafe fn dict_index_mut<'a>(dict: *mut FrogDict) -> &'a mut Box<dyn DictIndex> {
    &mut *((*dict).index as *mut Box<dyn DictIndex>)
}

// ── FFI ──────────────────────────────────────────────────────────────────────

/// Allocate an empty `Dict` with room for `cap` entries. `kstride`/
/// `vstride`/`ptr_mask` describe one entry's flattened layout (key
/// leaves then value leaves — see `FrogDict`'s doc comment); `key_kind`
/// is a `KeyKind` discriminant.
#[no_mangle]
pub extern "C" fn frog_alloc_dict(cap: i64, kstride: i64, vstride: i64, ptr_mask: i64, key_kind: i64) -> i64 {
    let _jit_frame = crate::jit_frame_guard!();
    with_heap(|heap| {
        heap.maybe_collect();
        heap.alloc_dict(cap as usize, kstride as usize, vstride as usize, ptr_mask as u64, key_kind as u32) as i64
    })
}

#[no_mangle]
pub extern "C" fn frog_dict_len(d: i64) -> i64 {
    unsafe { (*(d as *const FrogDict)).len as i64 }
}

fn stride_of(d: *const FrogDict) -> usize {
    unsafe { ((*d).kstride + (*d).vstride).max(1) as usize }
}

/// Find `key`'s entry index, or `-1` if absent. `d`/`key` are not
/// rooted here — this never allocates, so nothing can move or collect
/// out from under the caller.
#[no_mangle]
pub extern "C" fn frog_dict_find(d: i64, key: i64) -> i64 {
    let dict = d as *mut FrogDict;
    unsafe {
        let kind = KeyKind::from_u32((*dict).key_kind);
        let stride = stride_of(dict);
        let entries = (*dict).entries;
        let index = dict_index_mut(dict);
        let hash = hash_key(kind, key);
        let mut eq = |entry: u32| key_eq(kind, key, *entries.add(entry as usize * stride));
        match index.find(hash, &mut eq) {
            Some(e) => e as i64,
            None => -1,
        }
    }
}

/// Find `key`'s entry index, creating a new (zero-initialized value)
/// entry at the end if absent. Returns the entry index either way —
/// callers write/overwrite the value leaves via `frog_dict_set_slot`
/// afterward, whether the entry is new or not (a literal's later
/// duplicate key overwrites the earlier one's value, matching Python's
/// `{1: "a", 1: "b"}`).
#[no_mangle]
pub extern "C" fn frog_dict_insert(d: i64, key: i64) -> i64 {
    let _jit_frame = crate::jit_frame_guard!();
    let dict = d as *mut FrogDict;
    let kind = unsafe { KeyKind::from_u32((*dict).key_kind) };
    // Only a `Str` key is a GC pointer — the caller's stack map doesn't
    // cover `key` past this call (it "died" from the JIT's point of view
    // at the call site), so it must be re-rooted before `with_heap`'s
    // closure has any chance to allocate/collect. An `Int`/`Float`/`Bool`
    // key has no tag bits and must never be pushed as a root (`RuntimeRoots
    // ::hold`'s doc comment) — `hold_masked` scopes the root to just this
    // one case.
    let _roots = RuntimeRoots::hold_masked(&[key], &[kind == KeyKind::Str]);
    with_heap(|heap| {
        let stride = stride_of(dict);
        let hash = hash_key(kind, key);
        let found = unsafe {
            let entries = (*dict).entries;
            let index = dict_index_mut(dict);
            let mut eq = |entry: u32| key_eq(kind, key, *entries.add(entry as usize * stride));
            index.find(hash, &mut eq)
        };
        if let Some(e) = found {
            return e as i64;
        }

        grow_if_full(heap, dict, stride);

        let idx = unsafe { (*dict).len };
        unsafe {
            let base = (*dict).entries.add(idx as usize * stride);
            // Key leaves first (`kstride` wide — 1 in v1), then zeroed
            // value leaves, matching `FrogDict`'s entry layout.
            *base = key;
            let kstride = (*dict).kstride as usize;
            for i in kstride..stride {
                *base.add(i) = 0;
            }
            (*dict).len += 1;
            *(*dict).hashes.add(idx as usize) = hash;
            dict_index_mut(dict).insert(hash, idx);
        }
        idx as i64
    })
}

/// Grow `entries` (and the parallel cached-hash `hashes` buffer) if `dict`
/// is at capacity — same doubling-realloc shape as `frog_list_push`,
/// sharing its underlying allocate-or-grow primitive (`ffi::grow_raw_buffer`).
fn grow_if_full(heap: &mut GcHeap, dict: *mut FrogDict, stride: usize) {
    unsafe {
        if (*dict).len < (*dict).cap { return; }
        let old_cap = (*dict).cap as usize;
        let new_cap = if old_cap == 0 { 4 } else { old_cap * 2 };
        if new_cap > u32::MAX as usize {
            frog_abort(format_args!("dict grew past the maximum length of {} entries", u32::MAX));
        }
        let old_size = old_cap * stride * std::mem::size_of::<i64>();
        let new_size = new_cap * stride * std::mem::size_of::<i64>();
        let new_data = super::ffi::grow_raw_buffer((*dict).entries as *mut u8, old_size, new_size) as *mut i64;
        heap.bytes_allocated += new_size - old_size;

        let old_hashes_size = old_cap * std::mem::size_of::<u64>();
        let new_hashes_size = new_cap * std::mem::size_of::<u64>();
        let new_hashes = super::ffi::grow_raw_buffer((*dict).hashes as *mut u8, old_hashes_size, new_hashes_size) as *mut u64;
        heap.bytes_allocated += new_hashes_size - old_hashes_size;

        (*dict).entries = new_data;
        (*dict).hashes = new_hashes;
        (*dict).cap = new_cap as u32;
    }
}

/// Read leaf `slot` (`0..kstride` is the key, `kstride..kstride+vstride`
/// is the value) of entry `entry`.
#[no_mangle]
pub extern "C" fn frog_dict_slot(d: i64, entry: i64, slot: i64) -> i64 {
    let dict = d as *const FrogDict;
    let stride = stride_of(dict);
    unsafe { *(*dict).entries.add(entry as usize * stride + slot as usize) }
}

#[no_mangle]
pub extern "C" fn frog_dict_set_slot(d: i64, entry: i64, slot: i64, val: i64) {
    let dict = d as *mut FrogDict;
    let stride = stride_of(dict);
    unsafe { *(*dict).entries.add(entry as usize * stride + slot as usize) = val; }
}

/// Remove `key`'s entry, if present, compacting `entries` (everything
/// after the removed entry shifts down one slot-block) and rebuilding
/// the index. Returns `1` if a key was removed, `0` if `key` was absent.
/// Callers that need the removed value must read it (via
/// `frog_dict_slot`) *before* calling this — compaction invalidates
/// every entry index at or after the removed one.
#[no_mangle]
pub extern "C" fn frog_dict_remove(d: i64, key: i64) -> i64 {
    let dict = d as *mut FrogDict;
    let kind = unsafe { KeyKind::from_u32((*dict).key_kind) };
    let stride = stride_of(dict);
    let found = unsafe {
        let entries = (*dict).entries;
        let index = dict_index_mut(dict);
        let hash = hash_key(kind, key);
        let mut eq = |entry: u32| key_eq(kind, key, *entries.add(entry as usize * stride));
        index.find(hash, &mut eq)
    };
    let Some(e) = found else { return 0; };
    unsafe {
        let entries = (*dict).entries;
        let hashes = (*dict).hashes;
        let len = (*dict).len as usize;
        let e = e as usize;
        if e + 1 < len {
            std::ptr::copy(
                entries.add((e + 1) * stride),
                entries.add(e * stride),
                (len - e - 1) * stride,
            );
            std::ptr::copy(hashes.add(e + 1), hashes.add(e), len - e - 1);
        }
        (*dict).len = (len - 1) as u32;

        // Every surviving entry's hash was cached at insert (or carried
        // over by a prior compaction/clone), so the rebuild below reads it
        // back instead of re-hashing each key — the whole point of
        // `hashes` existing.
        let new_len = (*dict).len as usize;
        let index = dict_index_mut(dict);
        let mut pairs = (0..new_len).map(|i| (*hashes.add(i), i as u32));
        index.rebuild(&mut pairs);
    }
    1
}

/// Terminate the process: `key` has no entry in `d` — the `d[key]`
/// panic path (`get`/`in` never call this; only bare `[]` indexing
/// does). See `ffi::frog_index_out_of_bounds`'s doc comment for why this
/// is a hard exit rather than a catchable error.
#[no_mangle]
pub extern "C" fn frog_dict_key_missing(_d: i64, _key: i64) -> ! {
    frog_abort(format_args!("key not found in Dict"));
}
