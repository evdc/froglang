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
pub enum ObjKind { Str = 0, List = 1 }

#[repr(u8)]
#[derive(Clone, Copy)]
pub enum ElemTag { Scalar = 0, Ptr = 1 }

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

#[repr(C)]
pub struct FrogList {
    pub header:   GcHeader,
    pub len:      u32,
    pub cap:      u32,
    pub elem_tag: ElemTag,
    pub data:     *mut i64,
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

#[repr(C)]
pub struct ShadowFrame {
    pub prev:  *mut ShadowFrame,
    pub slots: *mut i64,
    pub len:   usize,
}

// ── GcHeap ───────────────────────────────────────────────────────────────────

pub struct GcHeap {
    head:            *mut GcHeader,  // head of the intrusive linked list of all objects
    pub bytes_allocated: usize,
    gc_threshold:    usize,
    roots:           Vec<(i64, bool)>,  // (value, is_ptr)
    shadow_head:     *mut ShadowFrame,  // head of the JIT shadow stack, see above
}

thread_local! {
    pub static GC_HEAP: RefCell<GcHeap> = RefCell::new(GcHeap::new());
    /// Pointer to the GcHeap of the FrogState currently executing on this thread.
    /// Null when no froglang code is running (falls back to GC_HEAP).
    pub static ACTIVE_HEAP: Cell<*mut GcHeap> = Cell::new(std::ptr::null_mut());
}

impl GcHeap {
    pub fn new() -> Self {
        GcHeap {
            head:            std::ptr::null_mut(),
            bytes_allocated: 0,
            gc_threshold:    1024 * 1024,  // 1 MB initial threshold
            roots:           Vec::new(),
            shadow_head:     std::ptr::null_mut(),
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

    /// Push a new shadow-stack frame describing `len` `i64` root slots at
    /// `slots` (owned by the JIT function's own stack frame). Zeroes the
    /// slots first: an unwritten slot otherwise holds stack garbage that
    /// `mark` would follow as a pointer.
    pub fn push_frame(&mut self, slots: *mut i64, len: usize) {
        if len > 0 {
            unsafe { std::ptr::write_bytes(slots, 0, len); }
        }
        let frame = Box::new(ShadowFrame { prev: self.shadow_head, slots, len });
        self.shadow_head = Box::into_raw(frame);
    }

    /// Pop the most recently pushed shadow-stack frame. Must be called
    /// exactly once per `push_frame`, in LIFO order (i.e. matching the
    /// native call stack — codegen emits one push/pop pair per function
    /// invocation).
    pub fn pop_frame(&mut self) {
        if self.shadow_head.is_null() { return; }
        unsafe {
            let frame = Box::from_raw(self.shadow_head);
            self.shadow_head = frame.prev;
            // `frame` (the ShadowFrame box) drops here; `frame.slots` itself
            // is not owned memory — it points into the JIT function's own
            // stack frame, which the JIT's own epilogue reclaims.
        }
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
            if is_ptr && value != 0 {
                unsafe { Self::mark(value as *mut GcHeader); }
            }
        }

        // ...plus every live JIT shadow-stack frame.
        let mut _frame_count = 0usize;
        let mut frame = self.shadow_head;
        while !frame.is_null() {
            _frame_count += 1;
            unsafe {
                let f = &*frame;
                for i in 0..f.len {
                    let v = *f.slots.add(i);
                    if v != 0 {
                        Self::mark(v as *mut GcHeader);
                    }
                }
                frame = f.prev;
            }
        }
        gc_trace!("marked {} shadow frame(s)", _frame_count);

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
    unsafe fn mark(obj: *mut GcHeader) {
        let mut worklist = vec![obj];
        while let Some(obj) = worklist.pop() {
            if (*obj).marked { continue; }
            (*obj).marked = true;
            gc_trace!("mark  {:p} ({})", obj,
                match (*obj).kind { ObjKind::Str => "Str", ObjKind::List => "List" });
            if let ObjKind::List = (*obj).kind {
                let list = obj as *mut FrogList;
                if let ElemTag::Ptr = (*list).elem_tag {
                    for i in 0..(*list).len as usize {
                        let elem = *(*list).data.add(i);
                        if elem != 0 {
                            worklist.push(elem as *mut GcHeader);
                        }
                    }
                }
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

    /// Free a single GC object; returns the number of bytes freed.
    unsafe fn free_obj(&self, obj: *mut GcHeader) -> usize {
        match (*obj).kind {
            ObjKind::Str => {
                let str_ptr = obj as *mut FrogStr;
                let len = (*str_ptr).len as usize;
                let total = std::mem::size_of::<FrogStr>() + len + 1;
                let layout = Layout::from_size_align(total, std::mem::align_of::<FrogStr>())
                    .expect("FrogStr layout");
                gc_trace!("sweep free {:p} Str  {} bytes", obj, total);
                dealloc(obj as *mut u8, layout);
                total
            }
            ObjKind::List => {
                let list_ptr = obj as *mut FrogList;
                let cap = (*list_ptr).cap as usize;
                let data_size = cap * std::mem::size_of::<i64>();
                let list_size = std::mem::size_of::<FrogList>();
                gc_trace!("sweep free {:p} List {} bytes", obj, list_size + data_size);
                if cap > 0 {
                    let data_layout = Layout::array::<i64>(cap).expect("list data layout");
                    dealloc((*list_ptr).data as *mut u8, data_layout);
                }
                dealloc(obj as *mut u8, Layout::new::<FrogList>());
                list_size + data_size
            }
        }
    }

    // ── Allocators ───────────────────────────────────────────────────────────

    /// Allocate a GC-managed FrogStr and copy `len` bytes from `data` into it.
    /// Appends a NUL terminator. `data` only needs to be valid for the duration of this call.
    pub fn alloc_str(&mut self, data: *const u8, len: usize) -> *mut FrogStr {
        let struct_size = std::mem::size_of::<FrogStr>();
        let total = struct_size + len + 1;
        let layout = Layout::from_size_align(total, std::mem::align_of::<FrogStr>())
            .expect("FrogStr layout");

        let ptr = unsafe { alloc(layout) as *mut FrogStr };
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

    /// Allocate a GC-managed FrogList with the given initial capacity and element tag.
    /// The data buffer is separately allocated (not a GC object).
    pub fn alloc_list(&mut self, cap: usize, elem_tag: ElemTag) -> *mut FrogList {
        let actual_cap = cap.max(1);
        let data_layout = Layout::array::<i64>(actual_cap).expect("list data layout");
        let data = unsafe { alloc(data_layout) as *mut i64 };

        let list_layout = Layout::new::<FrogList>();
        let ptr = unsafe { alloc(list_layout) as *mut FrogList };

        unsafe {
            (*ptr).header = GcHeader {
                next:   self.head,
                marked: false,
                kind:   ObjKind::List,
            };
            (*ptr).len      = 0;
            (*ptr).cap      = actual_cap as u32;
            (*ptr).elem_tag = elem_tag;
            (*ptr).data     = data;
        }

        self.head = ptr as *mut GcHeader;
        self.bytes_allocated += list_layout.size() + data_layout.size();
        gc_trace!("alloc List {} bytes -> {:p}  (total: {} bytes)",
            list_layout.size() + data_layout.size(), ptr, self.bytes_allocated);
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
                        let tag = match (*l).elem_tag {
                            ElemTag::Scalar => "Scalar",
                            ElemTag::Ptr    => "Ptr   ",
                        };
                        eprint!("  [{:p}] List  len={:<4} cap={:<4} {}  [",
                            current, (*l).len, (*l).cap, tag);
                        let show = ((*l).len as usize).min(8);
                        for i in 0..show {
                            if i > 0 { eprint!(", "); }
                            eprint!("{}", *(*l).data.add(i));
                        }
                        if (*l).len > 8 { eprint!(", …"); }
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
