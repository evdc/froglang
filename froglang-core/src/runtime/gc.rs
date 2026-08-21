use std::alloc::{alloc, dealloc, Layout};
use std::cell::{Cell, RefCell};

/// Zero-cost GC tracing. Enable with `--features gc_trace`.
/// Expands to an eprintln! at compile time; completely removed without the feature.
macro_rules! gc_trace {
    ($fmt:literal $(, $arg:expr)* $(,)?) => {{
        #[cfg(feature = "gc_trace")]
        eprintln!(concat!("[GC] ", $fmt) $(, $arg)*);
    }};
}

// ── Object kinds ─────────────────────────────────────────────────────────────

#[repr(u8)]
pub enum ObjKind { Str = 0, List = 1, Variant = 2 }

// ── GC header (prefix for every heap object) ─────────────────────────────────

#[repr(C)]
pub struct GcHeader {
    pub next:   *mut GcHeader,
    pub marked: bool,
    pub kind:   ObjKind,
}

// ── FrogStr — immutable, inline bytes immediately after the struct ────────────

#[repr(C)]
pub struct FrogStr {
    pub header: GcHeader,
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
// pointer) — `GcHeap::mark`'s List-tracing loop below consults this once
// per element block instead of assuming every slot (the old `ElemTag`) or
// no slots are pointers.
#[repr(C)]
pub struct FrogList {
    pub header:   GcHeader,
    pub len:      u32,
    pub cap:      u32,
    pub stride:   u32,
    pub ptr_mask: u64,
    /// Bit `i` set means slot `i` within each element block is a
    /// *conditional* pointer: a heap pointer iff the element's slot `i-1`
    /// (its tag — always the immediately preceding leaf, see
    /// `struct_fields`'s two-slot `Union` arm) holds one of `boxed_tags`'s
    /// bits. Exists because a scalar-carrying union (`Int | Str`) uses an
    /// unboxed `{tag, payload}` pair when it's a local/param/return (no
    /// allocation needed), but `ptr_mask` alone can't express "this slot is
    /// a pointer only sometimes" — see `codegen::gc_masks`, the one place
    /// that computes this. At most one distinct such union shape is
    /// supported per list (`TypeChecker::check_scalar_union_consistency`
    /// rejects more before codegen ever sees them), so one `boxed_tags` set
    /// is enough regardless of how many `cond_mask` bits are set.
    pub cond_mask:  u64,
    pub boxed_tags: u64,
    pub data:     *mut i64,
}

// ── FrogVariant — immutable, inline payload immediately after the struct ──────
//
// Runtime representation of a nominal union's member (`data X is A | B` —
// the language's only sum type): `tag` is the member's declaration index
// within its union (fixed by `TypeChecker::UnionDef`, consulted only at
// codegen time — the GC itself never needs to know which union this came
// from). The payload is `nslots` `i64`s, laid out as the union's common
// fields (flattened, in declared order) followed by this member's own
// fields (flattened, in declared order) — see
// `codegen::enum_field_leaf_types`. `ptr_mask` marks which payload slots
// are heap pointers, exactly like `FrogList::ptr_mask` but one bit per
// slot directly (no `stride` — a variant is always exactly one "element").
#[repr(C)]
pub struct FrogVariant {
    pub header:   GcHeader,
    pub tag:      u32,
    pub nslots:   u32,
    pub ptr_mask: u64,
    /// See `FrogList::cond_mask`'s doc comment — the identical mechanism,
    /// applied to a boxed union/struct's own payload slots instead of a
    /// list's per-element slots.
    pub cond_mask:  u64,
    pub boxed_tags: u64,
    _data: [i64; 0],  // zero-sized marker; slots live at (ptr + size_of::<FrogVariant>())
}

// ── Immediate (unboxed) values ────────────────────────────────────────────────
//
// A payload-less enum variant — no fields of its own and no common fields on
// its enum, e.g. `Red` in `data Color is Red | Green | Blue`, or `NoDiscount`
// in an enum whose other variants do carry fields — needs no heap object at
// all: the tag *is* the whole value.  Codegen emits such a value as the
// immediate `(tag << 1) | 1` instead of calling `frog_alloc_variant`
// (see `TypedExprKind::VariantInit` in codegen/mod.rs).
//
// The low bit is what tells the two apart: every heap object comes from
// `alloc`, so its address is at least 8-byte aligned and has the low bit
// clear.  An enum-typed slot therefore holds either a real pointer or an
// odd immediate, and everything that follows enum-typed words — the mark
// phase's roots, shadow-stack slots, list element slots, and variant payload
// slots — must ask `is_heap_ptr` first rather than dereferencing blind.

/// Is `v` a pointer the GC may follow, rather than 0 (an empty slot) or an
/// unboxed immediate (low bit set — see above)?
#[inline]
pub fn is_heap_ptr(v: i64) -> bool {
    v != 0 && v & 1 == 0
}

/// Encode variant index `tag` as an unboxed enum value.
#[inline]
pub fn immediate_variant(tag: u32) -> i64 {
    ((tag as i64) << 1) | 1
}

/// Decode an unboxed enum value produced by `immediate_variant`.
#[inline]
pub fn immediate_variant_tag(v: i64) -> i64 {
    v >> 1
}

// ── Shadow stack ──────────────────────────────────────────────────────────────
//
// Codegen roots every heap pointer produced inside a JIT-compiled function by
// storing it into a dedicated stack slot immediately after it's computed
// (`root_heap_value` in codegen/mod.rs). Each function pushes one `ShadowFrame`
// describing that slot's memory at entry and pops it before returning, so the
// frames form a linked list that mirrors the native call stack. The mark phase
// walks every frame and treats every non-zero slot as a live root — this is
// conservative (a value can outlive its last use within one call) but never
// under-roots, since a value stays reachable until the frame that stored it
// returns.

/// One JIT function's shadow-stack frame, living *inside* that function's
/// own native stack frame: a `prev` link to the caller's frame, this
/// frame's root-slot count, then `len` `i64` root slots laid out inline
/// immediately after (offset `size_of::<ShadowFrame>()`).
///
/// Codegen emits the push and the pop as a handful of plain loads and
/// stores in the prologue/epilogue (`setup_shadow_frame` /
/// `teardown_shadow_frame` in codegen/mod.rs). This used to be a
/// `Vec<ShadowFrame>` entry pushed by an out-of-line `frog_frame_push`
/// call, and that call — plus the thread-local lookup it needed to find
/// the heap, plus the `memset` libcall it made to zero the slots — was the
/// single largest cost in the `orders` benchmark's profile, ahead of both
/// `malloc`/`free` and the collector itself.
#[repr(C)]
pub struct ShadowFrame {
    pub prev: *mut ShadowFrame,
    pub len:  usize,
}

/// The "innermost live frame" cell that the JIT prologue/epilogue update.
/// Its address is baked into the generated machine code as a constant, so
/// the cell must never move: `Codegen` owns exactly one in a `Box` and
/// hands `GcHeap` a pointer to it before any JIT code runs (see
/// `FrogState::call_jit`). One cell per `Codegen` — not a process global —
/// keeps `FrogState`s on different threads independent, as they have
/// always been.
#[repr(transparent)]
pub struct ShadowTop(pub *mut ShadowFrame);

impl ShadowTop {
    pub fn new() -> Self { ShadowTop(std::ptr::null_mut()) }
}

// ── GcHeap ───────────────────────────────────────────────────────────────────

pub struct GcHeap {
    head:            *mut GcHeader,  // head of the intrusive linked list of all objects
    pub bytes_allocated: usize,
    gc_threshold:    usize,
    roots:           Vec<(i64, bool)>,  // (value, is_ptr)
    /// Head of the JIT shadow stack: a pointer to the `ShadowTop` cell the
    /// generated code writes, whose `.0` is the innermost live
    /// `ShadowFrame`. Null until `set_shadow_top` is called, which is the
    /// state for a heap that no JIT code has ever run against (an embedding
    /// that only uses `alloc_*` directly, and every unit test in this file).
    shadow_top:      *const ShadowTop,
    /// Recycled blocks from swept objects, bucketed by size in 8-byte
    /// words: `free_lists[w]` heads an intrusive singly-linked list of
    /// blocks of exactly `w * 8` bytes (the `next` link lives in the
    /// block's first word, which is always at least 8 bytes wide).
    ///
    /// Without this, every `data`-union value the `orders` benchmark builds
    /// costs a `malloc` when it is created and a `free` when it is swept —
    /// together about a fifth of that benchmark's profile, for blocks whose
    /// sizes repeat endlessly. Sizes above `MAX_FREE_WORDS` (rare, and not
    /// repetitive enough to be worth retaining) go straight back to the
    /// system allocator.
    ///
    /// Recycled bytes are held for the process's lifetime rather than
    /// returned to the OS. That is the usual trade for a bump/free-list
    /// nursery: `bytes_allocated` still counts only *live* bytes, so the
    /// collection threshold is unaffected.
    free_lists:      Vec<*mut u8>,
    /// Reusable mark-phase worklist. Kept on the heap rather than allocated
    /// per `mark` call: `mark` is invoked once per root, so a fresh `Vec`
    /// each time is a `malloc`/`free` pair per root per collection.
    mark_worklist:   Vec<*mut GcHeader>,
}

thread_local! {
    pub static GC_HEAP: RefCell<GcHeap> = RefCell::new(GcHeap::new());
    /// Pointer to the GcHeap of the FrogState currently executing on this thread.
    /// Null when no froglang code is running (falls back to GC_HEAP).
    pub static ACTIVE_HEAP: Cell<*mut GcHeap> = Cell::new(std::ptr::null_mut());
}

/// Largest block size, in 8-byte words, that `GcHeap::free_bytes` keeps on
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
    ((size + 7) / 8).max(1)
}

impl GcHeap {
    /// Allocate `size` bytes (rounded up to a word), reusing a recycled
    /// block of that exact size class if one is available. All GC object
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
        let layout = Layout::from_size_align(words * 8, 8).expect("gc block layout");
        unsafe { alloc(layout) }
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
        let layout = Layout::from_size_align(words * 8, 8).expect("gc block layout");
        unsafe { dealloc(ptr, layout) }
    }

    pub fn new() -> Self {
        GcHeap {
            head:            std::ptr::null_mut(),
            bytes_allocated: 0,
            gc_threshold:    1024 * 1024,  // 1 MB initial threshold
            roots:           Vec::new(),
            shadow_top:      std::ptr::null(),
            free_lists:      vec![std::ptr::null_mut(); MAX_FREE_WORDS + 1],
            mark_worklist:   Vec::new(),
        }
    }

    pub fn push_root(&mut self, value: i64, is_ptr: bool) {
        self.roots.push((value, is_ptr));
    }

    /// Discard every explicit root pushed via `push_root`. Callers that
    /// re-derive their live root set from scratch before each collection
    /// (e.g. `FrogState::eval`, from its current `env`) should call this
    /// first — otherwise `roots` only ever grows, keeping every value any
    /// caller has ever rooted alive for the process's lifetime.
    pub fn clear_roots(&mut self) {
        self.roots.clear();
    }

    /// Point this heap at the `ShadowTop` cell the JIT code it is about to
    /// run updates, so `collect` can walk that code's frames. `top` must
    /// outlive every collection this heap performs — `Codegen` owns it in a
    /// `Box` for exactly that reason.
    ///
    /// Frames themselves are pushed and popped entirely by generated code;
    /// the runtime never sees an individual push or pop.
    pub fn set_shadow_top(&mut self, top: *const ShadowTop) {
        self.shadow_top = top;
    }

    pub fn maybe_collect(&mut self) {
        if self.bytes_allocated > self.gc_threshold {
            self.collect();
        }
    }

    /// Collect unconditionally, ignoring `gc_threshold`. Mainly for tests
    /// that want a deterministic sweep rather than waiting on the
    /// self-growing threshold (`gc_threshold` doubles the live set on every
    /// collection, so it can take many more allocations than a test wants to
    /// wait for before the *next* automatic collection fires).
    pub fn force_collect(&mut self) {
        self.collect();
    }

    fn collect(&mut self) {
        gc_trace!("collect start — {} bytes allocated, threshold {}",
            self.bytes_allocated, self.gc_threshold);

        // Mark phase — explicit roots pushed by the embedding API...
        let roots = self.roots.clone();
        gc_trace!("marking {} roots", roots.len());
        for (value, is_ptr) in roots {
            if is_ptr && is_heap_ptr(value) {
                unsafe { Self::mark_from(&mut self.mark_worklist, value as *mut GcHeader); }
            }
        }

        // ...plus every live JIT shadow-stack frame, walked from the
        // innermost outwards along the `prev` chain the generated
        // prologues built.
        let mut _nframes = 0usize;
        let mut frame = if self.shadow_top.is_null() {
            std::ptr::null_mut()
        } else {
            unsafe { (*self.shadow_top).0 }
        };
        while !frame.is_null() {
            unsafe {
                let len = (*frame).len;
                let slots = (frame as *mut u8).add(std::mem::size_of::<ShadowFrame>()) as *const i64;
                for i in 0..len {
                    let v = *slots.add(i);
                    if is_heap_ptr(v) {
                        Self::mark_from(&mut self.mark_worklist, v as *mut GcHeader);
                    }
                }
                frame = (*frame).prev;
            }
            _nframes += 1;
        }
        gc_trace!("marked {} shadow frame(s)", _nframes);

        // Sweep phase
        let before = self.bytes_allocated;
        self.sweep();
        let _freed = before - self.bytes_allocated;

        // Update threshold
        self.gc_threshold = (self.bytes_allocated * 2).max(1024 * 1024);
        gc_trace!("collect done  — freed {} bytes, {} bytes live, threshold now {}",
            _freed, self.bytes_allocated, self.gc_threshold);
    }

    /// Mark `obj` and everything transitively reachable from it. Iterative
    /// (explicit worklist) rather than recursive, since the shadow stack
    /// makes deep object graphs reachable from ordinary programs.
    ///
    /// `worklist` is supplied by the caller (`GcHeap::mark_worklist`) and
    /// left empty on return, so a collection with many roots reuses one
    /// allocation instead of making a fresh `Vec` — and freeing it — per
    /// root.
    unsafe fn mark_from(worklist: &mut Vec<*mut GcHeader>, obj: *mut GcHeader) {
        debug_assert!(worklist.is_empty());
        worklist.push(obj);
        while let Some(obj) = worklist.pop() {
            if (*obj).marked { continue; }
            (*obj).marked = true;
            gc_trace!("mark  {:p} ({})", obj,
                match (*obj).kind { ObjKind::Str => "Str", ObjKind::List => "List", ObjKind::Variant => "Variant" });
            match (*obj).kind {
                ObjKind::List => {
                    let list = obj as *mut FrogList;
                    let mask = (*list).ptr_mask;
                    let cond_mask = (*list).cond_mask;
                    let boxed_tags = (*list).boxed_tags;
                    if mask != 0 || cond_mask != 0 {
                        let stride = ((*list).stride as usize).max(1);
                        let elem_len = (*list).len as usize / stride;
                        for i in 0..elem_len {
                            let base = i * stride;
                            for bit in 0..stride {
                                let unconditional = mask & (1u64 << bit) != 0;
                                // A conditional slot's own pointer-ness
                                // depends on its tag, the immediately
                                // preceding slot (see `cond_mask`'s doc
                                // comment) — always in-bounds, since the
                                // tag leaf is never the first of a field.
                                let conditional = cond_mask & (1u64 << bit) != 0
                                    && bit > 0
                                    && boxed_tags & (1u64 << *(*list).data.add(base + bit - 1)) != 0;
                                if unconditional || conditional {
                                    let elem = *(*list).data.add(base + bit);
                                    if is_heap_ptr(elem) {
                                        worklist.push(elem as *mut GcHeader);
                                    }
                                }
                            }
                        }
                    }
                }
                ObjKind::Variant => {
                    let variant = obj as *mut FrogVariant;
                    let mask = (*variant).ptr_mask;
                    let cond_mask = (*variant).cond_mask;
                    let boxed_tags = (*variant).boxed_tags;
                    if mask != 0 || cond_mask != 0 {
                        let nslots = (*variant).nslots as usize;
                        let data = (obj as *mut u8).add(std::mem::size_of::<FrogVariant>()) as *mut i64;
                        for i in 0..nslots {
                            let unconditional = mask & (1u64 << i) != 0;
                            let conditional = cond_mask & (1u64 << i) != 0
                                && i > 0
                                && boxed_tags & (1u64 << *data.add(i - 1)) != 0;
                            if unconditional || conditional {
                                let elem = *data.add(i);
                                if is_heap_ptr(elem) {
                                    worklist.push(elem as *mut GcHeader);
                                }
                            }
                        }
                    }
                }
                ObjKind::Str => {}
            }
        }
    }

    fn sweep(&mut self) {
        let mut prev: *mut *mut GcHeader = &mut self.head;
        let mut current = self.head;

        while !current.is_null() {
            let next = unsafe { (*current).next };
            if unsafe { !(*current).marked } {
                // Unlink and free
                unsafe { *prev = next; }
                let freed = unsafe { self.free_obj(current) };
                self.bytes_allocated -= freed;
            } else {
                // Keep; clear mark bit; advance prev
                unsafe { (*current).marked = false; }
                prev = unsafe { &mut (*current).next };
            }
            current = next;
        }
    }

    /// Free a single GC object; returns the number of bytes freed. The
    /// blocks go onto `free_lists` for reuse rather than back to the system
    /// allocator — see `free_bytes`. The byte counts returned here are the
    /// same rounded sizes the matching `alloc_*` added to
    /// `bytes_allocated`, so the running total stays exact.
    unsafe fn free_obj(&mut self, obj: *mut GcHeader) -> usize {
        match (*obj).kind {
            ObjKind::Str => {
                let str_ptr = obj as *mut FrogStr;
                let len = (*str_ptr).len as usize;
                let total = words_for(std::mem::size_of::<FrogStr>() + len + 1) * 8;
                gc_trace!("sweep free {:p} Str  {} bytes", obj, total);
                self.free_bytes(obj as *mut u8, total);
                total
            }
            ObjKind::List => {
                let list_ptr = obj as *mut FrogList;
                let cap = (*list_ptr).cap as usize;
                let data_size = cap * std::mem::size_of::<i64>();
                let list_size = words_for(std::mem::size_of::<FrogList>()) * 8;
                gc_trace!("sweep free {:p} List {} bytes", obj, list_size + data_size);
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
                gc_trace!("sweep free {:p} Variant {} bytes", obj, total);
                self.free_bytes(obj as *mut u8, total);
                total
            }
        }
    }

    // ── Allocators ───────────────────────────────────────────────────────────

    /// Allocate a GC-managed FrogStr and copy `len` bytes from `data` into it.
    /// Appends a NUL terminator. `data` only needs to be valid for the duration of this call.
    pub fn alloc_str(&mut self, data: *const u8, len: usize) -> *mut FrogStr {
        let struct_size = std::mem::size_of::<FrogStr>();
        let total = words_for(struct_size + len + 1) * 8;
        let ptr = self.alloc_bytes(total) as *mut FrogStr;
        unsafe {
            (*ptr).header = GcHeader {
                next:   self.head,
                marked: false,
                kind:   ObjKind::Str,
            };
            (*ptr).len = len as u32;
            let dst = (ptr as *mut u8).add(struct_size);
            if len > 0 {
                std::ptr::copy_nonoverlapping(data, dst, len);
            }
            *dst.add(len) = 0;  // NUL terminator
        }

        self.head = ptr as *mut GcHeader;
        self.bytes_allocated += total;
        gc_trace!("alloc Str  {} bytes -> {:p}  (total: {} bytes)",
            total, ptr, self.bytes_allocated);
        ptr
    }

    /// Allocate a GC-managed FrogList with room for `cap` elements, each
    /// `stride` `i64` slots wide (`stride == 1` for every non-struct element
    /// type). `ptr_mask` marks which of the `stride` per-element slot
    /// offsets are heap pointers — see `FrogList`'s doc comment.
    /// The data buffer is separately allocated (not a GC object).
    pub fn alloc_list(&mut self, cap: usize, stride: usize, ptr_mask: u64, cond_mask: u64, boxed_tags: u64) -> *mut FrogList {
        let stride = stride.max(1);
        let actual_elem_cap = cap.max(1);
        let slot_cap = actual_elem_cap * stride;
        let data_size = slot_cap * std::mem::size_of::<i64>();
        let data = self.alloc_bytes(data_size) as *mut i64;

        let list_size = words_for(std::mem::size_of::<FrogList>()) * 8;
        let ptr = self.alloc_bytes(list_size) as *mut FrogList;

        unsafe {
            (*ptr).header = GcHeader {
                next:   self.head,
                marked: false,
                kind:   ObjKind::List,
            };
            (*ptr).len        = 0;
            (*ptr).cap        = slot_cap as u32;
            (*ptr).stride     = stride as u32;
            (*ptr).ptr_mask   = ptr_mask;
            (*ptr).cond_mask  = cond_mask;
            (*ptr).boxed_tags = boxed_tags;
            (*ptr).data       = data;
        }

        self.head = ptr as *mut GcHeader;
        self.bytes_allocated += list_size + data_size;
        gc_trace!("alloc List {} bytes -> {:p}  (total: {} bytes)",
            list_size + data_size, ptr, self.bytes_allocated);
        ptr
    }

    /// Allocate a GC-managed `FrogVariant` with `nslots` `i64` payload
    /// slots, zero-initialized (so a collection triggered while a
    /// still-being-populated field is being computed never follows
    /// garbage through an as-yet-unwritten slot — mirrors why
    /// `setup_shadow_frame` zeroes shadow-stack slots). `ptr_mask` marks which
    /// slots are heap pointers, exactly like `alloc_list`'s.
    pub fn alloc_variant(&mut self, tag: u32, nslots: usize, ptr_mask: u64, cond_mask: u64, boxed_tags: u64) -> *mut FrogVariant {
        let struct_size = std::mem::size_of::<FrogVariant>();
        let data_size = nslots * std::mem::size_of::<i64>();
        let total = words_for(struct_size + data_size) * 8;
        let ptr = self.alloc_bytes(total) as *mut FrogVariant;
        unsafe {
            (*ptr).header = GcHeader {
                next:   self.head,
                marked: false,
                kind:   ObjKind::Variant,
            };
            (*ptr).tag        = tag;
            (*ptr).nslots     = nslots as u32;
            (*ptr).ptr_mask   = ptr_mask;
            (*ptr).cond_mask  = cond_mask;
            (*ptr).boxed_tags = boxed_tags;
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

        self.head = ptr as *mut GcHeader;
        self.bytes_allocated += total;
        gc_trace!("alloc Variant tag={} {} bytes -> {:p}  (total: {} bytes)",
            tag, total, ptr, self.bytes_allocated);
        ptr
    }

    /// Print every live object in this heap to stderr.
    pub fn dump(&self) {
        let struct_size = std::mem::size_of::<FrogStr>();
        eprintln!("=== GC Heap Dump ===");
        let mut count = 0usize;
        let mut current = self.head;
        while !current.is_null() {
            count += 1;
            unsafe {
                match (*current).kind {
                    ObjKind::Str => {
                        let s = current as *const FrogStr;
                        let len = (*s).len as usize;
                        let data = (s as *const u8).add(struct_size);
                        let bytes = std::slice::from_raw_parts(data, len);
                        let text = std::str::from_utf8(bytes).unwrap_or("<invalid utf-8>");
                        eprintln!("  [{:p}] Str   len={:<4} {:?}", current, len, text);
                    }
                    ObjKind::List => {
                        let l = current as *const FrogList;
                        let stride = ((*l).stride as usize).max(1);
                        let elem_len = (*l).len as usize / stride;
                        let tag = if (*l).ptr_mask != 0 { "Ptr   " } else { "Scalar" };
                        eprint!("  [{:p}] List  len={:<4} cap={:<4} stride={:<2} {}  [",
                            current, elem_len, (*l).cap as usize / stride, stride, tag);
                        let show = elem_len.min(8);
                        for i in 0..show {
                            if i > 0 { eprint!(", "); }
                            // Only the first slot of each element is shown —
                            // a full struct-aware dump isn't implemented.
                            eprint!("{}", *(*l).data.add(i * stride));
                        }
                        if elem_len > 8 { eprint!(", …"); }
                        eprintln!("]");
                    }
                    ObjKind::Variant => {
                        let v = current as *const FrogVariant;
                        let nslots = (*v).nslots as usize;
                        let data = (current as *const u8).add(std::mem::size_of::<FrogVariant>()) as *const i64;
                        eprint!("  [{:p}] Variant tag={:<2} nslots={:<2} ptr_mask={:#x}  [",
                            current, (*v).tag, nslots, (*v).ptr_mask);
                        for i in 0..nslots {
                            if i > 0 { eprint!(", "); }
                            eprint!("{}", *data.add(i));
                        }
                        eprintln!("]");
                    }
                }
                current = (*current).next;
            }
        }
        eprintln!("--- {} object(s)  |  {} bytes allocated  |  threshold {} ---",
            count, self.bytes_allocated, self.gc_threshold);
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

pub fn push_root(value: i64, is_ptr: bool) {
    GC_HEAP.with(|h| h.borrow_mut().push_root(value, is_ptr));
}

pub fn gc_collect() {
    GC_HEAP.with(|h| {
        let mut h = h.borrow_mut();
        if h.bytes_allocated > h.gc_threshold {
            h.collect();
        }
    });
}

pub fn bytes_allocated() -> usize {
    GC_HEAP.with(|h| h.borrow().bytes_allocated)
}

/// Print a full diagnostic dump of every live object in the GC heap to stderr.
/// Safe to call at any time from Rust. Also callable as `gc_dump()` from froglang.
pub fn gc_dump() {
    let ptr = ACTIVE_HEAP.with(|h| h.get());
    if !ptr.is_null() {
        unsafe { (*ptr).dump(); }
    } else {
        GC_HEAP.with(|h| h.borrow().dump());
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
        GC_HEAP.with(|h| {
            let mut h = h.borrow_mut();
            let s = b"hello";
            let ptr = h.alloc_str(s.as_ptr(), s.len());
            unsafe {
                assert_eq!((*ptr).len, 5);
                let txt = frog_str_as_str(ptr as *const FrogStr);
                assert_eq!(txt, "hello");
            }
        });
    }

    #[test]
    fn test_gc_sweep_clears_unreferenced() {
        // Reset GC state for this test via a fresh GcHeap inline.
        let mut heap = GcHeap::new();
        let s = b"ephemeral";
        heap.alloc_str(s.as_ptr(), s.len());
        let before = heap.bytes_allocated;
        assert!(before > 0);
        // No roots → everything swept
        heap.collect();
        assert_eq!(heap.bytes_allocated, 0, "all bytes should be freed after sweep with no roots");
        let _ = before;
    }

    #[test]
    fn test_gc_sweep_keeps_rooted() {
        let mut heap = GcHeap::new();
        let s = b"kept";
        let ptr = heap.alloc_str(s.as_ptr(), s.len()) as i64;
        heap.push_root(ptr, true);
        heap.collect();
        assert!(heap.bytes_allocated > 0, "rooted string should survive GC");
        heap.roots.clear();
        heap.collect();
        assert_eq!(heap.bytes_allocated, 0);
    }
}
