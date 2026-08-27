//! Regression tests for `print`'s scalar-to-string coercions, and for
//! `print_union` — the runtime tag dispatch that renders a union-typed
//! argument, since which member it holds is only known at runtime.

mod common;
use common::run;

#[test]
fn print_coerces_scalar_values_to_text() {
    assert_eq!(run("print(3)\nprint(1.5)\nprint(true)\nprint(\"ok\")\nprint([1, 2])"), "3\n1.5\ntrue\nok\n[1, 2]\n");
}

#[test]
fn print_formats_structs_and_nested_structs() {
    assert_eq!(run(
        "data Address(city: Str, zip: Int)\ndata Person(name: Str, address: Address)\nprint(Person(name=\"Ada\", address=Address(city=\"London\", zip=123)))"
    ), "Person(name=\"Ada\", address=Address(city=\"London\", zip=123))\n");
}

// ── lists ────────────────────────────────────────────────────────────────────
//
// Before plans/DATA.md stage 0, a list was printed by the runtime from a
// pointer plus a one-byte element-kind tag, which is all the type
// information that survived — so every element that wasn't a scalar came out
// as `<struct>` or `<list>`. The element loop is emitted in codegen now, with
// the static element type in hand, so the walk reaches through the list.

#[test]
fn print_walks_nested_lists() {
    assert_eq!(run("print([[1, 2], [3]])"), "[[1, 2], [3]]\n");
    assert_eq!(run("print([[[\"x\"]]])"), "[[[\"x\"]]]\n");
}

#[test]
fn print_walks_struct_elements() {
    assert_eq!(
        run("data Person(name: Str, age: Int)\nprint([Person(name=\"Ada\", age=36), Person(name=\"Bo\", age=1)])"),
        "[Person(name=\"Ada\", age=36), Person(name=\"Bo\", age=1)]\n",
    );
}

#[test]
fn print_walks_union_elements() {
    assert_eq!(
        run("data Shape is Sq(w: Int) | Circle(r: Int)\nprint([Sq(w=2), Circle(r=3)])"),
        "[Shape.Sq(w=2), Shape.Circle(r=3)]\n",
    );
}

#[test]
fn print_quotes_string_elements() {
    // A `Str` inside a composite prints as a literal, the same as a struct
    // field does — the list walk shares `print_value`'s leaf policy rather
    // than having its own. A bare `print(s)` still prints raw text.
    assert_eq!(run("print([\"a\", \"b\"])"), "[\"a\", \"b\"]\n");
    assert_eq!(run("print(\"a\")"), "a\n");
}

#[test]
fn print_handles_an_empty_list() {
    // Nothing fixed the element type, so it is still a `TypeVar` at codegen
    // — the loop body is dead, and the brackets are all that print.
    assert_eq!(run("print([])"), "[]\n");
}

/// A field-less struct (`data E()`) flattens to zero leaves — `struct_fields`
/// returns an empty slice for it — so a naive per-element push loop
/// (`zip`ping pushed values against leaf types) pushes nothing per element
/// even though the list's `stride` was computed as `leafs.len().max(1) ==
/// 1`. `len` then never advances, and every such list read back as empty
/// regardless of how many elements it held.
#[test]
fn print_counts_field_less_struct_elements() {
    assert_eq!(run("data E()\nprint([E(), E()])"), "[E(), E()]\n");
}

/// Stage 0 taught the printing walk to descend into a list's element type,
/// which introduced a cycle it hadn't had before: `Tree` → `List<Tree>` →
/// `Tree`. `print_value`/`print_list` monomorphize one static type per step,
/// so this overflowed the *compiler's* stack — `check_printable` predicts it
/// now, the same way it always did for a recursive union.
#[test]
fn print_rejects_a_struct_that_recurses_through_a_list() {
    let out = common::run_raw("data Tree(v: Int, kids: List<Tree>)\nprint(Tree(v=1, kids=[]))");
    let text = format!("{}{}", out.stdout, out.stderr);
    assert!(text.contains("recursive") && text.contains("Tree"), "unexpected output: {}", text);
}

#[test]
fn print_distinguishes_float_and_int_elements() {
    assert_eq!(run("print([1.0, 2.5])"), "[1.0, 2.5]\n");
    assert_eq!(run("print([1, 2])"), "[1, 2]\n");
}

// ── unions ───────────────────────────────────────────────────────────────────

#[test]
fn print_labels_a_nominal_variant_by_its_declared_tag() {
    // `Type::normalize` sorts a union's members by display string, but a
    // nominal union's runtime tag is the *declaration* index. `print_union`
    // used the sorted position instead, so a union whose variants aren't
    // declared alphabetically printed under the wrong variant's name and
    // field names — `Sq(w=2)` came out as `Shape.Circle(r=2)`, a wrong
    // answer with no error. Declared deliberately out of alphabetical
    // order here so the two orderings disagree.
    assert_eq!(
        run("data Shape is Sq(w: Int) | Circle(r: Int)\nprint(Sq(w=2))\nprint(Circle(r=3))"),
        "Shape.Sq(w=2)\nShape.Circle(r=3)\n",
    );
}

#[test]
fn print_handles_payload_less_variants() {
    // A payload-less variant is an unboxed immediate (`(tag << 1) | 1`), not
    // a pointer. `print_union` derived "does this union have immediate
    // members?" from `Type::None` membership alone — true for an anonymous
    // union, but blind to a nominal union's nullary variants — so it emitted
    // an unconditional tag load and dereferenced the immediate. `print(Red)`
    // segfaulted the process (exit 139).
    assert_eq!(
        run("data Color is Red | Green | Blue\nprint(Red)\nprint(Green)\nprint(Blue)"),
        "Color.Red()\nColor.Green()\nColor.Blue()\n",
    );
}

#[test]
fn print_handles_mixed_payload_and_payload_less_variants() {
    // Both representations inside one union, so the tag test has to take
    // the guarded-load path rather than either shortcut.
    assert_eq!(
        run("data Maybe is Nothing | Just(v: Int)\nprint(Nothing)\nprint(Just(v=7))"),
        "Maybe.Nothing()\nMaybe.Just(v=7)\n",
    );
}

#[test]
fn print_handles_an_optional() {
    // `Int | None` — `print_value` had no `Type::None` arm at all, so this
    // aborted codegen with "print codegen does not support None". The
    // printed form is the lowercase *value* literal `none`, not the type
    // name `None`: what is printed has to be what reads back
    // (plans/DATA.md stage 1).
    assert_eq!(
        run("func f(c: Bool): Int | None = if c then 1 else none\nprint(f(true))\nprint(f(false))"),
        "1\nnone\n",
    );
}

#[test]
fn print_handles_a_bare_none() {
    // The top-level `print` path never reached `print_value`'s `None` arm at
    // all, so this aborted codegen outright.
    assert_eq!(run("print(none)"), "none\n");
}

#[test]
fn print_dispatches_on_the_member_actually_held() {
    // The whole point of the runtime dispatch: one call site, both members.
    assert_eq!(
        run(
            "data Shape is Circle(r: Int) | Sq(w: Int)\n\
             func pick(c: Bool): Shape = if c then Circle(r=1) else Sq(w=2)\n\
             print(pick(true))\n\
             print(pick(false))"
        ),
        "Shape.Circle(r=1)\nShape.Sq(w=2)\n",
    );
}

#[test]
fn print_handles_a_positional_variant() {
    assert_eq!(
        run("data Node is Lit(Int) | Neg(Int)\nprint(Lit(4))\nprint(Neg(9))"),
        "Node.Lit(4)\nNode.Neg(9)\n",
    );
}
