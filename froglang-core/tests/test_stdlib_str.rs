//! String stdlib functions — `plans/STDLIB.md` Phase 3
//! (`froglang-core/src/stdlib/str.rs`).
//!
//! `FrogState::with_stdlib()`, not `FrogState::new()`: every function here
//! is a registered host function, not a language builtin.

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
fn boolean(v: &FrogValue) -> bool {
    match v {
        FrogValue::Bool(b) => *b,
        other => panic!("expected Bool, got {:?}", other),
    }
}
fn list_of_str(v: &FrogValue) -> Vec<String> {
    match v {
        FrogValue::List(items) => items.iter().map(string).collect(),
        other => panic!("expected List, got {:?}", other),
    }
}

fn eval_str(src: &str) -> String {
    let mut s = FrogState::with_stdlib().unwrap();
    let (v, _) = s.eval(src).unwrap();
    string(&v)
}

#[test]
fn trim_removes_leading_and_trailing_whitespace() {
    assert_eq!(eval_str(r#"trim("  hi  ")"#), "hi");
}

#[test]
fn to_upper_and_to_lower() {
    assert_eq!(eval_str(r#""hi".to_upper()"#), "HI");
    assert_eq!(eval_str(r#""HI".to_lower()"#), "hi");
}

#[test]
fn split_on_a_separator() {
    let mut s = FrogState::with_stdlib().unwrap();
    let (v, _) = s.eval(r#"split("a,b,c", ",")"#).unwrap();
    assert_eq!(list_of_str(&v), vec!["a", "b", "c"]);
}

#[test]
fn split_on_empty_separator_returns_the_whole_string_unsplit() {
    let mut s = FrogState::with_stdlib().unwrap();
    let (v, _) = s.eval(r#"split("abc", "")"#).unwrap();
    assert_eq!(list_of_str(&v), vec!["abc"]);
}

#[test]
fn join_bare_call_and_ufcs() {
    assert_eq!(eval_str(r#"join(["a", "b", "c"], " ")"#), "a b c");
    assert_eq!(eval_str(r#"["a", "b", "c"].join("-")"#), "a-b-c");
}

#[test]
fn chars_splits_by_codepoint_not_by_byte() {
    let mut s = FrogState::with_stdlib().unwrap();
    let (v, _) = s.eval(r#"chars("hé")"#).unwrap();
    assert_eq!(list_of_str(&v), vec!["h", "é"]);
}

#[test]
fn contains_starts_with_ends_with() {
    let mut s = FrogState::with_stdlib().unwrap();
    assert!(boolean(&s.eval(r#"contains("hello", "ell")"#).unwrap().0));
    assert!(boolean(&s.eval(r#""hello".starts_with("he")"#).unwrap().0));
    assert!(boolean(&s.eval(r#""hello".ends_with("lo")"#).unwrap().0));
    assert!(!boolean(&s.eval(r#""hello".contains("xyz")"#).unwrap().0));
}

#[test]
fn slice_ok_case() {
    let mut s = FrogState::with_stdlib().unwrap();
    let (v, _) = s.eval("let r = slice(\"hello\", 1, 3)\nif r is Str then r else \"err\"").unwrap();
    assert_eq!(string(&v), "el");
}

/// Out-of-range/reversed bounds are a caught `ErrMsg`, not a Rust panic
/// unwinding across the shim boundary — the whole reason `slice` is
/// fallible instead of trusting its caller (`str.rs`'s doc comment).
#[test]
fn slice_out_of_range_is_a_catchable_error_not_a_crash() {
    let mut s = FrogState::with_stdlib().unwrap();
    let (v, _) = s.eval("slice(\"hello\", 3, 1) catch [e] -> e.msg").unwrap();
    assert!(string(&v).contains("out of range"), "{:?}", v);
}

#[test]
fn index_of_found_and_not_found() {
    let mut s = FrogState::with_stdlib().unwrap();
    let (v, _) = s.eval("let r = index_of(\"hello world\", \"world\")\nif r is Int then r else -1").unwrap();
    assert_eq!(int(&v), 6);

    let (v, _) = s.eval("let r = index_of(\"hello\", \"xyz\")\nif r is Int then r else -1").unwrap();
    assert_eq!(int(&v), -1);
}

#[test]
fn to_int_to_float_to_bool_ok_and_err() {
    let mut s = FrogState::with_stdlib().unwrap();
    let (v, _) = s.eval("let r = to_int(\"42\")\nif r is Int then r else -1").unwrap();
    assert_eq!(int(&v), 42);
    let (v, _) = s.eval("let r = to_int(\"nope\")\nif r is Int then r else -1").unwrap();
    assert_eq!(int(&v), -1);

    let (v, _) = s.eval("let r = to_bool(\"true\")\nif r is Bool then r else false").unwrap();
    assert!(boolean(&v));
    let (v, _) = s.eval("let r = to_bool(\"nope\")\nif r is Bool then r else false").unwrap();
    assert!(!boolean(&v));
}

#[test]
fn round_trip_conversions() {
    let mut s = FrogState::with_stdlib().unwrap();
    assert_eq!(string(&s.eval("int_to_str(42)").unwrap().0), "42");
    assert_eq!(string(&s.eval("float_to_str(3.5)").unwrap().0), "3.5");
    assert_eq!(string(&s.eval("bool_to_str(true)").unwrap().0), "true");
}

/// `float_to_str` used to go through Rust's `Display` (`"1"`, `"NaN"`),
/// disagreeing with `print`'s own frog-notation formatting (`"1.0"`,
/// `"nan"`) — see `crate::notation`. Both go through the same formatter now.
#[test]
fn float_to_str_matches_frog_notation() {
    let mut s = FrogState::with_stdlib().unwrap();
    assert_eq!(string(&s.eval("float_to_str(1.0)").unwrap().0), "1.0");
    assert_eq!(string(&s.eval("float_to_str(1.0 / 0.0)").unwrap().0), "inf");
    assert_eq!(string(&s.eval("float_to_str(0.0 / 0.0)").unwrap().0), "nan");
}

/// `slice`'s `ErrMsg` and its `Ok` value are both `Str`-shaped
/// (`Result<String, ErrMsg>`) — the exact shape `host.rs`'s doc comment
/// warns a bare `Result<String, String>` would collide on. `ErrMsg` being a
/// distinct nominal struct (not `Str`) is what keeps this working; this
/// regression-guards that choice.
#[test]
fn slice_error_and_ok_are_distinguishable_even_though_both_carry_a_string() {
    let mut s = FrogState::with_stdlib().unwrap();
    let (ok, _) = s.eval("let r = slice(\"hello\", 1, 3)\nif r is Str then \"str\" else \"errmsg\"").unwrap();
    assert_eq!(string(&ok), "str");
    let (err, _) = s.eval("let r = slice(\"hello\", 3, 1)\nif r is Str then \"str\" else \"errmsg\"").unwrap();
    assert_eq!(string(&err), "errmsg");
}

/// `FROG_GC_STRESS=1` across a mix of allocating conversions — each of
/// these allocates a `Str` (and, for `to_int`'s error path, an `ErrMsg`
/// struct wrapping one) inside the shim's `FrogCtx::scope()`, exactly the
/// incremental-rooting scenario `test_host_fns.rs`/`test_result_marshalling.rs`
/// already cover for other shapes.
#[test]
fn stdlib_str_functions_survive_gc_stress() {
    std::env::set_var("FROG_GC_STRESS", "1");
    let mut s = FrogState::with_stdlib().unwrap();
    let (v, _) = s.eval(
        "let a = trim(\"  hi  \")\n\
         let b = a.to_upper()\n\
         let c = split(\"x,y,z\", \",\").join(\"-\")\n\
         let d = index_of(c, \"y\")\n\
         let e = if d is Int then d else -1\n\
         b + \"/\" + c + \"/\" + int_to_str(e)"
    ).unwrap();
    std::env::remove_var("FROG_GC_STRESS");
    assert_eq!(string(&v), "HI/x-y-z/2");
}
