//! `json.to_str(x)` / `json.parse(s): T | JsonError` (`plans/DATA.md`
//! stage 8).
//!
//! Two halves that mirror each other, and two things worth knowing before
//! reading:
//!
//!  - **The wire format is asserted literally**, not just round-tripped.
//!    A round-trip test passes just as happily against a private encoding
//!    nobody else can read, and JSON's whole point is that somebody else
//!    reads it — so every shape decision (`{"Circle":{"r":1}}`, positional
//!    structs as arrays, non-finite floats as `null`) has a test that spells
//!    out the exact bytes.
//!  - **Every test here runs identically under `--features json_simd`.**
//!    The DOM backend is a compile-time choice (`runtime/json/dom.rs`) and
//!    nothing in this file may depend on which one is in play.

mod common;
use common::{run, run_raw, run_gc_stress};

fn type_error(src: &str) -> String {
    let ast = froglang_core::frontend::parser::Parser::parse(src).expect("parse error");
    let mut tc = froglang_core::frontend::typeck::TypeChecker::new();
    let err = tc.check_and_lower(ast).expect_err("expected a type error");
    err.item.msg
}

// ── the wire format, spelled out ─────────────────────────────────────────────

#[test]
fn scalars_serialize_to_their_json_spellings() {
    assert_eq!(
        run("print(json.to_str(42))\nprint(json.to_str(-7))\nprint(json.to_str(1.5))\nprint(json.to_str(true))\nprint(json.to_str(false))"),
        "42\n-7\n1.5\ntrue\nfalse\n",
    );
}

#[test]
fn a_str_uses_jsons_escapes_not_frog_notations() {
    // `DATA.md`'s two tiers must not share an escape table. The visible
    // difference: frog notation escapes `'` and spells a codepoint
    // `\u{1F600}`; JSON does neither.
    assert_eq!(run(r#"print(json.to_str("a\"b\n\tc"))"#), "\"a\\\"b\\n\\tc\"\n");
    assert_eq!(run(r#"print(json.to_str("it's"))"#), "\"it's\"\n");
    assert_eq!(run(r#"print(json.to_str("\u{1F600}"))"#), "\"\u{1F600}\"\n");
}

#[test]
fn a_non_finite_float_serializes_as_null() {
    // JSON has no spelling for either, and `json.to_str` returns `Str`,
    // not `Str | JsonError` — so `null` is the only total answer.
    assert_eq!(
        run("print(json.to_str(1.0 / 0.0))\nprint(json.to_str(0.0 / 0.0))"),
        "null\nnull\n",
    );
}

#[test]
fn a_named_struct_is_an_object_in_declared_order() {
    assert_eq!(
        run("data Person(name: Str, age: Int)\nprint(json.to_str(Person(name=\"Alice\", age=42)))"),
        "{\"name\":\"Alice\",\"age\":42}\n",
    );
}

#[test]
fn a_positional_struct_is_an_array() {
    // A "tuple struct" has no field names to key an object by, and
    // froglang's internal synthetic `"0"`/`"1"` keys have no business on
    // the wire.
    assert_eq!(
        run("data Pair(Int, Str)\nprint(json.to_str(Pair(1, \"x\")))"),
        "[1,\"x\"]\n",
    );
}

#[test]
fn a_list_is_an_array_and_a_range_is_an_object() {
    assert_eq!(run("print(json.to_str([1, 2, 3]))"), "[1,2,3]\n");
    assert_eq!(run("print(json.to_str([]))"), "[]\n");
    assert_eq!(run("print(json.to_str(0..10))"), "{\"start\":0,\"end\":10}\n");
}

#[test]
fn a_nominal_union_is_externally_tagged() {
    // `{"Variant": payload}`, uniformly — including the two shapes
    // internal tagging could not represent at all, which is why this is
    // the default rather than `{"kind":"Circle",...}`.
    let src = "data Shape is Circle(r: Float) | Both(Int, Int) | Blank()\n\
               print(json.to_str(Shape.Circle(r=1.5)))\n\
               print(json.to_str(Shape.Both(3, 7)))\n\
               print(json.to_str(Shape.Blank()))";
    assert_eq!(run(src), "{\"Circle\":{\"r\":1.5}}\n{\"Both\":[3,7]}\n{\"Blank\":{}}\n");
}

#[test]
fn a_unions_common_fields_ride_in_the_variant_payload() {
    let src = "data Event(at: Int) is Click(x: Int) | Key(c: Str)\n\
               print(json.to_str(Event.Click(at=1, x=2)))";
    assert_eq!(run(src), "{\"Click\":{\"at\":1,\"x\":2}}\n");
}

#[test]
fn an_anonymous_union_serializes_permissively_and_untagged() {
    // Only *parsing* needs to tell members apart again; refusing to write
    // a value froglang can perfectly well describe would be a restriction
    // with nothing behind it.
    let src = "let a: Int | Str = 1\nlet b: Int | Str = \"x\"\nlet c: Int | None = none\n\
               print(json.to_str(a))\nprint(json.to_str(b))\nprint(json.to_str(c))";
    assert_eq!(run(src), "1\n\"x\"\nnull\n");
}

#[test]
fn nesting_composes() {
    let src = "data Tag(k: Str)\ndata Doc(id: Int, tags: List<Tag>, span: Range<Int>)\n\
               print(json.to_str(Doc(id=1, tags=[Tag(k=\"a\"), Tag(k=\"b\")], span=0..2)))";
    assert_eq!(
        run(src),
        "{\"id\":1,\"tags\":[{\"k\":\"a\"},{\"k\":\"b\"}],\"span\":{\"start\":0,\"end\":2}}\n",
    );
}

// ── the law: parse(to_str(x)) == x ───────────────────────────────────────────

#[test]
fn the_law_holds_for_scalars() {
    let cases = [
        ("let x = 42", "Int"),
        ("let x = -7", "Int"),
        ("let x = 1.5", "Float"),
        ("let x = true", "Bool"),
        ("let x = \"hi\\n\"", "Str"),
    ];
    for (decl, ty) in cases {
        let src = format!("{decl}\nlet back: {ty} | JsonError = json.parse(json.to_str(x))\nprint(back! == x)");
        assert_eq!(run(&src), "true\n", "failed for {ty}: {src}");
    }
}

#[test]
fn the_law_holds_for_composites() {
    assert_eq!(
        run("let x = [1, 2, 3]\nlet back: List<Int> | JsonError = json.parse(json.to_str(x))\nprint(back! == x)"),
        "true\n",
    );
    assert_eq!(
        run("data Person(name: Str, age: Int)\nlet x = Person(name=\"Alice\", age=42)\n\
             let back: Person | JsonError = json.parse(json.to_str(x))\nprint(back! == x)"),
        "true\n",
    );
    assert_eq!(
        run("data Pair(Int, Str)\nlet x = Pair(1, \"a\")\n\
             let back: Pair | JsonError = json.parse(json.to_str(x))\nprint(back! == x)"),
        "true\n",
    );
    assert_eq!(
        run("let x = 0..10\nlet back: Range<Int> | JsonError = json.parse(json.to_str(x))\n\
             print(back!.start == x.start)\nprint(back!.end == x.end)"),
        "true\ntrue\n",
    );
}

#[test]
fn the_law_holds_for_a_nominal_union() {
    // Via `match` rather than `!`: `!` on a value whose success type is
    // itself a union hits a pre-existing union-into-union widening gap in
    // the `!` desugaring, unrelated to json (`read` hits it identically —
    // see `plans/DATA.md` stage 5's note on `build_read_union_as`).
    let src = "data Shape is Circle(r: Float) | Rect(w: Int, h: Int)\n\
               let x = Shape.Rect(w=1, h=2)\n\
               let back: Shape | JsonError = json.parse(json.to_str(x))\n\
               print(match back { is JsonError(e) then \"err\" else repr(back) })";
    assert_eq!(run(src), "Shape.Rect(w=1, h=2)\n");
}

#[test]
fn the_law_holds_for_an_optional() {
    let src = "data Person(name: Str, nick: Str?)\n\
               let a = Person(name=\"x\", nick=\"n\")\nlet b = Person(name=\"x\", nick=none)\n\
               let ra: Person | JsonError = json.parse(json.to_str(a))\n\
               let rb: Person | JsonError = json.parse(json.to_str(b))\n\
               print(ra! == a)\nprint(rb! == b)";
    assert_eq!(run(src), "true\ntrue\n");
}

#[test]
fn the_law_holds_for_a_positional_struct_inside_an_optional() {
    // A positional struct is written as an *array*, so an anonymous union
    // it appears in has to dispatch on `frog_json_is_array` — classifying
    // it as an object (as anything reached through `as_struct_name` used
    // to be) makes a value fail to parse back from its own `json.to_str`
    // output, which is the one thing the law forbids. Both the nested
    // (`Pair?` as a field) and top-level (`Pair? | JsonError`) spellings,
    // since they reach the dispatch by different routes.
    let src = "data Pair(Int, Int)\n\
               data Holder(p: Pair?)\n\
               let h = Holder(p=Pair(1, 2))\n\
               print(json.to_str(h))\n\
               let rh: Holder | JsonError = json.parse(json.to_str(h))\n\
               print(rh! == h)\n\
               let e = Holder(p=none)\n\
               let re: Holder | JsonError = json.parse(json.to_str(e))\n\
               print(re! == e)\n\
               let top: Pair? | JsonError = json.parse(\"[3,4]\")\n\
               print(repr(top!))";
    assert_eq!(run(src), "{\"p\":[1,2]}\ntrue\ntrue\nPair(3, 4)\n");
}

#[test]
fn the_law_holds_for_a_union_nested_inside_a_struct() {
    // The composition that exercises the most of the parse path at once:
    // a `VariantInit` built from dynamic input, inside a `StructInit`,
    // alongside a list and an optional. Also the shape where `!` works
    // (the struct is the success type, not the union), unlike the
    // top-level-union case above.
    let src = "data Shape is Circle(r: Float) | Rect(w: Int, h: Int)\n\
               data Doc(name: Str, shape: Shape, tags: List<Str>, opt: Int?)\n\
               let a = Doc(name=\"d\", shape=Shape.Circle(r=2.0), tags=[\"a\", \"b\"], opt=3)\n\
               let b = Doc(name=\"d\", shape=Shape.Rect(w=1, h=2), tags=[], opt=none)\n\
               print(json.to_str(a))\n\
               let ra: Doc | JsonError = json.parse(json.to_str(a))\n\
               let rb: Doc | JsonError = json.parse(json.to_str(b))\n\
               print(ra! == a)\nprint(rb! == b)";
    assert_eq!(
        run(src),
        "{\"name\":\"d\",\"shape\":{\"Circle\":{\"r\":2.0}},\"tags\":[\"a\",\"b\"],\"opt\":3}\ntrue\ntrue\n",
    );
}

// ── parsing: the semantics decisions ─────────────────────────────────────────

#[test]
fn object_key_order_does_not_matter() {
    // Fields are fetched by key, so this is free — but it is the single
    // most load-bearing difference from a positional format, and worth
    // pinning.
    let src = "data Person(name: Str, age: Int)\n\
               let p: Person | JsonError = json.parse(\"{\\\"age\\\":42,\\\"name\\\":\\\"Alice\\\"}\")\n\
               print(repr(p!))";
    assert_eq!(run(src), "Person(name=\"Alice\", age=42)\n");
}

#[test]
fn an_optional_field_may_be_absent_or_null() {
    // Serde's rule: absence and `null` mean the same thing, because union
    // normalization flattens and dedups so froglang structurally cannot
    // express `Option<Option<T>>` for them to differ in.
    let src = "data Person(name: Str, nick: Str?)\n\
               let a: Person | JsonError = json.parse(\"{\\\"name\\\":\\\"x\\\"}\")\n\
               let b: Person | JsonError = json.parse(\"{\\\"name\\\":\\\"x\\\",\\\"nick\\\":null}\")\n\
               print(repr(a!))\nprint(repr(b!))";
    assert_eq!(run(src), "Person(name=\"x\", nick=none)\nPerson(name=\"x\", nick=none)\n");
}

#[test]
fn a_required_field_that_is_absent_or_null_is_an_error() {
    let src = "data Person(name: Str, age: Int)\n\
               let a: Person | JsonError = json.parse(\"{\\\"name\\\":\\\"x\\\"}\")\n\
               let b: Person | JsonError = json.parse(\"{\\\"name\\\":\\\"x\\\",\\\"age\\\":null}\")\n\
               print(repr(a))\nprint(repr(b))";
    let out = run(src);
    assert!(out.contains("missing key 'age'"), "expected a missing-key error, got: {out}");
    assert!(out.contains("expected an Int"), "expected a type error for null, got: {out}");
}

#[test]
fn an_integer_is_accepted_where_a_float_is_expected() {
    // JSON has one number type, so `1` is the only spelling a whole-valued
    // float has on the wire from any other producer.
    let src = "data P(w: Float)\nlet p: P | JsonError = json.parse(\"{\\\"w\\\":1}\")\nprint(repr(p!))";
    assert_eq!(run(src), "P(w=1.0)\n");
}

#[test]
fn an_integer_is_accepted_where_an_optional_float_is_expected() {
    // The same rule as above, reached through a union's dispatch instead
    // of straight through `frog_json_float`. A `Float` arm therefore has
    // to be guarded by "is a number", not "was written with a `.`" —
    // otherwise making a field optional would silently narrow what parses.
    let src = "data P(w: Float?)\n\
               let a: P | JsonError = json.parse(\"{\\\"w\\\":1}\")\n\
               let b: P | JsonError = json.parse(\"{\\\"w\\\":1.5}\")\n\
               let c: Float | Str | JsonError = json.parse(\"2\")\n\
               print(repr(a!))\nprint(repr(b!))\nprint(repr(c))";
    assert_eq!(run(src), "P(w=1.0)\nP(w=1.5)\n2.0\n");
}

#[test]
fn a_float_where_an_int_is_expected_is_an_error() {
    let src = "data P(n: Int)\nlet p: P | JsonError = json.parse(\"{\\\"n\\\":1.5}\")\nprint(repr(p))";
    assert!(run(src).contains("expected an Int"));
}

#[test]
fn an_unmatched_nominal_variant_is_an_error_naming_the_union() {
    let src = "data Shape is Circle(r: Float) | Rect(w: Int, h: Int)\n\
               let s: Shape | JsonError = json.parse(\"{\\\"Square\\\":{}}\")\n\
               print(match s { is JsonError(e) then e.msg else \"parsed\" })";
    assert_eq!(run(src), "expected an object tagged with a variant of Shape\n");
}

#[test]
fn an_anonymous_union_parses_when_its_members_have_distinct_json_shapes() {
    let src = "let a: Int | Str | JsonError = json.parse(\"1\")\n\
               let b: Int | Str | JsonError = json.parse(\"\\\"x\\\"\")\n\
               print(repr(a))\nprint(repr(b))";
    assert_eq!(run(src), "1\n\"x\"\n");
}

#[test]
fn an_ambiguous_anonymous_union_is_rejected_with_a_real_message() {
    // `Int | Float` is the case DATA.md names: both are just "a number" on
    // the wire, and which one you got would depend on whether the producer
    // happened to write a `.`.
    let msg = type_error("let x: Int | Float | JsonError = json.parse(\"1\")\nx");
    assert!(msg.contains("can't tell"), "got: {msg}");
    assert!(msg.contains("a number"), "got: {msg}");
    assert!(msg.contains("data ... is ..."), "got: {msg}");

    // Two struct-shaped members are ambiguous for the same reason: both
    // are objects. The fix is the nominal union, which is externally
    // tagged and therefore never ambiguous.
    let msg = type_error("data A(x: Int)\ndata B(y: Int)\nlet v: A | B | JsonError = json.parse(\"{}\")\nv");
    assert!(msg.contains("an object"), "got: {msg}");

    // And a positional struct collides with a *list*, not with an object:
    // both are arrays on the wire, so `[1,2]` has two equally valid
    // readings and picking one silently would be the worse answer.
    let msg = type_error("data Pair(Int, Int)\nlet v: Pair | List<Int> | JsonError = json.parse(\"[1,2]\")\nv");
    assert!(msg.contains("an array"), "got: {msg}");
    // ... while against an *object*-shaped member it is unambiguous.
    let src = "data Pair(Int, Int)\ndata Named(x: Int)\n\
               let a: Pair | Named | JsonError = json.parse(\"[1,2]\")\n\
               let b: Pair | Named | JsonError = json.parse(\"{\\\"x\\\":9}\")\n\
               print(repr(a))\nprint(repr(b))";
    assert_eq!(run(src), "Pair(1, 2)\nNamed(x=9)\n");
}

#[test]
fn malformed_input_yields_a_jsonerror_with_an_offset() {
    let src = "let x: Int | JsonError = json.parse(\"[1, 2, ]\")\n\
               print(match x { is JsonError(e) then repr(e.offset) else \"parsed\" })";
    assert_eq!(run(src), "7\n");
}

// ── type errors and the namespace ────────────────────────────────────────────

#[test]
fn an_unannotated_parse_says_to_annotate() {
    let msg = type_error("let x = json.parse(\"1\")\nx");
    assert!(msg.contains("cannot infer what to parse"), "got: {msg}");
    assert!(msg.contains("JsonError"), "got: {msg}");
}

#[test]
fn an_annotation_without_jsonerror_is_rejected() {
    let msg = type_error("let x: Int = json.parse(\"1\")\nx");
    assert!(msg.contains("json.parse returns 'T | JsonError'"), "got: {msg}");
}

#[test]
fn a_function_has_no_json_form() {
    let msg = type_error("func f(x: Int): Int = x\nprint(json.to_str(f))");
    assert!(msg.contains("has no JSON form"), "got: {msg}");
}

#[test]
fn parses_argument_must_be_a_str() {
    let msg = type_error("let x: Int | JsonError = json.parse(1)\nx");
    assert!(msg.contains("json.parse's argument must be Str"), "got: {msg}");
}

#[test]
fn an_unknown_json_member_names_the_two_that_exist() {
    let msg = type_error("print(json.dump(1))");
    assert!(msg.contains("unknown json builtin 'json.dump'"), "got: {msg}");
    assert!(msg.contains("'to_str' and 'parse'"), "got: {msg}");
}

#[test]
fn a_binding_named_json_shadows_the_namespace() {
    // The namespace is gated on the name being free, so `json` stays an
    // ordinary identifier for anyone who wants it. Here it shadows into a
    // struct, and `json.to_str` then resolves as an ordinary field access
    // — which fails, as it should, naming the field and not the namespace.
    let out = run_raw("data Box(to_str: Int)\nlet json = Box(to_str=1)\nprint(json.to_str)");
    assert_eq!(out.stdout, "1\n");
    assert_eq!(out.status, Some(0));
}

// ── generics, and the deferred-check path ────────────────────────────────────

#[test]
fn json_to_str_works_inside_a_generic_function() {
    // The `Show` check has to defer past a bare generic `TypeVar` and
    // re-run per monomorphized instantiation, exactly as `repr`'s does —
    // otherwise this could never type-check for any concrete `T`.
    let src = "func dump<T>(x: T): Str = json.to_str(x)\nprint(dump(1))\nprint(dump(\"a\"))\nprint(dump([1, 2]))";
    assert_eq!(run(src), "1\n\"a\"\n[1,2]\n");
}

// ── GC ───────────────────────────────────────────────────────────────────────

#[test]
fn heap_allocating_reads_are_rooted() {
    // `frog_json_str` allocates inside a loop for a `List<Str>` — the
    // exact shape whose missing `jit_frame_guard!` made `frog_read_str`
    // alias list elements in stage 5, and invisible without this.
    let src = "let xs: List<Str> | JsonError = json.parse(\"[\\\"a\\\",\\\"bb\\\",\\\"ccc\\\",\\\"dddd\\\"]\")\n\
               print(repr(xs!))\n\
               data Person(name: Str, note: Str)\n\
               let ps: List<Person> | JsonError = json.parse(\"[{\\\"name\\\":\\\"a\\\",\\\"note\\\":\\\"x\\\"},{\\\"name\\\":\\\"b\\\",\\\"note\\\":\\\"y\\\"}]\")\n\
               print(json.to_str(ps!))";
    assert_eq!(
        run_gc_stress(src),
        "[\"a\", \"bb\", \"ccc\", \"dddd\"]\n[{\"name\":\"a\",\"note\":\"x\"},{\"name\":\"b\",\"note\":\"y\"}]\n",
    );
}
