//! `json.parse(s): T | JsonError` (`plans/DATA.md` stage 8) — the runtime
//! half, and a deliberate near-transcription of `runtime/read.rs`.
//!
//! `read` and `json.parse` are the same shape twice: parse the input once
//! into a DOM held in thread-local storage, hand the desugar an opaque
//! handle to its root, and let it navigate with handle-in/scalar-out
//! accessors. Only the DOM differs — `read`'s is a frog `Expression` tree
//! from `frontend::parser::Parser` (`repr`'s output *is* frog source, so
//! the authority on reading it back is the authority on reading frog);
//! this one's comes from a real JSON library behind [`dom`]'s seam, which
//! is what lets froglang inherit a SIMD parser without the compiler
//! knowing.
//!
//! **Sticky-error-and-continue**, identical to `read.rs`'s contract: a
//! shape mismatch records the *first* `(message, offset)` seen and returns
//! an always-valid placeholder — a navigating accessor (`get`/`at`)
//! returns the *same node it was given*, a leaf accessor a zero-ish value
//! of its own type. That is what lets `build_read_json` synthesize one
//! unconditional happy-path expression per type with no early-exit control
//! flow, and check `frog_json_failed()` exactly once at the top.
//!
//! **One honest gap versus `read`.** A frog `Expression` node carries its
//! own source `Position`, so `read` can point `ReadError.offset` at the
//! part of the input that was wrong. A parsed JSON value carries no such
//! thing in either backend, so `JsonError.offset` is exact for a *parse*
//! error and `0` for a *shape* mismatch — the message names the field and
//! the expected type, which is the granularity that actually diagnoses.
//! Fabricating an offset would be worse than admitting there isn't one.

pub mod dom;

use std::cell::RefCell;

use super::ffi::with_heap;
use super::gc::{GcHeap, RuntimeRoots};
use super::read::read_str_arg;
use dom::{Json, Kind};

struct JsonState {
    // Never read directly — every node handle in play is a raw pointer
    // into this boxed value, so its only job is to keep the document (and
    // everything it owns) alive until `frog_json_close` drops it.
    #[allow(dead_code)]
    root: Box<Json>,
    err:  Option<(String, usize)>,
}

thread_local! {
    static JSON_STATE: RefCell<Option<JsonState>> = const { RefCell::new(None) };
}

/// Record a shape mismatch. Unlike `read.rs`'s `mark_failed_at` there is
/// no position to resolve (see the module doc), so the offset is always
/// `0`; only the *first* mismatch is kept.
fn mark_failed(msg: impl Into<String>) {
    JSON_STATE.with(|s| {
        let mut s = s.borrow_mut();
        if let Some(state) = s.as_mut() {
            if state.err.is_none() {
                state.err = Some((msg.into(), 0));
            }
        }
    });
}

fn with_node<R>(node: i64, f: impl FnOnce(&Json) -> R) -> R {
    let ptr = node as *const Json;
    f(unsafe { &*ptr })
}

fn node_handle(v: &Json) -> i64 {
    v as *const Json as i64
}

/// The document `frog_json_open` falls back to when parsing fails, so
/// every accessor still has a valid node to be handed. `read.rs` uses a
/// `none` literal for the same purpose.
fn null_doc() -> Json {
    dom::parse("null").expect("`null` is valid JSON")
}

// ── serialize leaves ─────────────────────────────────────────────────────────
//
// `json.to_str` is a fragment swap on `repr`'s typed-AST walk
// (`TypeChecker::build_json`), so most of its leaves are `repr`'s own —
// `__repr_int` and `__repr_bool` already emit JSON-legal text. Only these
// two differ, and both differ for a reason that is about JSON the format,
// not about performance: frog notation and JSON have genuinely different
// escape tables, and JSON has no spelling for a non-finite float.

/// `__json_str` — a `Str` as a JSON string literal, quotes included.
/// `frog_str_repr`'s Tier-2 twin (`DATA.md`'s "the central decision": the
/// two tiers must not share an escape table).
#[no_mangle]
pub extern "C" fn frog_json_escape(s: i64) -> i64 {
    let _jit_frame = crate::jit_frame_guard!();
    let _roots = RuntimeRoots::hold(&[s]);
    let escaped = dom::escape(&read_str_arg(s));
    with_heap(|heap: &mut GcHeap| {
        heap.maybe_collect();
        heap.alloc_str(escaped.as_bytes()) as i64
    })
}

/// `__json_float` — a `Float` as a JSON number, or `null` if it is not
/// finite. `frog_float_repr`'s Tier-2 twin; see [`dom::float`] for why
/// `null` rather than an error.
#[no_mangle]
pub extern "C" fn frog_json_float_repr(f: f64) -> i64 {
    let _jit_frame = crate::jit_frame_guard!();
    let s = dom::float(f);
    with_heap(|heap: &mut GcHeap| {
        heap.maybe_collect();
        heap.alloc_str(s.as_bytes()) as i64
    })
}

// ── entry / exit ────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn frog_json_open(s: i64) -> i64 {
    let _jit_frame = crate::jit_frame_guard!();
    let _roots = RuntimeRoots::hold(&[s]);
    let src = read_str_arg(s);

    let (root, err) = match dom::parse(&src) {
        Ok(v) => (Box::new(v), None),
        Err(e) => (Box::new(null_doc()), Some(e)),
    };

    let handle = root.as_ref() as *const Json as i64;
    JSON_STATE.with(|s| *s.borrow_mut() = Some(JsonState { root, err }));
    handle
}

#[no_mangle]
pub extern "C" fn frog_json_close() {
    JSON_STATE.with(|s| *s.borrow_mut() = None);
}

#[no_mangle]
pub extern "C" fn frog_json_failed() -> i8 {
    JSON_STATE.with(|s| s.borrow().as_ref().and_then(|st| st.err.as_ref()).is_some() as i8)
}

#[no_mangle]
pub extern "C" fn frog_json_offset() -> i64 {
    JSON_STATE.with(|st| st.borrow().as_ref().and_then(|s| s.err.as_ref()).map(|(_, o)| *o as i64).unwrap_or(0))
}

/// The empty string if, somehow, this is called with no recorded failure
/// (should not happen: the caller always checks `frog_json_failed` first)
/// — kept total rather than panicking, exactly like `frog_read_msg`.
#[no_mangle]
pub extern "C" fn frog_json_msg() -> i64 {
    let _jit_frame = crate::jit_frame_guard!();
    let msg = JSON_STATE.with(|st| st.borrow().as_ref().and_then(|s| s.err.clone()).map(|(m, _)| m))
        .unwrap_or_default();
    with_heap(|heap: &mut GcHeap| {
        heap.maybe_collect();
        heap.alloc_str(msg.as_bytes()) as i64
    })
}

// ── kind predicates ──────────────────────────────────────────────────────────

/// One `i8` boolean predicate per JSON kind, for the same reason
/// `read.rs`'s `read_is_kind!` exists rather than a multi-valued
/// classifier: froglang has no integer-enum type to receive a tag as, and
/// `cl_type(Type::Bool)` is already `I8`, matching this ABI exactly, so
/// each predicate slots straight into a `Conditional`'s `cond`.
///
/// **None of these mark failure** — they are questions, not assertions.
/// `build_read_json`'s union arms ask them in turn and only the final
/// "nothing matched" fallback (`frog_json_expect`) marks.
macro_rules! json_is_kind {
    ($name:ident, $kind:expr) => {
        #[no_mangle]
        pub extern "C" fn $name(node: i64) -> i8 {
            with_node(node, |n| (dom::kind(n) == $kind) as i8)
        }
    };
}
json_is_kind!(frog_json_is_int, Kind::Int);
json_is_kind!(frog_json_is_float, Kind::Float);
json_is_kind!(frog_json_is_bool, Kind::Bool);
json_is_kind!(frog_json_is_str, Kind::Str);
json_is_kind!(frog_json_is_null, Kind::Null);
json_is_kind!(frog_json_is_array, Kind::Array);
json_is_kind!(frog_json_is_object, Kind::Object);

/// "Is this *any* number?" — the predicate a `Float` inside an anonymous
/// union dispatches on. `frog_json_is_float` would be wrong there: it
/// answers whether the producer wrote a `.`, and `frog_json_float`
/// deliberately accepts `1` for a `Float` (see its comment), so a
/// `Float`-guarded arm has to admit every spelling that its own leaf
/// admits — otherwise `{"w":1}` parses as a `Float` field and fails as a
/// `Float?` one. `Int` keeps the exact `frog_json_is_int`, matching
/// `frog_json_int`'s deliberate strictness about `1.0`; a union can never
/// hold both (`check_json_union_readable` rejects `Int | Float`), so the
/// two rules cannot collide.
#[no_mangle]
pub extern "C" fn frog_json_is_number(node: i64) -> i8 {
    with_node(node, |n| matches!(dom::kind(n), Kind::Int | Kind::Float) as i8)
}

/// Whether `node` is an object carrying `key` — the other non-marking
/// probe, and the one JSON needs that `read` does not. `DATA.md` stage 8's
/// null-vs-missing rule (absent *or* `null` yields `none`, but only for a
/// field whose type admits `None`) has to *ask* before committing, so this
/// cannot be the marking `frog_json_get`.
///
/// Also the nominal-union dispatch test: external tagging means "is this
/// the `Circle` variant?" is exactly "does this object have a `Circle`
/// key?".
#[no_mangle]
pub extern "C" fn frog_json_has(node: i64, key: i64) -> i8 {
    let key = read_str_arg(key);
    with_node(node, |n| dom::get(n, &key).is_some() as i8)
}

// ── leaves ───────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn frog_json_int(node: i64) -> i64 {
    with_node(node, |n| match dom::as_i64(n) {
        Some(v) => v,
        // Deliberately strict about `1.0`: `dom::kind` calls a number
        // written with a `.` or an exponent a `Float`, and an `Int` slot
        // accepting it would contradict the `Int | Float` dispatch that
        // reads the same classification.
        None => { mark_failed("expected an Int"); 0 }
    })
}

#[no_mangle]
pub extern "C" fn frog_json_float(node: i64) -> f64 {
    // An integer where a `Float` is expected *is* accepted (`as_f64`
    // converts): JSON has one number type, so `1` is the only spelling a
    // whole-valued float has on the wire, and rejecting it would make
    // `json.to_str`/`json.parse` fail to round-trip through any other
    // JSON producer.
    with_node(node, |n| match dom::as_f64(n) {
        Some(v) => v,
        None => { mark_failed("expected a Float"); 0.0 }
    })
}

#[no_mangle]
pub extern "C" fn frog_json_bool(node: i64) -> i8 {
    with_node(node, |n| match dom::as_bool(n) {
        Some(v) => v as i8,
        None => { mark_failed("expected a Bool"); 0 }
    })
}

#[no_mangle]
pub extern "C" fn frog_json_str(node: i64) -> i64 {
    // ⚠ The `jit_frame_guard!` is load-bearing and its absence is silent:
    // this is the one accessor here that allocates *and* is reachable from
    // a loop (`List<Str>`). `frog_read_str` shipped without it in stage 5
    // and aliased list elements under `FROG_GC_STRESS=1`.
    let _jit_frame = crate::jit_frame_guard!();
    let s: Option<String> = with_node(node, |n| dom::as_str(n).map(str::to_string));
    let s = s.unwrap_or_else(|| { mark_failed("expected a Str"); String::new() });
    with_heap(|heap: &mut GcHeap| {
        heap.maybe_collect();
        heap.alloc_str(s.as_bytes()) as i64
    })
}

// ── navigation ───────────────────────────────────────────────────────────────

/// `node`'s `key` member, or `node` itself, unchanged, on any mismatch —
/// `frog_read_field`'s contract exactly, so further navigation off a
/// failed node fails the same harmless way rather than dereferencing
/// something invalid.
#[no_mangle]
pub extern "C" fn frog_json_get(node: i64, key: i64) -> i64 {
    let key = read_str_arg(key);
    with_node(node, |n| {
        if dom::kind(n) != Kind::Object {
            mark_failed(format!("expected an object while looking for key '{}'", key));
            return node;
        }
        match dom::get(n, &key) {
            Some(v) => node_handle(v),
            None => { mark_failed(format!("missing key '{}'", key)); node }
        }
    })
}

/// `node`'s `i`th array element, or `node` itself on any mismatch.
#[no_mangle]
pub extern "C" fn frog_json_at(node: i64, i: i64) -> i64 {
    with_node(node, |n| {
        if dom::kind(n) != Kind::Array {
            mark_failed("expected an array");
            return node;
        }
        match dom::at(n, i as usize) {
            Some(v) => node_handle(v),
            None => { mark_failed(format!("missing array element {}", i)); node }
        }
    })
}

#[no_mangle]
pub extern "C" fn frog_json_len(node: i64) -> i64 {
    with_node(node, |n| match dom::len(n) {
        Some(l) => l as i64,
        None => { mark_failed("expected an array"); 0 }
    })
}

/// `Dict<Str, V>` parsing's entry points — `frog_json_at`/`frog_json_len`'s
/// object counterparts. `dom::entry_at`'s doc comment covers why the order
/// isn't the original document's.
#[no_mangle]
pub extern "C" fn frog_json_obj_len(node: i64) -> i64 {
    with_node(node, |n| match dom::obj_len(n) {
        Some(l) => l as i64,
        None => { mark_failed("expected an object"); 0 }
    })
}

/// `node`'s `i`th member's key, as a fresh `Str`. `frog_json_str`'s twin —
/// see its doc comment for why `jit_frame_guard!` is load-bearing here too.
#[no_mangle]
pub extern "C" fn frog_json_key_at(node: i64, i: i64) -> i64 {
    let _jit_frame = crate::jit_frame_guard!();
    let k: Option<String> = with_node(node, |n| dom::entry_at(n, i as usize).map(|(k, _)| k.to_string()));
    let k = k.unwrap_or_else(|| { mark_failed(format!("missing object member {}", i)); String::new() });
    with_heap(|heap: &mut GcHeap| {
        heap.maybe_collect();
        heap.alloc_str(k.as_bytes()) as i64
    })
}

/// `node`'s `i`th member's value, or `node` itself on any mismatch.
#[no_mangle]
pub extern "C" fn frog_json_val_at(node: i64, i: i64) -> i64 {
    with_node(node, |n| {
        if dom::kind(n) != Kind::Object {
            mark_failed("expected an object");
            return node;
        }
        match dom::entry_at(n, i as usize) {
            Some((_, v)) => node_handle(v),
            None => { mark_failed(format!("missing object member {}", i)); node }
        }
    })
}

/// Unconditionally marks failure with `what` and returns `node` unchanged
/// — the union arms' "no alternative matched" fallback, reached only after
/// every kind/tag test has already failed. `frog_read_expect`'s twin.
#[no_mangle]
pub extern "C" fn frog_json_expect(node: i64, what: i64) -> i64 {
    let what = read_str_arg(what);
    mark_failed(what);
    node
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::gc::FrogStr;

    fn alloc(s: &str) -> i64 {
        with_heap(|h: &mut GcHeap| h.alloc_str(s.as_bytes()) as i64)
    }

    fn open(s: &str) -> i64 {
        frog_json_open(alloc(s))
    }

    fn str_of(handle: i64) -> String {
        let ptr = handle as *const FrogStr;
        unsafe {
            let len = (*ptr).len as usize;
            let data = (ptr as *const u8).add(std::mem::size_of::<FrogStr>());
            String::from_utf8_lossy(std::slice::from_raw_parts(data, len)).into_owned()
        }
    }

    #[test]
    fn reads_scalars() {
        let n = open("42");
        assert_eq!(frog_json_int(n), 42);
        assert_eq!(frog_json_failed(), 0);
        frog_json_close();

        let n = open("-7");
        assert_eq!(frog_json_int(n), -7);
        frog_json_close();

        let n = open("1.5");
        assert_eq!(frog_json_float(n), 1.5);
        assert_eq!(frog_json_failed(), 0);
        frog_json_close();

        let n = open("true");
        assert_eq!(frog_json_bool(n), 1);
        frog_json_close();

        let n = open("\"hi\\n\"");
        assert_eq!(str_of(frog_json_str(n)), "hi\n");
        assert_eq!(frog_json_failed(), 0);
        frog_json_close();
    }

    #[test]
    fn an_integer_is_accepted_where_a_float_is_expected_but_not_the_reverse() {
        // JSON has one number type, so `1` is the only spelling a whole
        // float has on the wire; the other direction stays strict so the
        // `Int`/`Float` kind dispatch and the leaves agree.
        let n = open("1");
        assert_eq!(frog_json_float(n), 1.0);
        assert_eq!(frog_json_failed(), 0);
        frog_json_close();

        let n = open("1.0");
        frog_json_int(n);
        assert_eq!(frog_json_failed(), 1);
        frog_json_close();
    }

    #[test]
    fn navigates_objects_and_arrays() {
        let n = open(r#"{"name":"Alice","age":42}"#);
        assert_eq!(str_of(frog_json_str(frog_json_get(n, alloc("name")))), "Alice");
        assert_eq!(frog_json_int(frog_json_get(n, alloc("age"))), 42);
        assert_eq!(frog_json_failed(), 0);
        frog_json_close();

        let n = open("[1, 2, 3]");
        assert_eq!(frog_json_len(n), 3);
        assert_eq!(frog_json_int(frog_json_at(n, 1)), 2);
        assert_eq!(frog_json_failed(), 0);
        frog_json_close();
    }

    #[test]
    fn key_order_does_not_matter() {
        let n = open(r#"{"age":42,"name":"Alice"}"#);
        assert_eq!(str_of(frog_json_str(frog_json_get(n, alloc("name")))), "Alice");
        assert_eq!(frog_json_int(frog_json_get(n, alloc("age"))), 42);
        assert_eq!(frog_json_failed(), 0);
        frog_json_close();
    }

    #[test]
    fn has_and_is_null_probe_without_marking() {
        // The null-vs-missing rule has to ask before committing, so these
        // two must be the only accessors that see a mismatch and stay
        // quiet about it.
        let n = open(r#"{"a":null}"#);
        assert_eq!(frog_json_has(n, alloc("a")), 1);
        assert_eq!(frog_json_has(n, alloc("b")), 0);
        assert_eq!(frog_json_is_null(frog_json_get(n, alloc("a"))), 1);
        assert_eq!(frog_json_failed(), 0);
        frog_json_close();
    }

    #[test]
    fn kind_predicates_classify_and_stay_quiet() {
        let n = open(r#"[1, 1.5, true, "s", null, [], {}]"#);
        let at = |i| frog_json_at(n, i);
        assert_eq!(frog_json_is_int(at(0)), 1);
        assert_eq!(frog_json_is_float(at(1)), 1);
        assert_eq!(frog_json_is_bool(at(2)), 1);
        assert_eq!(frog_json_is_str(at(3)), 1);
        assert_eq!(frog_json_is_null(at(4)), 1);
        assert_eq!(frog_json_is_array(at(5)), 1);
        assert_eq!(frog_json_is_object(at(6)), 1);
        assert_eq!(frog_json_is_int(at(1)), 0);
        // `is_number` spans both spellings — it is what a `Float` arm of a
        // union dispatches on, so it has to admit everything
        // `frog_json_float` admits, and nothing else.
        assert_eq!(frog_json_is_number(at(0)), 1);
        assert_eq!(frog_json_is_number(at(1)), 1);
        assert_eq!(frog_json_is_number(at(2)), 0);
        assert_eq!(frog_json_is_number(at(3)), 0);
        assert_eq!(frog_json_is_number(at(4)), 0);
        assert_eq!(frog_json_failed(), 0);
        frog_json_close();
    }

    #[test]
    fn a_shape_mismatch_sticks_to_the_first_error_and_returns_a_usable_node() {
        let n = open("42");
        // Not an object: `get` fails, but hands back a still-valid node.
        let field = frog_json_get(n, alloc("x"));
        assert_eq!(frog_json_failed(), 1);
        assert_eq!(str_of(frog_json_msg()), "expected an object while looking for key 'x'");
        // Reading it as an int still works (it's the same, valid node).
        assert_eq!(frog_json_int(field), 42);
        // ...and the *first* message is the one kept.
        assert_eq!(str_of(frog_json_msg()), "expected an object while looking for key 'x'");
        frog_json_close();
    }

    #[test]
    fn a_parse_error_is_reported_with_an_offset() {
        let n = open(r#"{"a": }"#);
        assert_eq!(frog_json_failed(), 1);
        assert_eq!(frog_json_offset(), 6);
        // The fallback document is still navigable, per the sticky-error
        // contract — the happy path runs to completion and is discarded.
        assert_eq!(frog_json_is_null(n), 1);
        frog_json_close();
    }

    #[test]
    fn expect_marks_unconditionally() {
        let n = open("1");
        assert_eq!(frog_json_expect(n, alloc("expected Shape")), n);
        assert_eq!(frog_json_failed(), 1);
        assert_eq!(str_of(frog_json_msg()), "expected Shape");
        frog_json_close();
    }
}
