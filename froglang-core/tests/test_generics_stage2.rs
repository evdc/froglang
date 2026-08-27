//! End-to-end tests for `TRAITS.md` Stage 2's codegen gate: a generalized
//! `func`/let-bound-lambda used at exactly one concrete type compiles and
//! runs, and one used at two or more distinct concrete types is rejected
//! with a clean, non-zero exit rather than a codegen panic.
//!
//! `TypeChecker::check_generic_monomorphism` and
//! `resolve_single_instantiations` run only in `FrogState::eval`
//! (`state.rs`'s `eval_with_base`), not in `codegen::compile_and_run` —
//! that lower-level helper calls `check_and_lower` directly with none of
//! `state.rs`'s post-lowering passes (not even the pre-existing
//! `validate_codegen_constraints`), so it can't exercise this gate. These
//! tests go through the `froglang-core` binary instead, which is the real
//! `FrogState::eval` path.
//!
//! `frog check` only type-checks (`TypeChecker::check_and_lower`) and never
//! reaches this gate at all — so the split this file exercises is
//! specifically "check accepts, run enforces", stated in the test names
//! below.

mod common;

/// A generic used at exactly one concrete type across the whole program
/// compiles and runs — the case `resolve_single_instantiations` exists to
/// make work, since without it a generalized declaration's own
/// `params`/`return_type` never resolve to a concrete type at all.
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

/// The plan's stated acceptance criterion, run end-to-end rather than only
/// checked: `let f = x -> x + x; f(1); f(1.5)` type-checks (`frog check`
/// accepts it — see `test_typeck.rs`'s
/// `a_let_bound_lambda_generalizes_across_calls_at_different_types`) but
/// can't yet be compiled, since monomorphization is Stage 3. This must
/// fail as a clean type error with a non-zero exit, not a codegen panic.
#[test]
fn a_generic_used_at_two_types_is_rejected_at_run_not_check() {
    let src = "\
        let f = x -> x + x\n\
        let a = f(1)\n\
        let b = f(1.5)\n\
        print(a)";
    let out = common::run_raw(src);
    assert_eq!(out.status, Some(1), "expected a clean non-zero exit, got: {:?}\nstderr: {}", out.status, out.stderr);
    assert!(
        out.stdout.contains("is generic and is used at two different types") || out.stderr.contains("is generic and is used at two different types"),
        "expected the Stage 3 message, got stdout: {:?} stderr: {:?}", out.stdout, out.stderr,
    );
}
