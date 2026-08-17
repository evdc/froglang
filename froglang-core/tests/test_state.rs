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

/// A `let` made inside an `if` branch must not be visible after the
/// conditional. Before this was fixed at the type-checker level, this
/// program type-checked successfully and then crashed the Cranelift
/// verifier in codegen (the branch-local SSA value doesn't dominate the use
/// site) — now it's rejected with an ordinary type error, across both the
/// braced and bare-branch spellings.
#[test]
fn test_let_in_conditional_branch_is_a_type_error_not_a_crash() {
    let mut s = FrogState::new();
    assert!(s.eval("let c = true\nif c then { let y = 5 } else 0\ny").is_err());

    let mut s2 = FrogState::new();
    assert!(s2.eval("let c = true\nif c then let y = 5 else 0\ny").is_err());
}

/// Rebinding the same name must let the *old* value become garbage. Before
/// this was fixed, `GcHeap`'s explicit root set only ever grew (every value
/// any entry had ever produced was pushed once and never popped), so a
/// shadowed/rebound value stayed live — and rooted — for the rest of the
/// process, even though nothing could reach it anymore.
#[test]
fn test_rebinding_same_name_does_not_leak_old_value() {
    let mut s = FrogState::new();
    let big = "x".repeat(4000);
    for _ in 0..500 {
        s.eval(&format!(r#"let s = "{}""#, big)).unwrap();
    }
    // gc_threshold grows to 2x the live set on every collection, so an
    // automatic collection can legitimately skip several hundred KB of
    // *real* garbage before the next one fires — force one final sweep for
    // a deterministic check, rather than assert on however far the
    // self-growing threshold happened to get in 500 iterations.
    s.heap.force_collect();
    // Only the *latest* `s` (~4000 bytes plus its GC header) should still be
    // live. Under the old accumulate-forever roots, this would be on the
    // order of 500 * 4000 = 2,000,000 bytes instead.
    assert!(
        s.heap.bytes_allocated < 100_000,
        "expected old rebindings of `s` to be collected, but {} bytes are still live",
        s.heap.bytes_allocated
    );
}

/// Codegen still panics internally on constructs the type checker allows but
/// doesn't implement (e.g. calling an immediately-invoked lambda expression,
/// as opposed to a bare named function — see codegen/mod.rs's "only named
/// function calls supported" panic). `eval` must convert that panic into a
/// clean `Err`, not let it escape — and, critically, the `FrogState` must
/// stay fully usable afterward: defining and calling new functions, and
/// referencing bindings made before the panic.
///
/// This prints a panic message to stderr (Rust's default panic hook runs
/// before `catch_unwind` recovers) — that's expected, not a test failure.
#[test]
fn test_codegen_panic_becomes_clean_error_and_state_survives() {
    use froglang_core::state::FrogError;

    let mut s = FrogState::new();
    s.eval("let kept = 41").unwrap();

    match s.eval("(x -> x + 1)(5)") {
        Err(FrogError::Codegen(_)) => {},
        other => panic!("expected a Codegen error, got {:?}", other),
    }

    // Old bindings survived, and the state can still compile and run.
    let (kept, _) = s.eval("kept").unwrap();
    assert_eq!(int(&kept), 41);

    s.eval("func double(n: Int): Int = n * 2").unwrap();
    let (doubled, _) = s.eval("double(21)").unwrap();
    assert_eq!(int(&doubled), 42);
}

/// A binding is still visible to the rest of *its own* branch, and the
/// `FrogState` recovers cleanly and keeps working after the type error above.
#[test]
fn test_let_in_conditional_branch_visible_within_branch_and_state_recovers() {
    let mut s = FrogState::new();
    let (ok, _) = s.eval("if true then { let y = 5; y + 1 } else 0").unwrap();
    assert_eq!(int(&ok), 6);

    assert!(s.eval("if true then { let z = 1 } else 0\nz").is_err());

    let (recovered, _) = s.eval("1 + 1").unwrap();
    assert_eq!(int(&recovered), 2);
}
