//! Regression/integration tests for the host-function embedding API
//! (`FrogState::builder()`, `#[frog_fn]`) — `plans/EMBEDDING.md`.
//!
//! These drive `FrogState` in-process like `test_state.rs`, since the
//! whole point is exercising the real GC/JIT path a `#[frog_fn]`-generated
//! shim runs under, not just that the Rust side compiles.

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

#[frog_fn]
fn host_add(a: i64, b: i64) -> i64 {
    a + b
}

#[test]
fn scalar_args_and_return_round_trip() {
    let mut s = FrogState::builder().func(host_add_host()).build().unwrap();
    let (v, _) = s.eval("host_add(2, 3)").unwrap();
    assert_eq!(int(&v), 5);
}

#[frog_fn]
fn shout(s: String) -> String {
    format!("{}!", s.to_uppercase())
}

#[test]
fn str_arg_and_return_round_trip_and_survive_a_collection() {
    let mut s = FrogState::builder().func(shout_host()).build().unwrap();
    let (v, _) = s.eval("let r = shout(\"hi\")\nr").unwrap();
    assert_eq!(string(&v), "HI!");

    // The returned Str must have been rooted (by `compile_host_call`'s
    // `declare_gc_leaves` on the way out, and by `FrogState::eval`'s own
    // root rebuild once it's bound to `r`) — force a collection and read
    // it back through a fresh binding to prove it wasn't swept.
    s.heap.force_collect();
    let (v2, _) = s.eval("r").unwrap();
    assert_eq!(string(&v2), "HI!");
}

#[frog_fn]
fn double_all(xs: Vec<i64>) -> Vec<i64> {
    xs.into_iter().map(|x| x * 2).collect()
}

#[test]
fn list_arg_and_return_round_trip() {
    let mut s = FrogState::builder().func(double_all_host()).build().unwrap();
    let (v, _) = s.eval("double_all([1, 2, 3])").unwrap();
    match v {
        FrogValue::List(items) => {
            assert_eq!(items.iter().map(int).collect::<Vec<_>>(), vec![2, 4, 6]);
        }
        other => panic!("expected List, got {:?}", other),
    }
}

/// Registering a name the frontend already matches specially must fail at
/// `build()`, not silently shadow it or panic deep inside codegen.
#[test]
fn reserved_name_is_rejected_at_build() {
    #[frog_fn]
    fn host_print(_s: String) {}

    // `host_print_host()`'s frog name is "host_print", not reserved — swap
    // in a hand-built descriptor claiming "print" to hit the check.
    let mut host = host_print_host();
    host.name = "print";
    let err = match froglang_core::state::FrogState::builder().func(host).build() {
        Ok(_) => panic!("expected build() to reject the reserved name 'print'"),
        Err(e) => e,
    };
    assert!(format!("{}", err).contains("reserved"), "error should mention the reserved name: {}", err);
}

#[test]
fn duplicate_registration_is_rejected_at_build() {
    let err = match FrogState::builder().func(host_add_host()).func(host_add_host()).build() {
        Ok(_) => panic!("expected build() to reject a duplicate registration"),
        Err(e) => e,
    };
    assert!(format!("{}", err).contains("more than once"), "{}", err);
}

/// A host function called with the wrong argument type is an ordinary type
/// error at the call site, not a codegen panic.
#[test]
fn type_mismatch_is_a_clean_type_error() {
    let mut s = FrogState::builder().func(host_add_host()).build().unwrap();
    let err = s.eval("host_add(\"x\", 1)").unwrap_err();
    assert!(matches!(err, froglang_core::state::FrogError::Type(_)), "{:?}", err);
}

/// A host function that allocates several values before any of them is
/// written into `out` must keep every one of them alive — the scenario
/// `FrogCtx::scope`'s root guard exists for. Exercised harder under
/// `FROG_GC_STRESS=1` (collect on every allocation), which the test suite
/// doesn't force globally, but this shape (multiple allocations, threshold
/// forced low) is exactly what that mode stresses.
#[frog_fn]
fn build_three_strings(a: String, b: String, c: String) -> Vec<String> {
    vec![a, b, c]
}

#[test]
fn multiple_allocations_in_one_host_call_all_survive() {
    let mut s = FrogState::builder().func(build_three_strings_host()).build().unwrap();
    s.heap.force_collect(); // drop the threshold's slack so the next few allocations are likelier to trigger one
    let (v, _) = s.eval(r#"build_three_strings("a", "b", "c")"#).unwrap();
    match v {
        FrogValue::List(items) => {
            assert_eq!(items.iter().map(string).collect::<Vec<_>>(), vec!["a", "b", "c"]);
        }
        other => panic!("expected List, got {:?}", other),
    }
}

/// A raw `Int`/`Float` argument slot must never be rooted as a GC pointer
/// (`RuntimeRoots::hold_masked`) — an integer big enough to look like a
/// heap address, followed by an argument that actually allocates, used to
/// crash the collector when it dereferenced the int as if it were a
/// pointer.
#[frog_fn]
fn tag_it(n: i64, s: String) -> String {
    format!("{}{}", s, n)
}

#[test]
fn a_large_int_argument_slot_is_never_mistaken_for_a_gc_pointer() {
    std::env::set_var("FROG_GC_STRESS", "1");
    let mut s = FrogState::builder().func(tag_it_host()).build().unwrap();
    let (v, _) = s.eval(r#"tag_it(123456789, "x")"#).unwrap();
    std::env::remove_var("FROG_GC_STRESS");
    assert_eq!(string(&v), "x123456789");
}

/// A host function that returns `()` and takes further arguments after
/// another `()`-typed one must keep every argument's slot correctly
/// aligned — `FromFrog for ()`'s `SLOTS` used to disagree with codegen's
/// one-slot-per-`None`-arg layout, misaligning any argument after it.
#[frog_fn]
fn take_unit_then_int(_u: (), n: i64) -> i64 {
    n
}

#[test]
fn a_unit_argument_does_not_misalign_the_argument_after_it() {
    let mut s = FrogState::builder().func(take_unit_then_int_host()).build().unwrap();
    let (v, _) = s.eval("take_unit_then_int(gc_dump(), 42)").unwrap();
    assert_eq!(int(&v), 42);
}

/// A user `func` sharing a registered host function's name must be
/// rejected at typeck, not silently hijack the host call site (codegen
/// routes calls by name alone, ignoring what the name is actually bound
/// to).
#[test]
fn a_func_redefining_a_host_name_is_a_clean_type_error() {
    let mut s = FrogState::builder().func(host_add_host()).build().unwrap();
    let err = s.eval("func host_add(a: Int, b: Int): Int = a * b\nhost_add(3, 4)").unwrap_err();
    match &err {
        froglang_core::state::FrogError::Type(msg) => {
            assert!(msg.contains("host_add"), "{}", msg);
        }
        other => panic!("expected a clean Type error, got {:?}", other),
    }
}
