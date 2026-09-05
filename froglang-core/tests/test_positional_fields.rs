//! Positional ("tuple struct") fields: `data Lit(Int)` / `Lit(3)` rather
//! than `data Lit(v: Int)` / `Lit(v=3)`.
//!
//! A field list is all-named or all-positional, decided per list at parse
//! time (`Grammar::field_list`), so a union's common fields and each
//! variant's own fields choose independently — with one restriction: a
//! variant's marker struct is the common fields followed by its own, and
//! each list numbers its slots from 0, so positional fields on *both*
//! sides would collide. A union with positional common fields therefore
//! requires named variant fields (see
//! `positional_variant_fields_collide_with_positional_common_fields`).
//! Internally a positional field
//! is keyed by its stringified declaration index (`"0"`, `"1"`, …) — see
//! `field_name_or_positional` and `is_positional_fields` in `typeck.rs` —
//! which is unambiguous because a lexed identifier can never be all digits.
//! These tests pin both the surface behaviour and the places that index
//! marker leaks into: construction, field layout, matching, and printing.

mod common;
use common::{run, run_raw};

use froglang_core::codegen::compile_and_run;

fn prog(name: &str) -> String {
    let path = format!("{}/tests/programs/{}", env!("CARGO_MANIFEST_DIR"), name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("could not read {}: {}", path, e))
}

// ── construction and field order ─────────────────────────────────────────────

#[test]
fn positional_fields_keep_declaration_order() {
    // Reads the *second* field, to prove the arguments aren't being
    // reordered or collapsed onto the first slot. Goes through a union
    // rather than a plain struct because a positional struct's fields
    // aren't readable at all yet — see
    // `a_positional_structs_fields_can_be_read_back` below.
    assert_eq!(
        compile_and_run(
            "data Pair is Both(Int, Int)\n\
             let p = Both(3, 7)\n\
             match p { is Both(a, b) then b }"
        ),
        7,
    );
}

/// KNOWN GAP: a positional struct is write-only. You can construct one and
/// print it, but there is no way to get a field back out: `p.0` doesn't
/// parse (`ExpectedIdentifier` — the field-access grammar wants a name, and
/// the synthetic `"0"` key is deliberately unspellable), and `match` rejects
/// a non-union subject ("Can only match on a union value"). Positional
/// *variant* fields are fine, since `match ... is V(a, b)` binds them
/// positionally — which is why `calculator.frog` works.
///
/// Fixing this means either a tuple-index syntax (`p.0`) or extending
/// `match`/`is` to plain structs. Ignored, not deleted, so the gap stays
/// visible and this test starts passing the day it's closed.
#[test]
#[ignore = "positional struct fields are not readable yet: no `p.0` syntax, and `match` rejects a struct subject"]
fn a_positional_structs_fields_can_be_read_back() {
    assert_eq!(compile_and_run("data Pair(Int, Int)\nlet p = Pair(3, 7)\np.1"), 7);
}

#[test]
fn positional_variants_carry_independent_payloads() {
    assert_eq!(
        compile_and_run(
            "data Shape is Circle(Int) | Rect(Int, Int)\n\
             func area(s: Shape): Int = match s {\n\
               is Circle(r) then r * r * 3\n\
               is Rect(w, h) then w * h\n\
             }\n\
             area(Rect(4, 5)) + area(Circle(2))"
        ),
        // 20 + 12
        32,
    );
}

#[test]
fn a_nullary_positional_declaration_still_constructs() {
    // `data X()` has an empty field list, which `is_positional_fields`
    // reports as vacuously positional — it takes no arguments either way,
    // so neither calling convention applies and both must work.
    assert_eq!(
        compile_and_run("data Marker()\nlet m = Marker()\n1"),
        1,
    );
}

// ── the two calling conventions don't mix ────────────────────────────────────

#[test]
fn a_positional_declaration_rejects_named_arguments() {
    let out = run_raw("data Pair(Int, Int)\nlet p = Pair(a=1, b=2)\n1");
    assert_ne!(out.status, Some(0), "expected a type error, got success");
    assert!(
        out.stdout.contains("positional") || out.stderr.contains("positional"),
        "error should explain the calling convention, got:\nstdout: {}\nstderr: {}",
        out.stdout, out.stderr,
    );
}

#[test]
fn a_named_declaration_rejects_positional_arguments() {
    let out = run_raw("data Pair(a: Int, b: Int)\nlet p = Pair(1, 2)\n1");
    assert_ne!(out.status, Some(0), "expected a type error, got success");
}

#[test]
fn a_mixed_field_declaration_is_rejected_at_parse_time() {
    let out = run_raw("data Bad(a: Int, Str)\n1");
    assert_ne!(out.status, Some(0), "expected a parse error, got success");
    assert!(
        out.stdout.contains("all named") || out.stderr.contains("all named"),
        "error should explain the all-or-nothing rule, got:\nstdout: {}\nstderr: {}",
        out.stdout, out.stderr,
    );
}

#[test]
fn wrong_positional_argument_count_is_rejected() {
    assert_ne!(run_raw("data Pair(Int, Int)\nlet p = Pair(1)\n1").status, Some(0));
    assert_ne!(run_raw("data Pair(Int, Int)\nlet p = Pair(1, 2, 3)\n1").status, Some(0));
}

#[test]
fn a_positional_argument_is_still_type_checked() {
    assert_ne!(run_raw("data Pair(Int, Str)\nlet p = Pair(1, 2)\n1").status, Some(0));
}

// ── each field list picks its own style ──────────────────────────────────────

/// KNOWN GAP: a union that has *both* common fields and positional variant
/// fields cannot be constructed at all — every form of `A(...)` is rejected
/// with "Construction requires named fields".
///
/// The cause is that the synthetic index keys are assigned per *field list*
/// (`field_name_or_positional` numbers from 0 within each list), but
/// `lower_record_args` runs on the **concatenation**
/// of common fields and variant fields. So:
///   - `data Tagged(id: Int) is A(Int)` merges to keys `["id", "0"]`, which
///     is neither all-named nor all-positional, so `is_positional_fields`
///     says false and the named path demands a name for field `"0"` that
///     can never be written;
///   - `data Tagged(Int) is A(Int)` merges to `["0", "0"]` — duplicate keys
///     whose positions no longer equal their indices, so
///     `is_positional_fields` says false there too.
///
/// The fix is to renumber positional keys over the merged list rather than
/// per list (or to key fields by list-plus-index instead of a bare index).
/// Note `expression.rs`'s `FieldDecl` doc currently claims the two lists
/// "may independently pick either style", which is true at parse time but
/// not at construction time — worth correcting alongside.
#[test]
#[ignore = "a union with common fields plus positional variant fields can't be constructed: per-list index keys collide when the two lists are concatenated"]
fn common_fields_and_variant_fields_choose_independently() {
    assert_eq!(
        compile_and_run(
            "data Tagged(id: Int) is A(Int) | B(Str)\n\
             let a = A(id=7, 5)\n\
             a.id"
        ),
        7,
    );
}

#[test]
fn common_fields_still_work_when_every_list_is_named() {
    // The combination that does work today, pinned so a fix for the gap
    // above can't regress it.
    assert_eq!(
        compile_and_run(
            "data Tagged(id: Int) is A(v: Int) | B(s: Str)\n\
             let a = A(id=7, v=5)\n\
             a.id"
        ),
        7,
    );
}

// ── printing ─────────────────────────────────────────────────────────────────

#[test]
fn a_positional_struct_prints_without_synthetic_field_names() {
    // The `"0"`/`"1"` index keys are an internal representation detail; a
    // positional value must print as `Point(1, 2)`, never `Point(0=1, 1=2)`.
    assert_eq!(run("data Point(Int, Int)\nprint(Point(1, 2))"), "Point(1, 2)\n");
}

#[test]
fn a_named_struct_still_prints_its_field_names() {
    assert_eq!(
        run("data Point(x: Int, y: Int)\nprint(Point(x=1, y=2))"),
        "Point(x=1, y=2)\n",
    );
}

// ── the self-referential case positional fields were added for ───────────────

#[test]
fn calculator_program_evaluates_and_propagates_its_error() {
    // `data Node is Lit(Int) | Add(Node, Node) | ...` — a recursive union
    // built entirely from positional fields, evaluated through `?` error
    // propagation. Div(Add(Lit(2), Lit(4)), Lit(3)) = 6/3 = 2, then a
    // second expression divides by zero and yields the error value.
    assert_eq!(run(&prog("calculator.frog")), "2\nDivideByZero()\n");
}

// ── duplicate field names ────────────────────────────────────────────────────

#[test]
fn positional_variant_fields_collide_with_positional_common_fields() {
    // Both lists number their slots from 0, so the flattened marker struct
    // for `U.A` would hold two fields named "0" — the second unreachable by
    // name and silently occupying a layout slot. Rejected rather than
    // reported as a duplicate of a name the source never wrote.
    let out = run_raw("data U(Int) is A(Str) | B(Int)\nprint(\"unreachable\")\n");
    assert_ne!(out.status, Some(0));
    assert!(
        out.stdout.contains("variants' fields must be named"),
        "stdout: {}", out.stdout,
    );
}

#[test]
fn positional_variant_fields_are_fine_when_the_union_has_no_common_fields() {
    let out = run("data U is A(Str) | B(Int)\nlet u = A(\"hi\")\nprint(u)\n");
    assert_eq!(out, "U.A(\"hi\")\n");
}
