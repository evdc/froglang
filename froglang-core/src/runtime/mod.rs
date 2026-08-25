pub mod gc;
pub mod ffi;
pub mod host;

pub use gc::{push_root, gc_collect, bytes_allocated};
