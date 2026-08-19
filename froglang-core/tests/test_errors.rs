// Tests for Phase 4 (ERRORS.md implementation plan): `Trait::Error`, the
// `provides` clause, the `error X(...)` shorthand, and the must-handle
// rule (a statement-position value may not silently discard a possible
// Error).
//
// `Trait::Error` is granted, not structural: nothing implements it by
// construction, only by a `data`/`error` declaration's `provides` clause
// (`TypeChecker.provides`, consulted from `type_implements`). A union
// satisfies it iff every member does (`type_implements`'s existing
// `Type::Union` rule, unchanged since it predates this phase) — so
// `provides Error` on a `data X is A | B` declaration grants it to every
// variant, not the alias itself, and the union rule makes the alias
// satisfy it for free.
//
// `?`/`!`/`catch` (Phase 5) don't exist yet, so every test here reaches a
// fallible value's error side through `match`, the only handling
// mechanism available this phase.

use froglang_core::codegen::compile_and_run;

fn type_error(src: &str) -> String {
    let ast = froglang_core::frontend::parser::Parser::parse(src).expect("parse error");
    let mut tc = froglang_core::frontend::typeck::TypeChecker::new();
    let err = tc.check_and_lower(ast).expect_err("expected a type error");
    format!("{:?}", err)
}

#[test]
fn test_error_shorthand_and_must_handle_are_matched_correctly() {
    assert_eq!(compile_and_run(
        "error DbError(msg: Str)\n\
         func risky(fail: Bool): Int | DbError = if fail then DbError(msg=\"bad\") else 5\n\
         match risky(true) {\n\
         is Int(n) then n\n\
         is DbError(m) then -1\n\
         } * 100 + match risky(false) {\n\
         is Int(n) then n\n\
         is DbError(m) then -1\n\
         }"
    ), -95);
}

#[test]
fn test_data_with_explicit_provides_error_clause_behaves_like_error_shorthand() {
    assert_eq!(compile_and_run(
        "data DbError(msg: Str) provides Error\n\
         func risky(fail: Bool): Int | DbError = if fail then DbError(msg=\"bad\") else 5\n\
         match risky(true) {\n\
         is Int(n) then n\n\
         is DbError(m) then -1\n\
         }"
    ), -1);
}

#[test]
fn test_let_bound_fallible_value_is_not_must_handle_rejected() {
    // Binding to a name defers handling to wherever the name is later
    // used — it is not the same as discarding the value outright.
    assert_eq!(compile_and_run(
        "error DbError(msg: Str)\n\
         func risky(fail: Bool): Int | DbError = if fail then DbError(msg=\"bad\") else 5\n\
         let r = risky(true)\n\
         match r {\n\
         is Int(n) then n\n\
         is DbError(m) then -2\n\
         }"
    ), -2);
}

#[test]
fn test_discarding_a_fallible_statement_is_a_must_handle_error() {
    let err = type_error(
        "error DbError(msg: Str)\n\
         func risky(fail: Bool): Int | DbError = if fail then DbError(msg=\"bad\") else 5\n\
         risky(true)\n\
         5"
    );
    assert!(err.contains("unhandled error"), "unexpected error: {}", err);
}

#[test]
fn test_discarding_a_fallible_for_loop_body_is_a_must_handle_error() {
    let err = type_error(
        "error DbError(msg: Str)\n\
         func risky(fail: Bool): Int | DbError = if fail then DbError(msg=\"bad\") else 5\n\
         for i in 0..3 do risky(true)\n\
         5"
    );
    assert!(err.contains("unhandled error"), "unexpected error: {}", err);
}

#[test]
fn test_tail_position_fallible_value_is_exempt_from_must_handle() {
    // The tail value propagates to the caller (here, the whole program's
    // own result), so it isn't "discarded" — must-handle only fires on a
    // *non-tail* statement.
    assert_eq!(compile_and_run(
        "error DbError(msg: Str)\n\
         func risky(fail: Bool): Int | DbError = if fail then DbError(msg=\"bad\") else 5\n\
         match risky(false) {\n\
         is Int(n) then n\n\
         is DbError(m) then -1\n\
         }"
    ), 5);
}

#[test]
fn test_union_provides_error_grants_it_to_every_variant() {
    // `provides Error` on the union declaration grants the trait to each
    // variant (there's no `Type::Struct` for the alias itself to key
    // `provides` off — see `hoist_data_decls`), so a bare nominal-union
    // value (not even wrapped in a further anonymous union) is still
    // must-handle-rejected in statement position.
    let err = type_error(
        "data ParseError is UnexpectedEof | UnexpectedToken(tok: Str) provides Error\n\
         func fail(): ParseError = UnexpectedEof\n\
         fail()\n\
         5"
    );
    assert!(err.contains("unhandled error"), "unexpected error: {}", err);
}

#[test]
fn test_union_provides_error_variant_is_handled_via_match() {
    assert_eq!(compile_and_run(
        "data ParseError is UnexpectedEof | UnexpectedToken(tok: Str) provides Error\n\
         func fail(): ParseError = UnexpectedEof\n\
         match fail() {\n\
         is UnexpectedEof then -1\n\
         is UnexpectedToken(t) then -2\n\
         }"
    ), -1);
}

#[test]
fn test_unknown_provides_trait_name_is_a_type_error() {
    let err = type_error("data Oops(msg: Str) provides NotARealTrait\n5");
    assert!(err.contains("Unknown trait"), "unexpected error: {}", err);
}

#[test]
fn test_plain_struct_without_provides_does_not_trigger_must_handle() {
    // A struct with no `provides Error` is an ordinary value — using it
    // in statement position, even inside a union, is not an error.
    assert_eq!(compile_and_run(
        "data Point(x: Int, y: Int)\n\
         func maybe(fail: Bool): Int | Point = if fail then Point(x=1, y=2) else 9\n\
         maybe(true)\n\
         match maybe(false) {\n\
         is Int(n) then n\n\
         is Point(p) then -1\n\
         }"
    ), 9);
}
