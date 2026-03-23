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

#[no_mangle]
pub extern "C" fn frog_list_get(list: i64, idx: i64) -> i64 {
    unsafe {
        let list_ptr = list as *const FrogList;
        *(*list_ptr).data.add(idx as usize)
    }
}

#[no_mangle]
pub extern "C" fn frog_list_set(list: i64, idx: i64, val: i64) {
    unsafe {
        let list_ptr = list as *mut FrogList;
        *(*list_ptr).data.add(idx as usize) = val;
    }
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

// ── GC diagnostics ───────────────────────────────────────────────────────────

/// Print a full heap dump to stderr. Callable as `gc_dump()` from froglang.
#[no_mangle]
pub extern "C" fn frog_gc_dump() {
    super::gc::gc_dump();
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
