// Tests for Phase 6 (ERRORS.md implementation plan): `Trait::Truthy`
// coercion in condition position, and flow narrowing of a bindless `is`
// match arm's subject.
//
// `Trait::Truthy` is implemented structurally by `Int`/`Float`/`Str`/
// `List`/`None` (`Bool` needs no coercion at all) — deliberately *not* by
// a bare struct, so a union with any non-`Truthy` member (an `Error` type
// included) isn't `Truthy` either via the pre-existing "a union satisfies
// a trait iff every member does" rule, with no special-casing of `Error`
// needed here. Coercion only ever happens in condition position (`if`, a
// match/`for` guard, `and`/`or`/`not`) via `TypeChecker::coerce_truthy`,
// which wraps a non-`Bool` value in the new `TypedExprKind::Truthy` node;
// it never applies anywhere else (e.g. `let ok: Bool = 5` is still a type
// error).
//
// Flow narrowing: a bindless `is Variant`/`is Type` match arm, when the
// subject is itself a bare already-bound identifier, rebinds that name —
// inside the arm's own scope only — to the matched member's own narrower
// type, so e.g. `u.code` inside `is DbError` reads `DbError`'s own field
// directly with no explicit binder. For a nominal union this required
// registering each qualified variant name (`Shape.Circle`) as an ordinary
// struct too (`hoist_data_decls`), reconstructed via `StructInit` reading
// every field (common, then the variant's own) back out through
// `FieldAccess`/`VariantField` — for an anonymous union it reuses the
// existing `Narrow` node exactly as an explicit bind already did.
//
// `is Error` (a trait-bound match arm) expands, before the ordinary
// per-arm loop runs, into one concrete arm per `Error`-providing member —
// same guard/body cloned into each — so exhaustiveness, binding, and flow
// narrowing all fall out of the *existing* per-arm machinery with no new
// runtime representation.

use froglang_core::codegen::compile_and_run;

fn type_error(src: &str) -> String {
    let ast = froglang_core::frontend::parser::Parser::parse(src).expect("parse error");
    let mut tc = froglang_core::frontend::typeck::TypeChecker::new();
    let err = tc.check_and_lower(ast).expect_err("expected a type error");
    format!("{:?}", err)
}

// ── Truthy ───────────────────────────────────────────────────────────────

#[test]
fn test_truthy_int_and_float_in_if() {
    let src = "let a = if 5 then 1 else 0\nlet b = if 0 then 1 else 0\nlet c = if 0.0 then 1 else 0\na * 100 + b * 10 + c";
    assert_eq!(compile_and_run(src), 100);
}

#[test]
fn test_truthy_str_and_list_and_none_in_if() {
    let src = "let a = if \"\" then 1 else 0\nlet b = if \"x\" then 1 else 0\nlet c = if [] then 1 else 0\nlet d = if [1] then 1 else 0\nlet e = if none then 1 else 0\na * 10000 + b * 1000 + c * 100 + d * 10 + e";
    assert_eq!(compile_and_run(src), 1010);
}

#[test]
fn test_truthy_and_or_not_short_circuit_with_non_bool_operands() {
    // `and`/`or` must still short-circuit (right-hand side never evaluated
    // when the left already decides it) even when the operands are
    // Truthy-coerced rather than already Bool.
    let src = "if (3 and 4) and not 0 then 1 else 0";
    assert_eq!(compile_and_run(src), 1);
}

#[test]
fn test_truthy_for_loop_guard() {
    let src = "let xs = [0, 1, 0, 2, 3]\nmut total = 0\nfor x in xs if x do { total = total + x }\ntotal";
    assert_eq!(compile_and_run(src), 6);
}

#[test]
fn test_truthy_match_guard() {
    // The guard `n` (an `Int`, not a `Bool`) must Truthy-coerce: 0 is
    // falsey (guard fails, falls through to `-1`), 5 is truthy (guard
    // passes, `50`).
    let src = "\
        data W() is V(n: Int)\n\
        func f(n: Int): Int = match W.V(n=n) {\n\
        is V(n) and n then n * 10\n\
        else -1\n\
        }\n\
        f(0) * 1000 + f(5)";
    assert_eq!(compile_and_run(src), -1000 + 50);
}

#[test]
fn test_truthy_rejected_for_non_truthy_type() {
    // A Function is neither Bool nor Truthy.
    let msg = type_error("let f = [x] -> x\nif f then 1 else 0");
    assert!(msg.contains("condition must be Bool or a Truthy type"), "{}", msg);
}

#[test]
fn test_truthy_not_implicit_outside_condition_position() {
    // Truthy coercion is condition-position only — assigning an Int where
    // Bool is declared is still a type error.
    let msg = type_error("let ok: Bool = 5");
    assert!(msg.contains("Expected") || msg.contains("Bool"), "{}", msg);
}

#[test]
fn test_error_union_is_not_truthy() {
    // A union with an `Error` member is not `Truthy` — `Error` doesn't
    // implement it, so the union fails the "every member does" rule, with
    // no special-casing of `Error` needed in `type_implements`.
    let msg = type_error("error DbError(code: Int)\nfunc f(x: Int | DbError): Int = if x then 1 else 0\n0");
    assert!(msg.contains("condition must be Bool or a Truthy type"), "{}", msg);
}

// ── Flow narrowing ───────────────────────────────────────────────────────

#[test]
fn test_flow_narrowing_nominal_bindless_arm_reads_own_field() {
    let src = "\
        data Shape(tag: Int) is Circle(r: Int) | Rect(w: Int, h: Int)\n\
        func area(s: Shape): Int = match s {\n\
        is Circle then s.r * s.r + s.tag\n\
        is Rect then s.w * s.h + s.tag\n\
        }\n\
        area(Shape.Circle(tag=1, r=5))";
    assert_eq!(compile_and_run(src), 26);
}

#[test]
fn test_flow_narrowing_nominal_explicit_bind_still_works() {
    // An explicit bind takes priority over narrowing and is unaffected.
    let src = "\
        data Shape(tag: Int) is Circle(r: Int) | Rect(w: Int, h: Int)\n\
        func area(s: Shape): Int = match s {\n\
        is Circle(r) then r * r\n\
        is Rect(w, h) then w * h\n\
        }\n\
        area(Shape.Rect(tag=0, w=3, h=4))";
    assert_eq!(compile_and_run(src), 12);
}

#[test]
fn test_flow_narrowing_anonymous_union() {
    let src = "func describe(x: Int | Str): Int = if x is Int then x + 1 else 0\ndescribe(41)";
    assert_eq!(compile_and_run(src), 42);
}

#[test]
fn test_flow_narrowing_only_applies_to_bound_identifier_subject() {
    // A non-identifier subject (a call result) still matches correctly —
    // narrowing just doesn't try (and doesn't need to) rebind anything.
    let src = "\
        data Shape(tag: Int) is Circle(r: Int) | Rect(w: Int, h: Int)\n\
        func make(): Shape = Shape.Circle(tag=0, r=6)\n\
        match make() {\n\
        is Circle then 1\n\
        is Rect then 2\n\
        }";
    assert_eq!(compile_and_run(src), 1);
}

// ── `is Error` trait arms ────────────────────────────────────────────────

#[test]
fn test_is_error_trait_arm_anonymous_union() {
    let src = "\
        error DbError(code: Int)\n\
        func classify(x: Int | DbError): Int = match x {\n\
        is Int then x\n\
        is Error then -1\n\
        }\n\
        classify(DbError(code=5)) * 1000 + classify(7)";
    assert_eq!(compile_and_run(src), -1000 + 7);
}

#[test]
fn test_is_error_trait_arm_nominal_union() {
    let src = "\
        data Response(status: Int) is DbError(code: Int) | NetError(host: Str) provides Error\n\
        func classify(r: Response): Int = match r {\n\
        is Error then r.status * 100 + 1\n\
        }\n\
        classify(Response.DbError(status=7, code=99))";
    assert_eq!(compile_and_run(src), 701);
}

#[test]
fn test_is_error_trait_arm_still_requires_exhaustiveness() {
    let msg = type_error("\
        error DbError(code: Int)\n\
        func classify(x: Int | DbError): Int = match x { is Error then -1 }\n\
        0");
    assert!(msg.contains("Non-exhaustive"), "{}", msg);
}

#[test]
fn test_is_error_trait_pattern_rejects_binds() {
    let msg = type_error("\
        error DbError(code: Int)\n\
        func classify(x: Int | DbError): Int = match x {\n\
        is Int then x\n\
        is Error(e) then -1\n\
        }\n\
        0");
    assert!(msg.contains("cannot bind fields"), "{}", msg);
}
