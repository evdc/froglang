//! `Result<T, E>: ToFrog` — packing a Rust `Result` into a real frog
//! `T | E` union return, rather than a sentinel or a panic. `plans/STDLIB.md`
//! Phase 2; the impl lives in `froglang-core/src/host.rs`.
//!
//! `Str` doesn't implement `Trait::Error` (only a nominal struct declared
//! `error X(...)`/`provides Error` does — `frontend/typeck.rs`'s
//! `type_implements`), so these functions' `T | Str` returns can't be
//! consumed with `?`/`!`/`catch`; that's exercised in `test_try_catch.rs`
//! against frog-declared error types instead. What's under test here is
//! purely the marshalling: does the union arrive with the right tag and the
//! right payload, checked with an ordinary `match`/`is`.

use froglang_core::frog_fn;
use froglang_core::state::{FrogState, FrogValue};

fn int(v: &FrogValue) -> i64 {
    match v {
        FrogValue::Int(n) => *n,
        other => panic!("expected Int, got {:?}", other),
    }
}

fn string(v: &FrogValue) -> String {
    match v {
        FrogValue::Str(s) => s.clone(),
        other => panic!("expected Str, got {:?}", other),
    }
}

/// `Ok`/`Err` where one member is scalar (`Int`) and the other is a pointer
/// (`Str`) — `UnionLayout::width() == 2` (a forced tag/pointer column plus
/// one scalar column).
#[frog_fn]
fn parse_positive(s: String) -> Result<i64, String> {
    match s.parse::<i64>() {
        Ok(n) if n > 0 => Ok(n),
        Ok(_) => Err(format!("{} is not positive", s)),
        Err(_) => Err(format!("{} is not a number", s)),
    }
}

#[test]
fn ok_case_round_trips_through_match() {
    let mut s = FrogState::builder().func(parse_positive_host()).build().unwrap();
    let (v, _) = s.eval(r#"match parse_positive("42") {
is Int(n) then n
is Str(e) then -1
}"#).unwrap();
    assert_eq!(int(&v), 42);
}

#[test]
fn err_case_round_trips_through_match() {
    let mut s = FrogState::builder().func(parse_positive_host()).build().unwrap();
    let (v, _) = s.eval(r#"match parse_positive("-5") {
is Int(n) then "ok"
is Str(e) then e
}"#).unwrap();
    assert_eq!(string(&v), "-5 is not positive");
}

#[test]
fn err_case_from_unparseable_input() {
    let mut s = FrogState::builder().func(parse_positive_host()).build().unwrap();
    let (v, _) = s.eval(r#"match parse_positive("nope") {
is Int(n) then "ok"
is Str(e) then e
}"#).unwrap();
    assert_eq!(string(&v), "nope is not a number");
}

/// `Ok`/`Err` where *both* members are pointer-shaped (`Str`) — forced
/// through a distinct wrapper on the `Err` side (`Vec<i64>`, standing in for
/// "an error code list") specifically because `Result<String, String>`
/// itself is documented as unsupported (`Type::normalize` collapses `Str |
/// Str` to `Str`, losing the tag). `UnionLayout::width() == 1` here: a
/// single pointer/tag column, no scalar column at all.
#[frog_fn]
fn first_word(s: String) -> Result<String, Vec<i64>> {
    match s.split_whitespace().next() {
        Some(w) => Ok(w.to_string()),
        None => Err(vec![404]),
    }
}

// `List(...)` isn't destructurable in a `match` pattern, and `is List(Int)`
// isn't a legal type-test either (`Unknown type 'List'` — the same
// pre-existing, unrelated gap `test_anon_unions.rs`'s
// `test_list_member_round_trips` documents), so there's no way in frog
// source today to narrow `Str | List(Int)` down to its `List` side at all.
// These two therefore only check the tag (`r is Str`), not the `List`
// payload — payload correctness for this impl is what the mixed
// scalar/pointer tests above already establish, and is shared code
// (`pack_union_member_runtime`) regardless of which column shape is hit.

#[test]
fn both_members_pointer_shaped_ok_case_is_tagged_str() {
    let mut s = FrogState::builder().func(first_word_host()).build().unwrap();
    let (v, _) = s.eval(r#"let r = first_word("hello world")
if r is Str then r else "wrong-tag""#).unwrap();
    assert_eq!(string(&v), "hello");
}

#[test]
fn both_members_pointer_shaped_err_case_is_tagged_list() {
    let mut s = FrogState::builder().func(first_word_host()).build().unwrap();
    let (v, _) = s.eval(r#"let r = first_word("")
if r is Str then -1 else 404"#).unwrap();
    assert_eq!(int(&v), 404);
}

/// A collection that survives `FROG_GC_STRESS=1` — the `Err` arm allocates
/// a `List` before the union around it is ever written to `out`, exactly
/// the `FrogCtx::scope()` incremental-rooting scenario `test_host_fns.rs`'s
/// `multiple_allocations_in_one_host_call_all_survive` already covers for a
/// non-union return.
#[test]
fn err_arms_list_allocation_survives_a_collection() {
    std::env::set_var("FROG_GC_STRESS", "1");
    let mut s = FrogState::builder().func(first_word_host()).build().unwrap();
    let (v, _) = s.eval(r#"let r = first_word("")
if r is Str then -1 else 404"#).unwrap();
    std::env::remove_var("FROG_GC_STRESS");
    assert_eq!(int(&v), 404);
}
