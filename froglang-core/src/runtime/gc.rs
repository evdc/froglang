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
#[derive(Clone, Copy, PartialEq, Eq)]
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
    /// Bit `i` set means slot `i` within each element block is a
    /// scannable column: the collector reads it and applies the uniform
    /// `is_heap_ptr`/`heap_ptr` rule (see "Word encoding" above). Columns
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
    _data: [i64; 0],  // zero-sized marker; slots live at (ptr + size_of::<FrogVariant>())
}

// ── Word encoding ─────────────────────────────────────────────────────────────
//
// Every GC-visible word in the system — a Cranelift-tracked root, a list element
// slot, a boxed variant's payload slot, a value crossing the FFI — uses one
// encoding, so the collector needs no per-slot type information and no
// per-slot metadata beyond "is this column scannable at all" (`ptr_mask`).
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
// The collector's whole rule is `is_heap_ptr` + `heap_ptr` below: two ALU ops,
// no branch on slot kind, no tag lookup. That uniformity is what makes precise
// roots possible at all — see RUNTIME.md.
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

/// Is `w` a pointer the GC may follow? False for `0` (an empty slot), for an
/// immediate (`111`), and for a tag-only inline-union word (`1..=6`, whose
/// pointer part is zero).
#[inline]
pub fn is_heap_ptr(w: i64) -> bool {
    w & TAG_MASK != TAG_IMMEDIATE && (w & !TAG_MASK) != 0
}

/// The object `w` points at, with any tag bits stripped. Only meaningful
/// when `is_heap_ptr(w)`.
#[inline]
pub fn heap_ptr(w: i64) -> *mut GcHeader {
    (w & !TAG_MASK) as *mut GcHeader
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

// ── Precise roots: Cranelift stack maps ──────────────────────────────────────
//
// Every GC-managed value a JIT-compiled function holds is declared to
// Cranelift (`declare_var_needs_stack_map` / `declare_value_needs_stack_map`
// in codegen/mod.rs). Cranelift spills those values around every safepoint —
// which is every call — and emits, per safepoint, the SP-relative byte
// offsets at which they sit. Collection therefore means walking the native
// stack and reading the words each frame's map names.
//
// This replaces a hand-written shadow stack: one root slot per producer
// site, stored on production and held for the whole call. That was both
// slower (a frame push, pop and zeroing per call — ~30% of `orders.frog`)
// and unsound in two ways we found and one we probably had not, because its
// slot-reuse rules were a structural argument about control flow rather than
// a live-range analysis. Cranelift's is the analysis its register allocator
// already depends on. See RUNTIME.md Part 2.
//
// The walk needs three things, in this order:
//
// 1. The frame pointer of the runtime function the mutator called into.
//    `caller_frame_pointer!` reads it at each collecting FFI entry point and
//    `set_jit_frame` hands it to the collector. From a frame pointer `f`,
//    `*(f + 8)` is the return address into the caller and `*f` is the
//    caller's own frame pointer — the standard chain, which requires frame
//    pointers to actually be present (see `.cargo/config.toml`, and
//    `preserve_frame_pointers` for the JIT side).
//
// 2. A return address resolved to the function that contains it, and to that
//    function's map for exactly that address. `JitCode` is that table.
//
// 3. That frame's stack pointer, to which the map's offsets are relative.
//    A callee's frame pointer sits directly below the two words its prologue
//    pushed (saved FP and return address on aarch64; return address pushed
//    by `call` plus saved RBP on x86-64), so the caller's stack pointer at
//    the call site is `callee_fp + 16` on both.

/// Bytes between a callee's frame pointer and its caller's stack pointer at
/// the call site: the saved frame pointer and the return address.
const FRAME_LINK_BYTES: usize = 16;

/// Read the frame pointer of the function this expands inside. Must be used
/// directly in a function the mutator calls, not in a helper it calls — the
/// whole point is *which* frame it names.
#[macro_export]
macro_rules! caller_frame_pointer {
    () => {{
        let fp: usize;
        #[cfg(target_arch = "aarch64")]
        unsafe { core::arch::asm!("mov {}, x29", out(reg) fp, options(nomem, nostack, preserves_flags)) };
        #[cfg(target_arch = "x86_64")]
        unsafe { core::arch::asm!("mov {}, rbp", out(reg) fp, options(nomem, nostack, preserves_flags)) };
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        compile_error!("froglang's GC stack walk needs a frame-pointer read for this architecture");
        fp
    }};
}

/// Publishes the calling function's frame pointer as the place the next
/// collection should start walking from, and puts the previous one back on
/// the way out.
///
/// Must be created with the `jit_frame_guard!` macro, never by calling
/// `new` directly: the frame pointer has to be read *in* the function the
/// mutator called, which is what the macro guarantees and a plain call
/// would quietly get wrong.
///
/// The save-and-restore matters because froglang code re-enters the runtime
/// (printing a union runs JIT-compiled formatting, which allocates), so
/// these nest. Leaving an inner frame published after it returned would have
/// the collector walk a dead frame.
pub struct JitFrameGuard {
    previous: usize,
}

impl JitFrameGuard {
    /// Use `jit_frame_guard!()` instead.
    #[inline]
    pub fn new(fp: usize) -> Self {
        JitFrameGuard { previous: with_active_heap(|h| h.set_jit_frame(fp)) }
    }
}

impl Drop for JitFrameGuard {
    #[inline]
    fn drop(&mut self) {
        with_active_heap(|h| h.set_jit_frame(self.previous));
    }
}

/// Publish this function's frame pointer for the duration of the returned
/// guard. See `JitFrameGuard`.
#[macro_export]
macro_rules! jit_frame_guard {
    () => {
        $crate::runtime::gc::JitFrameGuard::new($crate::caller_frame_pointer!())
    };
}

/// Roots GC pointers a runtime function received as arguments, for as long
/// as that function still needs them.
///
/// Cranelift's stack maps are *precise*: a value that is used by a call and
/// dead afterwards is not recorded at that call, because from the mutator's
/// point of view nothing can reach it again. That is exactly right for JIT
/// frames and exactly wrong for the runtime function on the other side of
/// the call, which is Rust code with no stack map of its own. So a runtime
/// function that can trigger a collection and still needs an argument
/// afterwards must say so.
///
/// The rule, applied at the entry points rather than case by case: **if a
/// runtime function takes a GC pointer and can collect, it holds its
/// pointer arguments for its whole body.** Reasoning per-function about
/// whether the last use happens before or after the collection is how this
/// becomes a use-after-free the next time one of them is edited.
pub struct RuntimeRoots {
    held: usize,
}

impl RuntimeRoots {
    /// Root `values` until the returned guard drops. Every value is treated
    /// as a GC pointer — callers must know that's true of every slot, e.g.
    /// because they allocated all of them. A slot that instead holds a raw
    /// `Int`/`Float` has no tag bits and must never be passed here; use
    /// [`Self::hold_masked`] when a slot's scannability isn't uniform (as
    /// with `#[frog_fn]`'s flattened argument buffer).
    #[inline]
    pub fn hold(values: &[i64]) -> Self {
        with_active_heap(|h| {
            for &v in values {
                h.push_root(v, true);
            }
        });
        RuntimeRoots { held: values.len() }
    }

    /// Root `values` until the returned guard drops, scanning only the
    /// slots `is_ptr` marks `true`. `is_ptr` must be exactly `values.len()`
    /// long.
    #[inline]
    pub fn hold_masked(values: &[i64], is_ptr: &[bool]) -> Self {
        debug_assert_eq!(values.len(), is_ptr.len());
        with_active_heap(|h| {
            for (&v, &p) in values.iter().zip(is_ptr.iter()) {
                h.push_root(v, p);
            }
        });
        RuntimeRoots { held: values.len() }
    }
}

impl Drop for RuntimeRoots {
    #[inline]
    fn drop(&mut self) {
        with_active_heap(|h| h.pop_roots(self.held));
    }
}

/// One JIT-compiled function's stack maps, in the form the collector wants:
/// no Cranelift types, just addresses and offsets.
pub struct JitFunctionMaps {
    /// Where this function's machine code starts, once finalized.
    pub start: usize,
    /// How many bytes of it there are.
    pub len: usize,
    /// `(return-address offset from `start`, SP-relative byte offsets of the
    /// live GC-managed values at that safepoint)`, sorted by the first
    /// element — Cranelift emits them in that order and the lookup below
    /// binary-searches on it.
    pub maps: Vec<(u32, Vec<u32>)>,
}

/// Every JIT-compiled function's maps, sorted by `start` so a return address
/// resolves in `O(log n)`.
///
/// A process-wide table rather than one per `Codegen`: a return address is
/// unique across the process, and the collector reaching a frame it cannot
/// resolve has no way to tell "not froglang code" from "the wrong table".
pub struct JitCode {
    functions: Vec<JitFunctionMaps>,
}

impl JitCode {
    const fn new() -> Self {
        JitCode { functions: Vec::new() }
    }

    /// Register one finalized function. Called once per compiled function,
    /// after `Module::finalize_definitions` has fixed its address.
    pub fn register(&mut self, f: JitFunctionMaps) {
        let at = self.functions.partition_point(|g| g.start < f.start);
        self.functions.insert(at, f);
    }

    /// The live-value offsets at `return_addr`, if it is a safepoint in a
    /// function this table knows.
    ///
    /// `None` covers three different things and deliberately does not
    /// distinguish them: a frame belonging to the Rust runtime or to libc,
    /// a JIT frame stopped somewhere that is not a safepoint (impossible
    /// while a collection is running, since a collection only starts from a
    /// call), and an address in a function compiled without any GC values
    /// at all.
    pub fn lookup(&self, return_addr: usize) -> Option<&[u32]> {
        let at = self.functions.partition_point(|g| g.start <= return_addr);
        let f = self.functions.get(at.checked_sub(1)?)?;
        if return_addr >= f.start + f.len {
            return None;
        }
        let offset = (return_addr - f.start) as u32;
        let i = f.maps.binary_search_by_key(&offset, |(o, _)| *o).ok()?;
        Some(&f.maps[i].1)
    }
}

// ── GcHeap ───────────────────────────────────────────────────────────────────

pub struct GcHeap {
    head:            *mut GcHeader,  // head of the intrusive linked list of all objects
    pub bytes_allocated: usize,
    gc_threshold:    usize,
    roots:           Vec<(i64, bool)>,  // (value, is_ptr)
    /// Caller-owned buffers that JIT code writes GC pointers into, which
    /// the collector must therefore scan — see `push_scanned_span`.
    scanned_spans:   Vec<(usize, Vec<usize>)>,
    /// Frame pointer of the runtime function the mutator most recently
    /// called into — where the native stack walk starts. `0` when no JIT
    /// code is on the stack, which is the state for an embedding that only
    /// uses `alloc_*` directly, and for every unit test in this file; the
    /// walk is then skipped entirely and only the explicit `roots` apply.
    ///
    /// Set by every collecting FFI entry point (see `caller_frame_pointer!`)
    /// and cleared when that entry point returns, so a collection triggered
    /// from outside JIT code never walks a stale frame.
    jit_frame:       usize,
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
    /// When set (from the `FROG_GC_STRESS` env var, read once in `new()`),
    /// `maybe_collect` sweeps on *every* allocation instead of waiting for
    /// `gc_threshold`. A missed-declaration rooting bug (a GC-scannable SSA
    /// value that reaches `declare_gc_value`/`declare_gc_var` too late, or
    /// not at all) only under-roots a value that is genuinely still live at
    /// the moment a collection actually runs — with the normal 1 MB
    /// threshold, most test programs never collect at all, so such a bug
    /// can sit undetected. Forcing a
    /// collection at every allocation point turns that into a
    /// close-to-immediate, reproducible failure instead of an intermittent
    /// one. Never enabled by default — it makes every allocation as
    /// expensive as a full sweep, which is only acceptable for a test run
    /// explicitly opting in.
    stress:          bool,
    /// Reusable mark-phase worklist. Kept on the heap rather than allocated
    /// per `mark` call: `mark` is invoked once per root, so a fresh `Vec`
    /// each time is a `malloc`/`free` pair per root per collection.
    mark_worklist:   Vec<*mut GcHeader>,
}

thread_local! {
    pub static GC_HEAP: RefCell<GcHeap> = RefCell::new(GcHeap::new());
    /// Every JIT-compiled function's stack maps — see `JitCode`. Populated
    /// by `Codegen` as it finalizes each function, read by the collector's
    /// native stack walk.
    pub static JIT_CODE: RefCell<JitCode> = RefCell::new(JitCode::new());
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
            scanned_spans:   Vec::new(),
            jit_frame:       0,
            free_lists:      vec![std::ptr::null_mut(); MAX_FREE_WORDS + 1],
            stress:          std::env::var_os("FROG_GC_STRESS").is_some(),
            mark_worklist:   Vec::new(),
        }
    }

    pub fn push_root(&mut self, value: i64, is_ptr: bool) {
        self.roots.push((value, is_ptr));
    }

    /// How many explicit roots are currently pushed. Lets a caller that
    /// pushes an unknown number of roots one at a time (`runtime::host`'s
    /// `FrogCtx`, allocating values a host function builds) snapshot a mark
    /// and later `pop_roots(roots_len() - mark)` to release exactly what it
    /// added, mirroring `RuntimeRoots`' fixed-size version of the same
    /// discipline.
    pub fn roots_len(&self) -> usize {
        self.roots.len()
    }

    /// Drop the `n` most recently pushed explicit roots. Paired with
    /// `push_root` by `RuntimeRoots`, which is strictly nested, so a
    /// truncate from the end is the right discipline.
    pub fn pop_roots(&mut self, n: usize) {
        let keep = self.roots.len().saturating_sub(n);
        self.roots.truncate(keep);
    }

    /// Discard every explicit root pushed via `push_root`. Callers that
    /// re-derive their live root set from scratch before each collection
    /// (e.g. `FrogState::eval`, from its current `env`) should call this
    /// first — otherwise `roots` only ever grows, keeping every value any
    /// caller has ever rooted alive for the process's lifetime.
    pub fn clear_roots(&mut self) {
        self.roots.clear();
    }

    /// Register a caller-owned `i64` buffer at `base` whose slots at
    /// `slots` hold GC pointers, so the collector scans it for the duration
    /// of the JIT call that fills it.
    ///
    /// `__frog_main` writes each top-level binding into such a buffer the
    /// moment the binding is created, and `FrogState::eval` only turns that
    /// buffer into explicit roots *after* the call returns. In between, a
    /// binding whose last JIT-side use has passed is reachable only through
    /// this buffer — Cranelift is right that the mutator is done with it,
    /// and the collector would be right to sweep it, and the result is a
    /// dangling pointer in `env`. This is a narrow, deliberate exception to
    /// "precise roots" — the buffer is conservatively scanned in full for
    /// the duration of one call, the same trade the old shadow stack made
    /// everywhere, just now confined to the one place Cranelift's own
    /// analysis cannot reach.
    ///
    /// Only the listed slots are read: an unlisted one holds a raw `Int` or
    /// `Float`, which has no tag bits and must never be mistaken for an
    /// address. Slots not yet written hold `0`, which is not a pointer.
    pub fn push_scanned_span(&mut self, base: *const i64, slots: Vec<usize>) {
        self.scanned_spans.push((base as usize, slots));
    }

    /// Drop the most recently registered span. The buffer must still be
    /// alive at this point; these nest with the call that owns them.
    pub fn pop_scanned_span(&mut self) {
        self.scanned_spans.pop();
    }

    /// Record the frame pointer the next collection should walk from — the
    /// frame of the runtime function the mutator just called into. See the
    /// "Precise roots" section above for what the walk does with it.
    ///
    /// Returns the previous value, which the caller must restore on the way
    /// out: froglang code can re-enter the runtime (a `print` of a union
    /// walks back into JIT-compiled formatting), and leaving a stale inner
    /// frame behind would make the collector walk a frame that has already
    /// returned.
    #[inline]
    pub fn set_jit_frame(&mut self, fp: usize) -> usize {
        std::mem::replace(&mut self.jit_frame, fp)
    }

    pub fn maybe_collect(&mut self) {
        if self.stress || self.bytes_allocated > self.gc_threshold {
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
                unsafe { Self::mark_from(&mut self.mark_worklist, heap_ptr(value)); }
            }
        }

        // ...plus any caller-owned buffer the running JIT call is filling
        // with GC pointers (`push_scanned_span`).
        let spans = std::mem::take(&mut self.scanned_spans);
        for (base, slots) in &spans {
            for &i in slots {
                let w = unsafe { *((*base as *const i64).add(i)) };
                if is_heap_ptr(w) {
                    unsafe { Self::mark_from(&mut self.mark_worklist, heap_ptr(w)) };
                }
            }
        }
        self.scanned_spans = spans;

        // ...plus every GC-managed value live in a JIT frame on the native
        // stack. `jit_frame` is the frame pointer of the runtime function
        // the mutator called into, so the first iteration below already
        // describes the innermost JIT frame: its return address is the
        // safepoint we are stopped at, and its stack pointer is just above
        // the two words that frame link occupies.
        let mut _nframes = 0usize;
        let mut fp = self.jit_frame;
        while fp != 0 {
            let (ret, caller_fp) = unsafe {
                (*((fp + 8) as *const usize), *(fp as *const usize))
            };
            let sp = fp + FRAME_LINK_BYTES;
            JIT_CODE.with(|c| {
                if let Some(offsets) = c.borrow().lookup(ret) {
                    for &off in offsets {
                        let w = unsafe { *((sp + off as usize) as *const i64) };
                        if is_heap_ptr(w) {
                            unsafe { Self::mark_from(&mut self.mark_worklist, heap_ptr(w)) };
                        }
                    }
                    _nframes += 1;
                }
            });
            // Stop at the first frame that is not above the current one:
            // the chain runs from inner to outer, so a frame pointer that
            // does not increase means we have walked off the end of it (or
            // into a frame built without one) and must not keep following.
            if caller_fp <= fp {
                break;
            }
            fp = caller_fp;
        }
        gc_trace!("marked {} JIT frame(s)", _nframes);

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
    /// (explicit worklist) rather than recursive, since ordinary programs
    /// build deep object graphs (a long list, a recursive `Tree`) that a
    /// stack-recursive mark could overflow on.
    ///
    /// `worklist` is supplied by the caller (`GcHeap::mark_worklist`) and
    /// left empty on return, so a collection with many roots reuses one
    /// allocation instead of making a fresh `Vec` — and freeing it — per
    /// root.
    unsafe fn mark_from(worklist: &mut Vec<*mut GcHeader>, obj: *mut GcHeader) {
        debug_assert!(worklist.is_empty());
        worklist.push(obj);
        while let Some(obj) = worklist.pop() {
            // Cheap O(1) screen on the uniform word encoding: anything the
            // collector reaches must be a real object, so its `kind` byte
            // must be a valid `ObjKind` discriminant. A word that was
            // written under the wrong encoding — a raw `Int` in a scanned
            // column, a tag OR'd onto a word that already had one — almost
            // always lands here rather than silently corrupting the heap.
            // See gc.rs's "Word encoding": this is the assertion RUNTIME.md
            // asks for, at the point of *consumption* (one place) rather
            // than at every point of production.
            debug_assert!(
                std::ptr::read(&(*obj).kind) as u8 <= ObjKind::Variant as u8,
                "GC followed {:p}, which is not a heap object — a word reached the collector \
                 under the wrong encoding (see gc.rs's \"Word encoding\")",
                obj,
            );
            if (*obj).marked { continue; }
            (*obj).marked = true;
            gc_trace!("mark  {:p} ({})", obj,
                match (*obj).kind { ObjKind::Str => "Str", ObjKind::List => "List", ObjKind::Variant => "Variant" });
            match (*obj).kind {
                ObjKind::List => {
                    let list = obj as *mut FrogList;
                    let mask = (*list).ptr_mask;
                    if mask != 0 {
                        let stride = ((*list).stride as usize).max(1);
                        let elem_len = (*list).len as usize / stride;
                        for i in 0..elem_len {
                            let base = i * stride;
                            for bit in 0..stride {
                                if mask & (1u64 << bit) == 0 { continue; }
                                let w = *(*list).data.add(base + bit);
                                if is_heap_ptr(w) {
                                    worklist.push(heap_ptr(w));
                                }
                            }
                        }
                    }
                }
                ObjKind::Variant => {
                    let variant = obj as *mut FrogVariant;
                    let mask = (*variant).ptr_mask;
                    if mask != 0 {
                        let nslots = (*variant).nslots as usize;
                        let data = (obj as *mut u8).add(std::mem::size_of::<FrogVariant>()) as *mut i64;
                        for i in 0..nslots {
                            if mask & (1u64 << i) == 0 { continue; }
                            let w = *data.add(i);
                            if is_heap_ptr(w) {
                                worklist.push(heap_ptr(w));
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
            (*ptr).header = GcHeader {
                next:   self.head,
                marked: false,
                kind:   ObjKind::List,
            };
            (*ptr).len        = 0;
            (*ptr).cap        = slot_cap as u32;
            (*ptr).stride     = stride as u32;
            (*ptr).ptr_mask   = ptr_mask;
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
    /// garbage through an as-yet-unwritten slot). `ptr_mask` marks which
    /// slots are heap pointers, exactly like `alloc_list`'s.
    pub fn alloc_variant(&mut self, tag: u32, nslots: usize, ptr_mask: u64) -> *mut FrogVariant {
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

    /// Deep-clone the heap object at `obj`, returning a pointer with no
    /// aliasing to the original — the runtime primitive value semantics for
    /// `List` needs (MUTABILITY.md tier 1: "copy on write-through-a-non-unique
    /// root"). Dispatches on `ObjKind`, exactly like `mark_from`, rather than
    /// on any new per-object metadata:
    ///
    /// - `Str` is returned unchanged. It's immutable, so aliasing it is
    ///   unobservable — the same reasoning MUTABILITY.md gives for why
    ///   immutable types never copy at all.
    /// - `List` gets a fresh buffer (same `stride`/`ptr_mask`/length) with the
    ///   raw slots copied, then every `ptr_mask`-set slot in every element
    ///   block is recursively cloned, re-OR'ing the original tag bits
    ///   (`TAG_MASK`) onto the clone — an element's pointer column is *not*
    ///   always a plain untagged pointer: a `List(Tagged)` where `Tagged` is
    ///   an inline union has element leaves labelled `Type::Union` by
    ///   `struct_fields`, and that column's word is the union's own
    ///   tagged-pointer encoding (RUNTIME.md Part 1), same as a struct field
    ///   or a variant payload slot of that type.
    /// - `Variant` gets a fresh payload the same way, tag bits preserved
    ///   identically.
    ///
    /// **Assumes the object graph is acyclic** — plain structural recursion,
    /// no visited-set. This holds today because a self-referential union
    /// always boxes as a single opaque node rather than being expressed as a
    /// cycle of clonable values, and there is no other way to build a cycle
    /// from surface syntax (MUTABILITY.md's "Graphs get arenas": cyclic or
    /// shared-observer structures are explicitly out of scope for value
    /// semantics, and must be arena-plus-index instead). A future `Ref(T)` or
    /// handle type would need to revisit this.
    ///
    /// **GC-safety**: every allocation this makes can itself trigger a
    /// collection. The object being cloned is assumed already rooted by the
    /// caller for the whole call (see `ffi::frog_clone`'s `RuntimeRoots`) —
    /// that keeps every *original* descendant alive, since a collection
    /// simply marks the whole reachable graph from it. But a freshly
    /// allocated *destination* object is reachable from nothing until its
    /// parent stores its pointer, so each recursion level roots its own new
    /// object for the duration of that level (`push_root`/`pop_roots`,
    /// popped only after every child has been written into it) rather than
    /// relying on `RuntimeRoots` from within `GcHeap` itself.
    pub unsafe fn clone_obj(&mut self, obj: *mut GcHeader) -> *mut GcHeader {
        match (*obj).kind {
            ObjKind::Str => obj,
            ObjKind::List => {
                let src = obj as *mut FrogList;
                let stride = ((*src).stride as usize).max(1);
                let len = (*src).len as usize;
                let mask = (*src).ptr_mask;
                let elem_cap = (len / stride).max(1);
                let new_list = self.alloc_list(elem_cap, stride, mask);
                self.push_root(new_list as i64, true);
                if len > 0 {
                    std::ptr::copy_nonoverlapping((*src).data, (*new_list).data, len);
                }
                (*new_list).len = len as u32;
                if mask != 0 {
                    let elem_len = len / stride;
                    for i in 0..elem_len {
                        let base = i * stride;
                        for bit in 0..stride {
                            if mask & (1u64 << bit) == 0 { continue; }
                            let w = *(*new_list).data.add(base + bit);
                            if is_heap_ptr(w) {
                                let cloned = self.clone_obj(heap_ptr(w));
                                *(*new_list).data.add(base + bit) = (cloned as i64) | (w & TAG_MASK);
                            }
                        }
                    }
                }
                self.pop_roots(1);
                new_list as *mut GcHeader
            }
            ObjKind::Variant => {
                let src = obj as *mut FrogVariant;
                let tag = (*src).tag;
                let nslots = (*src).nslots as usize;
                let mask = (*src).ptr_mask;
                let new_variant = self.alloc_variant(tag, nslots, mask);
                self.push_root(new_variant as i64, true);
                let src_data = (obj as *mut u8).add(std::mem::size_of::<FrogVariant>()) as *mut i64;
                let dst_data = (new_variant as *mut u8).add(std::mem::size_of::<FrogVariant>()) as *mut i64;
                if nslots > 0 {
                    std::ptr::copy_nonoverlapping(src_data, dst_data, nslots);
                }
                if mask != 0 {
                    for i in 0..nslots {
                        if mask & (1u64 << i) == 0 { continue; }
                        let w = *dst_data.add(i);
                        if is_heap_ptr(w) {
                            let cloned = self.clone_obj(heap_ptr(w));
                            *dst_data.add(i) = (cloned as i64) | (w & TAG_MASK);
                        }
                    }
                }
                self.pop_roots(1);
                new_variant as *mut GcHeader
            }
        }
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

/// Run `f` against whichever heap is current — the `FrogState`-owned one if
/// froglang code is executing on this thread, else the thread-local
/// fallback. The same choice `ffi::with_heap` makes, duplicated here
/// because `JitFrameGuard` lives on this side of the module boundary.
#[inline]
pub fn with_active_heap<R>(f: impl FnOnce(&mut GcHeap) -> R) -> R {
    let ptr = ACTIVE_HEAP.with(|h| h.get());
    if ptr.is_null() {
        GC_HEAP.with(|h| f(&mut *h.borrow_mut()))
    } else {
        unsafe { f(&mut *ptr) }
    }
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
