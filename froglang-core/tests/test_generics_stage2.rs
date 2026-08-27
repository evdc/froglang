//! End-to-end tests for `TRAITS.md` Stage 2/3b's generic-function codegen
//! path: a generalized `func`/let-bound-lambda used at exactly one
//! concrete type compiles and runs (Stage 2), and — since Stage 3b landed
//! real monomorphization — one used at two or more distinct concrete
//! types now compiles and runs too, each instantiation independently
//! correct, rather than being rejected.
//!
//! `TypeChecker::monomorphize_generics` runs only in `FrogState::eval`
//! (`state.rs`'s `eval_with_base`), not in `codegen::compile_and_run` —
//! that lower-level helper calls `check_and_lower` directly with none of
//! `state.rs`'s post-lowering passes (not even the pre-existing
//! `validate_codegen_constraints`), so it can't exercise this gate. These
//! tests go through the `froglang-core` binary instead, which is the real
//! `FrogState::eval` path.

mod common;

/// A generic used at exactly one concrete type across the whole program
/// compiles and runs.
#[test]
fn a_generic_used_at_one_type_runs_end_to_end() {
    let src = "\
        let f = x -> x + x\n\
        let a = f(1)\n\
        let b = f(2)\n\
        print(a + b)";
    assert_eq!(common::run(src).trim(), "6");
}

/// Same shape as the previous test, but with a `func` declaration and UFCS
/// dot-calls instead of a let-bound lambda called twice directly —
/// exercises the `lower_ufcs_call` instantiation seam end-to-end, not just
/// at the type-checker level.
#[test]
fn a_generic_dot_called_at_one_type_runs_end_to_end() {
    let src = "\
        func id(x) = x\n\
        print((5).id())";
    assert_eq!(common::run(src).trim(), "5");
}

/// `TRAITS.md` Stage 3b's headline case, run end-to-end: `let f = x -> x +
/// x; f(1); f(1.5)` — a generic called at two genuinely different
/// concrete types in the same program — now compiles into two distinct
/// mangled instantiations and runs both correctly, rather than being
/// rejected (see `test_typeck.rs`'s
/// `a_let_bound_lambda_generalizes_across_calls_at_different_types` for
/// the type-checking side of this, which already accepted it before
/// monomorphization existed to compile it).
#[test]
fn a_generic_used_at_two_types_runs_both_instantiations_correctly() {
    let src = "\
        let f = x -> x + x\n\
        let a = f(1)\n\
        let b = f(1.5)\n\
        print(a)\n\
        print(b)";
    let out = common::run(src);
    let mut lines = out.lines();
    assert_eq!(lines.next(), Some("2"));
    assert_eq!(lines.next(), Some("3.0"));
}
