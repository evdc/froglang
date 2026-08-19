// Tests for `return` / `Type::Never` (ERRORS.md implementation plan, Phase 1).
//
// `return` is the language's first early-exit construct: before this, a
// function body was a single expression whose tail value was the result,
// and codegen emitted exactly one `return_` per function. `return` needed
// real multi-exit codegen (teardown the shadow frame, `return_`, then keep
// building into a fresh block for whatever source follows) and a bottom
// type (`Never`) so `if c then return 1 else 2` types as plain `Int` rather
// than some union of "the branch that never produces a value" with `Int`.

use froglang_core::codegen::compile_and_run;
use froglang_core::frontend::parser::Parser;
use froglang_core::frontend::typeck::{Type, TypeChecker};

fn infer_src(src: &str) -> Result<Type, String> {
    let ast = Parser::parse(src).map_err(|e| format!("{:?}", e))?;
    let mut tc = TypeChecker::new();
    tc.infer(&ast).map_err(|e| format!("{:?}", e))
}

fn type_error(src: &str) -> String {
    let ast = Parser::parse(src).expect("parse error");
    let mut tc = TypeChecker::new();
    let err = tc.check_and_lower(ast).expect_err("expected a type error");
    format!("{:?}", err)
}

// ── typing ───────────────────────────────────────────────────────────────────

#[test]
fn test_return_outside_function_is_an_error() {
    assert!(type_error("return 1").contains("outside"));
}

#[test]
fn test_bare_return_outside_function_is_an_error() {
    assert!(type_error("return").contains("outside"));
}

#[test]
fn test_conditional_with_one_returning_branch_types_as_the_other_branch() {
    // Not `Never | Int` — `Never` must vanish from the join entirely, which
    // is what makes `return` usable in ordinary code without every caller
    // seeing a union leak out of the branch it didn't take.
    let src = "func f(c: Bool): Int = if c then return 1 else 2\nf(true)";
    assert_eq!(infer_src(src).unwrap(), Type::Int);
}

#[test]
fn test_conditional_with_both_branches_returning_types_as_never() {
    let src = "func f(c: Bool): Int = { if c then return 1 else return 2\n0 }\nf(true)";
    assert_eq!(infer_src(src).unwrap(), Type::Int);
}

#[test]
fn test_return_type_must_match_declared_return_type() {
    assert!(type_error("func f(): Int = return \"nope\"\nf()").contains("Expected"));
}

#[test]
fn test_bare_return_requires_none_return_type() {
    assert!(type_error("func f(): Int = { if true then return else 0\n1 }").contains("Expected"));
}

#[test]
fn test_unannotated_function_infers_return_type_from_return_statements() {
    // No `: RetType` at all — `return`'s type rule must still work off a
    // fresh, unbound type var rather than assuming an already-resolved one.
    let src = "let f = n -> { if n < 0 then return 0 else 0\nn * n }\nf(4)";
    assert_eq!(infer_src(src).unwrap(), Type::Int);
}

#[test]
fn test_unannotated_function_rejects_inconsistent_return_types() {
    let src = "let f = n -> { if n < 0 then return \"neg\" else 0\nn }";
    assert!(type_error(src).contains("disagree") || type_error(src).contains("Expected"));
}

// ── running programs ────────────────────────────────────────────────────────

#[test]
fn test_early_return_from_if_branch() {
    let src = "func f(x: Int): Int = { if x < 0 then return 0 else 0\nx * 2 }\nf(-5) * 100 + f(5)";
    assert_eq!(compile_and_run(src), 10);
}

#[test]
fn test_return_as_the_entire_function_body() {
    assert_eq!(compile_and_run("func f(): Int = return 5\nf()"), 5);
}

#[test]
fn test_two_sequential_early_returns() {
    let src = "\
        func classify(n: Int): Int = {\n\
            if n < 0 then return -1 else 0\n\
            if n == 0 then return 0 else 0\n\
            1\n\
        }\n\
        classify(-5) * 100 + classify(0) * 10 + classify(5)";
    assert_eq!(compile_and_run(src), -1 * 100 + 0 * 10 + 1);
}

#[test]
fn test_dead_code_after_return_does_not_execute() {
    // `1 / 0` traps/crashes the process (integer division). If the dead
    // code after `return` executed, this test process would not survive to
    // report a result at all — so a passing result *is* the proof.
    let src = "func f(): Int = { return 7\n1 / 0 }\nf()";
    assert_eq!(compile_and_run(src), 7);
}

#[test]
fn test_early_return_inside_for_loop() {
    let src = "\
        func find_three(n: Int): Int = {\n\
            for i in 0..n do {\n\
                if i == 3 then return i else 0\n\
            }\n\
            -1\n\
        }\n\
        find_three(10) * 100 + find_three(2)";
    assert_eq!(compile_and_run(src), 3 * 100 + (-1));
}

#[test]
fn test_early_return_of_a_heap_value() {
    let src = "\
        func label(n: Int): Str = {\n\
            if n < 0 then return \"neg\" else 0\n\
            \"pos\"\n\
        }\n\
        if label(-1) == \"neg\" then 1 else 0";
    assert_eq!(compile_and_run(src), 1);
}

#[test]
fn test_early_return_of_a_struct_value() {
    let src = "\
        data Point(x: Int, y: Int)\n\
        func pick(n: Int): Point = {\n\
            if n < 0 then return Point(x=0, y=0) else 0\n\
            Point(x=n, y=n * 2)\n\
        }\n\
        let p = pick(-5)\n\
        let q = pick(3)\n\
        p.x * 1000 + p.y * 100 + q.x * 10 + q.y";
    assert_eq!(compile_and_run(src), 0 * 1000 + 0 * 100 + 3 * 10 + 6);
}

#[test]
fn test_bare_return_from_none_returning_function() {
    let src = "\
        func log_positive(n: Int, out: Int): Int = {\n\
            if n < 0 then return out else 0\n\
            out + 1\n\
        }\n\
        log_positive(-1, 9) * 100 + log_positive(1, 9)";
    assert_eq!(compile_and_run(src), 9 * 100 + 10);
}

#[test]
fn test_return_from_a_lambda_returns_from_the_lambda_not_the_caller() {
    // `f` calls `g`, which returns early; `f` itself must keep running —
    // `return` only ever unwinds to its own lexically-enclosing function.
    let src = "\
        func g(x: Int): Int = { if x < 0 then return 0 else 0\nx }\n\
        func f(x: Int): Int = g(x) + 100\n\
        f(-5)";
    assert_eq!(compile_and_run(src), 100);
}

#[test]
fn test_no_early_return_still_behaves_normally() {
    // Guards against a regression where adding multi-exit plumbing changes
    // behavior for functions that never use it.
    assert_eq!(compile_and_run("func f(x: Int): Int = x * x\nf(6)"), 36);
}

#[test]
fn test_early_returned_heap_value_survives_a_collection_triggered_before_it() {
    // Every `Str`/`List` allocation checks the GC threshold (`maybe_collect`
    // in `runtime::ffi`), so enough throwaway string allocations inside the
    // loop below force a real, non-forced collection to happen *before* the
    // early `return` executes. If the early-return codegen path (the new
    // `TypedExprKind::Return` arm) failed to keep the shadow frame's
    // accounting in sync with what it actually roots — see
    // `for_each_heap_producer`'s `Return` arm — the string built on the
    // final iteration and returned here would already be collected garbage
    // by the time `print` reads it back, and this would print corrupted
    // bytes or crash rather than the exact string constructed.
    let src = "\
        func make(n: Int): Str = {\n\
            for i in 0..n do {\n\
                let junk = \"garbage-string-padding-to-force-allocation-pressure-0123456789\"\n\
                if junk == \"unreachable\" then return \"wrong\" else 0\n\
            }\n\
            if n > 0 then return \"returned-early-after-loop\" else 0\n\
            \"fallthrough\"\n\
        }\n\
        if make(50000) == \"returned-early-after-loop\" then 1 else 0";
    assert_eq!(compile_and_run(src), 1);
}

// ── unannotated functions and float returns ──────────────────────────────────

#[test]
fn test_unannotated_function_body_that_is_entirely_a_return() {
    // With no declared return type, the body's own type is `Never` (it never
    // falls through to a final value). `Never` unifies with nothing, so the
    // return slot has to be taken as-is rather than unified with the body —
    // otherwise *every* unannotated function ending in `return` is rejected.
    assert_eq!(infer_src("func f() = { return 1 }\nf()").unwrap(), Type::Int);
    assert_eq!(infer_src("func f() = return 1\nf()").unwrap(), Type::Int);
    assert_eq!(infer_src("func f(x: Int) = { return x }\nf(2)").unwrap(), Type::Int);
    assert_eq!(compile_and_run("func f() = { return 7 }\nf()"), 7);
    assert_eq!(compile_and_run("func f(x: Int) = { return x * 2 }\nf(21)"), 42);
}

#[test]
fn test_unannotated_function_with_a_return_and_a_tail_value_still_unifies() {
    // The body is *not* `Never` here, so the return slot and the tail value
    // must still agree — this is the case the `Never` shortcut must not eat.
    assert_eq!(infer_src("func f(x: Int) = { if x < 0 then return 0 else 0\nx }\nf(1)").unwrap(), Type::Int);
    assert!(infer_src("func f(x: Int) = { if x < 0 then return true else 0\nx }\nf(1)").is_err());
}

#[test]
fn test_float_returning_function_whose_body_is_entirely_a_return() {
    // The unreachable block after an unconditional `return` still needs a
    // value list matching the signature. Those placeholders must be built
    // with `f64const`, not `iconst` — `iconst` on an `F64` trips the
    // Cranelift verifier (`placeholder_value` in codegen).
    assert_eq!(compile_and_run("func f(): Float = return 1.5\nif f() == 1.5 then 1 else 0"), 1);
    let src = "\
        func f(c: Bool): Float = { if c then return 1.5 else return 2.5 }\n\
        if f(true) == 1.5 and f(false) == 2.5 then 1 else 0";
    assert_eq!(compile_and_run(src), 1);
}
