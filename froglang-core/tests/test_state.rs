//! Regression tests for `FrogState`'s multi-entry REPL/embedding semantics.
//!
//! `tests/test_run.rs` exercises `codegen::compile_and_run`, a single-shot
//! path that never touches `FrogState::eval`'s environment-threading logic.
//! These tests instead go through `FrogState` directly, which is what the
//! REPL and the embedding API (`README.md`'s `FrogState` example) actually
//! use — the bugs below shipped for months precisely because nothing here
//! existed to catch them.

use froglang_core::state::{FrogState, FrogValue};

fn int(v: &FrogValue) -> i64 {
    match v {
        FrogValue::Int(n) => *n,
        other => panic!("expected Int, got {:?}", other),
    }
}

/// A `let` followed by a bare expression in the *same* entry must bind the
/// `let`'s own value, not the entry's trailing expression value.
#[test]
fn test_let_then_bare_expr_binds_correct_value() {
    let mut s = FrogState::new();
    let (result, _) = s.eval("let a = 1\n99").unwrap();
    assert_eq!(int(&result), 99, "entry's own result should be the trailing expression");

    let (a, _) = s.eval("a").unwrap();
    assert_eq!(int(&a), 1, "`a` should be bound to 1, not to the entry's trailing value 99");
}

/// Multiple `let` bindings in a single entry must *all* survive into later
/// entries, not just the last one found.
#[test]
fn test_multiple_bindings_in_one_entry_all_survive() {
    let mut s = FrogState::new();
    s.eval("let a = 1\nlet b = 2").unwrap();

    let (a, _) = s.eval("a").unwrap();
    assert_eq!(int(&a), 1);
    let (b, _) = s.eval("b").unwrap();
    assert_eq!(int(&b), 2);
}

/// Three bindings, to make sure ordering/offsets into the out-buffer are
/// correct, not just "first vs last" as with two.
#[test]
fn test_three_bindings_in_one_entry_all_survive_in_order() {
    let mut s = FrogState::new();
    s.eval("let a = 10\nlet b = 20\nlet c = 30").unwrap();

    assert_eq!(int(&s.eval("a").unwrap().0), 10);
    assert_eq!(int(&s.eval("b").unwrap().0), 20);
    assert_eq!(int(&s.eval("c").unwrap().0), 30);
}

/// Redefining a function in a later entry must not panic, and later calls
/// must dispatch to the new definition.
#[test]
fn test_function_redefinition_uses_new_body() {
    let mut s = FrogState::new();
    s.eval("func f(n: Int): Int = n + 1").unwrap();
    s.eval("func f(n: Int): Int = n + 2").unwrap();

    let (result, _) = s.eval("f(10)").unwrap();
    assert_eq!(int(&result), 12, "f(10) should use the second definition (n + 2)");
}

/// A string bound early must remain readable — and correct — after many
/// subsequent entries that themselves allocate strings (and thus may
/// trigger GC collections between JIT calls).
#[test]
fn test_str_binding_survives_across_many_entries() {
    let mut s = FrogState::new();
    s.eval(r#"let keep = "SENTINEL""#).unwrap();

    for i in 0..40 {
        s.eval(&format!(
            r#"let junk{} = "padding padding padding" + " more more more more""#,
            i
        )).unwrap();
    }

    let (keep, _) = s.eval("keep").unwrap();
    match keep {
        FrogValue::Str(s) => assert_eq!(s, "SENTINEL"),
        other => panic!("expected Str, got {:?}", other),
    }
}

/// A failed `eval` due to a *type* error (as opposed to a codegen panic,
/// which is a separate, still-open issue — see DESIGN.md / project notes on
/// `builder_ctx` poisoning) must not poison the state: a subsequent valid
/// `eval` on the same `FrogState` should still succeed. The type checker
/// already rolls back via `checkpoint`/`restore`; this just guards it.
#[test]
fn test_eval_recovers_after_type_error() {
    let mut s = FrogState::new();
    assert!(s.eval(r#"1 + "oops""#).is_err());

    let (result, _) = s.eval("1 + 2").unwrap();
    assert_eq!(int(&result), 3);
}
