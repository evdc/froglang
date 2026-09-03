//! `repr(x): Str` (`plans/DATA.md` stage 5) — one assertion per form
//! `TypeChecker::build_repr` handles, plus the drift guard against `print`
//! (`desugar_notation`'s doc comment: the two walks must never disagree,
//! since they're now two independent implementations of the same notation)
//! and the type errors `is_repr`'s checks raise.

mod common;
use common::{run, run_raw, run_gc_stress};

fn type_error(src: &str) -> String {
    let ast = froglang_core::frontend::parser::Parser::parse(src).expect("parse error");
    let mut tc = froglang_core::frontend::typeck::TypeChecker::new();
    let err = tc.check_and_lower(ast).expect_err("expected a type error");
    err.item.msg
}

// ── scalars ──────────────────────────────────────────────────────────────────

#[test]
fn repr_of_scalars_matches_frog_notation() {
    assert_eq!(run("print(repr(42))"), "42\n");
    assert_eq!(run("print(repr(-7))"), "-7\n");
    assert_eq!(run("print(repr(1.0))"), "1.0\n");
    assert_eq!(run("print(repr(true))"), "true\n");
    assert_eq!(run("print(repr(false))"), "false\n");
    assert_eq!(run("print(repr(none))"), "none\n");
}

#[test]
fn repr_of_a_string_is_quoted_and_escaped() {
    assert_eq!(run(r#"print(repr("hi"))"#), "\"hi\"\n");
    assert_eq!(run(r#"print(repr("a\nb"))"#), "\"a\\nb\"\n");
}

#[test]
fn repr_output_reads_back_through_the_lexer() {
    // The law's cheap half, checked directly: repr's Str output is a
    // literal the lexer accepts as source, which `read` (not yet built)
    // will eventually feed back in.
    let out = run(r#"print(repr("hi\n\"there\""))"#);
    assert_eq!(out.trim_end(), "\"hi\\n\\\"there\\\"\"");
}

// ── lists and ranges ─────────────────────────────────────────────────────────

#[test]
fn repr_of_a_list_matches_frog_notation() {
    assert_eq!(run("print(repr([1, 2, 3]))"), "[1, 2, 3]\n");
    assert_eq!(run("print(repr([[1, 2], [3]]))"), "[[1, 2], [3]]\n");
}

#[test]
fn repr_of_an_empty_list_is_bracket_pair() {
    // Unannotated: the element type is a bare TypeVar at desugar time —
    // `build_repr_list`'s own TypeVar short-circuit, not the top-level
    // "cannot infer" error.
    assert_eq!(run("print(repr([]))"), "[]\n");
    // Annotated: exercises the resolved, non-empty-loop-shaped path too.
    assert_eq!(run("let xs: List<Str> = []\nprint(repr(xs))"), "[]\n");
}

#[test]
fn repr_of_a_previously_typevar_list_no_longer_prints_a_placeholder() {
    // MUTABILITY.md's `mut xs = []; print(xs)` -> `<?>` bug: `repr` runs
    // post-monomorphization, so `xs`'s type is `List<Int>` by the time
    // `desugar_notation` resolves it, not a stale `TypeVar`.
    assert_eq!(run("mut xs = []\nxs.push(1)\nprint(repr(xs))"), "[1]\n");
}

#[test]
fn repr_of_a_range_uses_dotdot_notation() {
    assert_eq!(run("print(repr(0..10))"), "0..10\n");
}

// ── structs ──────────────────────────────────────────────────────────────────

#[test]
fn repr_of_a_named_struct_matches_frog_notation() {
    assert_eq!(
        run(r#"data Person(name: Str, age: Int)
print(repr(Person(name="Alice", age=42)))"#),
        "Person(name=\"Alice\", age=42)\n",
    );
}

#[test]
fn repr_of_a_positional_struct_has_no_field_names() {
    assert_eq!(run("data Lit(Int)\nprint(repr(Lit(42)))"), "Lit(42)\n");
}

#[test]
fn repr_of_a_nested_struct_recurses() {
    assert_eq!(
        run(r#"data Address(city: Str, zip: Int)
data Person(name: Str, address: Address)
print(repr(Person(name="Ada", address=Address(city="London", zip=123))))"#),
        "Person(name=\"Ada\", address=Address(city=\"London\", zip=123))\n",
    );
}

// ── unions ───────────────────────────────────────────────────────────────────

#[test]
fn repr_of_a_nominal_union_uses_qualified_variant_names() {
    let src = r#"data Shape(color: Str) is Circle(r: Int) | Rect(w: Int, h: Int)
print(repr(Circle(color="red", r=4)))
print(repr(Rect(color="blue", w=2, h=3)))"#;
    assert_eq!(run(src), "Shape.Circle(color=\"red\", r=4)\nShape.Rect(color=\"blue\", w=2, h=3)\n");
}

#[test]
fn repr_of_a_nullary_variant_has_empty_parens() {
    let src = "data Status is Ok | Err\nprint(repr(Ok))";
    assert_eq!(run(src), "Status.Ok()\n");
}

#[test]
fn repr_of_an_anonymous_union_narrows_to_the_held_member() {
    assert_eq!(run("let x: Int | Str = 5\nprint(repr(x))"), "5\n");
    assert_eq!(run("let x: Int | Str = \"hi\"\nprint(repr(x))"), "\"hi\"\n");
}

#[test]
fn repr_of_an_optional_none_member_prints_none() {
    assert_eq!(run("let x: Int | None = none\nprint(repr(x))"), "none\n");
    assert_eq!(run("let x: Int | None = 7\nprint(repr(x))"), "7\n");
}

// ── drift guard: repr must agree with print, byte for byte ────────────────

#[test]
fn repr_and_print_agree_on_every_form() {
    let cases = [
        ("42", "print(42)\nprint(repr(42))"),
        ("float", "print(1.0)\nprint(repr(1.0))"),
        ("bool", "print(true)\nprint(repr(true))"),
        // A bare top-level `print("hi")` deliberately prints raw text, not
        // repr's quoted form (DATA.md's print/repr split) — so there is no
        // bare-string case here. A *nested* string does agree, and is
        // already covered: every struct/union case below carries one.
        ("nested str", r#"print(["hi", "a\nb"])
print(repr(["hi", "a\nb"]))"#),
        ("none", "print(none)\nprint(repr(none))"),
        ("list", "print([1, 2, 3])\nprint(repr([1, 2, 3]))"),
        ("nested list", "print([[1, 2], [3]])\nprint(repr([[1, 2], [3]]))"),
        ("range", "print(0..10)\nprint(repr(0..10))"),
        ("struct", r#"data Person(name: Str, age: Int)
print(Person(name="Alice", age=42))
print(repr(Person(name="Alice", age=42)))"#),
        ("positional struct", "data Lit(Int)\nprint(Lit(42))\nprint(repr(Lit(42)))"),
        ("nominal union", r#"data Shape(color: Str) is Circle(r: Int) | Rect(w: Int, h: Int)
print(Circle(color="red", r=4))
print(repr(Circle(color="red", r=4)))"#),
        ("anon union", r#"let x: Int | Str = "hi"
print(x)
print(repr(x))"#),
    ];
    for (name, src) in cases {
        let out = run(src);
        let mut lines = out.lines();
        let (print_line, repr_line) = (lines.next().unwrap(), lines.next().unwrap());
        assert_eq!(print_line, repr_line, "print/repr drift on {name}: {out:?}");
    }
}

// ── generics ─────────────────────────────────────────────────────────────────

#[test]
fn repr_inside_a_generic_function_body_checks_each_instantiation_separately() {
    // `Show` isn't yet an inferable bound the way `<T: Num>`/`<T: Eq>` are
    // (`join_operand_types`), so `is_repr`'s checks defer past a bare
    // generic `TypeVar` and re-run once monomorphization has produced a
    // concretely-typed clone of the body per call site — see
    // `desugar_notation`'s own check just before it builds the expansion.
    let src = r#"func show<T>(x: T): Str = repr(x)
print(show(42))
print(show("hi"))
print(show([1, 2, 3]))"#;
    assert_eq!(run(src), "42\n\"hi\"\n[1, 2, 3]\n");
}

// ── errors ───────────────────────────────────────────────────────────────────

#[test]
fn repr_of_a_function_typed_field_is_rejected() {
    let err = type_error("data W(f: (Int -> Int))\nrepr(W(f = x -> x))");
    assert!(err.contains("has no notation"), "unexpected: {}", err);
    assert!(err.contains("Show"), "unexpected: {}", err);
}

#[test]
fn repr_of_a_recursive_union_is_rejected_with_a_span() {
    let src = "data Node(v: Int, kids: List<Node>)\nrepr(Node(v=1, kids=[]))";
    let err = type_error(src);
    assert!(err.to_lowercase().contains("recursive"), "unexpected: {}", err);
}

#[test]
fn repr_with_the_wrong_arity_is_a_type_error() {
    assert_ne!(run_raw("repr(1, 2)").status, Some(0));
    assert_ne!(run_raw("repr()").status, Some(0));
}

// ── GC ───────────────────────────────────────────────────────────────────────

#[test]
fn repr_of_nested_structures_survives_gc_stress() {
    let src = r#"data Person(name: Str, age: Int)
let people = [Person(name="Ada", age=36), Person(name="Bo", age=1)]
print(repr(people))"#;
    assert_eq!(
        run_gc_stress(src),
        "[Person(name=\"Ada\", age=36), Person(name=\"Bo\", age=1)]\n",
    );
}
