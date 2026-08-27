//! `len` — a hardcoded builtin over `List<T>`/`Str`, the same mechanism as
//! `push` (`frontend/typeck.rs`'s `finish_len`, `codegen/mod.rs`'s
//! `func_name == "len"` arm) rather than a `FrogStateBuilder`-registered
//! host function, since it needs to be polymorphic over `T` with no
//! generics system to express that. `plans/STDLIB.md` Phase 1.
//!
//! `FrogState::new()`, not `with_stdlib()`: `len` is a language builtin,
//! available with no registration at all — same status as `push`.

use froglang_core::state::{FrogState, FrogValue};

fn int(v: &FrogValue) -> i64 {
    match v {
        FrogValue::Int(n) => *n,
        other => panic!("expected Int, got {:?}", other),
    }
}

#[test]
fn len_of_a_list_bare_call() {
    let mut s = FrogState::new();
    let (v, _) = s.eval("len([1, 2, 3])").unwrap();
    assert_eq!(int(&v), 3);
}

#[test]
fn len_of_an_empty_list() {
    let mut s = FrogState::new();
    let (v, _) = s.eval("let xs: List<Int> = []\nlen(xs)").unwrap();
    assert_eq!(int(&v), 0);
}

#[test]
fn len_of_a_str_bare_call() {
    let mut s = FrogState::new();
    let (v, _) = s.eval(r#"len("hello")"#).unwrap();
    assert_eq!(int(&v), 5);
}

#[test]
fn len_as_a_ufcs_method_on_a_list() {
    let mut s = FrogState::new();
    let (v, _) = s.eval("[1, 2, 3, 4].len()").unwrap();
    assert_eq!(int(&v), 4);
}

#[test]
fn len_as_a_ufcs_method_on_a_str() {
    let mut s = FrogState::new();
    let (v, _) = s.eval(r#""hi".len()"#).unwrap();
    assert_eq!(int(&v), 2);
}

#[test]
fn len_of_a_list_of_str_counts_elements_not_bytes() {
    let mut s = FrogState::new();
    let (v, _) = s.eval(r#"len(["a", "bb", "ccc"])"#).unwrap();
    assert_eq!(int(&v), 3);
}

/// `len` on a type it doesn't support is a clean type error, not a codegen
/// panic — mirrors `test_host_fns.rs`'s `type_mismatch_is_a_clean_type_error`.
#[test]
fn len_of_an_unsupported_type_is_a_clean_type_error() {
    let mut s = FrogState::new();
    let err = s.eval("len(5)").unwrap_err();
    assert!(matches!(err, froglang_core::state::FrogError::Type(_)), "{:?}", err);
}

/// Wrong arity, both call forms, must also be a clean type error.
#[test]
fn len_with_wrong_arity_is_a_clean_type_error() {
    let mut s = FrogState::new();
    let err = s.eval("len([1], [2])").unwrap_err();
    assert!(matches!(err, froglang_core::state::FrogError::Type(_)), "{:?}", err);

    let err = s.eval("[1].len(2)").unwrap_err();
    assert!(matches!(err, froglang_core::state::FrogError::Type(_)), "{:?}", err);
}
