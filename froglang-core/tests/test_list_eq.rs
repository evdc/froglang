//! Structural `==` / `!=` on lists.
//!
//! A list comparison can't be desugared into a fixed conjunction the way a
//! struct's is (`TypeChecker::desugar_struct_eq`) — the length is a runtime
//! value — so codegen emits the element loop instead (`eq_list`/`eq_value`
//! in `codegen/mod.rs`), for the same reason `print_list` is emitted there:
//! only codegen still knows the static element type. The runtime sees a
//! pointer and a stride, and would compare a `Str` element by address.
//!
//! The trait side lives in `test_typeck.rs`: `List(T)` is `Eq` iff `T` is.

use froglang_core::state::{FrogState, FrogValue};

fn eval_bool(src: &str) -> bool {
    let mut s = FrogState::new();
    let (v, _) = s.eval(src).unwrap_or_else(|e| panic!("{}: {:?}", src, e));
    match v {
        FrogValue::Bool(b) => b,
        other => panic!("{}: expected Bool, got {:?}", src, other),
    }
}

fn assert_eq_and_ne(src_l: &str, src_r: &str, expected: bool) {
    assert_eq!(eval_bool(&format!("{} == {}", src_l, src_r)), expected, "{} == {}", src_l, src_r);
    assert_eq!(eval_bool(&format!("{} != {}", src_l, src_r)), !expected, "{} != {}", src_l, src_r);
}

// ── scalars ──────────────────────────────────────────────────────────────────

#[test]
fn equal_int_lists_compare_equal() {
    assert_eq_and_ne("[1, 2, 3]", "[1, 2, 3]", true);
}

#[test]
fn a_differing_element_decides_it() {
    assert_eq_and_ne("[1, 2, 3]", "[1, 2, 4]", false);
}

/// Unequal lengths are decided before any element is touched — including
/// the case where one list is a prefix of the other, which an element-only
/// comparison would call equal.
#[test]
fn different_lengths_are_never_equal() {
    assert_eq_and_ne("[1, 2, 3]", "[1, 2]", false);
    assert_eq_and_ne("[1, 2]", "[1, 2, 3]", false);
    assert_eq_and_ne("[1]", "[]", false);
}

#[test]
fn two_empty_lists_are_equal() {
    // Neither literal fixes an element type, so the comparison is emitted
    // over a bare type variable and the loop body is dead — the length
    // check alone answers it.
    assert_eq_and_ne("[]", "[]", true);
}

#[test]
fn float_and_bool_elements_compare_by_value() {
    assert_eq_and_ne("[1.5, 2.5]", "[1.5, 2.5]", true);
    assert_eq_and_ne("[1.5]", "[2.5]", false);
    assert_eq_and_ne("[true, false]", "[true, false]", true);
    assert_eq_and_ne("[true]", "[false]", false);
}

/// The point of emitting this in codegen: a `Str` element is a pointer, so
/// the comparison has to go through `frog_str_eq`. Two separately built
/// strings with the same contents are equal.
#[test]
fn string_elements_compare_by_contents() {
    assert_eq_and_ne("[\"ab\", \"c\"]", "[\"ab\", \"c\"]", true);
    assert_eq_and_ne("[\"a\"]", "[\"b\"]", false);
    assert!(eval_bool("[\"a\" + \"b\"] == [\"ab\"]"));
}

// ── composite elements ───────────────────────────────────────────────────────

#[test]
fn nested_lists_compare_element_wise() {
    assert_eq_and_ne("[[1, 2], [3]]", "[[1, 2], [3]]", true);
    assert_eq_and_ne("[[1, 2], [3]]", "[[1, 2], [4]]", false);
    assert_eq_and_ne("[[1, 2], [3]]", "[[1, 2], []]", false);
    assert_eq_and_ne("[[[\"x\"]]]", "[[[\"x\"]]]", true);
}

#[test]
fn struct_elements_compare_field_wise() {
    let decl = "data P(name: Str, age: Int)\n";
    assert!(eval_bool(&format!("{}[P(name=\"Ada\", age=36)] == [P(name=\"Ada\", age=36)]", decl)));
    assert!(!eval_bool(&format!("{}[P(name=\"Ada\", age=36)] == [P(name=\"Ada\", age=37)]", decl)));
    assert!(!eval_bool(&format!("{}[P(name=\"Ada\", age=36)] == [P(name=\"Bo\", age=36)]", decl)));
}

/// A payload-less variant is its tag, an immediate — so a list of them
/// compares as scalars do.
#[test]
fn payload_less_union_elements_compare_by_tag() {
    let decl = "data Color is Red | Green | Blue\n";
    assert!(eval_bool(&format!("{}[Red, Green] == [Red, Green]", decl)));
    assert!(!eval_bool(&format!("{}[Red, Green] == [Red, Blue]", decl)));
}

/// A payload-carrying variant needs `eq_union`'s runtime dispatch, emitted
/// inside the element loop.
#[test]
fn union_elements_compare_by_variant_and_payload() {
    let decl = "data Shape is Sq(w: Int) | Circle(r: Int)\n";
    assert!(eval_bool(&format!("{}[Sq(w=1), Circle(r=2)] == [Sq(w=1), Circle(r=2)]", decl)));
    assert!(!eval_bool(&format!("{}[Sq(w=1)] == [Sq(w=2)]", decl)));
    assert!(!eval_bool(&format!("{}[Sq(w=1)] == [Circle(r=1)]", decl)));
}

// ── lists inside other things ────────────────────────────────────────────────

/// A `List`-typed struct field. `build_struct_eq` synthesizes a per-field
/// `Binary` node with the field's own type, which lands in `compile_binary`'s
/// list arm.
///
/// This is the case that was silently wrong before the field types were
/// checked at all: the field compared as a raw pointer, so two structs with
/// equal contents answered `false`. It then became a clean type error, and
/// now it answers.
#[test]
fn a_list_typed_struct_field_compares_structurally() {
    let decl = "data W(xs: List<Int>)\n";
    assert!(eval_bool(&format!("{}W(xs=[1, 2]) == W(xs=[1, 2])", decl)));
    assert!(!eval_bool(&format!("{}W(xs=[1, 2]) == W(xs=[1, 3])", decl)));
    assert!(!eval_bool(&format!("{}W(xs=[1, 2]) == W(xs=[1])", decl)));
}

#[test]
fn a_list_comparison_works_in_condition_position() {
    assert_eq!(
        eval_bool("let xs = [1, 2]\nif xs == [1, 2] then true else false"),
        true,
    );
}

/// Both operands are evaluated once, as everywhere else — the loop reads
/// each list through the value it was handed, never by re-running the
/// operand expression.
#[test]
fn a_list_returned_from_a_call_compares_by_value() {
    let src = "func mk(n: Int): List<Int> = [n, n + 1]\n\
               mk(1) == mk(1)";
    assert!(eval_bool(src));
    let src = "func mk(n: Int): List<Int> = [n, n + 1]\n\
               mk(1) == mk(2)";
    assert!(!eval_bool(src));
}

// ── field-less struct elements ────────────────────────────────────────────

/// A field-less struct (`data E()`) flattens to zero leaves, so it
/// contributes no per-field values the way any other struct's elements do —
/// but the list built from it must still count each `E()` as one element.
/// Before this was fixed, no per-element push happened for these at all
/// (the allocation site's `stride` was `leafs.len().max(1) == 1`, but the
/// push loop only pushed `leafs.len() == 0` slots per element), so `len`
/// never advanced and every such list read back as empty.
fn e_decl() -> &'static str { "data E()\n" }

#[test]
fn field_less_struct_lists_compare_by_length() {
    assert_eq_and_ne(&format!("{}[E(), E()]", e_decl()), "[E()]", false);
    assert_eq_and_ne(&format!("{}[E(), E()]", e_decl()), "[E(), E()]", true);
}

#[test]
fn field_less_struct_lists_report_the_right_length() {
    let mut s = FrogState::new();
    let (v, _) = s.eval(&format!("{}len([E(), E(), E()])", e_decl())).unwrap();
    assert!(matches!(v, FrogValue::Int(3)), "expected Int(3), got {:?}", v);
}

#[test]
fn pushing_field_less_struct_elements_advances_len() {
    let mut s = FrogState::new();
    let src = format!(
        "{}mut xs = [E()]\npush(mut xs, E())\npush(mut xs, E())\nlen(xs)",
        e_decl(),
    );
    let (v, _) = s.eval(&src).unwrap();
    assert!(matches!(v, FrogValue::Int(3)), "expected Int(3), got {:?}", v);
}
