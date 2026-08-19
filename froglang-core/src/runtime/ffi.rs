use std::alloc::Layout;
use std::io::Write;

use super::gc::{FrogList, FrogStr, FrogVariant, GcHeap, GC_HEAP, ACTIVE_HEAP};

/// Call `f` with a mutable reference to the active GcHeap.
/// Uses the `FrogState`-owned heap if one is executing on this thread,
/// otherwise falls back to the thread-local GC_HEAP.
fn with_heap<F, R>(f: F) -> R
where
    F: FnOnce(&mut GcHeap) -> R,
{
    let ptr = ACTIVE_HEAP.with(|h| h.get());
    if !ptr.is_null() {
        unsafe { f(&mut *ptr) }
    } else {
        GC_HEAP.with(|h| {
            let mut borrowed = h.borrow_mut();
            f(&mut *borrowed)
        })
    }
}

// ── String operations ─────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn frog_alloc_str(data: i64, len: i64) -> i64 {
    with_heap(|heap| {
        heap.maybe_collect();
        heap.alloc_str(data as *const u8, len as usize) as i64
    })
}

#[no_mangle]
pub extern "C" fn frog_str_len(s: i64) -> i64 {
    unsafe { (*(s as *const FrogStr)).len as i64 }
}

#[no_mangle]
pub extern "C" fn frog_str_concat(a: i64, b: i64) -> i64 {
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
        heap.alloc_str(buf.as_ptr(), total_len) as i64
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

/// Print a string as a quoted, escaped literal. Composite value formatting
/// uses this so string fields remain unambiguous while plain `print(str)`
/// keeps its existing raw-text behavior.
#[no_mangle]
pub extern "C" fn frog_str_repr_print(s: i64) {
    let ptr = s as *const FrogStr;
    unsafe {
        let len = (*ptr).len as usize;
        let data = (ptr as *const u8).add(std::mem::size_of::<FrogStr>());
        let bytes = std::slice::from_raw_parts(data, len);
        let _ = write!(std::io::stdout(), "{:?}", String::from_utf8_lossy(bytes));
        let _ = std::io::stdout().flush();
    }
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
    print!("{n}\n");
}

#[no_mangle]
pub extern "C" fn frog_float_println(n: f64) {
    print!("{n:?}\n");
}

#[no_mangle]
pub extern "C" fn frog_bool_println(b: i8) {
    print!("{}\n", b != 0);
}

#[no_mangle]
pub extern "C" fn frog_int_print(n: i64) { print!("{n}"); }

#[no_mangle]
pub extern "C" fn frog_float_print(n: f64) { print!("{n:?}"); }

#[no_mangle]
pub extern "C" fn frog_bool_print(b: i8) { print!("{}", b != 0); }

/// Print a list whose element representation is described by `kind`.
/// Nested lists deliberately use a compact placeholder: list elements carry
/// no recursive type metadata at runtime.
#[no_mangle]
pub extern "C" fn frog_list_println(list: i64, kind: i64) {
    frog_list_print(list, kind);
    println!();
}

#[no_mangle]
pub extern "C" fn frog_list_print(list: i64, kind: i64) {
    let list = unsafe { &*(list as *const FrogList) };
    let stride = (list.stride as usize).max(1);
    let elem_len = list.len as usize / stride;
    let mut out = String::from("[");
    for i in 0..elem_len {
        if i != 0 { out.push_str(", "); }
        if stride != 1 {
            // Struct elements aren't printable yet — see DESIGN discussion.
            out.push_str("<struct>");
            continue;
        }
        let value = unsafe { *list.data.add(i) };
        match kind {
            0 => out.push_str(&value.to_string()),
            1 => out.push_str(&f64::from_bits(value as u64).to_string()),
            2 => out.push_str(&(value != 0).to_string()),
            3 => unsafe {
                let s = value as *const FrogStr;
                let data = (s as *const u8).add(std::mem::size_of::<FrogStr>());
                let bytes = std::slice::from_raw_parts(data, (*s).len as usize);
                out.push_str(&String::from_utf8_lossy(bytes));
            },
            _ => out.push_str("<list>"),
        }
    }
    print!("{out}]");
}

// ── List operations ───────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn frog_alloc_list(cap: i64, stride: i64, ptr_mask: i64) -> i64 {
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
    eprintln!("frog: index {} out of bounds for list of length {}", idx, len);
    std::process::exit(1);
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

#[no_mangle]
pub extern "C" fn frog_list_push(list: i64, val: i64) -> i64 {
    with_heap(|heap| {
        let list_ptr = list as *mut FrogList;
        unsafe {
            if (*list_ptr).len == (*list_ptr).cap {
                let old_cap = (*list_ptr).cap as usize;
                let new_cap = old_cap * 2;
                let old_layout = Layout::array::<i64>(old_cap).expect("list realloc layout");
                let new_size = new_cap * std::mem::size_of::<i64>();
                let new_data = std::alloc::realloc(
                    (*list_ptr).data as *mut u8,
                    old_layout,
                    new_size,
                ) as *mut i64;
                (*list_ptr).data = new_data;
                (*list_ptr).cap = new_cap as u32;
                heap.bytes_allocated += (new_cap - old_cap) * std::mem::size_of::<i64>();
            }
            let idx = (*list_ptr).len as usize;
            *(*list_ptr).data.add(idx) = val;
            (*list_ptr).len += 1;
        }
        list
    })
}

/// Materialize `start..end` (end-exclusive) as a freshly-allocated
/// `List(Int)`. `end <= start` yields an empty list, matching `frog_list_slice`'s
/// "never errors, just clamps" convention rather than `frog_list_get`'s.
#[no_mangle]
pub extern "C" fn frog_range(start: i64, end: i64) -> i64 {
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
    with_heap(|heap| {
        heap.maybe_collect();
        heap.alloc_variant(tag as u32, nslots as usize, ptr_mask as u64) as i64
    })
}

/// A payload-less variant is unboxed — the value carries its own tag and
/// there is nothing to dereference. See gc.rs's "Immediate (unboxed)
/// values" section for the encoding.
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

// ── GC diagnostics ───────────────────────────────────────────────────────────

/// Print a full heap dump to stderr. Callable as `gc_dump()` from froglang.
#[no_mangle]
pub extern "C" fn frog_gc_dump() {
    super::gc::gc_dump();
}

// ── Shadow stack (GC roots for JIT-local heap pointers) ────────────────────────
//
// Called from the prologue/epilogue codegen emits around every JIT function
// that has at least one heap-typed subexpression. See gc.rs's "Shadow stack"
// section for the rooting invariant this maintains.

#[no_mangle]
pub extern "C" fn frog_frame_push(slots: i64, len: i64) {
    with_heap(|heap| heap.push_frame(slots as *mut i64, len as usize));
}

#[no_mangle]
pub extern "C" fn frog_frame_pop() {
    with_heap(|heap| heap.pop_frame());
}

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
        let a = b"abc";
        let b = b"abc";
        let c = b"xyz";
        let pa = frog_alloc_str(a.as_ptr() as i64, a.len() as i64);
        let pb = frog_alloc_str(b.as_ptr() as i64, b.len() as i64);
        let pc = frog_alloc_str(c.as_ptr() as i64, c.len() as i64);
        assert_eq!(frog_str_eq(pa, pb), 1);
        assert_eq!(frog_str_eq(pa, pc), 0);
    }

    #[test]
    fn test_frog_str_cmp() {
        let a = b"abc";
        let b = b"abd";
        let pa = frog_alloc_str(a.as_ptr() as i64, a.len() as i64);
        let pb = frog_alloc_str(b.as_ptr() as i64, b.len() as i64);
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
}
