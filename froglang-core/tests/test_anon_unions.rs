// Tests for Phase 3 (ERRORS.md implementation plan): boxing a value into
// an *anonymous* union (`Int | Str`, `T?`) so it's actually representable
// at runtime, not just type-checked.
//
// Before this, `Type::Union` never reached codegen except as a nominal
// union's own alias (`data X is A | B` — see `test_unions.rs`, Phase 2).
// An annotation like `let x: Int | Str = 5` type-checked but, if ever run,
// degenerated to a raw, untagged `i64` slot — GC-unsafe for any member
// that isn't already a heap pointer, and with no way to tell which member
// was actually stored. Scoped down from the original plan's 2-slot/
// `cond_mask` design (see the plan file): every non-`None` member is
// boxed the same way a nominal union's non-nullary member already is
// (`FrogVariant`, local tag = index in the union's own sorted member
// list), trading one extra allocation for a scalar member against not
// needing a global type-id registry or GC representation changes at all.
// `Str`/`List` members and widening an already-union-typed value into a
// different union are explicitly rejected for now (`TypeChecker::lower_widen`)
// rather than silently generating something wrong.

use froglang_core::codegen::compile_and_run;

fn type_error(src: &str) -> String {
    let ast = froglang_core::frontend::parser::Parser::parse(src).expect("parse error");
    let mut tc = froglang_core::frontend::typeck::TypeChecker::new();
    let err = tc.check_and_lower(ast).expect_err("expected a type error");
    format!("{:?}", err)
}

// ── T? (Int | None) ─────────────────────────────────────────────────────────

#[test]
fn test_optional_int_round_trips_through_a_function_boundary() {
    assert_eq!(compile_and_run(
        "func maybe(cond: Bool): Int? = if cond then 5 else none\n\
         match maybe(true) {\n\
         is Int(n) then n\n\
         is None then -1\n\
         } * 100 + match maybe(false) {\n\
         is Int(n) then n\n\
         is None then -1\n\
         }"
    ), 499);
}

#[test]
fn test_bare_is_type_test_on_optional_int() {
    assert_eq!(compile_and_run(
        "let x: Int? = 5\n\
         if x is Int then 1 else 0"
    ), 1);
    assert_eq!(compile_and_run(
        "let x: Int? = none\n\
         if x is Int then 1 else 0"
    ), 0);
}

#[test]
fn test_unannotated_function_returning_none_infers_optional() {
    // The body's own type (`Int | None`, from the `if`'s branch join) is
    // what the return slot unifies against — no annotation needed.
    assert_eq!(compile_and_run(
        "func maybe(cond: Bool) = if cond then 5 else none\n\
         if maybe(true) is Int then 1 else 0"
    ), 1);
}

// ── general anonymous unions: scalar and struct members ────────────────────

#[test]
fn test_scalar_or_struct_union_call_argument_widening() {
    assert_eq!(compile_and_run(
        "data Oops(msg: Str)\n\
         func classify(x: Int | Oops): Int = match x {\n\
         is Int(n) then n * 10\n\
         is Oops(o) then -1\n\
         }\n\
         classify(5) * 100 + classify(Oops(msg=\"bad\"))"
    ), 4999);
}

#[test]
fn test_struct_field_of_union_type() {
    assert_eq!(compile_and_run(
        "data Cell(v: Int | Str)\n\
         let c = Cell(v=5)\n\
         match c.v {\n\
         is Int(n) then n\n\
         is Str(s) then -1\n\
         }"
    ), 5);
}

#[test]
fn test_wildcard_bind_skips_narrowing_the_value() {
    assert_eq!(compile_and_run(
        "let x: Int | Bool = 7\n\
         match x {\n\
         is Int(_) then 1\n\
         is Bool(_) then 0\n\
         }"
    ), 1);
}

#[test]
fn test_scalar_union_member_survives_gc_pressure() {
    // Each loop iteration widens a fresh Int into `Int | Oops` (a real
    // heap allocation — see the module doc comment), forcing at least one
    // real collection before the loop ends. If the shadow-frame rooting
    // for `Widen`'s box (`for_each_heap_producer`'s matching arm) were out
    // of sync, the running `total` would corrupt partway through.
    assert_eq!(compile_and_run(
        "data Oops(msg: Str)\n\
         func classify(x: Int | Oops): Int = match x {\n\
         is Int(n) then n\n\
         is Oops(o) then -1\n\
         }\n\
         let total = 0\n\
         for i in 0..3000 do {\n\
         let v: Int | Oops = i\n\
         total = total + classify(v)\n\
         }\n\
         total"
    ), 3000 * 2999 / 2);
}

// ── errors ───────────────────────────────────────────────────────────────────

#[test]
fn test_non_exhaustive_anonymous_match_is_rejected() {
    let err = type_error(
        "func f(x: Int?): Int = match x {\nis Int(n) then n\n}"
    );
    assert!(err.contains("Non-exhaustive"), "unexpected error: {}", err);
}

#[test]
fn test_is_on_a_non_union_value_is_rejected() {
    let err = type_error("5 is Str");
    assert!(err.contains("Can only use 'is' on a union value"), "unexpected error: {}", err);
}

#[test]
fn test_str_member_is_not_yet_supported() {
    let err = type_error(
        "func describe(x: Int | Str): Int = match x {\nis Int(n) then n\nis Str(s) then -1\n}\ndescribe(\"hi\")"
    );
    assert!(err.contains("not yet supported"), "unexpected error: {}", err);
}

#[test]
fn test_assigning_into_a_union_typed_struct_field_widens() {
    // A field assignment places a value into a union-typed slot exactly as
    // much as `StructInit` does, so it needs the same `lower_widen` — the
    // field-assign branch of `Expression::Assign` used to skip it, storing
    // the raw immediate `9` over the boxed field and leaving the following
    // `TypeTag`/`Narrow` to dereference `9` as a `FrogVariant*`.
    assert_eq!(compile_and_run(
        "data Cell(v: Int | Bool)\n\
         let c = Cell(v=5)\n\
         c.v = 9\n\
         match c.v {\n\
         is Int(n) then n\n\
         is Bool(b) then -1\n\
         }"
    ), 9);
}
