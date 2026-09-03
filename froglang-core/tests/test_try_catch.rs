// Tests for Phase 5 (ERRORS.md implementation plan): the `?`, `!`, and
// `catch` operators.
//
// All three desugar entirely in `TypeChecker` (`lower_try`/`lower_unwrap`/
// `lower_catch`) into an ordinary `match` over the subject's union members
// — literally built as synthetic `MatchArm`s and handed to the same
// `lower_match` machinery a source-level `match` uses (see
// `TypeChecker::union_entries`). Codegen gains no new control flow for any
// of the three; the only codegen-visible addition is a `Never`-typed
// `Conditional` fix (see below) and a `Never`-typed `Call` codegen path
// (needed for `panic`, below).
//
// `e?`'s error arm is `return __err`, so its join is exactly the doc's
// "set subtraction" type rule for free — a `return` arm is `Never`-typed
// and vanishes from `lower_match`'s union-join, leaving only the
// non-`Error` members. `e!`'s error arm calls the builtin `panic(msg: Str):
// Never` (registered in `default_context`, exactly like `print`/`gc_dump`)
// instead of returning — an ordinary function call, not a dedicated node,
// so user code can call `panic` directly too (see the tests at the bottom
// of this file). `catch`'s handler is inlined per `Error`-providing member
// (a single-param lambda contributes its own param name as the arm's bind
// and its body as the arm's body — no actual closure/call codegen needed)
// rather than called.
//
// Building this surfaced a real, general codegen bug, fixed alongside:
// `TypedExprKind::Conditional`'s codegen only ever special-cased a `Never`
// branch paired with a real value on the *other* side (`return`'s
// pre-existing early-exit tests). Nothing before this phase built a
// conditional where *both* branches are `Never` — which `lower_match`'s
// arm chain does whenever every match arm's body is itself `Never`-typed
// (every arm is `return`/panic, as `?`/`!` build): the innermost arm is
// then a bare `Never` expression and the one wrapping it has `Never` on
// both sides.
// The old codegen still tried to produce a value-carrying merge block for
// it, leaving that block reachable only from a path nobody above ever
// jumped into — "you have to fill your block before switching" from
// Cranelift's verifier. Fixed by giving a `Never`-typed `Conditional` its
// own codegen path: every branch supplies its own terminator (a nested
// `Never` conditional's own trap, or `return_`/`Panic`'s trap), and the
// "missing tail" case traps directly, mirroring `TypedExprKind::Panic`'s
// own "trap, then open a fresh dead block" shape.

use froglang_core::codegen::compile_and_run;

fn type_error(src: &str) -> String {
    let ast = froglang_core::frontend::parser::Parser::parse(src).expect("parse error");
    let mut tc = froglang_core::frontend::typeck::TypeChecker::new();
    let err = tc.check_and_lower(ast).expect_err("expected a type error");
    format!("{:?}", err)
}

// ── `?` ──────────────────────────────────────────────────────────────────

#[test]
fn test_try_passes_through_the_ok_case() {
    assert_eq!(compile_and_run(
        "error ParseError(msg: Str)\n\
         func parse(s: Str): Int | ParseError = if s == \"bad\" then ParseError(msg=\"nope\") else 42\n\
         func run(s: Str): Int | ParseError = {\n\
         let n = parse(s)?\n\
         n * 2\n\
         }\n\
         match run(\"ok\") {\n\
         is Int(n) then n\n\
         is ParseError(e) then -1\n\
         }"
    ), 84);
}

#[test]
fn test_try_propagates_the_error_case() {
    assert_eq!(compile_and_run(
        "error ParseError(msg: Str)\n\
         func parse(s: Str): Int | ParseError = if s == \"bad\" then ParseError(msg=\"nope\") else 42\n\
         func run(s: Str): Int | ParseError = {\n\
         let n = parse(s)?\n\
         n * 2\n\
         }\n\
         match run(\"bad\") {\n\
         is Int(n) then n\n\
         is ParseError(e) then -1\n\
         }"
    ), -1);
}

#[test]
fn test_try_widens_a_narrower_error_into_a_broader_declared_error_union() {
    // Free widening, no `From`/conversion: `parse`'s error (`ParseError`)
    // is a strict subset of `run`'s declared error set (`ParseError |
    // OtherError`) — `?` must still typecheck and propagate it as-is.
    assert_eq!(compile_and_run(
        "error ParseError(msg: Str)\n\
         error OtherError(msg: Str)\n\
         func parse(s: Str): Int | ParseError = if s == \"bad\" then ParseError(msg=\"nope\") else 42\n\
         func run(s: Str): Int | ParseError | OtherError = {\n\
         let n = parse(s)?\n\
         n * 2\n\
         }\n\
         match run(\"bad\") {\n\
         is Int(n) then n\n\
         is ParseError(e) then -1\n\
         is OtherError(e) then -2\n\
         }"
    ), -1);
}

#[test]
fn test_try_propagates_through_two_levels() {
    assert_eq!(compile_and_run(
        "error ParseError(msg: Str)\n\
         func parse(s: Str): Int | ParseError = if s == \"bad\" then ParseError(msg=\"nope\") else 42\n\
         func double(s: Str): Int | ParseError = parse(s)? * 2\n\
         func run(s: Str): Int | ParseError = double(s)? + 1\n\
         match run(\"bad\") {\n\
         is Int(n) then n\n\
         is ParseError(e) then -1\n\
         }"
    ), -1);
}

#[test]
fn test_try_on_nominal_union_where_every_member_provides_error() {
    // `provides Error` on a union declaration grants the trait to *every*
    // variant (see `ERRORS.md` Phase 4), so `s?` here always propagates —
    // exercises `union_entries`' nominal-union path (`resolve_union`)
    // directly, rather than a value nested inside a further anonymous
    // union (a known, separately-documented gap: qualified variant names
    // aren't yet resolvable there).
    assert_eq!(compile_and_run(
        "data Shape is Circle(r: Int) | Bad provides Error\n\
         func run(s: Shape): Shape = s?\n\
         match run(Shape.Circle(r=4)) {\n\
         is Circle(r) then r\n\
         is Bad then -2\n\
         }"
    ), 4);
}

#[test]
fn test_try_outside_a_function_is_a_type_error() {
    let err = type_error(
        "error ParseError(msg: Str)\n\
         func parse(s: Str): Int | ParseError = if s == \"bad\" then ParseError(msg=\"nope\") else 42\n\
         parse(\"ok\")?"
    );
    assert!(err.contains("outside"), "unexpected error: {}", err);
}

#[test]
fn test_try_on_a_non_union_value_is_a_type_error() {
    let err = type_error("func f(): Int = 5?\nf()");
    assert!(err.contains("union"), "unexpected error: {}", err);
}

#[test]
fn test_try_on_a_union_with_no_error_member_is_a_type_error() {
    let err = type_error("func f(): Int? = { let x: Int? = 5\nx? }\nf()");
    assert!(err.contains("Error"), "unexpected error: {}", err);
}

#[test]
fn test_try_propagating_an_undeclared_error_is_a_type_error() {
    let err = type_error(
        "error ParseError(msg: Str)\n\
         func parse(s: Str): Int | ParseError = if s == \"bad\" then ParseError(msg=\"nope\") else 42\n\
         func run(s: Str): Int = parse(s)? * 2\n\
         run(\"bad\")"
    );
    assert!(err.contains("return type"), "unexpected error: {}", err);
}

// ── `catch` ──────────────────────────────────────────────────────────────

#[test]
fn test_catch_passes_through_the_ok_case() {
    assert_eq!(compile_and_run(
        "error ParseError(msg: Str)\n\
         func parse(s: Str): Int | ParseError = if s == \"bad\" then ParseError(msg=\"nope\") else 42\n\
         parse(\"ok\") catch -99"
    ), 42);
}

#[test]
fn test_catch_uses_the_fallback_value_on_error() {
    assert_eq!(compile_and_run(
        "error ParseError(msg: Str)\n\
         func parse(s: Str): Int | ParseError = if s == \"bad\" then ParseError(msg=\"nope\") else 42\n\
         parse(\"bad\") catch -99"
    ), -99);
}

#[test]
fn test_catch_lambda_handler_ignoring_the_bound_error() {
    assert_eq!(compile_and_run(
        "error ParseError(msg: Str)\n\
         func parse(s: Str): Int | ParseError = if s == \"bad\" then ParseError(msg=\"nope\") else 42\n\
         parse(\"bad\") catch [e] -> 777"
    ), 777);
}

#[test]
fn test_catch_lambda_handler_using_the_bound_error_field() {
    assert_eq!(compile_and_run(
        "error ParseError(code: Int)\n\
         func parse(s: Str): Int | ParseError = if s == \"bad\" then ParseError(code=13) else 42\n\
         parse(\"bad\") catch [e] -> e.code * 1000"
    ), 13000);
}

#[test]
fn test_catch_can_be_used_inside_a_let() {
    assert_eq!(compile_and_run(
        "error ParseError(msg: Str)\n\
         func parse(s: Str): Int | ParseError = if s == \"bad\" then ParseError(msg=\"nope\") else 42\n\
         let n = parse(\"bad\") catch 0\n\
         n + 1"
    ), 1);
}

#[test]
fn test_catch_on_a_union_with_no_error_member_is_a_type_error() {
    let err = type_error(
        "let x: Int | Bool = 5\n\
         x catch 0"
    );
    assert!(err.contains("Error"), "unexpected error: {}", err);
}

// ── `!` ──────────────────────────────────────────────────────────────────

#[test]
fn test_unwrap_passes_through_the_ok_case() {
    assert_eq!(compile_and_run(
        "error ParseError(msg: Str)\n\
         func parse(s: Str): Int | ParseError = if s == \"bad\" then ParseError(msg=\"nope\") else 42\n\
         parse(\"ok\")!"
    ), 42);
}

#[test]
fn test_unwrap_on_a_union_with_no_error_member_is_a_type_error() {
    let err = type_error(
        "let x: Int | Bool = 5\n\
         x!"
    );
    assert!(err.contains("Error"), "unexpected error: {}", err);
}

#[test]
fn test_try_survives_gc_pressure() {
    // Each iteration's `parse(s)?` either passes an `Int` through unchanged
    // or reconstructs and boxes a fresh `ParseError` on the error arm
    // (`union_entries`' nominal-with-fields path — see the module doc
    // comment) — forcing at least one real collection across 3000
    // iterations. If either path's shadow-frame rooting were out of sync,
    // the running `total` would corrupt partway through.
    assert_eq!(compile_and_run(
        "error ParseError(code: Int)\n\
         func parse(i: Int): Int | ParseError = if i - i / 7 * 7 == 0 then ParseError(code=i) else i\n\
         func run(i: Int): Int | ParseError = parse(i)? * 2\n\
         mut total = 0\n\
         for i in 0..3000 do {\n\
         total = total + match run(i) {\n\
         is Int(n) then n\n\
         is ParseError(c) then c.code\n\
         }\n\
         }\n\
         total"
    ), (0..3000i64).map(|i| if i - i / 7 * 7 == 0 { i } else { i * 2 }).sum::<i64>());
}

// `e!`'s error arm genuinely traps the process (a deliberate placeholder —
// see `ERRORS.md` Phase 5, real unwinding is `CONCURRENCY.md` stage 1), so
// actually exercising it here would abort the test binary; only its
// type-checking and the ok-path are covered.

// ── `panic` as an ordinary builtin ──────────────────────────────────────────

#[test]
fn test_panic_is_directly_callable_and_unifies_with_any_branch_type() {
    // `panic` isn't a dedicated node reachable only through `!` — it's a
    // plain `Function`-typed builtin (`default_context`), so it's callable
    // like any other function, and (being `Never`-typed) unifies with
    // whatever real value sits on the other side of a branch. The `false`
    // branch is never taken at runtime, so this never actually traps —
    // exercising the ordinary `Never`-returning-`Call` codegen path
    // without aborting the test binary.
    assert_eq!(compile_and_run(
        "if true then 5 else panic(\"unreachable\")"
    ), 5);
}

#[test]
fn test_unwrap_desugars_to_a_panic_call_type_checking_identically() {
    // `!`'s error arm is exactly `panic(msg)`, so a function whose `!`
    // never actually reaches its error arm should type-check and run
    // identically to calling `panic` by hand in the analogous position.
    assert_eq!(compile_and_run(
        "error ParseError(msg: Str)\n\
         func parse(s: Str): Int | ParseError = if s == \"bad\" then ParseError(msg=\"nope\") else 42\n\
         parse(\"ok\")! + 1"
    ), 43);
}
