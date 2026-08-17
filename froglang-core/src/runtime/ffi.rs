use std::alloc::Layout;
use std::io::Write;

use super::gc::{ElemTag, FrogList, FrogStr, GcHeap, GC_HEAP, ACTIVE_HEAP};

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

#[no_mangle]
pub extern "C" fn frog_str_println(s: i64) {
    frog_str_print(s);
    let _ = std::io::stdout().write_all(b"\n");
    let _ = std::io::stdout().flush();
}

// ── List operations ───────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn frog_alloc_list(cap: i64, elem_tag: i64) -> i64 {
    let tag = if elem_tag == 0 { ElemTag::Scalar } else { ElemTag::Ptr };
    with_heap(|heap| {
        heap.maybe_collect();
        heap.alloc_list(cap as usize, tag) as i64
    })
}

#[no_mangle]
pub extern "C" fn frog_list_len(list: i64) -> i64 {
    unsafe { (*(list as *const FrogList)).len as i64 }
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

#[no_mangle]
pub extern "C" fn frog_list_get(list: i64, idx: i64) -> i64 {
    unsafe {
        let list_ptr = list as *const FrogList;
        let len = (*list_ptr).len as i64;
        let real_idx = resolve_index(idx, len);
        *(*list_ptr).data.add(real_idx)
    }
}

#[no_mangle]
pub extern "C" fn frog_list_set(list: i64, idx: i64, val: i64) {
    unsafe {
        let list_ptr = list as *mut FrogList;
        let len = (*list_ptr).len as i64;
        let real_idx = resolve_index(idx, len);
        *(*list_ptr).data.add(real_idx) = val;
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
    let (len, tag) = unsafe { ((*list_ptr).len as i64, (*list_ptr).elem_tag) };

    let s = if start == i64::MIN { 0 } else { clamp_bound(start, len) };
    let e = if end == i64::MAX { len } else { clamp_bound(end, len) };
    let e = e.max(s);
    let slice_len = (e - s) as usize;

    with_heap(|heap| {
        heap.maybe_collect();
        let new_list = heap.alloc_list(slice_len.max(1), tag);
        unsafe {
            (*new_list).len = slice_len as u32;
            if slice_len > 0 {
                let src = (*list_ptr).data.add(s as usize);
                std::ptr::copy_nonoverlapping(src, (*new_list).data, slice_len);
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
        let list = heap.alloc_list(len.max(1), ElemTag::Scalar);
        unsafe {
            (*list).len = len as u32;
            for i in 0..len {
                *(*list).data.add(i) = start + i as i64;
            }
        }
        list as i64
    })
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
        let list = frog_alloc_list(2, 0);
        frog_list_push(list, 10);
        frog_list_push(list, 20);
        frog_list_push(list, 30);  // triggers realloc
        assert_eq!(frog_list_len(list), 3);
        assert_eq!(frog_list_get(list, 0), 10);
        assert_eq!(frog_list_get(list, 1), 20);
        assert_eq!(frog_list_get(list, 2), 30);
    }
}
