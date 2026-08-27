//! Structural `==` / `!=` on unions — `codegen::eq_union`, the equality
//! counterpart of `print_union`.
//!
//! Which member a union value holds is only known at runtime, so the
//! comparison is a dispatch: find the member the left operand holds, ask
//! whether the right one holds the same, and if so compare that member's
//! carrier. Getting the tag provenance wrong here is silently wrong rather
//! than loud (a nominal union's tag is its *declaration* index, an anonymous
//! one's is its normalized position), so the cases below deliberately
//! declare variants out of alphabetical order — the same hazard
//! `print_labels_a_nominal_variant_by_its_declared_tag` covers for printing.
//!
//! Before this existed, comparing a payload-carrying union passed the trait
//! check ("every variant is a struct, every struct is `Eq`") and then
//! aborted codegen on a multi-leaf operand.

use froglang_core::state::{FrogState, FrogValue};

fn eval_bool(src: &str) -> bool {
    let mut s = FrogState::new();
    let (v, _) = s.eval(src).unwrap_or_else(|e| panic!("{}: {:?}", src, e));
    match v {
        FrogValue::Bool(b) => b,
        other => panic!("{}: expected Bool, got {:?}", src, other),
    }
}

const SHAPE: &str = "data Shape is Sq(w: Int) | Circle(r: Int)\n";

// ── nominal unions ───────────────────────────────────────────────────────────

#[test]
fn the_same_variant_with_the_same_payload_is_equal() {
    assert!(eval_bool(&format!("{}Sq(w=2) == Sq(w=2)", SHAPE)));
    assert!(!eval_bool(&format!("{}Sq(w=2) != Sq(w=2)", SHAPE)));
}

#[test]
fn the_same_variant_with_a_different_payload_is_not() {
    assert!(!eval_bool(&format!("{}Sq(w=2) == Sq(w=3)", SHAPE)));
    assert!(eval_bool(&format!("{}Sq(w=2) != Sq(w=3)", SHAPE)));
}

/// Different variants are never equal, whatever they carry — including when
/// the payloads would compare equal as raw words. This is the case the
/// right-hand tag test exists for.
#[test]
fn different_variants_are_never_equal() {
    assert!(!eval_bool(&format!("{}Sq(w=2) == Circle(r=2)", SHAPE)));
    assert!(eval_bool(&format!("{}Sq(w=2) != Circle(r=2)", SHAPE)));
}

/// Both operands decided at runtime, one call site — the point of the
/// dispatch. A constant-folded comparison would pass the tests above
/// without ever emitting the tag tests.
#[test]
fn both_operands_are_dispatched_at_runtime() {
    let src = "data Shape is Sq(w: Int) | Circle(r: Int)\n\
               func pick(c: Bool): Shape = if c then Sq(w=1) else Circle(r=1)\n";
    assert!(eval_bool(&format!("{}pick(true) == pick(true)", src)));
    assert!(!eval_bool(&format!("{}pick(true) == pick(false)", src)));
    assert!(eval_bool(&format!("{}pick(false) == pick(false)", src)));
}

#[test]
fn payload_less_variants_compare_by_tag() {
    let decl = "data Color is Red | Green | Blue\n";
    assert!(eval_bool(&format!("{}Red == Red", decl)));
    assert!(!eval_bool(&format!("{}Red == Green", decl)));
}

/// Both representations in one union, so the dispatch has to take the
/// guarded-load path rather than either shortcut — an immediate variant
/// must not be dereferenced.
#[test]
fn mixed_payload_and_payload_less_variants() {
    let decl = "data Maybe is Nothing | Just(v: Str)\n";
    assert!(eval_bool(&format!("{}Nothing == Nothing", decl)));
    assert!(eval_bool(&format!("{}Just(v=\"a\") == Just(v=\"a\")", decl)));
    assert!(!eval_bool(&format!("{}Just(v=\"a\") == Just(v=\"b\")", decl)));
    assert!(!eval_bool(&format!("{}Nothing == Just(v=\"a\")", decl)));
}

#[test]
fn positional_variants_compare_by_payload() {
    let decl = "data Node is Lit(Int) | Neg(Int)\n";
    assert!(eval_bool(&format!("{}Lit(4) == Lit(4)", decl)));
    assert!(!eval_bool(&format!("{}Lit(4) == Lit(9)", decl)));
    assert!(!eval_bool(&format!("{}Lit(4) == Neg(4)", decl)));
}

// ── anonymous unions ─────────────────────────────────────────────────────────

/// No `UnionDef` to consult, so the tag is the member's normalized position
/// and the test is `emit_tag_test`, not `emit_is_variant`.
#[test]
fn an_anonymous_union_compares_by_member() {
    let src = "func g(c: Bool): Int | Str = if c then 1 else \"x\"\n";
    assert!(eval_bool(&format!("{}g(true) == g(true)", src)));
    assert!(eval_bool(&format!("{}g(false) == g(false)", src)));
    assert!(!eval_bool(&format!("{}g(true) == g(false)", src)));
}

/// An optional. `None` carries no data, so two of them are equal without a
/// load — and `None` had to become `Eq` for `Int | None` to satisfy the
/// "every member is `Eq`" rule at all.
#[test]
fn an_optional_compares_by_member() {
    let src = "func f(c: Bool): Int | None = if c then 1 else none\n";
    assert!(eval_bool(&format!("{}f(true) == f(true)", src)));
    assert!(eval_bool(&format!("{}f(false) == f(false)", src)));
    assert!(!eval_bool(&format!("{}f(true) == f(false)", src)));
    assert!(eval_bool("none == none"));
}

// ── unions inside other things ───────────────────────────────────────────────

/// A union-typed struct field. `build_struct_eq` synthesizes a per-field
/// `Binary` node with the field's own type, which lands in `compile_binary`'s
/// union arm.
#[test]
fn a_union_typed_struct_field_compares_structurally() {
    let decl = "data Shape is Sq(w: Int) | Circle(r: Int)\n\
                data Wrap(s: Shape, n: Int)\n";
    assert!(eval_bool(&format!("{}Wrap(s=Sq(w=1), n=0) == Wrap(s=Sq(w=1), n=0)", decl)));
    assert!(!eval_bool(&format!("{}Wrap(s=Sq(w=1), n=0) == Wrap(s=Circle(r=1), n=0)", decl)));
    assert!(!eval_bool(&format!("{}Wrap(s=Sq(w=1), n=0) == Wrap(s=Sq(w=1), n=1)", decl)));
}

/// A variant carrying a list: the dispatch unpacks the carrier and hands it
/// to `eq_value`, which recurses into `eq_list`.
#[test]
fn a_variant_carrying_a_list_compares_element_wise() {
    let decl = "data Bag is Empty | Items(xs: List<Int>)\n";
    assert!(eval_bool(&format!("{}Items(xs=[1, 2]) == Items(xs=[1, 2])", decl)));
    assert!(!eval_bool(&format!("{}Items(xs=[1, 2]) == Items(xs=[1, 3])", decl)));
    assert!(!eval_bool(&format!("{}Items(xs=[1]) == Empty", decl)));
}

// ── a bare member as one operand ─────────────────────────────────────────────

/// `x == none` — the most common optional test there is. Both operands are
/// widened into the joined union before codegen sees them (`lower_binary`,
/// the same widening `lower_conditional` does to its branches), because
/// `eq_union` reads the union's full layout width from each side and a bare
/// member occupies one leaf. Unwidened, this used to index past the
/// operand's leaves and abort codegen.
#[test]
fn a_union_compares_against_a_bare_member() {
    let f = "func f(c: Bool): Int | None = if c then 1 else none\n";
    assert!(eval_bool(&format!("{}f(false) == none", f)));
    assert!(!eval_bool(&format!("{}f(true) == none", f)));
    assert!(eval_bool(&format!("{}f(true) == 1", f)));
    assert!(!eval_bool(&format!("{}f(false) == 1", f)));
    assert!(eval_bool(&format!("{}f(true) != none", f)));
}

/// The member may be a `Str` (a boxed carrier, not a scalar) and it may sit
/// on either side: `unify` accepts a member against the union it belongs to
/// but not the reverse, so the operand order must not decide whether this
/// type-checks.
#[test]
fn a_bare_member_compares_from_either_side() {
    let g = "func g(c: Bool): Int | Str = if c then 1 else \"hi\"\n";
    assert!(eval_bool(&format!("{}g(false) == \"hi\"", g)));
    assert!(eval_bool(&format!("{}\"hi\" == g(false)", g)));
    assert!(!eval_bool(&format!("{}g(true) == \"hi\"", g)));
    assert!(eval_bool(&format!("{}1 == g(true)", g)));
}

/// A nominal union against one of its own variants — the widened operand
/// still has to carry the *declaration* tag, not a fresh anonymous one.
#[test]
fn a_nominal_union_compares_against_a_bare_variant() {
    let decl = format!("{}func pick(c: Bool): Shape = if c then Sq(w=2) else Circle(r=2)\n", SHAPE);
    assert!(eval_bool(&format!("{}pick(true) == Sq(w=2)", decl)));
    assert!(!eval_bool(&format!("{}pick(true) == Circle(r=2)", decl)));
    assert!(eval_bool(&format!("{}Circle(r=2) == pick(false)", decl)));
}

// ── the limit ────────────────────────────────────────────────────────────────

/// A union that encloses itself would need an unbounded dispatch tree. It is
/// still structurally `Eq` — what's missing is a way to emit the comparison
/// — so this is a spanned error from `check_comparable` at the operator, not
/// a "requires Eq" from the trait check, and above all not a codegen panic.
#[test]
fn a_recursive_union_is_a_clean_error() {
    let mut s = FrogState::new();
    let err = s.eval("data Node is Lit(v: Int) | Add(lhs: Node, rhs: Node)\nLit(v=1) == Lit(v=2)")
        .unwrap_err();
    let msg = format!("{:?}", err);
    assert!(msg.contains("recursive") && msg.contains("comparing"), "unexpected error: {}", msg);
}

/// The same limit, but the cycle only closes through a *type argument*:
/// `Node` holds a `Box<Node>`. The guard resolves each struct's fields the
/// way codegen does — substituting the instantiation's `args` into the
/// declared template — because the bare `Box` template's field is still the
/// binder `T`, and a walk over that closes no cycle and would let
/// `eq_value` recurse until the compiler's own stack ran out.
#[test]
fn a_recursive_generic_instantiation_is_a_clean_error() {
    let mut s = FrogState::new();
    let err = s.eval(
        "data Box<T>(v: List<T>)\n         data Node(x: Int, b: Box<Node>)\n         Node(x=1, b=Box(v=[])) == Node(x=1, b=Box(v=[]))",
    ).unwrap_err();
    let msg = format!("{:?}", err);
    assert!(msg.contains("recursive") && msg.contains("comparing"), "unexpected error: {}", msg);
}
