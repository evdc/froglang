//! `read(s): T | ReadError` (`plans/DATA.md` stage 5) — one assertion per
//! form `TypeChecker::build_read` handles, the round-trip law
//! (`read(repr(x))! == x`) per form, and the errors `is_read`/`lower_read`
//! and `runtime::read`'s sticky-error accessors raise.

mod common;
use common::{run, run_raw, run_gc_stress};

fn type_error(src: &str) -> String {
    let ast = froglang_core::frontend::parser::Parser::parse(src).expect("parse error");
    let mut tc = froglang_core::frontend::typeck::TypeChecker::new();
    let err = tc.check_and_lower(ast).expect_err("expected a type error");
    err.item.msg
}

// ── the law, per form ────────────────────────────────────────────────────────

#[test]
fn the_law_holds_for_scalars() {
    // `None` is deliberately not in this table: `None | ReadError` hits a
    // pre-existing, unrelated bug where `==` (not `read`/`repr` — `match`
    // narrowing is fine, and `Int | None` compares correctly) is wrong for
    // a union whose only members are `None` plus a struct/error type.
    // `the_law_holds_for_an_optional` below covers `None` inside a wider
    // (`Int | None`) union instead, which isn't affected.
    let cases = [
        ("let x = 42", "Int"),
        ("let x = -7", "Int"),
        ("let x = 1.5", "Float"),
        ("let x = true", "Bool"),
        ("let x = \"hi\\n\"", "Str"),
    ];
    for (decl, ty) in cases {
        let src = format!("{decl}\nlet back: {ty} | ReadError = read(repr(x))\nprint(back! == x)");
        assert_eq!(run(&src), "true\n", "failed for {ty}: {src}");
    }
}

#[test]
fn the_law_holds_for_a_list_and_a_range() {
    assert_eq!(
        run("let x = [1, 2, 3]\nlet back: List<Int> | ReadError = read(repr(x))\nprint(back! == x)"),
        "true\n",
    );
    assert_eq!(
        run("let x = 0..10\nlet back: Range<Int> | ReadError = read(repr(x))\nprint(back!.start == x.start)\nprint(back!.end == x.end)"),
        "true\ntrue\n",
    );
}

#[test]
fn the_law_holds_for_a_named_struct() {
    let src = r#"data Person(name: Str, age: Int)
let x = Person(name="Alice", age=42)
let back: Person | ReadError = read(repr(x))
print(back! == x)"#;
    assert_eq!(run(src), "true\n");
}

#[test]
fn the_law_holds_for_a_positional_struct() {
    assert_eq!(
        run("data Lit(Int)\nlet x = Lit(42)\nlet back: Lit | ReadError = read(repr(x))\nprint(back! == x)"),
        "true\n",
    );
}

#[test]
fn the_law_holds_for_a_nested_struct() {
    let src = r#"data Address(city: Str, zip: Int)
data Person(name: Str, address: Address)
let x = Person(name="Ada", address=Address(city="London", zip=123))
let back: Person | ReadError = read(repr(x))
print(back! == x)"#;
    assert_eq!(run(src), "true\n");
}

#[test]
fn the_law_holds_for_a_list_of_structs() {
    let src = r#"data Person(name: Str, age: Int)
let x = [Person(name="Ada", age=36), Person(name="Bo", age=1)]
let back: List<Person> | ReadError = read(repr(x))
print(back! == x)"#;
    assert_eq!(run(src), "true\n");
}

#[test]
fn the_law_holds_for_a_nominal_union_every_variant() {
    let src = r#"data Shape(color: Str) is Circle(r: Int) | Rect(w: Int, h: Int)
let a = Circle(color="red", r=4)
let back_a: Shape | ReadError = read(repr(a))
print(back_a! == a)
let b = Rect(color="blue", w=2, h=3)
let back_b: Shape | ReadError = read(repr(b))
print(back_b! == b)"#;
    assert_eq!(run(src), "true\ntrue\n");
}

#[test]
fn the_law_holds_for_a_nominal_union_with_positional_variant_fields() {
    // `repr` writes a positional ("tuple struct") variant's fields bare
    // (`GenU.A(7)`, matching `print`), so `read` has to take them back by
    // position too — reading them by their synthetic "0"/"1" field names
    // fails with `missing field '0'`, which `!` then aborts on.
    let src = r#"data GenU is A(Int) | B(Str, Bool)
let a: GenU = GenU.A(7)
let back_a: GenU | ReadError = read(repr(a))
print(back_a! == a)
let b: GenU = GenU.B("hi", true)
let back_b: GenU | ReadError = read(repr(b))
print(back_b! == b)"#;
    assert_eq!(run(src), "true\ntrue\n");
}

#[test]
fn the_law_holds_for_a_boxed_nominal_union() {
    // Past `MAX_INLINE_UNION_MEMBERS` the target union (`Big | ReadError`,
    // seven members) is boxed, where a nominal union's tag — the variant's
    // *declaration* index — and an anonymous union's — the normalized
    // (display-sorted) member position — are different numbers.
    // Declaration order here is deliberately the reverse of sorted order,
    // so stamping the wider union onto a `VariantInit` reads the value
    // back as the wrong variant instead of happening to agree.
    let src = r#"data Big is Zed(a: Int) | Yank(b: Int) | Xray(c: Int) | Whisky(d: Int) | Victor(e: Int) | Uniform(f: Int)
let x: Big = Big.Zed(a=42)
let back: Big | ReadError = read(repr(x))
print(repr(back))"#;
    // Asserted through `repr` on the un-unwrapped result rather than
    // `back! == x`: `!` here narrows to a *partial* union (`Big.Yank |
    // Big.Zed`), and using that at type `Big` would need the
    // union-into-union widening the language doesn't have yet — an
    // unrelated pre-existing gap. `repr` still shows which variant came
    // back, which is the whole point of the case.
    assert_eq!(run(src), "Big.Zed(a=42)\n");
}

#[test]
fn the_law_holds_for_a_nullary_variant() {
    let src = "data Status is Ok | Err\nlet x = Ok\nlet back: Status | ReadError = read(repr(x))\nprint(back! == x)";
    assert_eq!(run(src), "true\n");
}

#[test]
fn the_law_holds_for_an_anonymous_union() {
    let src = r#"let a: Int | Str = 5
let back_a: (Int | Str) | ReadError = read(repr(a))
print(back_a! == a)
let b: Int | Str = "hi"
let back_b: (Int | Str) | ReadError = read(repr(b))
print(back_b! == b)"#;
    assert_eq!(run(src), "true\ntrue\n");
}

#[test]
fn the_law_holds_for_an_optional() {
    let src = r#"let a: Int | None = none
let back_a: (Int | None) | ReadError = read(repr(a))
print(back_a! == a)
let b: Int | None = 7
let back_b: (Int | None) | ReadError = read(repr(b))
print(back_b! == b)"#;
    assert_eq!(run(src), "true\ntrue\n");
}

// ── malformed input ──────────────────────────────────────────────────────────

#[test]
fn a_parse_error_yields_a_readerror_not_a_crash() {
    assert_eq!(run(r#"let x: Int | ReadError = read("(")
print(x catch [e] -> "err")"#), "\"err\"\n");
}

#[test]
fn a_shape_mismatch_yields_a_readerror() {
    assert_eq!(run(r#"let x: Int | ReadError = read("\"not an int\"")
print(x catch [e] -> "err")"#), "\"err\"\n");
}

#[test]
fn a_missing_struct_field_yields_a_readerror() {
    let src = r#"data Person(name: Str, age: Int)
let x: Person | ReadError = read("Person(name=\"Alice\")")
print(x catch [e] -> "err")"#;
    assert_eq!(run(src), "\"err\"\n");
}

#[test]
fn an_unmatched_nominal_union_variant_yields_a_readerror() {
    // `x catch [e] -> "err"` would join `Shape` (itself a multi-member
    // union) with the handler's `Str` — a pre-existing `catch` limitation
    // (`build_catch_arms`) unrelated to `read`, hitting the same
    // union-into-union widening gap `build_read_union_as` sidesteps for
    // `read` itself. A same-typed fallback avoids needing that join.
    let src = r#"data Shape(color: Str) is Circle(r: Int) | Rect(w: Int, h: Int)
let x: Shape | ReadError = read("Triangle(color=\"red\")")
let ok = x catch Circle(color="fallback", r=-1)
print(ok.color)"#;
    assert_eq!(run(src), "fallback\n");
}

#[test]
fn a_readerror_carries_the_offset_of_the_offending_node() {
    // End-to-end counterpart to `runtime::read`'s own offset unit tests:
    // every shape-mismatch site reports the position of the node it was
    // handed, so `offset` is meaningful for mismatches and not just for
    // parse errors. `"nope"` starts at byte 23; the inner `Inner(` that
    // should have carried `b` starts at byte 13.
    let src = r#"data Person(name: Str, age: Int)
let x: Person | ReadError = read("Person(name=\"Ada\", age=\"nope\")")
print(x catch [e] -> repr(e))"#;
    assert!(run(src).contains("offset=23"), "unexpected: {}", run(src));

    let src = r#"data Inner(a: Int, b: Int)
data Outer(x: Int, y: Inner)
let x: Outer | ReadError = read("Outer(x=1, y=Inner(a=2))")
print(x catch [e] -> repr(e))"#;
    assert!(run(src).contains("offset=13"), "unexpected: {}", run(src));
}

// ── type errors ──────────────────────────────────────────────────────────────

#[test]
fn bare_read_with_no_expected_type_is_a_type_error() {
    let err = type_error("read(\"1\")");
    assert!(err.contains("annotate the expected type"), "unexpected: {}", err);
}

#[test]
fn read_without_readerror_in_the_expected_type_is_a_type_error() {
    let err = type_error("let x: Int = read(\"1\")");
    assert!(err.contains("ReadError"), "unexpected: {}", err);
}

#[test]
fn read_with_a_non_str_argument_is_a_type_error() {
    let err = type_error("let x: Int | ReadError = read(1)");
    assert!(err.contains("Str"), "unexpected: {}", err);
}

#[test]
fn read_of_a_recursive_union_is_rejected() {
    let src = "data Node(v: Int, kids: List<Node>)\nlet x: Node | ReadError = read(\"\")";
    let err = type_error(src);
    assert!(err.to_lowercase().contains("recursive"), "unexpected: {}", err);
}

#[test]
fn read_of_two_struct_shaped_anonymous_union_members_is_rejected() {
    let src = r#"data A(x: Int)
data B(y: Int)
let z: (A | B) | ReadError = read("")"#;
    let err = type_error(src);
    assert!(err.contains("can't tell apart"), "unexpected: {}", err);
}

#[test]
fn read_of_two_struct_shaped_members_nested_in_a_struct_field_is_rejected() {
    // Same ambiguity as the top-level case above, reached recursively —
    // the kind tests `build_read_anon_union` emits are identical wherever
    // the union sits, so this used to compile and silently return
    // `ReadError(msg="missing field 'a'")` instead of the value.
    let src = r#"data A(x: Int)
data B(y: Int)
data W(f: A | B)
let z: W | ReadError = read("")"#;
    let err = type_error(src);
    assert!(err.contains("can't tell apart"), "unexpected: {}", err);
}

#[test]
fn read_of_two_struct_shaped_members_nested_in_a_list_is_rejected() {
    let src = r#"data A(x: Int)
data B(y: Int)
let z: List<A | B> | ReadError = read("")"#;
    let err = type_error(src);
    assert!(err.contains("can't tell apart"), "unexpected: {}", err);
}

#[test]
fn the_law_holds_for_an_unambiguous_union_nested_in_a_struct_field() {
    // The nested check must reject only what the kind tests genuinely
    // can't separate: one struct-shaped member alongside scalars is fine.
    let src = r#"data A(x: Int)
data W(f: A | Int)
let a = W(f=A(x=1))
let back_a: W | ReadError = read(repr(a))
print(back_a! == a)
let b = W(f=7)
let back_b: W | ReadError = read(repr(b))
print(back_b! == b)"#;
    assert_eq!(run(src), "true\ntrue\n");
}

#[test]
fn read_with_the_wrong_arity_is_a_type_error() {
    assert_ne!(run_raw("let x: Int | ReadError = read(\"1\", \"2\")").status, Some(0));
}

// ── GC ───────────────────────────────────────────────────────────────────────

#[test]
fn reading_nested_structures_survives_gc_stress() {
    let src = r#"data Person(name: Str, age: Int)
let x = [Person(name="Ada", age=36), Person(name="Bo", age=1)]
let back: List<Person> | ReadError = read(repr(x))
print(back! == x)"#;
    assert_eq!(run_gc_stress(src), "true\n");
}
