use std::alloc::{alloc, dealloc, handle_alloc_error, Layout};
use std::cell::{Cell, RefCell};

/// Zero-cost GC tracing. Enable with `--features heap_trace`.
/// Expands to an eprintln! at compile time; completely removed without the feature.
macro_rules! heap_trace {
    ($fmt:literal $(, $arg:expr)* $(,)?) => {{
        #[cfg(feature = "heap_trace")]
        eprintln!(concat!("[GC] ", $fmt) $(, $arg)*);
    }};
}

// ── Object kinds ─────────────────────────────────────────────────────────────

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ObjKind { Str = 0, List = 1, Variant = 2, Dict = 3 }

// ── Object header (prefix for every heap object) ─────────────────────────────

/// Every heap object starts with one of these. Each object has exactly one
/// owner, which frees it (`Heap::drop_tree`), so the header only has to
/// say what kind of object follows.
#[repr(C)]
pub struct ObjHeader {
    pub kind:   ObjKind,
}

// ── FrogStr — immutable, inline bytes immediately after the struct ────────────

#[repr(C)]
pub struct FrogStr {
    pub header: ObjHeader,
    pub len:    u32,
    _data: [u8; 0],  // zero-sized marker; actual bytes live at (ptr + size_of::<FrogStr>())
}

// ── FrogList — mutable, separate data buffer ──────────────────────────────────
//
// Elements occupy `stride` consecutive `i64` slots each (`stride == 1` for
// every non-struct element type — this degenerates to the list's old flat
// one-slot-per-element layout exactly). `len`/`cap` are raw *slot* counts,
// not element counts — `frog_list_len` (ffi.rs) divides by `stride` to
// report the element count callers actually want; `frog_list_push`
// (ffi.rs) is unchanged and simply keeps appending one raw slot at a time,
// which is exactly right since codegen always pushes a struct element's
// `stride` leaf values back-to-back in one go.
//
// `ptr_mask` marks which of the `stride` per-element slot offsets are heap
// pointers (bit `i` set => offset `i` within each element block is a
// pointer) — `Heap::for_each_slot` consults this once per element block
// to find what the list owns.
#[repr(C)]
pub struct FrogList {
    pub header:   ObjHeader,
    pub len:      u32,
    pub cap:      u32,
    pub stride:   u32,
    /// Bit `i` set means slot `i` within each element block is a pointer
    /// column: drop and clone read it and apply the uniform
    /// `is_heap_ptr`/`heap_ptr` rule (see "Word encoding" below). Columns
    /// that hold raw scalars are left clear, since a raw `Int` has no tag
    /// bits and could otherwise be mistaken for an address.
    pub ptr_mask: u64,
    pub data:     *mut i64,
}

// ── FrogVariant — immutable, inline payload immediately after the struct ──────
//
// Runtime representation of a nominal union's member (`data X is A | B` —
// the language's only sum type): `tag` is the member's declaration index
// within its union (fixed by `TypeChecker::UnionDef`, consulted only at
// codegen time — the runtime itself never needs to know which union this
// came from). The payload is `nslots` `i64`s, laid out as the union's common
// fields (flattened, in declared order) followed by this member's own
// fields (flattened, in declared order) — see
// `codegen::enum_field_leaf_types`. `ptr_mask` marks which payload slots
// are heap pointers, exactly like `FrogList::ptr_mask` but one bit per
// slot directly (no `stride` — a variant is always exactly one "element").
#[repr(C)]
pub struct FrogVariant {
    pub header:   ObjHeader,
    pub tag:      u32,
    pub nslots:   u32,
    pub ptr_mask: u64,
    _data: [i64; 0],  // zero-sized marker; slots live at (ptr + size_of::<FrogVariant>())
}

// ── FrogDict — mutable, separate entries buffer + a swappable hash index ──────
//
// `Dict<K, V>`'s runtime representation. Entries are stored tightly packed,
// in insertion order, `stride = kstride + vstride` `i64` slots each — no
// tombstones: `runtime::dict::frog_dict_remove` compacts immediately by
// shifting everything after the removed entry down one slot-block and
// rebuilding `index` from scratch, so `len` is always exactly the number of
// slot-blocks in use and every index in `0..len` is live. This trades
// removal for simplicity (`O(n)` per removal, same as the rebuild it does
// anyway) rather than a lazy-compaction tombstone scheme.
//
// `key_kind` (a `runtime::dict::KeyKind`) and `kstride` are fixed by the
// dict's key type at construction: `kstride` is always 1 in v1 (every
// supported key type — `Int`/`Float`/`Bool`/`Str` — is one `i64` slot), but
// carried as a field rather than assumed so `runtime::dict::hash_key`/
// `key_eq`'s scalar case and a wider structural-key case (`Trait::Hash`'s
// deferred widening) share one entry layout.
//
// `ptr_mask` marks pointer columns across one whole entry (key columns
// first, then value columns) — same convention as `FrogList::ptr_mask`,
// one bit per `stride` slot rather than one bit per element.
//
// `index` is a type-erased `Box<Box<dyn runtime::dict::DictIndex>>` — see
// `runtime::dict::dict_index_mut`/`dict_index_set` for the only sound way
// to read or write it. Double-boxed because `dyn DictIndex` is a fat
// pointer (data + vtable): a single `Box<dyn DictIndex>` can't be
// round-tripped through the single-word `*mut ()` this field has to be to
// stay a plain, header-shaped heap object. The index owns no frog values
// (`for_each_slot` reaches `entries` through `ptr_mask` alone) and is freed
// explicitly by `free_obj`.
#[repr(C)]
pub struct FrogDict {
    pub header:   ObjHeader,
    pub len:      u32,   // live entries (== populated slot-blocks in `entries`)
    pub cap:      u32,   // entry capacity (in slot-blocks) of the `entries` buffer
    pub kstride:  u32,
    pub vstride:  u32,
    pub ptr_mask: u64,
    pub key_kind: u32,   // runtime::dict::KeyKind
    pub entries:  *mut i64,
    /// One cached `runtime::dict::hash_key` result per live entry (same
    /// length/order as `entries`, `cap` slots allocated) — computed once at
    /// insert and reused by `remove`'s index rebuild and by `clone_obj`,
    /// so neither has to re-hash every surviving key (re-scanning a `Str`
    /// key's bytes) just to relocate it.
    pub hashes:   *mut u64,
    pub index:    *mut (),
}

// ── Word encoding ─────────────────────────────────────────────────────────────
//
// Every pointer-column word in the system — a list element slot, a boxed
// variant's payload slot, a value crossing the FFI — uses one encoding, so
// drop and clone need no per-slot type information and no per-slot metadata
// beyond "is this a pointer column at all" (`ptr_mask`).
//
// `alloc_bytes` lays every heap object out with `Layout::from_size_align(_, 8)`,
// so every object address has its low 3 bits clear. Those 3 bits carry a tag:
//
//   low 3 bits | meaning
//   -----------+--------------------------------------------------------------
//   000        | a plain pointer, or 0 for null/absent — `Str`, `List`, a boxed
//              | union
//   001..110   | an *inline* union's tag slot: the member tag in the low bits,
//              | and (when that member's first field is a plain pointer, see
//              | `codegen::UnionLayout`) the pointer itself in `w & !7`. A
//              | member with no pointer field leaves the pointer part zero, so
//              | its word is just the small tag `1..=6`, which masks to `0` and
//              | is correctly not followed.
//   111        | not a pointer: the upper 61 bits are data. This is a *boxed*
//              | union's payload-less member (`immediate_variant`) and the unit
//              | value `Type::None` (`IMMEDIATE_NONE`).
//
// The whole rule is `is_heap_ptr` + `heap_ptr` below: two ALU ops, no branch
// on slot kind, no tag lookup. Codegen inlines the same test before each
// drop and clone call (`codegen::emit_drop_word`).
//
// Member tags run `1..=6`: `0` is reserved so a plain non-union pointer is
// indistinguishable from an untagged one, and `7` is reserved for immediates.
// A union with more than `codegen::MAX_INLINE_UNION_MEMBERS` members (or one
// that is self-referential) falls back to the boxed one-slot representation
// instead of being laid out inline.

/// Low bits of a word reserved for a tag.
pub const TAG_MASK: i64 = 7;

/// The reserved tag class meaning "these bits are data, not an address".
pub const TAG_IMMEDIATE: i64 = 7;

/// The unit value `Type::None` when it stands on its own rather than as a
/// member of some union (where it is just that union's member tag). Tag
/// data zero in the immediate class.
pub const IMMEDIATE_NONE: i64 = TAG_IMMEDIATE;

/// Does `w` hold an object (that a drop frees and a clone copies)? False for `0` (an empty slot), for an
/// immediate (`111`), and for a tag-only inline-union word (`1..=6`, whose
/// pointer part is zero).
#[inline]
pub fn is_heap_ptr(w: i64) -> bool {
    w & TAG_MASK != TAG_IMMEDIATE && (w & !TAG_MASK) != 0
}

/// The object `w` points at, with any tag bits stripped. Only meaningful
/// when `is_heap_ptr(w)`.
#[inline]
pub fn heap_ptr(w: i64) -> *mut ObjHeader {
    (w & !TAG_MASK) as *mut ObjHeader
}

/// Encode variant index `tag` as a *boxed* union's unboxed immediate — the
/// representation a payload-less member of a union too wide (or too
/// self-referential) to lay out inline gets. An inline union's payload-less
/// member is not an immediate at all: it is simply its member tag.
#[inline]
pub fn immediate_variant(tag: u32) -> i64 {
    ((tag as i64) << 3) | TAG_IMMEDIATE
}

/// Decode an unboxed enum value produced by `immediate_variant`.
#[inline]
pub fn immediate_variant_tag(v: i64) -> i64 {
    v >> 3
}

// ── Heap ───────────────────────────────────────────────────────────────────

/// One `with Arena` region: a bump allocator whose objects are all released
/// together. See `Heap::arena_enter`.
struct Arena {
    /// Every chunk this arena owns, with its size.
    chunks:  Vec<(*mut u8, usize)>,
    /// The unused tail of the newest chunk.
    ptr:     *mut u8,
    end:     *mut u8,
    /// Dicts allocated here. Each holds a Rust-side index that is not arena
    /// memory, so releasing the arena has to drop it.
    dicts:   Vec<*mut FrogDict>,
    /// `bytes_allocated` and `live_objects` when the arena was entered, and
    /// when it stopped accepting allocations (`arena_evac_begin`). The
    /// difference is what the arena added, and release takes it back out.
    bytes0:  usize,
    live0:   usize,
    bytes1:  usize,
    live1:   usize,
}

/// The first chunk of an arena. Later chunks double, and an object larger
/// than the next chunk gets a chunk of its own.
const ARENA_FIRST_CHUNK: usize = 16 * 1024;

pub struct Heap {
    /// Bytes held by live objects, buffers included.
    pub bytes_allocated: usize,
    /// Recycled blocks of freed objects, bucketed by size in 8-byte
    /// words: `free_lists[w]` heads an intrusive singly-linked list of
    /// blocks of exactly `w * 8` bytes (the `next` link lives in the
    /// block's first word, which is always at least 8 bytes wide).
    ///
    /// Without this, every `data`-union value the `orders` benchmark builds
    /// costs a `malloc` when it is created and a `free` when it is dropped,
    /// for blocks whose sizes repeat endlessly. Sizes above `MAX_FREE_WORDS`
    /// (rare, and not repetitive enough to be worth retaining) go straight
    /// back to the system allocator.
    ///
    /// Recycled bytes are held for the process's lifetime rather than
    /// returned to the OS; `bytes_allocated` still counts only *live* bytes.
    free_lists:      Vec<*mut u8>,
    /// `FROG_LEAK_CHECK`: track freed addresses so a second free of one panics
    /// instead of corrupting a free list. It costs a set lookup per
    /// allocation and free, so it is opt-in.
    track_frees:     bool,
    /// Heap objects currently allocated and not yet freed.
    live_objects:    usize,
    freed:           std::collections::HashSet<usize>,
    /// Reusable worklist for the tree walks (`drop_tree`, `tree_len`,
    /// `clone_obj`), so a walk is not a `malloc`/`free` pair. None of them
    /// calls another while it holds the list.
    worklist:        Vec<*mut ObjHeader>,
    /// `with Arena` regions, innermost last. The first `arenas_active` of them
    /// accept allocations; the one after (at most one) is being evacuated.
    /// While `arenas_active` is non-zero, allocation bumps the top arena and
    /// every free is a no-op, which is sound because nothing outside the
    /// arena is freed inside it (`plans/NOGC.md` step 4).
    arenas:          Vec<Arena>,
    arenas_active:   usize,
    /// While any arena is active, this holds the real free lists and
    /// `free_lists` is all null, which sends `alloc_bytes` to its slow path,
    /// the only place that checks for an arena. Swapped on the 0/1 edge.
    free_lists_aside: Vec<*mut u8>,
    /// Builds the hash index every `FrogDict` this heap allocates uses —
    /// the "swappable from the host" seam (`plans/DATA.md`'s Dict design):
    /// an embedder can install a different `DictBackend` (e.g. a sorted
    /// index) via `FrogStateBuilder::dict_backend`, and every dict this
    /// heap creates afterward picks it up with no other change. Defaults
    /// to `runtime::dict::HashbrownBackend`.
    pub dict_backend: std::sync::Arc<dyn crate::runtime::dict::DictBackend>,
}

thread_local! {
    /// The heap runtime calls allocate into when no `FrogState` is running
    /// (`compile_and_run`, the AOT harness, Rust-side unit tests).
    pub static TASK_HEAP: RefCell<Heap> = RefCell::new(Heap::new());
    /// Pointer to the Heap of the FrogState currently executing on this thread.
    /// Null when no froglang code is running (falls back to TASK_HEAP).
    pub static ACTIVE_HEAP: Cell<*mut Heap> = const { Cell::new(std::ptr::null_mut()) };
}

/// Largest block size, in 8-byte words, that `Heap::free_bytes` keeps on
/// a free list instead of handing back to the system allocator. 64 words is
/// 512 bytes — comfortably above every `FrogVariant` and `FrogStr` a
/// realistic program allocates in bulk, and above a short list's data
/// buffer too.
const MAX_FREE_WORDS: usize = 64;

/// Round `size` up to a whole number of 8-byte words. Every block this
/// module allocates is 8-byte aligned and freed at its rounded size, so
/// allocation and deallocation always agree on the layout even when the
/// caller's natural size isn't a multiple of 8 (a `FrogStr`'s trailing
/// bytes plus NUL, typically).
#[inline]
fn words_for(size: usize) -> usize {
    // Never zero: a recycled block has to be wide enough to hold the free
    // list's `next` pointer in its first word, and a zero-sized `Layout` is
    // not valid to pass to `alloc` in the first place. No caller currently
    // asks for zero bytes (every object has a header), so this is a floor,
    // not a case that fires.
    size.div_ceil(8).max(1)
}

impl Default for Heap {
    fn default() -> Self {
        Self::new()
    }
}

impl Heap {
    /// Record a newly allocated object.
    #[inline]
    fn note_alloc(&mut self, obj: *mut ObjHeader) {
        self.live_objects += 1;
        if self.track_frees {
            self.freed.remove(&(obj as usize));
        }
    }

    /// Whether teardown checks that every object was freed: always in a
    /// debug build, and under `FROG_LEAK_CHECK` in a release one.
    pub fn leak_check_enabled(&self) -> bool {
        cfg!(debug_assertions) || self.track_frees
    }

    /// Free the object `w` holds, and everything it owns, if it holds one.
    /// Scalars and immediates are left alone.
    pub fn drop_word(&mut self, w: i64) {
        if is_heap_ptr(w) {
            unsafe { self.drop_tree(heap_ptr(w)) };
        }
    }

    /// Objects allocated and not yet freed. A program that frees everything
    /// it allocated ends at zero.
    pub fn live_objects(&self) -> usize {
        self.live_objects
    }

    /// How many objects the tree `w` holds (zero for a scalar or immediate):
    /// what `drop_word(w)` would free.
    pub fn tree_len(&mut self, w: i64) -> usize {
        if !is_heap_ptr(w) { return 0; }
        let mut work = std::mem::take(&mut self.worklist);
        let mut n = 0;
        work.push(heap_ptr(w));
        while let Some(obj) = work.pop() {
            n += 1;
            unsafe { Self::for_each_slot(obj, &mut |p| if is_heap_ptr(*p) { work.push(heap_ptr(*p)) }) };
        }
        self.worklist = work;
        n
    }

    /// Free `obj` and every object it owns, through its pointer slots
    /// (`for_each_slot`). Each object has exactly one owner, so the tree is
    /// freed without visiting anything twice. Iterative: a 100k-long `Cons`
    /// list is a tree that deep.
    ///
    /// # Safety
    ///
    /// `obj` must be a live object of this heap that nothing else owns, and
    /// nothing may use it or anything it owns afterwards.
    pub unsafe fn drop_tree(&mut self, obj: *mut ObjHeader) {
        if self.arenas_active != 0 { return; }
        let mut work = std::mem::take(&mut self.worklist);
        work.push(obj);
        while let Some(obj) = work.pop() {
            Self::for_each_slot(obj, &mut |p| {
                if is_heap_ptr(*p) { work.push(heap_ptr(*p)); }
            });
            self.free_object(obj);
        }
        self.worklist = work;
    }

    /// Free `obj` alone, leaving whatever it points at alone. The caller has
    /// moved those children elsewhere (or does not own them).
    ///
    /// # Safety
    ///
    /// `obj` must be a live object of this heap, unused afterwards.
    pub unsafe fn free_shallow(&mut self, obj: *mut ObjHeader) {
        if self.arenas_active != 0 { return; }
        self.free_object(obj);
    }

    /// `free_shallow` for a caller that has checked no arena is active.
    unsafe fn free_object(&mut self, obj: *mut ObjHeader) {
        if self.track_frees && !self.freed.insert(obj as usize) {
            panic!("double free of heap object {:p}", obj);
        }
        let freed = self.free_obj(obj);
        self.bytes_allocated -= freed;
    }

    /// Allocate `size` bytes (rounded up to a word), reusing a recycled
    /// block of that exact size class if one is available. All object
    /// allocation goes through here.
    #[inline]
    fn alloc_bytes(&mut self, size: usize) -> *mut u8 {
        let words = words_for(size);
        if words <= MAX_FREE_WORDS {
            let head = self.free_lists[words];
            if !head.is_null() {
                // The block's first word holds the next link — see
                // `free_bytes`. Nothing else in it is meaningful; every
                // caller overwrites the header immediately.
                self.free_lists[words] = unsafe { *(head as *mut *mut u8) };
                return head;
            }
        }
        // The free lists are empty while an arena is active (`arena_enter`),
        // so every allocation in one reaches here.
        self.alloc_slow(words)
    }

    /// `alloc_bytes` when no recycled block fits: bump the top arena if one
    /// is active (the free lists are empty then, see `arena_enter`), and
    /// otherwise ask the system allocator.
    #[inline(never)]
    fn alloc_slow(&mut self, words: usize) -> *mut u8 {
        if self.arenas_active != 0 { return self.arena_alloc(words * 8); }
        let layout = Layout::from_size_align(words * 8, 8).expect("heap block layout");
        // `alloc` signals failure with a null pointer, which every caller
        // here would otherwise write a header through. Route it to the
        // standard OOM handler instead of producing a null object.
        let p = unsafe { alloc(layout) };
        if p.is_null() { handle_alloc_error(layout); }
        p
    }

    /// Return `size` bytes at `ptr` (as passed to `alloc_bytes`) for reuse.
    #[inline]
    fn free_bytes(&mut self, ptr: *mut u8, size: usize) {
        let words = words_for(size);
        if words <= MAX_FREE_WORDS {
            unsafe { *(ptr as *mut *mut u8) = self.free_lists[words]; }
            self.free_lists[words] = ptr;
            return;
        }
        let layout = Layout::from_size_align(words * 8, 8).expect("heap block layout");
        unsafe { dealloc(ptr, layout) }
    }

    // ── Arenas ───────────────────────────────────────────────────────────────

    /// Bump `size` bytes (a whole number of words) from the top arena.
    #[inline(never)]
    fn arena_alloc(&mut self, size: usize) -> *mut u8 {
        let arena = &mut self.arenas[self.arenas_active - 1];
        if (arena.end as usize) - (arena.ptr as usize) < size {
            let last = arena.chunks.last().map_or(ARENA_FIRST_CHUNK / 2, |c| c.1);
            let chunk = (last * 2).max(size);
            let layout = Layout::from_size_align(chunk, 8).expect("arena chunk layout");
            let p = unsafe { alloc(layout) };
            if p.is_null() { handle_alloc_error(layout); }
            arena.chunks.push((p, chunk));
            arena.ptr = p;
            arena.end = unsafe { p.add(chunk) };
        }
        let p = arena.ptr;
        arena.ptr = unsafe { p.add(size) };
        p
    }

    /// Start a `with Arena` region. Allocation bumps it, and frees do
    /// nothing, until `arena_evac_begin`.
    pub fn arena_enter(&mut self) {
        debug_assert_eq!(self.arenas.len(), self.arenas_active, "arena_enter during an evacuation");
        let none = std::ptr::null_mut();
        self.arenas.push(Arena {
            chunks: Vec::new(), ptr: none, end: none, dicts: Vec::new(),
            bytes0: self.bytes_allocated, live0: self.live_objects, bytes1: 0, live1: 0,
        });
        if self.arenas_active == 0 { std::mem::swap(&mut self.free_lists, &mut self.free_lists_aside); }
        self.arenas_active += 1;
    }

    /// The top arena stops accepting allocations: the next one down (or the
    /// heap) does, so the caller can copy the block's result out with
    /// `clone_obj`. Then `arena_release` frees the arena.
    pub fn arena_evac_begin(&mut self) {
        let arena = self.arenas.last_mut().expect("arena_evac_begin without arena_enter");
        arena.bytes1 = self.bytes_allocated;
        arena.live1 = self.live_objects;
        self.arenas_active -= 1;
        if self.arenas_active == 0 { std::mem::swap(&mut self.free_lists, &mut self.free_lists_aside); }
    }

    /// Free the arena `arena_evac_begin` detached: its chunks, and the index
    /// of each dict in it. The live-object and byte counts drop by what it
    /// held, so the leak check still sees only objects from outside.
    pub fn arena_release(&mut self) {
        debug_assert_eq!(self.arenas.len(), self.arenas_active + 1, "arena_release without arena_evac_begin");
        let arena = self.arenas.pop().expect("arena_release without arena_enter");
        for d in arena.dicts {
            unsafe { drop(Box::from_raw((*d).index as *mut Box<dyn crate::runtime::dict::DictIndex>)) };
        }
        for (p, size) in arena.chunks {
            unsafe { dealloc(p, Layout::from_size_align(size, 8).expect("arena chunk layout")) };
        }
        self.bytes_allocated -= arena.bytes1 - arena.bytes0;
        self.live_objects -= arena.live1 - arena.live0;
    }

    /// Grow a list's or dict's buffer from `old_size` to `new_size` bytes,
    /// keeping its contents: `alloc` when there is nothing to carry forward
    /// (`old_size == 0`, a dict's buffers before its first insert), `realloc`
    /// otherwise. In an arena the old buffer stays where it is, unfreed. The
    /// caller adds `new_size - old_size` to `bytes_allocated`.
    ///
    /// # Safety
    ///
    /// `old_ptr` is null iff `old_size == 0`, and otherwise a buffer of
    /// `old_size` bytes this heap allocated.
    pub(crate) unsafe fn grow_buffer(&mut self, old_ptr: *mut u8, old_size: usize, new_size: usize) -> *mut u8 {
        if self.arenas_active != 0 {
            let p = self.arena_alloc(new_size.next_multiple_of(8));
            if old_size > 0 { std::ptr::copy_nonoverlapping(old_ptr, p, old_size); }
            return p;
        }
        let new_layout = Layout::from_size_align(new_size, 8).expect("buffer layout");
        let p = if old_size == 0 {
            alloc(new_layout)
        } else {
            std::alloc::realloc(old_ptr, Layout::from_size_align(old_size, 8).expect("buffer layout"), new_size)
        };
        if p.is_null() { handle_alloc_error(new_layout); }
        p
    }

    pub fn new() -> Self {
        Heap {
            bytes_allocated: 0,
            free_lists:      vec![std::ptr::null_mut(); MAX_FREE_WORDS + 1],
            track_frees:     std::env::var_os("FROG_LEAK_CHECK").is_some(),
            live_objects:    0,
            freed:           std::collections::HashSet::new(),
            worklist:        Vec::new(),
            arenas:          Vec::new(),
            arenas_active:   0,
            free_lists_aside: vec![std::ptr::null_mut(); MAX_FREE_WORDS + 1],
            dict_backend:    std::sync::Arc::new(crate::runtime::dict::HashbrownBackend),
        }
    }

    /// Call `f` with a pointer to every pointer slot inside `obj` (the
    /// columns its `ptr_mask` marks), so `drop_tree`, `tree_len` and
    /// `clone_obj` share one idea of what an object owns.
    unsafe fn for_each_slot(obj: *mut ObjHeader, f: &mut impl FnMut(*mut i64)) {
        match (*obj).kind {
            ObjKind::Str => {}
            ObjKind::List => {
                let list = obj as *mut FrogList;
                let mask = (*list).ptr_mask;
                if mask == 0 { return; }
                let stride = ((*list).stride as usize).max(1);
                for i in 0..((*list).len as usize / stride) {
                    for bit in 0..stride {
                        if mask & (1u64 << bit) == 0 { continue; }
                        f((*list).data.add(i * stride + bit));
                    }
                }
            }
            ObjKind::Dict => {
                let dict = obj as *mut FrogDict;
                let mask = (*dict).ptr_mask;
                if mask == 0 { return; }
                let stride = ((*dict).kstride + (*dict).vstride).max(1) as usize;
                for i in 0..(*dict).len as usize {
                    for bit in 0..stride {
                        if mask & (1u64 << bit) == 0 { continue; }
                        f((*dict).entries.add(i * stride + bit));
                    }
                }
            }
            ObjKind::Variant => {
                let variant = obj as *mut FrogVariant;
                let mask = (*variant).ptr_mask;
                if mask == 0 { return; }
                let data = (obj as *mut u8).add(std::mem::size_of::<FrogVariant>()) as *mut i64;
                for i in 0..((*variant).nslots as usize) {
                    if mask & (1u64 << i) == 0 { continue; }
                    f(data.add(i));
                }
            }
        }
    }

    /// Free a single object; returns the number of bytes freed. The
    /// blocks go onto `free_lists` for reuse rather than back to the system
    /// allocator — see `free_bytes`. The byte counts returned here are the
    /// same rounded sizes the matching `alloc_*` added to
    /// `bytes_allocated`, so the running total stays exact.
    unsafe fn free_obj(&mut self, obj: *mut ObjHeader) -> usize {
        self.live_objects -= 1;
        match (*obj).kind {
            ObjKind::Str => {
                let str_ptr = obj as *mut FrogStr;
                let len = (*str_ptr).len as usize;
                let total = words_for(std::mem::size_of::<FrogStr>() + len + 1) * 8;
                heap_trace!("free {:p} Str  {} bytes", obj, total);
                self.free_bytes(obj as *mut u8, total);
                total
            }
            ObjKind::List => {
                let list_ptr = obj as *mut FrogList;
                let cap = (*list_ptr).cap as usize;
                let data_size = cap * std::mem::size_of::<i64>();
                let list_size = words_for(std::mem::size_of::<FrogList>()) * 8;
                heap_trace!("free {:p} List {} bytes", obj, list_size + data_size);
                let data = (*list_ptr).data as *mut u8;
                self.free_bytes(obj as *mut u8, list_size);
                if cap > 0 {
                    self.free_bytes(data, data_size);
                }
                list_size + data_size
            }
            ObjKind::Variant => {
                let variant_ptr = obj as *mut FrogVariant;
                let nslots = (*variant_ptr).nslots as usize;
                let total = words_for(
                    std::mem::size_of::<FrogVariant>() + nslots * std::mem::size_of::<i64>()) * 8;
                heap_trace!("free {:p} Variant {} bytes", obj, total);
                self.free_bytes(obj as *mut u8, total);
                total
            }
            ObjKind::Dict => {
                let dict_ptr = obj as *mut FrogDict;
                let cap = (*dict_ptr).cap as usize;
                let stride = ((*dict_ptr).kstride + (*dict_ptr).vstride).max(1) as usize;
                let entries_size = cap * stride * std::mem::size_of::<i64>();
                let hashes_size = cap * std::mem::size_of::<u64>();
                let dict_size = words_for(std::mem::size_of::<FrogDict>()) * 8;
                heap_trace!("free {:p} Dict {} bytes", obj, dict_size + entries_size + hashes_size);
                // Not part of `bytes_allocated`/`free_bytes`'s accounting —
                // an ordinary Rust `Box`, not a free-listed heap block (see
                // `FrogDict`'s doc comment on why it's double-boxed).
                drop(Box::from_raw((*dict_ptr).index as *mut Box<dyn crate::runtime::dict::DictIndex>));
                let entries = (*dict_ptr).entries as *mut u8;
                let hashes = (*dict_ptr).hashes as *mut u8;
                self.free_bytes(obj as *mut u8, dict_size);
                if entries_size > 0 {
                    self.free_bytes(entries, entries_size);
                }
                if hashes_size > 0 {
                    self.free_bytes(hashes, hashes_size);
                }
                dict_size + entries_size + hashes_size
            }
        }
    }

    // ── Allocators ───────────────────────────────────────────────────────────

    /// Allocate a heap `FrogStr` holding a copy of `data`, with a NUL
    /// terminator appended.
    ///
    /// Takes a slice rather than the `(ptr, len)` pair the FFI boundary
    /// deals in: every caller but `ffi::frog_alloc_str` already has one, and
    /// keeping the raw-pointer reconstruction on that side puts the `unsafe`
    /// where the unchecked assumption actually is.
    pub fn alloc_str(&mut self, data: &[u8]) -> *mut FrogStr {
        let len = data.len();
        let struct_size = std::mem::size_of::<FrogStr>();
        let total = words_for(struct_size + len + 1) * 8;
        let ptr = self.alloc_bytes(total) as *mut FrogStr;
        unsafe {
            (*ptr).header = ObjHeader { kind: ObjKind::Str };
            (*ptr).len = len as u32;
            let dst = (ptr as *mut u8).add(struct_size);
            std::ptr::copy_nonoverlapping(data.as_ptr(), dst, len);
            *dst.add(len) = 0;  // NUL terminator
        }

        self.note_alloc(ptr as *mut ObjHeader);
        self.bytes_allocated += total;
        heap_trace!("alloc Str  {} bytes -> {:p}  (total: {} bytes)",
            total, ptr, self.bytes_allocated);
        ptr
    }

    /// Allocate a heap FrogList with room for `cap` elements, each
    /// `stride` `i64` slots wide (`stride == 1` for every non-struct element
    /// type). `ptr_mask` marks which of the `stride` per-element slot
    /// offsets are heap pointers — see `FrogList`'s doc comment.
    /// The data buffer is separately allocated (not an object of its own).
    pub fn alloc_list(&mut self, cap: usize, stride: usize, ptr_mask: u64) -> *mut FrogList {
        // A zero-wide element doesn't mean anything (every caller already
        // computes stride as a leaf/slot count, which is at least 1 for any
        // real type — see `struct_fields`, `FromFrog`/`ToFrog::SLOTS`).
        // Rejecting it here, rather than silently clamping to 1 as before,
        // turns a future stride-computation bug into a clear panic instead
        // of a list quietly built with the wrong element width.
        assert!(stride > 0, "alloc_list: stride must be nonzero");
        let actual_elem_cap = cap.max(1);
        let slot_cap = actual_elem_cap * stride;
        let data_size = slot_cap * std::mem::size_of::<i64>();
        let data = self.alloc_bytes(data_size) as *mut i64;

        let list_size = words_for(std::mem::size_of::<FrogList>()) * 8;
        let ptr = self.alloc_bytes(list_size) as *mut FrogList;

        unsafe {
            (*ptr).header = ObjHeader { kind: ObjKind::List };
            (*ptr).len        = 0;
            (*ptr).cap        = slot_cap as u32;
            (*ptr).stride     = stride as u32;
            (*ptr).ptr_mask   = ptr_mask;
            (*ptr).data       = data;
        }

        self.note_alloc(ptr as *mut ObjHeader);
        self.bytes_allocated += list_size + data_size;
        heap_trace!("alloc List {} bytes -> {:p}  (total: {} bytes)",
            list_size + data_size, ptr, self.bytes_allocated);
        ptr
    }

    /// Allocate a `FrogVariant` with `nslots` `i64` payload slots,
    /// zero-initialized (so dropping one whose fields are still being
    /// computed never follows garbage through an as-yet-unwritten slot). `ptr_mask` marks which
    /// slots are heap pointers, exactly like `alloc_list`'s.
    pub fn alloc_variant(&mut self, tag: u32, nslots: usize, ptr_mask: u64) -> *mut FrogVariant {
        let struct_size = std::mem::size_of::<FrogVariant>();
        let data_size = nslots * std::mem::size_of::<i64>();
        let total = words_for(struct_size + data_size) * 8;
        let ptr = self.alloc_bytes(total) as *mut FrogVariant;
        unsafe {
            (*ptr).header = ObjHeader { kind: ObjKind::Variant };
            (*ptr).tag        = tag;
            (*ptr).nslots     = nslots as u32;
            (*ptr).ptr_mask   = ptr_mask;
            // Zero the payload. `write_bytes` compiles to a `memset`
            // *call* even for one or two slots, which is the common case
            // here and showed up in `benches/orders.frog`'s profile costing
            // as much as the allocation it belongs to. Store the small
            // counts directly and keep the libcall for genuinely wide
            // payloads.
            let dst = (ptr as *mut u8).add(struct_size) as *mut i64;
            match nslots {
                0 => {}
                1 => { *dst = 0; }
                2 => { *dst = 0; *dst.add(1) = 0; }
                3 => { *dst = 0; *dst.add(1) = 0; *dst.add(2) = 0; }
                4 => { *dst = 0; *dst.add(1) = 0; *dst.add(2) = 0; *dst.add(3) = 0; }
                _ => std::ptr::write_bytes(dst, 0, nslots),
            }
        }

        self.note_alloc(ptr as *mut ObjHeader);
        self.bytes_allocated += total;
        heap_trace!("alloc Variant tag={} {} bytes -> {:p}  (total: {} bytes)",
            tag, total, ptr, self.bytes_allocated);
        ptr
    }

    /// Allocate a heap, empty `FrogDict` with room for `cap` entries,
    /// each `kstride + vstride` `i64` slots wide. `ptr_mask` marks
    /// scannable columns across one whole entry (key columns first, then
    /// value columns), exactly `alloc_list`'s convention. The entries
    /// buffer is separately allocated (not an object of its own), and a fresh empty
    /// index is built from `self.dict_backend` — see `FrogDict`'s doc
    /// comment for why the index is stored double-boxed.
    pub fn alloc_dict(&mut self, cap: usize, kstride: usize, vstride: usize, ptr_mask: u64, key_kind: u32) -> *mut FrogDict {
        let stride = (kstride + vstride).max(1);
        let entries_size = cap * stride * std::mem::size_of::<i64>();
        let entries = if entries_size > 0 { self.alloc_bytes(entries_size) as *mut i64 } else { std::ptr::null_mut() };
        let hashes_size = cap * std::mem::size_of::<u64>();
        let hashes = if hashes_size > 0 { self.alloc_bytes(hashes_size) as *mut u64 } else { std::ptr::null_mut() };

        let dict_size = words_for(std::mem::size_of::<FrogDict>()) * 8;
        let ptr = self.alloc_bytes(dict_size) as *mut FrogDict;

        let index = self.dict_backend.new_index(cap);
        let boxed: Box<Box<dyn crate::runtime::dict::DictIndex>> = Box::new(index);

        unsafe {
            (*ptr).header = ObjHeader { kind: ObjKind::Dict };
            (*ptr).len       = 0;
            (*ptr).cap       = cap as u32;
            (*ptr).kstride   = kstride as u32;
            (*ptr).vstride   = vstride as u32;
            (*ptr).ptr_mask  = ptr_mask;
            (*ptr).key_kind  = key_kind;
            (*ptr).entries   = entries;
            (*ptr).hashes    = hashes;
            (*ptr).index     = Box::into_raw(boxed) as *mut ();
        }

        if self.arenas_active != 0 {
            self.arenas[self.arenas_active - 1].dicts.push(ptr);
        }
        self.note_alloc(ptr as *mut ObjHeader);
        self.bytes_allocated += dict_size + entries_size + hashes_size;
        heap_trace!("alloc Dict {} bytes -> {:p}  (total: {} bytes)",
            dict_size + entries_size + hashes_size, ptr, self.bytes_allocated);
        ptr
    }

    /// Deep-clone the heap object at `obj`, returning a pointer that shares
    /// no object with the original — every owner has its own copy, `Str`s
    /// included. Dispatches on `ObjKind` through `for_each_slot`, the same
    /// slots `drop_tree` frees, so the two cannot drift. A pointer column is
    /// not always a plain untagged pointer (an inline union's column carries
    /// its member tag), so each slot's tag bits are re-OR'd onto its copy.
    ///
    /// Iterative: `shallow_copy` copies one object with its slots still
    /// naming the originals, and the worklist then re-points each slot of
    /// each copy at a shallow copy of its own, so a very deep `Cons` list
    /// cannot overflow the stack.
    ///
    /// **Assumes the object graph is a tree**, which single ownership
    /// guarantees: there is no way to build a cycle or a shared child from
    /// surface syntax (MUTABILITY.md's "Graphs get arenas").
    ///
    /// # Safety
    ///
    /// `obj` must be a live heap object of this heap.
    pub unsafe fn clone_obj(&mut self, obj: *mut ObjHeader) -> *mut ObjHeader {
        let top = self.shallow_copy(obj);
        if !Self::has_slots(top) {
            return top;
        }
        let mut work = std::mem::take(&mut self.worklist);
        work.push(top);
        while let Some(copy) = work.pop() {
            Self::for_each_slot(copy, &mut |p| {
                let w = *p;
                if !is_heap_ptr(w) { return; }
                let child = self.shallow_copy(heap_ptr(w));
                *p = (child as i64) | (w & TAG_MASK);
                if Self::has_slots(child) { work.push(child); }
            });
        }
        self.worklist = work;
        top
    }

    /// Whether `obj` can point at other objects at all.
    unsafe fn has_slots(obj: *mut ObjHeader) -> bool {
        match (*obj).kind {
            ObjKind::Str => false,
            ObjKind::List => (*(obj as *mut FrogList)).ptr_mask != 0,
            ObjKind::Dict => (*(obj as *mut FrogDict)).ptr_mask != 0,
            ObjKind::Variant => (*(obj as *mut FrogVariant)).ptr_mask != 0,
        }
    }

    /// A copy of `obj` alone, its slots still naming `obj`'s children (see
    /// `clone_obj`).
    unsafe fn shallow_copy(&mut self, obj: *mut ObjHeader) -> *mut ObjHeader {
        match (*obj).kind {
            ObjKind::Str => {
                let len = (*(obj as *const FrogStr)).len as usize;
                let data = std::slice::from_raw_parts((obj as *const u8).add(std::mem::size_of::<FrogStr>()), len);
                self.alloc_str(data) as *mut ObjHeader
            }
            ObjKind::List => {
                let src = obj as *mut FrogList;
                let stride = ((*src).stride as usize).max(1);
                let len = (*src).len as usize;
                let new_list = self.alloc_list((len / stride).max(1), stride, (*src).ptr_mask);
                if len > 0 {
                    std::ptr::copy_nonoverlapping((*src).data, (*new_list).data, len);
                }
                (*new_list).len = len as u32;
                new_list as *mut ObjHeader
            }
            ObjKind::Dict => {
                let src = obj as *mut FrogDict;
                let len = (*src).len as usize;
                let kstride = (*src).kstride as usize;
                let vstride = (*src).vstride as usize;
                let stride = (kstride + vstride).max(1);
                let mask = (*src).ptr_mask;
                let key_kind = (*src).key_kind;
                let new_dict = self.alloc_dict(len, kstride, vstride, mask, key_kind);
                if len > 0 {
                    std::ptr::copy_nonoverlapping((*src).entries, (*new_dict).entries, len * stride);
                    // A key's hash is of its content, which a copy keeps, so
                    // the cached hashes carry over and the index is rebuilt
                    // from them without re-scanning any `Str` key — cheaper
                    // than carrying the source index's layout across.
                    std::ptr::copy_nonoverlapping((*src).hashes, (*new_dict).hashes, len);
                }
                (*new_dict).len = len as u32;
                let index = crate::runtime::dict::dict_index_mut(new_dict);
                let mut pairs = (0..len).map(|i| (*(*new_dict).hashes.add(i), i as u32));
                index.rebuild(&mut pairs);
                new_dict as *mut ObjHeader
            }
            ObjKind::Variant => {
                let src = obj as *mut FrogVariant;
                let nslots = (*src).nslots as usize;
                let new_variant = self.alloc_variant((*src).tag, nslots, (*src).ptr_mask);
                let header = std::mem::size_of::<FrogVariant>();
                std::ptr::copy_nonoverlapping(
                    (obj as *const u8).add(header) as *const i64,
                    (new_variant as *mut u8).add(header) as *mut i64,
                    nslots,
                );
                new_variant as *mut ObjHeader
            }
        }
    }

    /// Print a summary of this heap's live objects to stderr.
    pub fn dump(&self) {
        eprintln!("=== heap: {} live object(s), {} bytes ===", self.live_objects, self.bytes_allocated);
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Print a summary of the active heap to stderr. Safe to call at any time
/// from Rust. Also callable as `heap_dump()` from froglang.
pub fn heap_dump() {
    let ptr = ACTIVE_HEAP.with(|h| h.get());
    if !ptr.is_null() {
        unsafe { (*ptr).dump(); }
    } else {
        TASK_HEAP.with(|h| h.borrow().dump());
    }
}

/// Borrow the inline bytes of a FrogStr as a &str (panics on invalid UTF-8).
/// # Safety
/// `ptr` must be a valid, live FrogStr pointer.
pub unsafe fn frog_str_as_str<'a>(ptr: *const FrogStr) -> &'a str {
    let len = (*ptr).len as usize;
    let data = (ptr as *const u8).add(std::mem::size_of::<FrogStr>());
    let bytes = std::slice::from_raw_parts(data, len);
    std::str::from_utf8(bytes).unwrap_or("<invalid utf-8>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alloc_str_bytes_and_len() {
        TASK_HEAP.with(|h| {
            let mut h = h.borrow_mut();
            let s = b"hello";
            let ptr = h.alloc_str(s);
            unsafe {
                assert_eq!((*ptr).len, 5);
                let txt = frog_str_as_str(ptr as *const FrogStr);
                assert_eq!(txt, "hello");
            }
        });
    }

    // ── Arenas ───────────────────────────────────────────────────────────────

    /// A list of `n` strings, as an owning tree (`ptr_mask` 1).
    fn str_list(h: &mut Heap, n: usize) -> *mut FrogList {
        let list = h.alloc_list(n, 1, 1);
        for i in 0..n {
            let s = h.alloc_str(format!("item {i}").as_bytes());
            unsafe { *(*list).data.add(i) = s as i64; (*list).len += 1; }
        }
        list
    }

    #[test]
    fn arena_frees_nothing_until_release_then_counts_return() {
        let mut h = Heap::new();
        let outer = h.alloc_str(b"outer");
        let (bytes, live) = (h.bytes_allocated, h.live_objects());
        h.arena_enter();
        let list = str_list(&mut h, 10);
        assert_eq!(h.live_objects(), live + 11);
        // A drop in an arena is a no-op, and the objects stay readable.
        h.drop_word(list as i64);
        assert_eq!(h.live_objects(), live + 11);
        unsafe { assert_eq!(frog_str_as_str(*(*list).data as *const FrogStr), "item 0"); }
        h.arena_evac_begin();
        h.arena_release();
        assert_eq!((h.bytes_allocated, h.live_objects()), (bytes, live));
        h.drop_word(outer as i64);
        assert_eq!(h.live_objects(), 0);
    }

    #[test]
    fn arena_outer_frees_still_work_after_exit() {
        let mut h = Heap::new();
        h.arena_enter();
        h.alloc_str(b"scratch");
        h.arena_evac_begin();
        h.arena_release();
        let s = h.alloc_str(b"after");
        h.drop_word(s as i64);
        assert_eq!((h.live_objects(), h.bytes_allocated), (0, 0));
    }

    #[test]
    fn arena_oversized_object_gets_its_own_chunk() {
        let mut h = Heap::new();
        h.arena_enter();
        let big = vec![b'x'; 3 * ARENA_FIRST_CHUNK];
        let a = h.alloc_str(b"small");
        let b = h.alloc_str(&big);
        let c = h.alloc_str(b"after");
        unsafe {
            assert_eq!((*b).len as usize, big.len());
            assert_eq!(frog_str_as_str(a as *const FrogStr), "small");
            assert_eq!(frog_str_as_str(c as *const FrogStr), "after");
        }
        h.arena_evac_begin();
        h.arena_release();
        assert_eq!((h.live_objects(), h.bytes_allocated), (0, 0));
    }

    #[test]
    fn arena_list_growth_keeps_contents() {
        let mut h = Heap::new();
        ACTIVE_HEAP.with(|p| p.set(&mut h as *mut Heap));
        h.arena_enter();
        let list = h.alloc_list(1, 1, 0);
        for i in 0..1000 { crate::runtime::ffi::frog_list_push(list as i64, i); }
        unsafe {
            assert_eq!((*list).len, 1000);
            assert_eq!(*(*list).data.add(999), 999);
        }
        h.arena_evac_begin();
        h.arena_release();
        ACTIVE_HEAP.with(|p| p.set(std::ptr::null_mut()));
        assert_eq!((h.live_objects(), h.bytes_allocated), (0, 0));
    }

    #[test]
    fn arena_nested_evacuates_into_the_enclosing_arena() {
        let mut h = Heap::new();
        h.arena_enter();
        let outer_list = str_list(&mut h, 3);
        h.arena_enter();
        let inner = str_list(&mut h, 5);
        let live_inner = h.live_objects();
        h.arena_evac_begin();
        // The copy lands in the outer arena, so it survives the inner release.
        let copy = unsafe { h.clone_obj(inner as *mut ObjHeader) } as *mut FrogList;
        h.arena_release();
        assert_eq!(h.live_objects(), live_inner - 6 + 6);
        unsafe {
            assert_eq!((*copy).len, 5);
            assert_eq!(frog_str_as_str(*(*copy).data.add(4) as *const FrogStr), "item 4");
        }
        let _ = outer_list;
        h.arena_evac_begin();
        h.arena_release();
        assert_eq!((h.live_objects(), h.bytes_allocated), (0, 0));
    }

    #[test]
    fn arena_evacuates_list_variant_dict_and_str() {
        let mut h = Heap::new();
        ACTIVE_HEAP.with(|p| p.set(&mut h as *mut Heap));
        h.arena_enter();
        let list = str_list(&mut h, 4);
        // A variant holding the list, and a dict whose values are variants.
        let var = h.alloc_variant(1, 1, 1);
        unsafe { *((var as *mut u8).add(std::mem::size_of::<FrogVariant>()) as *mut i64) = list as i64; }
        let dict = h.alloc_dict(2, 1, 1, 0b10, 0);
        let d = dict as i64;
        let e = crate::runtime::dict::frog_dict_insert(d, 7);
        crate::runtime::dict::frog_dict_set_slot(d, e, 1, var as i64);
        let name = h.alloc_str(b"name");
        h.arena_evac_begin();
        let dict2 = unsafe { h.clone_obj(dict as *mut ObjHeader) };
        let name2 = unsafe { h.clone_obj(name as *mut ObjHeader) };
        h.arena_release();
        ACTIVE_HEAP.with(|p| p.set(std::ptr::null_mut()));
        // 1 dict + 1 variant + 1 list + 4 strs, and the name.
        assert_eq!(h.live_objects(), 7 + 1);
        let found = crate::runtime::dict::frog_dict_find(dict2 as i64, 7);
        assert!(found >= 0);
        h.drop_word(dict2 as i64);
        h.drop_word(name2 as i64);
        assert_eq!((h.live_objects(), h.bytes_allocated), (0, 0));
    }
}
