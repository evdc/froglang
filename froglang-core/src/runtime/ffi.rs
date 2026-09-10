use std::alloc::Layout;
use std::io::Write;

use super::gc::{FrogList, FrogStr, FrogVariant, GcHeap, RuntimeRoots, GC_HEAP, ACTIVE_HEAP};

/// Call `f` with a mutable reference to the active GcHeap.
/// Uses the `FrogState`-owned heap if one is executing on this thread,
/// otherwise falls back to the thread-local GC_HEAP.
pub(super) fn with_heap<F, R>(f: F) -> R
where
    F: FnOnce(&mut GcHeap) -> R,
{
    let ptr = ACTIVE_HEAP.with(|h| h.get());
    if !ptr.is_null() {
        unsafe { f(&mut *ptr) }
    } else {
        GC_HEAP.with(|h| {
            let mut borrowed = h.borrow_mut();
            f(&mut borrowed)
        })
    }
}

// ── String operations ─────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn frog_alloc_str(data: i64, len: i64) -> i64 {
    let _jit_frame = crate::jit_frame_guard!();
    with_heap(|heap| {
        heap.maybe_collect();
        heap.alloc_str(unsafe { std::slice::from_raw_parts(data as *const u8, len as usize) }) as i64
    })
}

#[no_mangle]
pub extern "C" fn frog_str_len(s: i64) -> i64 {
    unsafe { (*(s as *const FrogStr)).len as i64 }
}

#[no_mangle]
pub extern "C" fn frog_str_concat(a: i64, b: i64) -> i64 {
    let _jit_frame = crate::jit_frame_guard!();
    let _roots = RuntimeRoots::hold(&[a, b]);
    let a_ptr = a as *const FrogStr;
    let b_ptr = b as *const FrogStr;
    let struct_size = std::mem::size_of::<FrogStr>();
    let (a_len, b_len) = unsafe { ((*a_ptr).len as usize, (*b_ptr).len as usize) };
    let total_len = a_len + b_len;

    // Build concatenated bytes into a temporary stack buffer.
    let mut buf = Vec::with_capacity(total_len);
    unsafe {
        let a_data = (a_ptr as *const u8).add(struct_size);
        let b_data = (b_ptr as *const u8).add(struct_size);
        buf.extend_from_slice(std::slice::from_raw_parts(a_data, a_len));
        buf.extend_from_slice(std::slice::from_raw_parts(b_data, b_len));
    }

    with_heap(|heap| {
        heap.maybe_collect();
        heap.alloc_str(&buf) as i64
    })
}

#[no_mangle]
pub extern "C" fn frog_str_eq(a: i64, b: i64) -> i64 {
    let a_ptr = a as *const FrogStr;
    let b_ptr = b as *const FrogStr;
    let struct_size = std::mem::size_of::<FrogStr>();
    unsafe {
        let a_len = (*a_ptr).len as usize;
        let b_len = (*b_ptr).len as usize;
        if a_len != b_len { return 0; }
        let a_data = (a_ptr as *const u8).add(struct_size);
        let b_data = (b_ptr as *const u8).add(struct_size);
        let a_bytes = std::slice::from_raw_parts(a_data, a_len);
        let b_bytes = std::slice::from_raw_parts(b_data, b_len);
        if a_bytes == b_bytes { 1 } else { 0 }
    }
}

/// Lexicographic byte comparison: -1 if a < b, 0 if a == b, 1 if a > b.
#[no_mangle]
pub extern "C" fn frog_str_cmp(a: i64, b: i64) -> i64 {
    let a_ptr = a as *const FrogStr;
    let b_ptr = b as *const FrogStr;
    let struct_size = std::mem::size_of::<FrogStr>();
    unsafe {
        let a_len = (*a_ptr).len as usize;
        let b_len = (*b_ptr).len as usize;
        let a_data = (a_ptr as *const u8).add(struct_size);
        let b_data = (b_ptr as *const u8).add(struct_size);
        let a_bytes = std::slice::from_raw_parts(a_data, a_len);
        let b_bytes = std::slice::from_raw_parts(b_data, b_len);
        match a_bytes.cmp(b_bytes) {
            std::cmp::Ordering::Less    => -1,
            std::cmp::Ordering::Equal   => 0,
            std::cmp::Ordering::Greater => 1,
        }
    }
}

/// `needle in haystack` for strings — substring search. `1` if `haystack`
/// contains `needle` as a contiguous byte run (the empty string is always
/// contained, matching Python's `"" in s`), else `0`.
#[no_mangle]
pub extern "C" fn frog_str_contains(needle: i64, haystack: i64) -> i64 {
    let needle_ptr = needle as *const FrogStr;
    let haystack_ptr = haystack as *const FrogStr;
    let struct_size = std::mem::size_of::<FrogStr>();
    unsafe {
        let needle_len = (*needle_ptr).len as usize;
        let haystack_len = (*haystack_ptr).len as usize;
        if needle_len > haystack_len { return 0; }
        let needle_data = (needle_ptr as *const u8).add(struct_size);
        let haystack_data = (haystack_ptr as *const u8).add(struct_size);
        let needle_bytes = std::slice::from_raw_parts(needle_data, needle_len);
        let haystack_bytes = std::slice::from_raw_parts(haystack_data, haystack_len);
        if haystack_bytes.windows(needle_len.max(1)).any(|w| w == needle_bytes) || needle_len == 0 {
            1
        } else {
            0
        }
    }
}

#[no_mangle]
pub extern "C" fn frog_str_print(s: i64) {
    let ptr = s as *const FrogStr;
    unsafe {
        let len = (*ptr).len as usize;
        let data = (ptr as *const u8).add(std::mem::size_of::<FrogStr>());
        let bytes = std::slice::from_raw_parts(data, len);
        let _ = std::io::stdout().write_all(bytes);
        let _ = std::io::stdout().flush();
    }
}

/// `s`, quoted and escaped as a frog string literal — the shared body of
/// `frog_str_repr_print` (writes it to stdout) and `frog_str_repr`
/// (`plans/DATA.md` stage 5: `repr`'s allocating twin, returning a fresh
/// `FrogStr` instead). `crate::notation::escape_str`, not Rust's `{:?}`:
/// `escape_debug` spells escapes froglang's lexer doesn't accept.
fn escaped_str(s: i64) -> String {
    let ptr = s as *const FrogStr;
    unsafe {
        let len = (*ptr).len as usize;
        let data = (ptr as *const u8).add(std::mem::size_of::<FrogStr>());
        let bytes = std::slice::from_raw_parts(data, len);
        crate::notation::escape_str(&String::from_utf8_lossy(bytes))
    }
}

/// Print a string as a quoted, escaped literal. Composite value formatting
/// uses this so string fields remain unambiguous while plain `print(str)`
/// keeps its existing raw-text behavior.
#[no_mangle]
pub extern "C" fn frog_str_repr_print(s: i64) {
    let _ = write!(std::io::stdout(), "{}", escaped_str(s));
    let _ = std::io::stdout().flush();
}

/// Print a host-owned byte slice. Used by generated formatting code for
/// punctuation and struct field names; unlike `FrogStr`, these bytes do not
/// live on the GC heap.
#[no_mangle]
pub extern "C" fn frog_bytes_print(data: i64, len: i64) {
    let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, len as usize) };
    let _ = std::io::stdout().write_all(bytes);
    let _ = std::io::stdout().flush();
}

#[no_mangle]
pub extern "C" fn frog_str_println(s: i64) {
    frog_str_print(s);
    let _ = std::io::stdout().write_all(b"\n");
    let _ = std::io::stdout().flush();
}

#[no_mangle]
pub extern "C" fn frog_int_println(n: i64) {
    println!("{n}");
}

#[no_mangle]
pub extern "C" fn frog_float_println(n: f64) {
    println!("{}", crate::notation::float_repr(n));
}

#[no_mangle]
pub extern "C" fn frog_bool_println(b: i8) {
    println!("{}", b != 0);
}

#[no_mangle]
pub extern "C" fn frog_int_print(n: i64) { print!("{n}"); }

#[no_mangle]
pub extern "C" fn frog_float_print(n: f64) { print!("{}", crate::notation::float_repr(n)); }

#[no_mangle]
pub extern "C" fn frog_bool_print(b: i8) { print!("{}", b != 0); }

// List printing lives in codegen (`print_list`), not here: the runtime has
// no element-type information, so a runtime printer could only emit
// `<struct>`/`<list>` placeholders. See plans/DATA.md stage 0.

// ── `repr` leaves (plans/DATA.md stage 5) ───────────────────────────────────
//
// The allocating twins of `frog_{int,float,bool,str}_print` above: each
// returns a fresh `FrogStr` instead of writing to stdout, for
// `desugar_notation`'s scalar arms (`typeck.rs`) to concatenate.

#[no_mangle]
pub extern "C" fn frog_int_repr(n: i64) -> i64 {
    let _jit_frame = crate::jit_frame_guard!();
    let s = n.to_string();
    with_heap(|heap| {
        heap.maybe_collect();
        heap.alloc_str(s.as_bytes()) as i64
    })
}

#[no_mangle]
pub extern "C" fn frog_float_repr(n: f64) -> i64 {
    let _jit_frame = crate::jit_frame_guard!();
    let s = crate::notation::float_repr(n);
    with_heap(|heap| {
        heap.maybe_collect();
        heap.alloc_str(s.as_bytes()) as i64
    })
}

#[no_mangle]
pub extern "C" fn frog_bool_repr(b: i8) -> i64 {
    let _jit_frame = crate::jit_frame_guard!();
    let s = if b != 0 { "true" } else { "false" };
    with_heap(|heap| {
        heap.maybe_collect();
        heap.alloc_str(s.as_bytes()) as i64
    })
}

#[no_mangle]
pub extern "C" fn frog_str_repr(s: i64) -> i64 {
    let _jit_frame = crate::jit_frame_guard!();
    let _roots = RuntimeRoots::hold(&[s]);
    let escaped = escaped_str(s);
    with_heap(|heap| {
        heap.maybe_collect();
        heap.alloc_str(escaped.as_bytes()) as i64
    })
}

/// Concatenate a `List<Str>` with a separator between elements —
/// `desugar_notation`'s `List<T>` arm reduces its per-element `repr`
/// fragments this way instead of an O(depth) `+` chain. An **internal**
/// builtin, not `stdlib`'s host `join`: base `FrogState::new()` has no
/// stdlib, and `repr` must not be stdlib-only (`typeck.rs`'s
/// `compile_call` dispatches on the callee name `"__str_join"`, exactly
/// like `print`/`push`/`len`, and only `desugar_notation` ever synthesizes
/// a call to it — it is never a name user source can spell).
#[no_mangle]
pub extern "C" fn frog_str_join(list: i64, sep: i64) -> i64 {
    let _jit_frame = crate::jit_frame_guard!();
    let _roots = RuntimeRoots::hold(&[list, sep]);
    let len = frog_list_len(list);
    let sep_ptr = sep as *const FrogStr;
    let (sep_data, sep_len) = unsafe {
        let l = (*sep_ptr).len as usize;
        ((sep_ptr as *const u8).add(std::mem::size_of::<FrogStr>()), l)
    };
    let sep_bytes = unsafe { std::slice::from_raw_parts(sep_data, sep_len) };

    let mut buf: Vec<u8> = Vec::new();
    for i in 0..len {
        if i > 0 { buf.extend_from_slice(sep_bytes); }
        let elem = frog_list_get(list, i, 0);
        let elem_ptr = elem as *const FrogStr;
        unsafe {
            let elem_len = (*elem_ptr).len as usize;
            let elem_data = (elem_ptr as *const u8).add(std::mem::size_of::<FrogStr>());
            buf.extend_from_slice(std::slice::from_raw_parts(elem_data, elem_len));
        }
    }

    with_heap(|heap| {
        heap.maybe_collect();
        heap.alloc_str(&buf) as i64
    })
}

// ── List operations ───────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn frog_alloc_list(cap: i64, stride: i64, ptr_mask: i64) -> i64 {
    let _jit_frame = crate::jit_frame_guard!();
    with_heap(|heap| {
        heap.maybe_collect();
        heap.alloc_list(cap as usize, stride as usize, ptr_mask as u64) as i64
    })
}

#[no_mangle]
pub extern "C" fn frog_list_len(list: i64) -> i64 {
    unsafe {
        let list_ptr = list as *const FrogList;
        let stride = ((*list_ptr).stride as i64).max(1);
        (*list_ptr).len as i64 / stride
    }
}

/// Terminate the process with a diagnostic. Called for an out-of-range list
/// index (after negative-index normalization).
///
/// This is a hard exit, not a catchable froglang-level error: the caller is
/// always JIT-compiled code reached through a raw function-pointer call (see
/// `FrogState::call_jit`), which has no unwind tables, so panicking here
/// (unlike the codegen-construction panics caught in `FrogState::eval`)
/// cannot be safely unwound through — it would corrupt the stack rather than
/// produce a clean error. `process::exit` never unwinds, so it's the only
/// safe way out of this call frame.
fn frog_index_out_of_bounds(idx: i64, len: i64) -> ! {
    frog_abort(format_args!("index {} out of bounds for list of length {}", idx, len));
}

/// Terminate the process with `msg` — the single exit point every froglang
/// runtime abort goes through, so they all report the same way (see
/// `frog_index_out_of_bounds`'s note on why this is `process::exit` and not
/// a Rust panic).
///
/// Prefer this over emitting a Cranelift `trap` for anything a *program*
/// can trigger. A `trap` raises SIGILL, which kills the process with exit
/// 132 and no diagnostic whatsoever — `panic("boom")` used to print its
/// message and then die that way, and `1 / 0` died that way with no output
/// at all. `trap` stays correct for genuinely unreachable IR (a `Never`-typed
/// tail), which is what it's for.
pub(super) fn frog_abort(msg: std::fmt::Arguments) -> ! {
    let _ = std::io::stdout().flush();
    eprintln!("frog: {}", msg);
    std::process::exit(1);
}

/// Report an explicit `panic(msg)` (and `!`'s unwrap-failure desugaring,
/// which resolves to the same reserved builtin) and terminate.
#[no_mangle]
pub extern "C" fn frog_panic(s: i64) -> ! {
    let ptr = s as *const FrogStr;
    let msg = unsafe {
        let len = (*ptr).len as usize;
        let data = (ptr as *const u8).add(std::mem::size_of::<FrogStr>());
        String::from_utf8_lossy(std::slice::from_raw_parts(data, len)).into_owned()
    };
    frog_abort(format_args!("{}", msg));
}

/// Report an integer-division fault and terminate. `is_zero` is non-zero for
/// a divide-by-zero and zero for the one other case `sdiv`/`srem` fault on,
/// `Int::MIN / -1`, whose true quotient is not representable. Codegen tests
/// both conditions ahead of the division and passes the flag, so one call
/// site covers both messages — see `emit_int_div_guard` in `codegen/mod.rs`.
#[no_mangle]
pub extern "C" fn frog_div_error(is_zero: i64) -> ! {
    if is_zero != 0 {
        frog_abort(format_args!("division by zero"));
    }
    frog_abort(format_args!("division overflow: Int.MIN / -1 is not representable"));
}

/// Resolve a possibly-negative index (Python-style: -1 is the last element)
/// against `len`, terminating the process if it's still out of range.
fn resolve_index(idx: i64, len: i64) -> usize {
    let real_idx = if idx < 0 { len + idx } else { idx };
    if real_idx < 0 || real_idx >= len {
        frog_index_out_of_bounds(idx, len);
    }
    real_idx as usize
}

/// Read one raw `i64` slot at `field_offset` within element `idx`.
/// `field_offset` is always `0` for a scalar (non-struct) element type —
/// see `struct_fields` in `codegen/mod.rs`, which computes it for each
/// leaf of a struct-typed element.
#[no_mangle]
pub extern "C" fn frog_list_get(list: i64, idx: i64, field_offset: i64) -> i64 {
    unsafe {
        let list_ptr = list as *const FrogList;
        let stride = ((*list_ptr).stride as i64).max(1);
        let elem_len = (*list_ptr).len as i64 / stride;
        let real_idx = resolve_index(idx, elem_len);
        let slot = real_idx as i64 * stride + field_offset;
        *(*list_ptr).data.add(slot as usize)
    }
}

#[no_mangle]
pub extern "C" fn frog_list_set(list: i64, idx: i64, field_offset: i64, val: i64) {
    unsafe {
        let list_ptr = list as *mut FrogList;
        let stride = ((*list_ptr).stride as i64).max(1);
        let elem_len = (*list_ptr).len as i64 / stride;
        let real_idx = resolve_index(idx, elem_len);
        let slot = real_idx as i64 * stride + field_offset;
        *(*list_ptr).data.add(slot as usize) = val;
    }
}

/// Clamp a possibly-negative, possibly-out-of-range slice bound against
/// `len`. Unlike `resolve_index`, this never errors — Python-style slicing
/// silently clamps to the valid range instead of raising on an out-of-range
/// bound.
fn clamp_bound(idx: i64, len: i64) -> i64 {
    let real_idx = if idx < 0 { len + idx } else { idx };
    real_idx.clamp(0, len)
}

/// Slice `list[start..end]` into a freshly-allocated list, following
/// Python-style semantics: bounds are clamped (never an error), negative
/// bounds count from the end, and `end < start` yields an empty list.
///
/// `start`/`end` use `i64::MIN`/`i64::MAX` as sentinels for "omitted" (i.e.
/// `list[:end]` / `list[start:]` / `list[:]`) — see codegen's `Slice` arm.
#[no_mangle]
pub extern "C" fn frog_list_slice(list: i64, start: i64, end: i64) -> i64 {
    let _jit_frame = crate::jit_frame_guard!();
    // `list` is read again after the allocation below, which can collect —
    // and the caller's stack map does not cover it, since from the JIT's
    // point of view the value died at this call. See `gc::RuntimeRoots`.
    let _roots = RuntimeRoots::hold(&[list]);
    let list_ptr = list as *const FrogList;
    let (stride, ptr_mask) = unsafe { ((*list_ptr).stride as i64, (*list_ptr).ptr_mask) };
    let stride = stride.max(1);
    let elem_len = unsafe { (*list_ptr).len as i64 } / stride;

    let s = if start == i64::MIN { 0 } else { clamp_bound(start, elem_len) };
    let e = if end == i64::MAX { elem_len } else { clamp_bound(end, elem_len) };
    let e = e.max(s);
    let slice_elems = (e - s) as usize;
    let slice_slots = slice_elems * stride as usize;

    with_heap(|heap| {
        heap.maybe_collect();
        let new_list = heap.alloc_list(slice_elems.max(1), stride as usize, ptr_mask);
        unsafe {
            (*new_list).len = slice_slots as u32;
            if slice_slots > 0 {
                let src = (*list_ptr).data.add(s as usize * stride as usize);
                std::ptr::copy_nonoverlapping(src, (*new_list).data, slice_slots);
            }
        }
        new_list as i64
    })
}

/// Deep-clone the heap value `w` — the runtime primitive codegen's
/// `Ownership::Copy`-gated clone-on-read (see `codegen::compile_expr_multi`'s
/// `Var` arm) calls whenever a binding's value is duplicated and might still
/// be observed elsewhere. `w` may be `0` (an empty/absent slot) or a
/// non-pointer immediate, in which case it's returned unchanged — only an
/// actual heap pointer (`is_heap_ptr`) is cloned.
///
/// `w` itself may carry tag bits (an inline union's tagged pointer word, not
/// just a plain `List`/`Str` pointer — `is_heap_ty` covers `Type::Union` too,
/// so this is called on those leaves as well). The clone must carry the same
/// tag, or a union value would silently forget which member it was — the
/// same reasoning `GcHeap::clone_obj`'s `Variant` case already applies to a
/// tagged pointer nested *inside* a payload, just one level further out.
#[no_mangle]
pub extern "C" fn frog_clone(w: i64) -> i64 {
    if !super::gc::is_heap_ptr(w) {
        return w;
    }
    let _jit_frame = crate::jit_frame_guard!();
    // `w` is read (recursively) throughout `clone_obj`, which allocates and
    // can therefore collect — and the caller's stack map does not cover it,
    // since from the JIT's point of view the value died at this call. See
    // `gc::RuntimeRoots`, and `GcHeap::clone_obj`'s own doc comment for how
    // the objects it allocates along the way are separately kept alive.
    let _roots = RuntimeRoots::hold(&[w]);
    with_heap(|heap| {
        heap.maybe_collect();
        let cloned = unsafe { heap.clone_obj(super::gc::heap_ptr(w)) };
        (cloned as i64) | (w & super::gc::TAG_MASK)
    })
}

/// Doubles a raw (non-GC-managed) buffer of 8-byte-aligned words from
/// `old_size` to `new_size` bytes: `std::alloc::alloc` when there's
/// nothing to carry forward (`old_size == 0` — `Dict`'s entries/hashes
/// buffers before their first insert), `realloc` otherwise (`List`'s data
/// buffer, which `GcHeap::alloc_list` guarantees always starts non-empty,
/// so `old_size` is never 0 there). Aborts the process on allocation
/// failure — there's no error path out of JIT code.
///
/// Caller does the capacity-doubling arithmetic and the `u32::MAX`
/// overflow check first (`frog_list_push`'s and `dict::grow_if_full`'s
/// units differ — words vs. entries — so that check can't live here), and
/// updates `heap.bytes_allocated` by `new_size - old_size` afterward.
///
/// # Safety
/// `old_ptr` must be a pointer previously returned by this function (or
/// null iff `old_size == 0`) allocated with `old_size` bytes at 8-byte
/// alignment.
pub(crate) unsafe fn grow_raw_buffer(old_ptr: *mut u8, old_size: usize, new_size: usize) -> *mut u8 {
    let new_layout = Layout::from_size_align(new_size, 8).expect("buffer layout");
    let new_data = if old_size == 0 {
        std::alloc::alloc(new_layout)
    } else {
        let old_layout = Layout::from_size_align(old_size, 8).expect("buffer layout");
        std::alloc::realloc(old_ptr, old_layout, new_size)
    };
    if new_data.is_null() {
        std::alloc::handle_alloc_error(new_layout);
    }
    new_data
}

#[no_mangle]
pub extern "C" fn frog_list_push(list: i64, val: i64) -> i64 {
    with_heap(|heap| {
        let list_ptr = list as *mut FrogList;
        unsafe {
            if (*list_ptr).len == (*list_ptr).cap {
                let old_cap = (*list_ptr).cap as usize;
                // `cap` is a `u32`; doubling past that would silently
                // truncate and leave the list claiming capacity it doesn't
                // have. There is no error path out of JIT code, so abort.
                let new_cap = old_cap * 2;
                if new_cap > u32::MAX as usize {
                    frog_abort(format_args!("list grew past the maximum length of {} elements", u32::MAX));
                }
                let old_size = old_cap * std::mem::size_of::<i64>();
                let new_size = new_cap * std::mem::size_of::<i64>();
                let new_data = grow_raw_buffer((*list_ptr).data as *mut u8, old_size, new_size) as *mut i64;
                (*list_ptr).data = new_data;
                (*list_ptr).cap = new_cap as u32;
                heap.bytes_allocated += new_size - old_size;
            }
            let idx = (*list_ptr).len as usize;
            *(*list_ptr).data.add(idx) = val;
            (*list_ptr).len += 1;
        }
        list
    })
}

/// Materialize `start..end` (end-exclusive) as a freshly-allocated
/// `List<Int>`. `end <= start` yields an empty list, matching `frog_list_slice`'s
/// "never errors, just clamps" convention rather than `frog_list_get`'s.
#[no_mangle]
pub extern "C" fn frog_range(start: i64, end: i64) -> i64 {
    let _jit_frame = crate::jit_frame_guard!();
    let len = if end > start { (end - start) as usize } else { 0 };

    with_heap(|heap| {
        heap.maybe_collect();
        let list = heap.alloc_list(len.max(1), 1, 0);
        unsafe {
            (*list).len = len as u32;
            for i in 0..len {
                *(*list).data.add(i) = start + i as i64;
            }
        }
        list as i64
    })
}

// ── Variant (nominal union member) operations ───────────────────────────────

/// Allocate a GC-managed `FrogVariant`: `tag` is the member's declaration
/// index, `nslots` its flattened payload width, `ptr_mask` marks which
/// slots are heap pointers — all computed at codegen time from
/// `TypeChecker::UnionDef`/`codegen::enum_field_leaf_types`. Payload slots
/// start zeroed; the caller (codegen's `VariantInit`) fills them in with
/// `frog_variant_set` right after this returns.
#[no_mangle]
pub extern "C" fn frog_alloc_variant(tag: i64, nslots: i64, ptr_mask: i64) -> i64 {
    let _jit_frame = crate::jit_frame_guard!();
    with_heap(|heap| {
        heap.maybe_collect();
        heap.alloc_variant(tag as u32, nslots as usize, ptr_mask as u64) as i64
    })
}

/// A payload-less member of a *boxed* union is an immediate — the value
/// carries its own tag and there is nothing to dereference. See gc.rs's
/// "Word encoding" section.
#[no_mangle]
pub extern "C" fn frog_variant_tag(variant: i64) -> i64 {
    if !super::gc::is_heap_ptr(variant) {
        return super::gc::immediate_variant_tag(variant);
    }
    unsafe { (*(variant as *const FrogVariant)).tag as i64 }
}

/// Read raw payload slot `slot` — always in-bounds by construction (the
/// slot index is a compile-time constant computed from the variant's own
/// declared layout, never runtime-derived the way a list index is), so
/// unlike `frog_list_get` there is no bounds check here.
#[no_mangle]
pub extern "C" fn frog_variant_get(variant: i64, slot: i64) -> i64 {
    unsafe {
        let ptr = variant as *const FrogVariant;
        let data = (ptr as *const u8).add(std::mem::size_of::<FrogVariant>()) as *const i64;
        *data.add(slot as usize)
    }
}

#[no_mangle]
pub extern "C" fn frog_variant_set(variant: i64, slot: i64, val: i64) {
    unsafe {
        let ptr = variant as *mut FrogVariant;
        let data = (ptr as *mut u8).add(std::mem::size_of::<FrogVariant>()) as *mut i64;
        *data.add(slot as usize) = val;
    }
}

/// `FROG_COW_VERIFY`'s check: called from the write barrier's *unshared*
/// path (`codegen::emit_unshare`), only in a build whose codegen saw the env
/// var set. Aborts if `w` turns out to be reachable by more live references
/// than this position in a place walk permits — i.e. an aliasing site failed
/// to mark, which is the one failure mode copy-on-write actually has.
///
/// `allowed` is how many references reaching `w` are legitimate here. It is
/// 1 for a place rooted directly in a binding: the JIT slot holding the
/// pointer about to be written through. It is 2 for a list reached *through*
/// another list (`rows[y][x] = v`, `push(mut rows[0], v)`), where the parent
/// list's own slot is a second, expected reference — and mutating through it
/// is exactly what the walk is entitled to do, because `emit_place_ref`
/// unshared that parent immediately beforehand. Passing the depth in rather
/// than relaxing the check to `> 2` everywhere keeps the outer case strict.
///
/// See `GcHeap::count_refs` for why this is worth the cost, and
/// MUTABILITY.md Stages 7 and 8.
#[no_mangle]
pub extern "C" fn frog_cow_verify(w: i64, allowed: i64) {
    let _jit_frame = crate::jit_frame_guard!();
    if !super::gc::is_heap_ptr(w) { return; }
    let refs = with_heap(|heap| heap.count_refs(super::gc::heap_ptr(w)));
    if refs as i64 > allowed {
        frog_abort(format_args!(
            "FROG_COW_VERIFY: about to mutate {:#x} in place, but {} live references reach it \
             ({} expected here) — an aliasing site failed to set the shared bit \
             (MUTABILITY.md Stage 7)",
            w, refs, allowed,
        ));
    }
}

// ── GC diagnostics ───────────────────────────────────────────────────────────

/// Print a full heap dump to stderr. Callable as `gc_dump()` from froglang.
#[no_mangle]
pub extern "C" fn frog_gc_dump() {
    super::gc::gc_dump();
}

// ── GC roots for JIT-local heap pointers ──────────────────────────────────────
//
// Deliberately absent: there is no `frog_frame_push`/`frog_frame_pop` FFI
// pair, and no runtime function here maintains a root of its own for a
// value it merely passes through. Roots for JIT-produced values are
// Cranelift's stack maps (RUNTIME.md Part 2, `gc.rs`'s "Precise roots") —
// `Codegen` declares every GC-scannable SSA value as it is produced, and the
// collector walks the native stack directly rather than any structure this
// module maintains. The one thing a runtime function does still have to get
// right is `gc::RuntimeRoots`, just below `with_heap`: an argument it still
// needs *after* a call that can collect is not otherwise rooted, since from
// the JIT's point of view that argument died at the call.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frog_alloc_str_and_len() {
        let s = b"world";
        let ptr = frog_alloc_str(s.as_ptr() as i64, s.len() as i64);
        assert_eq!(frog_str_len(ptr), 5);
    }

    #[test]
    fn test_frog_str_concat_len() {
        let a = b"foo";
        let b = b"bar";
        let pa = frog_alloc_str(a.as_ptr() as i64, a.len() as i64);
        let pb = frog_alloc_str(b.as_ptr() as i64, b.len() as i64);
        let pc = frog_str_concat(pa, pb);
        assert_eq!(frog_str_len(pc), 6);
    }

    #[test]
    fn test_frog_str_eq() {
        // Root each string immediately — these
        // tests call the FFI entry points directly, so nothing else roots
        // `pa`/`pb`/`pc`, and under `FROG_GC_STRESS` every further
        // allocation collects for real: an unrooted earlier string is
        // exactly the "no observer" case the collector is entitled to
        // sweep.
        let a = b"abc";
        let b = b"abc";
        let c = b"xyz";
        let pa = frog_alloc_str(a.as_ptr() as i64, a.len() as i64);
        with_heap(|heap| heap.push_root(pa, true));
        let pb = frog_alloc_str(b.as_ptr() as i64, b.len() as i64);
        with_heap(|heap| heap.push_root(pb, true));
        let pc = frog_alloc_str(c.as_ptr() as i64, c.len() as i64);
        with_heap(|heap| heap.push_root(pc, true));
        assert_eq!(frog_str_eq(pa, pb), 1);
        assert_eq!(frog_str_eq(pa, pc), 0);
    }

    #[test]
    fn test_frog_str_cmp() {
        // See `test_frog_str_eq`'s comment on why these are rooted by hand.
        let a = b"abc";
        let b = b"abd";
        let pa = frog_alloc_str(a.as_ptr() as i64, a.len() as i64);
        with_heap(|heap| heap.push_root(pa, true));
        let pb = frog_alloc_str(b.as_ptr() as i64, b.len() as i64);
        with_heap(|heap| heap.push_root(pb, true));
        assert_eq!(frog_str_cmp(pa, pb), -1);
        assert_eq!(frog_str_cmp(pb, pa), 1);
        assert_eq!(frog_str_cmp(pa, pa), 0);
    }

    #[test]
    fn test_frog_list_push_and_get() {
        let list = frog_alloc_list(2, 1, 0);
        frog_list_push(list, 10);
        frog_list_push(list, 20);
        frog_list_push(list, 30);  // triggers realloc
        assert_eq!(frog_list_len(list), 3);
        assert_eq!(frog_list_get(list, 0, 0), 10);
        assert_eq!(frog_list_get(list, 1, 0), 20);
        assert_eq!(frog_list_get(list, 2, 0), 30);
    }

    /// Read a `FrogStr`'s bytes back into an owned Rust `String`, for
    /// asserting on what the new `repr`-leaf FFI functions allocated.
    fn read_str(ptr: i64) -> String {
        let p = ptr as *const FrogStr;
        unsafe {
            let len = (*p).len as usize;
            let data = (p as *const u8).add(std::mem::size_of::<FrogStr>());
            String::from_utf8_lossy(std::slice::from_raw_parts(data, len)).into_owned()
        }
    }

    #[test]
    fn test_frog_int_repr() {
        assert_eq!(read_str(frog_int_repr(42)), "42");
        assert_eq!(read_str(frog_int_repr(-7)), "-7");
    }

    #[test]
    fn test_frog_float_repr() {
        assert_eq!(read_str(frog_float_repr(1.0)), "1.0");
        assert_eq!(read_str(frog_float_repr(f64::INFINITY)), "inf");
    }

    #[test]
    fn test_frog_bool_repr() {
        assert_eq!(read_str(frog_bool_repr(1)), "true");
        assert_eq!(read_str(frog_bool_repr(0)), "false");
    }

    #[test]
    fn test_frog_str_repr_escapes_and_quotes() {
        let s = b"a\nb";
        let ptr = frog_alloc_str(s.as_ptr() as i64, s.len() as i64);
        with_heap(|heap| heap.push_root(ptr, true));
        assert_eq!(read_str(frog_str_repr(ptr)), "\"a\\nb\"");
    }

    #[test]
    fn test_frog_str_join() {
        // stride 1, ptr_mask 1: one pointer-typed slot per element — the
        // `List<Str>` layout `desugar_notation`'s `List<T>` arm produces.
        let list = frog_alloc_list(3, 1, 1);
        with_heap(|heap| heap.push_root(list, true));
        for s in [&b"a"[..], &b"b"[..], &b"c"[..]] {
            let ptr = frog_alloc_str(s.as_ptr() as i64, s.len() as i64);
            frog_list_push(list, ptr);
        }
        let sep = b", ";
        let sep_ptr = frog_alloc_str(sep.as_ptr() as i64, sep.len() as i64);
        with_heap(|heap| heap.push_root(sep_ptr, true));
        assert_eq!(read_str(frog_str_join(list, sep_ptr)), "a, b, c");
    }

    #[test]
    fn test_frog_str_join_empty_list_is_empty_string() {
        let list = frog_alloc_list(0, 1, 1);
        with_heap(|heap| heap.push_root(list, true));
        let sep = b", ";
        let sep_ptr = frog_alloc_str(sep.as_ptr() as i64, sep.len() as i64);
        with_heap(|heap| heap.push_root(sep_ptr, true));
        assert_eq!(read_str(frog_str_join(list, sep_ptr)), "");
    }
}
