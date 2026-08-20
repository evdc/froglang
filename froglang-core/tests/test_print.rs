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
    // aborted codegen with "print codegen does not support None". Note the
    // value literal is lowercase `none` while the *type* is `None`.
    assert_eq!(
        run("func f(c: Bool): Int | None = if c then 1 else none\nprint(f(true))\nprint(f(false))"),
        "1\nNone\n",
    );
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
