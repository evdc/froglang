//! String functions — `plans/STDLIB.md` Phase 3.
//!
//! Every conversion here (`to_int`/`to_float`/`to_bool`/`int_to_str`/
//! `float_to_str`/`bool_to_str`) is a plain named `#[frog_fn]`, callable
//! either bare (`to_int(s)`) or, since UFCS's step 3 resolves `x.f(...)` to
//! any free function whose first parameter accepts `typeof(x)`
//! (`frontend/typeck.rs`'s `lower_ufcs_call`), as a method
//! (`"3".to_int()`) — with no compiler changes of any kind. The
//! alternative — a single polymorphic `Int(x)`/`Str(x)` name dispatching on
//! `x`'s type, mirroring struct-constructor call syntax — was considered
//! and set aside for now: the type system has no overloading, so that
//! shape needs the same hardcoded-builtin treatment as `len`/`push`
//! (argument-type dispatch in both `typeck.rs` and `codegen/mod.rs`), which
//! is real, permanent compiler surface for what's otherwise an ordinary
//! fixed-signature host function. Nothing here would be wasted if that's
//! revisited later — `Int(x)` could dispatch to these same functions.
//!
//! `Str` is UTF-8 and byte-indexed throughout (`len`, Phase 1, already
//! returns byte length — `runtime/gc.rs`'s `FrogStr.len`), consistently.
//! There is deliberately no `s[i]` indexing operator on `Str`: mixing a
//! byte-based `len`/`slice` with a codepoint- or grapheme-based `[]` would
//! make `s[len(s) - 1]` silently wrong on any non-ASCII input. `chars`
//! below is the character-level escape hatch, returning codepoints (not
//! grapheme clusters — full Unicode segmentation is out of scope for a
//! minimal stdlib) as a `List(Str)`.

use crate::frog_fn;
use crate::state::FrogStateBuilder;
use crate::stdlib::ErrMsg;

pub(super) fn install(builder: FrogStateBuilder) -> FrogStateBuilder {
    builder
        .func(slice_host())
        .func(split_host())
        .func(trim_host())
        .func(to_upper_host())
        .func(to_lower_host())
        .func(contains_host())
        .func(starts_with_host())
        .func(ends_with_host())
        .func(index_of_host())
        .func(chars_host())
        .func(join_host())
        .func(to_int_host())
        .func(to_float_host())
        .func(to_bool_host())
        .func(int_to_str_host())
        .func(float_to_str_host())
        .func(bool_to_str_host())
}

/// A byte-range slice, `[start, end)`. Fallible rather than panicking —
/// an out-of-range or non-char-boundary split would otherwise be a Rust
/// panic unwinding across the `extern "C"` shim boundary, which is UB (or,
/// with the default Rust 2021 `panic = "abort-on-unwind-across-extern-C"`
/// behavior, aborts the whole embedding process) rather than a catchable
/// frog error.
#[frog_fn]
fn slice(s: String, start: i64, end: i64) -> Result<String, ErrMsg> {
    if start < 0 || end < 0 {
        return Err(ErrMsg(format!("slice({}, {}): negative index", start, end)));
    }
    let (start, end) = (start as usize, end as usize);
    if start > end || end > s.len() || !s.is_char_boundary(start) || !s.is_char_boundary(end) {
        return Err(ErrMsg(format!("slice({}, {}) is out of range or splits a character", start, end)));
    }
    Ok(s[start..end].to_string())
}

#[frog_fn]
fn split(s: String, sep: String) -> Vec<String> {
    if sep.is_empty() {
        return vec![s];
    }
    s.split(sep.as_str()).map(|p| p.to_string()).collect()
}

#[frog_fn]
fn trim(s: String) -> String {
    s.trim().to_string()
}

#[frog_fn]
fn to_upper(s: String) -> String {
    s.to_uppercase()
}

#[frog_fn]
fn to_lower(s: String) -> String {
    s.to_lowercase()
}

#[frog_fn]
fn contains(s: String, pat: String) -> bool {
    s.contains(pat.as_str())
}

#[frog_fn]
fn starts_with(s: String, pat: String) -> bool {
    s.starts_with(pat.as_str())
}

#[frog_fn]
fn ends_with(s: String, pat: String) -> bool {
    s.ends_with(pat.as_str())
}

/// The byte offset of `pat`'s first occurrence, or `Err` if absent — not
/// `Int?` (`Int | None`): `Result<T, ()>` isn't supported yet (see
/// `host.rs`'s `ToFrog for Result<T, E>` doc comment, precondition 4).
#[frog_fn]
fn index_of(s: String, pat: String) -> Result<i64, ErrMsg> {
    match s.find(pat.as_str()) {
        Some(i) => Ok(i as i64),
        None => Err(ErrMsg(format!("{:?} not found", pat))),
    }
}

/// Codepoints, not grapheme clusters — see this module's doc comment.
#[frog_fn]
fn chars(s: String) -> Vec<String> {
    s.chars().map(|c| c.to_string()).collect()
}

#[frog_fn]
fn join(items: Vec<String>, sep: String) -> String {
    items.join(&sep)
}

#[frog_fn]
fn to_int(s: String) -> Result<i64, ErrMsg> {
    s.trim().parse::<i64>().map_err(|_| ErrMsg(format!("{:?} is not an Int", s)))
}

#[frog_fn]
fn to_float(s: String) -> Result<f64, ErrMsg> {
    s.trim().parse::<f64>().map_err(|_| ErrMsg(format!("{:?} is not a Float", s)))
}

#[frog_fn]
fn to_bool(s: String) -> Result<bool, ErrMsg> {
    match s.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(ErrMsg(format!("{:?} is not a Bool", s))),
    }
}

#[frog_fn]
fn int_to_str(n: i64) -> String {
    n.to_string()
}

#[frog_fn]
fn float_to_str(f: f64) -> String {
    // Frog notation (`crate::notation`), not Rust's `Display` — this used
    // to disagree with `print`'s own float formatting (`"1"` not `"1.0"`,
    // `"NaN"` not `"nan"`), which is unreadable back into the lexer either
    // way.
    crate::notation::float_repr(f)
}

#[frog_fn]
fn bool_to_str(b: bool) -> String {
    b.to_string()
}
