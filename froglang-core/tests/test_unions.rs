// Tests for the Phase 2 union/enum unification (ERRORS.md implementation
// plan): `Type::Enum` is gone. `data X is A | B(...)` now desugars to a
// nominal `Type::Union` — a closed alias resolved through
// `TypeChecker::union_defs`/`union_names` — rather than its own dedicated
// `Type` variant. The runtime representation (`FrogVariant`, tag-as-
// declaration-index, unboxed immediates for nullary variants) is
// deliberately unchanged; `test_enums.rs` is the regression gate for that
// and should not need to change at all.
//
// These tests cover the ground that's new or was newly exercised while
// building this: single-member unions (a case the old `Type::Enum(name)`
// never had to distinguish from anything, since a bare name was never
// ambiguous with its own expansion) and a bug that surfaced through one —
// see `test_single_variant_exhaustive_match_no_else`.

use froglang_core::codegen::compile_and_run;

fn type_error(src: &str) -> String {
    let ast = froglang_core::frontend::parser::Parser::parse(src).expect("parse error");
    let mut tc = froglang_core::frontend::typeck::TypeChecker::new();
    let err = tc.check_and_lower(ast).expect_err("expected a type error");
    format!("{:?}", err)
}

#[test]
fn test_single_variant_union_construct_and_match() {
    assert_eq!(compile_and_run(
        "data Wrapper is Box(s: Str)\n\
         let w = Box(s=\"hello\")\n\
         match w { is Box(s) then if s == \"hello\" then 1 else 0 }"
    ), 1);
}

// Regression test: `lower_match` used to type a fully-covered match's
// (guard-free, no `else`) final synthesized fallback branch as `Type::None`
// instead of `Type::Never` — harmless for a multi-variant union, where a
// later `unify` against a real type absorbs the stray `None` via "concrete
// type is a member of the union it's compared against". A single-variant
// union has no later arm to do that absorbing, so the match's reported type
// came out `Int | None` instead of `Int`, and the whole program silently
// decoded to `FrogValue::None` at the embedding boundary — no panic, no
// type error, just a wrong answer. See `TypeChecker::lower_match`'s two
// `unwrap_or(Type::Never)` fallbacks (was `Type::None`).
#[test]
fn test_single_variant_exhaustive_match_no_else_types_as_the_arm_not_a_union() {
    let src = "data Wrapper is Box(s: Str)\n\
               let w = Box(s=\"hello\")\n\
               match w { is Box(s) then if s == \"hello\" then 1 else 0 }";
    let ast = froglang_core::frontend::parser::Parser::parse(src).expect("parse error");
    let mut tc = froglang_core::frontend::typeck::TypeChecker::new();
    let typed = tc.check_and_lower(ast).expect("type error");
    assert_eq!(typed.item.ty, froglang_core::frontend::typeck::Type::Int);
}

#[test]
fn test_single_variant_union_with_guarded_arm_and_no_else() {
    // Same fix, guarded-arm path (`lower_match`'s nested-Conditional shape
    // for a guard): the guard's own false path is the one that used to be
    // mistyped as `None`.
    assert_eq!(compile_and_run(
        "data Wrapper is Box(n: Int)\n\
         let w = Box(n=5)\n\
         match w {\n\
         is Box(n) and n > 0 then 1\n\
         is Box(n) then 0\n\
         }"
    ), 1);
}

#[test]
fn test_recursive_union_tree_sum() {
    assert_eq!(compile_and_run(
        "data Tree is Leaf | Node(l: Tree, r: Tree, v: Int)\n\
         func sum(t: Tree): Int = match t {\n\
         is Leaf then 0\n\
         is Node(l, r, v) then sum(l) + sum(r) + v\n\
         }\n\
         sum(Node(l=Node(l=Leaf, r=Leaf, v=1), r=Leaf, v=2))"
    ), 3);
}

#[test]
fn test_union_common_field_with_qualified_member_name() {
    assert_eq!(compile_and_run(
        "data Shape(color: Str) is Circle(r: Int) | Rect(w: Int, h: Int)\n\
         let s = Circle(color=\"red\", r=4)\n\
         if s.color == \"red\" then match s {\n\
         is Circle(r) then r * r\n\
         is Rect(w, h) then w * h\n\
         } else -1"
    ), 16);
}

#[test]
fn test_unknown_union_alias_name_is_a_type_error() {
    let err = type_error("let x: Nope = 1");
    assert!(err.contains("Unknown type 'Nope'"), "unexpected error: {}", err);
}
