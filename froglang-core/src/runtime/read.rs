//! `read(s): T` (`plans/DATA.md` stage 5) — the runtime half.
//!
//! `repr`'s output is frog source by definition (`notation.rs`'s
//! discipline, extended one level: DATA.md's own "why the notation is a
//! tree" reasoning), so the authority on reading it back is the authority
//! on reading frog — `frontend::parser::Parser`, not a bespoke tokenizer.
//! `frog_read_open` parses `s` once into an ordinary `Expression` tree and
//! stashes it in thread-local storage; every other `frog_read_*` function
//! below is a handle-in/handle-or-scalar-out accessor over that tree, and
//! `TypeChecker::build_read` (`typeck.rs`, `read`'s desugar, mirroring
//! `build_repr`) is what strings them together per type.
//!
//! **Sticky-error-and-continue.** A shape mismatch (the node isn't the
//! `Call`/`List`/`Range`/literal kind an accessor expected, a named field
//! or positional arg is missing, an index is out of range) records the
//! *first* `(message, byte offset)` seen — later mismatches are silently
//! ignored — and returns some always-valid placeholder instead of
//! panicking or needing an out-of-band signal: a navigating accessor
//! (`field`/`arg`/`list_at`/`range_lo`/`range_hi`) returns the *same node
//! it was given* (so further navigation off of it fails the same harmless
//! way rather than dereferencing something invalid), and a leaf accessor
//! (`int`/`float`/`bool`/`str`) returns a zero-ish value of its own type.
//! This is what lets `build_read` synthesize one unconditional "happy
//! path" expression per type, with no early-exit control flow of its own
//! threaded through the recursion — the desugar looks exactly as much like
//! `build_repr` as the direction of data flow allows, and the *caller*
//! (the top-level `read(s): T | ReadError` expansion) checks
//! `frog_read_failed()` exactly once, after the whole tree has been built,
//! discarding whatever garbage the happy path produced if it has.

use std::cell::RefCell;

use super::gc::{FrogStr, GcHeap, RuntimeRoots};
use super::ffi::with_heap;
use crate::frontend::expression::Expression;
use crate::frontend::parser::Parser;
use crate::frontend::tokens::{Position, Spanned};

struct ReadState {
    // Never read directly — every node handle in play is a raw pointer
    // into this boxed tree, so its only job is to keep the allocation (and
    // everything it owns) alive until `frog_read_close` drops it.
    #[allow(dead_code)]
    root: Box<Spanned<Expression>>,
    // The text `root` was parsed from, kept for `mark_failed_at`: a node
    // carries a `Position` (line/col), and `ReadError.offset` is a byte
    // offset, so reporting *where* a shape mismatch happened needs the
    // source the positions index into. Parse errors compute theirs in
    // `frog_read_open`, before this state exists.
    src:  String,
    err:  Option<(String, usize)>,
}

thread_local! {
    static READ_STATE: RefCell<Option<ReadState>> = const { RefCell::new(None) };
}

fn none_node(pos: Position) -> Spanned<Expression> {
    use crate::frontend::expression::LiteralExpr;
    use crate::frontend::tokens::Token;
    Spanned::new(Expression::Literal(LiteralExpr { token: Token::None }), pos, pos)
}

/// Approximate byte offset of `pos` within `src` — full line lengths (plus
/// one byte per newline) up to `pos.line`, then `pos.col` more. Exact iff
/// every line is ASCII and `Position::col` counts bytes the same way this
/// does; `ReadError.offset` is a diagnostic aid; the law does not depend on
/// its precision.
fn byte_offset(src: &str, pos: Position) -> usize {
    let mut offset = 0usize;
    for (i, line) in src.split('\n').enumerate() {
        if i as u32 == pos.line {
            return offset + pos.col as usize;
        }
        offset += line.len() + 1;
    }
    offset
}

/// Record a shape mismatch at `pos` — the position of the node the
/// accessor was actually handed, which is the closest thing to "where the
/// input went wrong" available at this point: an accessor knows what it
/// expected and what it got, and the node it got is the part of the input
/// to point at. The offset is resolved here rather than by the caller so
/// every call site is just "this node, this message"; only the *first*
/// mismatch is kept (the module doc's sticky-error contract), so the
/// resolution is done at most once per `read`.
fn mark_failed_at(msg: impl Into<String>, pos: Position) {
    READ_STATE.with(|s| {
        let mut s = s.borrow_mut();
        if let Some(state) = s.as_mut() {
            if state.err.is_none() {
                let offset = byte_offset(&state.src, pos);
                state.err = Some((msg.into(), offset));
            }
        }
    });
}

fn with_node<R>(node: i64, f: impl FnOnce(&Spanned<Expression>) -> R) -> R {
    let ptr = node as *const Spanned<Expression>;
    f(unsafe { &*ptr })
}

fn node_handle(e: &Spanned<Expression>) -> i64 {
    e as *const Spanned<Expression> as i64
}

/// A `FrogStr*` argument as an owned Rust `String`. `pub(super)` because
/// `runtime::json`'s accessors take their key/name arguments exactly the
/// same way — a `StrLit` synthesized by the desugar — and there is no
/// reason for two copies of this.
pub(super) fn read_str_arg(s: i64) -> String {
    let ptr = s as *const FrogStr;
    unsafe {
        let len = (*ptr).len as usize;
        let data = (ptr as *const u8).add(std::mem::size_of::<FrogStr>());
        String::from_utf8_lossy(std::slice::from_raw_parts(data, len)).into_owned()
    }
}

// ── entry / exit ────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn frog_read_open(s: i64) -> i64 {
    let _jit_frame = crate::jit_frame_guard!();
    let _roots = RuntimeRoots::hold(&[s]);
    let src = read_str_arg(s);

    // `parse_data`, not `parse`: this is frog *notation*, where `${` in a
    // string is two ordinary characters rather than an interpolation. Read
    // data is not code, and must not be executed as any.
    let (root, err) = match Parser::parse_data(&src) {
        Ok(Spanned { item: Expression::Block(mut stmts), .. }) if stmts.len() == 1 => {
            (Box::new(stmts.pop().expect("len checked above")), None)
        }
        Ok(other) => {
            // `repr`'s output is always exactly one statement — a
            // zero- or multi-statement parse is malformed input, not a
            // parse error the lexer/parser itself would report.
            let pos = other.span.start;
            (Box::new(none_node(pos)), Some(("expected exactly one expression".to_string(), byte_offset(&src, pos))))
        }
        Err(errors) => {
            let first = errors.into_iter().next().expect("Err always carries at least one error");
            let pos = first.span.start;
            (Box::new(none_node(pos)), Some((first.item.to_string(), byte_offset(&src, pos))))
        }
    };

    let handle = root.as_ref() as *const Spanned<Expression> as i64;
    READ_STATE.with(|s| *s.borrow_mut() = Some(ReadState { root, src, err }));
    handle
}

#[no_mangle]
pub extern "C" fn frog_read_close() {
    READ_STATE.with(|s| *s.borrow_mut() = None);
}

#[no_mangle]
pub extern "C" fn frog_read_failed() -> i8 {
    READ_STATE.with(|s| s.borrow().as_ref().and_then(|st| st.err.as_ref()).is_some() as i8)
}

#[no_mangle]
pub extern "C" fn frog_read_offset() -> i64 {
    READ_STATE.with(|st| st.borrow().as_ref().and_then(|s| s.err.as_ref()).map(|(_, o)| *o as i64).unwrap_or(0))
}

/// The empty string if, somehow, this is called with no recorded failure
/// (should not happen: the caller always checks `frog_read_failed` first)
/// — kept total rather than panicking.
#[no_mangle]
pub extern "C" fn frog_read_msg() -> i64 {
    let _jit_frame = crate::jit_frame_guard!();
    let msg = READ_STATE.with(|st| st.borrow().as_ref().and_then(|s| s.err.clone()).map(|(m, _)| m))
        .unwrap_or_default();
    with_heap(|heap: &mut GcHeap| {
        heap.maybe_collect();
        heap.alloc_str(msg.as_bytes()) as i64
    })
}

// ── navigation ───────────────────────────────────────────────────────────────

#[derive(PartialEq, Eq, Clone, Copy)]
enum Kind { Int, Float, Bool, Str, None, List, Dict, Range, Call, Other }

fn expression_kind(e: &Expression) -> Kind {
    use crate::frontend::expression::LiteralExpr;
    use crate::frontend::tokens::Token;
    match e {
        Expression::Literal(LiteralExpr { token: Token::Int(_) }) => Kind::Int,
        Expression::Literal(LiteralExpr { token: Token::Float(_) | Token::Inf | Token::Nan }) => Kind::Float,
        Expression::Literal(LiteralExpr { token: Token::True | Token::False }) => Kind::Bool,
        Expression::Literal(LiteralExpr { token: Token::String(_) }) => Kind::Str,
        Expression::Literal(LiteralExpr { token: Token::None }) => Kind::None,
        Expression::Unary(u) if u.op == Token::Minus => expression_kind(&u.expr.item),
        Expression::Tuple(_) => Kind::List,
        Expression::DictLit(_) => Kind::Dict,
        Expression::Range(_) => Kind::Range,
        Expression::Call(_) | Expression::FieldAccess(_) => Kind::Call,
        _ => Kind::Other,
    }
}

/// One `i8` boolean predicate per syntactic kind, rather than a single
/// `frog_read_kind(node) -> i8` classifier: froglang has no integer-enum
/// type to receive a multi-valued tag as, and every `cl_type(Type::Bool)`
/// is already `I8` — matching this function's real ABI exactly — so each
/// predicate slots directly into a `Conditional`'s `cond` with no
/// intermediate comparison node needed. Used only by `build_read`'s
/// anonymous-union arm, which tests each member's expected kind in turn.
macro_rules! read_is_kind {
    ($name:ident, $kind:expr) => {
        #[no_mangle]
        pub extern "C" fn $name(node: i64) -> i8 {
            with_node(node, |n| (expression_kind(&n.item) == $kind) as i8)
        }
    };
}
read_is_kind!(frog_read_is_int, Kind::Int);
read_is_kind!(frog_read_is_float, Kind::Float);
read_is_kind!(frog_read_is_bool, Kind::Bool);
read_is_kind!(frog_read_is_str, Kind::Str);
read_is_kind!(frog_read_is_none, Kind::None);
read_is_kind!(frog_read_is_list, Kind::List);
read_is_kind!(frog_read_is_dict, Kind::Dict);
read_is_kind!(frog_read_is_range, Kind::Range);
read_is_kind!(frog_read_is_struct, Kind::Call);

#[no_mangle]
pub extern "C" fn frog_read_int(node: i64) -> i64 {
    use crate::frontend::expression::LiteralExpr;
    use crate::frontend::tokens::Token;
    with_node(node, |n| match &n.item {
        Expression::Literal(LiteralExpr { token: Token::Int(v) }) => *v,
        Expression::Unary(u) if u.op == Token::Minus => -frog_read_int(node_handle(&u.expr)),
        _ => { mark_failed_at("expected an Int literal", n.span.start); 0 }
    })
}

#[no_mangle]
pub extern "C" fn frog_read_float(node: i64) -> f64 {
    use crate::frontend::expression::LiteralExpr;
    use crate::frontend::tokens::Token;
    with_node(node, |n| match &n.item {
        Expression::Literal(LiteralExpr { token: Token::Float(v) }) => *v,
        Expression::Literal(LiteralExpr { token: Token::Inf }) => f64::INFINITY,
        Expression::Literal(LiteralExpr { token: Token::Nan }) => f64::NAN,
        Expression::Unary(u) if u.op == Token::Minus => -frog_read_float(node_handle(&u.expr)),
        _ => { mark_failed_at("expected a Float literal", n.span.start); 0.0 }
    })
}

#[no_mangle]
pub extern "C" fn frog_read_bool(node: i64) -> i8 {
    use crate::frontend::expression::LiteralExpr;
    use crate::frontend::tokens::Token;
    with_node(node, |n| match &n.item {
        Expression::Literal(LiteralExpr { token: Token::True }) => 1,
        Expression::Literal(LiteralExpr { token: Token::False }) => 0,
        _ => { mark_failed_at("expected a Bool literal", n.span.start); 0 }
    })
}

#[no_mangle]
pub extern "C" fn frog_read_str(node: i64) -> i64 {
    let _jit_frame = crate::jit_frame_guard!();
    use crate::frontend::expression::LiteralExpr;
    use crate::frontend::tokens::Token;
    // `Err` carries the mismatched node's position rather than `()`: the
    // allocation below has to happen outside `with_node`, so the position
    // is the one thing the failure path needs to bring out with it.
    let s: Result<String, Position> = with_node(node, |n| match &n.item {
        Expression::Literal(LiteralExpr { token: Token::String(v) }) => Ok(v.clone()),
        _ => Err(n.span.start),
    });
    let s = s.unwrap_or_else(|pos| { mark_failed_at("expected a Str literal", pos); String::new() });
    with_heap(|heap: &mut GcHeap| {
        heap.maybe_collect();
        heap.alloc_str(s.as_bytes()) as i64
    })
}

/// `node`'s `name`-labelled keyword argument (`Person(name="Alice", ...)`),
/// or `node` itself, unchanged, on any mismatch — see the module doc's
/// sticky-error contract. `name` is an ordinary `FrogStr` (a `StrLit` node
/// in `build_read`'s synthesized tree), not a static byte fragment: the
/// field name is only known at `desugar_notation` time, which runs once
/// per compile, so there is no meaningful "bake it into the JIT module"
/// version of this the way `print`'s punctuation fragments have.
#[no_mangle]
pub extern "C" fn frog_read_field(node: i64, name: i64) -> i64 {
    // No `RuntimeRoots::hold` needed: nothing below can trigger a
    // collection (`mark_failed` builds a Rust `String`, not a `FrogStr`).
    let name = read_str_arg(name);
    with_node(node, |n| {
        let Expression::Call(c) = &n.item else {
            mark_failed_at(format!("expected a call while looking for field '{}'", name), n.span.start);
            return node;
        };
        for a in &c.args {
            if let Expression::Assign(asn) = &a.item {
                if asn.target.item.get_identifier() == Some(name.as_str()) {
                    return node_handle(&asn.value);
                }
            }
        }
        mark_failed_at(format!("missing field '{}'", name), n.span.start);
        node
    })
}

/// `node`'s `i`th positional argument, or `node` itself on any mismatch.
#[no_mangle]
pub extern "C" fn frog_read_arg(node: i64, i: i64) -> i64 {
    with_node(node, |n| {
        let Expression::Call(c) = &n.item else {
            mark_failed_at("expected a call", n.span.start);
            return node;
        };
        match c.args.get(i as usize) {
            Some(a) => node_handle(a),
            None => { mark_failed_at(format!("missing positional argument {}", i), n.span.start); node }
        }
    })
}

/// Whether `node` is a call to `variant`, bare (`Variant(...)`) or
/// qualified (`Enum.Variant(...)`) — either spelling matches regardless of
/// which one `enum_name` names, since `repr` always emits the qualified
/// form but a hand-written `.frogdata` file is not required to.
#[no_mangle]
pub extern "C" fn frog_read_is_call(node: i64, variant: i64) -> i8 {
    let variant = read_str_arg(variant);
    with_node(node, |n| {
        let Expression::Call(c) = &n.item else { return 0 };
        let matched = match &c.callable.item {
            Expression::Literal(_) => c.callable.item.get_identifier() == Some(variant.as_str()),
            Expression::FieldAccess(fa) => fa.field == variant,
            _ => false,
        };
        matched as i8
    })
}

#[no_mangle]
pub extern "C" fn frog_read_list_len(node: i64) -> i64 {
    with_node(node, |n| match &n.item {
        Expression::Tuple(elems) => elems.len() as i64,
        _ => { mark_failed_at("expected a List literal", n.span.start); 0 }
    })
}

/// `node`'s `i`th list element, or `node` itself on any mismatch.
#[no_mangle]
pub extern "C" fn frog_read_list_at(node: i64, i: i64) -> i64 {
    with_node(node, |n| {
        let Expression::Tuple(elems) = &n.item else {
            mark_failed_at("expected a List literal", n.span.start);
            return node;
        };
        match elems.get(i as usize) {
            Some(e) => node_handle(e),
            None => { mark_failed_at("list index out of range", n.span.start); node }
        }
    })
}

#[no_mangle]
pub extern "C" fn frog_read_dict_len(node: i64) -> i64 {
    with_node(node, |n| match &n.item {
        Expression::DictLit(pairs) => pairs.len() as i64,
        _ => { mark_failed_at("expected a Dict literal", n.span.start); 0 }
    })
}

/// `node`'s `i`th pair's key, or `node` itself on any mismatch.
#[no_mangle]
pub extern "C" fn frog_read_dict_key_at(node: i64, i: i64) -> i64 {
    with_node(node, |n| {
        let Expression::DictLit(pairs) = &n.item else {
            mark_failed_at("expected a Dict literal", n.span.start);
            return node;
        };
        match pairs.get(i as usize) {
            Some((k, _)) => node_handle(k),
            None => { mark_failed_at("dict index out of range", n.span.start); node }
        }
    })
}

/// `node`'s `i`th pair's value, or `node` itself on any mismatch.
#[no_mangle]
pub extern "C" fn frog_read_dict_val_at(node: i64, i: i64) -> i64 {
    with_node(node, |n| {
        let Expression::DictLit(pairs) = &n.item else {
            mark_failed_at("expected a Dict literal", n.span.start);
            return node;
        };
        match pairs.get(i as usize) {
            Some((_, v)) => node_handle(v),
            None => { mark_failed_at("dict index out of range", n.span.start); node }
        }
    })
}

#[no_mangle]
pub extern "C" fn frog_read_range_lo(node: i64) -> i64 {
    with_node(node, |n| match &n.item {
        Expression::Range(r) => node_handle(&r.start),
        _ => { mark_failed_at("expected a Range literal", n.span.start); node }
    })
}

#[no_mangle]
pub extern "C" fn frog_read_range_hi(node: i64) -> i64 {
    with_node(node, |n| match &n.item {
        Expression::Range(r) => node_handle(&r.end),
        _ => { mark_failed_at("expected a Range literal", n.span.start); node }
    })
}

/// Unconditionally marks failure with `what` and returns `node` unchanged
/// — `build_read`'s union arms' final "no alternative matched" fallback,
/// reached only after every named variant/kind test has already failed.
#[no_mangle]
pub extern "C" fn frog_read_expect(node: i64, what: i64) -> i64 {
    let what = read_str_arg(what);
    with_node(node, |n| mark_failed_at(what, n.span.start));
    node
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alloc(s: &str) -> i64 {
        with_heap(|h: &mut GcHeap| h.alloc_str(s.as_bytes()) as i64)
    }

    fn open(s: &str) -> i64 {
        frog_read_open(alloc(s))
    }

    #[test]
    fn reads_scalars() {
        let n = open("42");
        assert_eq!(frog_read_int(n), 42);
        assert_eq!(frog_read_failed(), 0);
        frog_read_close();

        let n = open("-7");
        assert_eq!(frog_read_int(n), -7);
        assert_eq!(frog_read_failed(), 0);
        frog_read_close();

        let n = open("1.5");
        assert_eq!(frog_read_float(n), 1.5);
        frog_read_close();

        let n = open("-inf");
        assert_eq!(frog_read_float(n), f64::NEG_INFINITY);
        frog_read_close();

        let n = open("true");
        assert_eq!(frog_read_bool(n), 1);
        frog_read_close();
    }

    #[test]
    fn reads_a_str_literal() {
        let n = open("\"hi\\n\"");
        let out = frog_read_str(n);
        let ptr = out as *const FrogStr;
        let s = unsafe {
            let len = (*ptr).len as usize;
            let data = (ptr as *const u8).add(std::mem::size_of::<FrogStr>());
            String::from_utf8_lossy(std::slice::from_raw_parts(data, len)).into_owned()
        };
        assert_eq!(s, "hi\n");
        assert_eq!(frog_read_failed(), 0);
        frog_read_close();
    }

    #[test]
    fn reads_named_fields_and_positional_args() {
        let n = open("Person(name=\"Alice\", age=42)");
        let name_field = frog_read_field(n, alloc("name"));
        assert_eq!(frog_read_failed(), 0);
        let s = frog_read_str(name_field);
        let ptr = s as *const FrogStr;
        assert_eq!(unsafe { (*ptr).len }, 5);
        frog_read_close();

        let n = open("Lit(42)");
        let arg0 = frog_read_arg(n, 0);
        assert_eq!(frog_read_int(arg0), 42);
        assert_eq!(frog_read_failed(), 0);
        frog_read_close();
    }

    #[test]
    fn reads_a_list_and_a_range() {
        let n = open("[1, 2, 3]");
        assert_eq!(frog_read_list_len(n), 3);
        assert_eq!(frog_read_int(frog_read_list_at(n, 1)), 2);
        assert_eq!(frog_read_failed(), 0);
        frog_read_close();

        let n = open("0..10");
        assert_eq!(frog_read_int(frog_read_range_lo(n)), 0);
        assert_eq!(frog_read_int(frog_read_range_hi(n)), 10);
        frog_read_close();
    }

    #[test]
    fn matches_qualified_and_bare_variant_names() {
        let n = open("Shape.Circle(r=4)");
        assert_eq!(frog_read_is_call(n, alloc("Circle")), 1);
        assert_eq!(frog_read_is_call(n, alloc("Rect")), 0);
        frog_read_close();

        let n = open("Circle(r=4)");
        assert_eq!(frog_read_is_call(n, alloc("Circle")), 1);
        frog_read_close();
    }

    #[test]
    fn a_shape_mismatch_sticks_to_the_first_error_and_returns_a_usable_node() {
        let n = open("42");
        // Not a call: `field` fails, but returns a still-valid node handle.
        let field = frog_read_field(n, alloc("x"));
        assert_eq!(frog_read_failed(), 1);
        // Reading it as an int still works (it's the same, valid node).
        assert_eq!(frog_read_int(field), 42);
        frog_read_close();
    }

    #[test]
    fn a_shape_mismatch_is_reported_at_the_offending_node() {
        // The mismatched node's own position, not the start of the input:
        // `"nope"` begins at byte 23.
        let n = open("Person(name=\"Ada\", age=\"nope\")");
        let age = frog_read_field(n, alloc("age"));
        assert_eq!(frog_read_failed(), 0);
        frog_read_int(age);
        assert_eq!(frog_read_failed(), 1);
        assert_eq!(frog_read_offset(), 23);
        frog_read_close();

        // A missing field points at the call that should have carried it —
        // the inner `Inner(` at byte 13, not the outer call at 0.
        let n = open("Outer(x=1, y=Inner(a=2))");
        let inner = frog_read_field(n, alloc("y"));
        frog_read_field(inner, alloc("b"));
        assert_eq!(frog_read_failed(), 1);
        assert_eq!(frog_read_offset(), 13);
        frog_read_close();
    }

    #[test]
    fn an_offset_counts_bytes_across_lines() {
        // `repr`'s own output is always one line, but a hand-written
        // `.frogdata` file need not be — `byte_offset` walks whole lines.
        let n = open("[1,\n 2,\n true]");
        frog_read_int(frog_read_list_at(n, 2));
        assert_eq!(frog_read_failed(), 1);
        assert_eq!(frog_read_offset(), 9);
        frog_read_close();
    }

    #[test]
    fn a_parse_error_is_reported_with_an_offset() {
        frog_read_open(with_heap(|h: &mut GcHeap| h.alloc_str(b"(") as i64));
        assert_eq!(frog_read_failed(), 1);
        frog_read_close();
    }
}
