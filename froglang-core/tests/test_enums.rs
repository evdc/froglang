use froglang_core::codegen::compile_and_run;

// ── helper: load a .frog program from tests/programs/ ────────────────────────

fn prog(name: &str) -> String {
    let path = format!("{}/tests/programs/{}", env!("CARGO_MANIFEST_DIR"), name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("could not read {}: {}", path, e))
}

fn type_error(src: &str) -> String {
    let ast = froglang_core::frontend::parser::Parser::parse(src).expect("parse error");
    let mut tc = froglang_core::frontend::typeck::TypeChecker::new();
    let err = tc.check_and_lower(ast).expect_err("expected a type error");
    format!("{:?}", err)
}

// ── construction, matching, guards ────────────────────────────────────────────

#[test]
fn test_enum_construct_and_match() {
    assert_eq!(compile_and_run(
        "data Shape is Circle(r: Int) | Rect(w: Int, h: Int)\n\
         let s = Circle(r=4)\n\
         match s {\nis Circle(r) then r * r\nis Rect(w, h) then w * h\n}"
    ), 16);
}

#[test]
fn test_enum_common_field_readable_without_matching() {
    assert_eq!(compile_and_run(
        "data Color is Red | Green | Blue\n\
         data Shape(color: Color) is Circle(r: Int) | Rect(w: Int, h: Int)\n\
         let s = Circle(color=Green, r=4)\n\
         if s.color is Green then 1 else 0"
    ), 1);
}

#[test]
fn test_enum_nullary_bare_construction() {
    assert_eq!(compile_and_run(
        "data Color is Red | Green | Blue\nlet c = Red\nif c is Red then 1 else 0"
    ), 1);
}

#[test]
fn test_enum_qualified_construction() {
    assert_eq!(compile_and_run(
        "data Shape(n: Int) is Circle(r: Int) | Rect(w: Int, h: Int)\n\
         let s = Shape.Circle(n=1, r=5)\n\
         match s { is Circle(r) then r  else 0 }"
    ), 5);
}

#[test]
fn test_enum_match_guard_can_reference_bound_field() {
    // Regression test: a guard referencing a bound field must see it
    // extracted *before* the guard runs, not after — see `lower_match`'s
    // nested (rather than `and`-combined) desugaring for guarded arms.
    assert_eq!(compile_and_run(
        "data Shape is Circle(r: Int)\n\
         let s = Circle(r=(0 - 5))\n\
         match s {\nis Circle(r) and r < 0 then -1\nis Circle(r) then 1\n}"
    ), -1);
}

#[test]
fn test_enum_match_wildcard_bind_skips_field() {
    assert_eq!(compile_and_run(
        "data Shape is Rect(w: Int, h: Int)\n\
         let s = Rect(w=3, h=4)\n\
         match s { is Rect(w, _) then w }"
    ), 3);
}

#[test]
fn test_enum_is_pattern_standalone_bool() {
    assert_eq!(compile_and_run(
        "data Color is Red | Green\n\
         let c = Green\n\
         let ok = c is Green\n\
         if ok then 1 else 0"
    ), 1);
}

#[test]
fn test_enum_struct_typed_field_inside_variant() {
    assert_eq!(compile_and_run(
        "data Point(x: Int, y: Int)\n\
         data Shape is Circle(center: Point, r: Int)\n\
         let s = Circle(center=Point(x=2, y=3), r=4)\n\
         match s { is Circle(center, r) then center.x + center.y + r * r }"
    ), 2 + 3 + 16);
}

#[test]
fn test_enum_recursive_type_via_tree_sum_program() {
    assert_eq!(compile_and_run(&prog("enum_tree_sum.frog")), 10);
}

#[test]
fn test_enum_shapes_area_program() {
    assert_eq!(compile_and_run(&prog("enum_shapes_area.frog")), 127);
}

#[test]
fn test_enum_unit_variants_in_list_program() {
    assert_eq!(compile_and_run(&prog("enum_color_count.frog")), 3);
}

/// GC stress: many enum-variant allocations, each holding a `Str` (heap
/// pointer) payload slot, forcing multiple collections — regression test
/// for `ObjKind::Variant`'s tracing in `GcHeap::mark` (runtime/gc.rs).
#[test]
fn test_enum_variant_with_str_field_survives_gc() {
    assert_eq!(compile_and_run(
        "data Wrapper is Box(s: Str)\n\
         let total = 0\n\
         for i in 0..3000 do { let w = Box(s=\"hello\")\ntotal = total + 1 }\n\
         total"
    ), 3000);
}

// ── error cases ────────────────────────────────────────────────────────────

#[test]
fn test_enum_non_exhaustive_match_rejected() {
    let err = type_error(
        "data Color is Red | Green | Blue\n\
         func f(c: Color): Int = match c {\nis Red then 1\nis Green then 2\n}\n\
         f(Red)"
    );
    assert!(err.contains("Non-exhaustive"), "unexpected error: {}", err);
}

#[test]
fn test_enum_ambiguous_bare_variant_rejected() {
    let err = type_error(
        "data A is X(n: Int)\ndata B is X(n: Int)\nlet v = X(n=1)\n1"
    );
    assert!(err.contains("ambiguous"), "unexpected error: {}", err);
}

#[test]
fn test_enum_variant_field_not_readable_without_matching() {
    let err = type_error(
        "data Shape is Circle(r: Int) | Rect(w: Int, h: Int)\n\
         let s = Circle(r=4)\n\
         s.r"
    );
    assert!(err.contains("match on it"), "unexpected error: {}", err);
}

#[test]
fn test_enum_pattern_wrong_arity_rejected() {
    let err = type_error(
        "data Shape is Rect(w: Int, h: Int)\n\
         let s = Rect(w=1, h=2)\n\
         match s { is Rect(w) then w }"
    );
    assert!(err.contains("expects 2 binding"), "unexpected error: {}", err);
}

#[test]
fn test_enum_unknown_variant_rejected() {
    let err = type_error(
        "data Shape is Circle(r: Int)\n\
         let s = Circle(r=4)\n\
         match s { is Square(x) then x  else 0 }"
    );
    assert!(err.contains("no variant"), "unexpected error: {}", err);
}

// ── unboxed (immediate) payload-less variants ─────────────────────────────────
//
// A variant with no fields of its own, on an enum with no common fields,
// compiles to an immediate tag rather than a `FrogVariant` allocation — see
// gc.rs's "Immediate (unboxed) values". These pin the behaviour that makes
// that safe: such a value still matches, still round-trips through lists and
// struct fields alongside *boxed* variants of the same enum, and the GC
// leaves it alone rather than following it as a pointer.

#[test]
fn test_unit_variants_allocate_nothing() {
    use froglang_core::state::FrogState;

    let mut s = FrogState::new();
    let before = s.heap.bytes_allocated;
    s.eval(
        "data Color is Red | Green | Blue\n\
         let n = 0\n\
         for i in 0..1000 do { let c = if i == 0 then Red else Green\n n = n + (if c is Red then 1 else 0) }\n\
         n"
    ).unwrap();
    // The only allocation this program can make is the range list it loops
    // over; 1000 unit-variant constructions must contribute nothing.
    assert!(
        s.heap.bytes_allocated - before < 16_000,
        "unit variants should not allocate, but the heap grew by {} bytes",
        s.heap.bytes_allocated - before
    );
}

#[test]
fn test_mixed_enum_unboxed_and_boxed_variants_match() {
    // `None_` is unboxed, `Some_` is a real allocation — one enum, two
    // runtime representations, and `is` has to read the tag out of both.
    assert_eq!(compile_and_run(
        "data Opt is None_ | Some_(v: Int)\n\
         func unwrap(o: Opt): Int = match o {\nis None_ then 0\nis Some_(v) then v\n}\n\
         unwrap(None_) + unwrap(Some_(v=7)) + unwrap(None_)"
    ), 7);
}

#[test]
fn test_unboxed_variants_survive_collection_inside_a_list() {
    // A list of enum values has its element slots marked as pointers, so the
    // mark phase walks straight over these immediates; without the low-bit
    // check it would dereference `(tag << 1) | 1` as a `GcHeader`.
    use froglang_core::state::FrogState;

    let mut s = FrogState::new();
    s.eval(
        "data Opt is None_ | Some_(v: Int)\n\
         let opts = [for i in 0..200 do if i == 0 then Some_(v=1) else None_]"
    ).unwrap();
    s.heap.force_collect();
    let (value, _) = s.eval(
        "let n = 0\n\
         for o in opts do { n = n + match o {\nis None_ then 1\nis Some_(v) then v\n} }\n\
         n"
    ).unwrap();
    assert!(matches!(value, froglang_core::state::FrogValue::Int(200)), "got {:?}", value);
}

#[test]
fn test_unboxed_variant_in_a_struct_field_survives_collection() {
    use froglang_core::state::FrogState;

    let mut s = FrogState::new();
    s.eval(
        "data Color is Red | Green | Blue\n\
         data Tag(name: Str, color: Color)\n\
         let t = Tag(name=\"hello\", color=Blue)"
    ).unwrap();
    s.heap.force_collect();
    // `name` must still be intact: the struct's `color` slot is marked as a
    // pointer, and mis-following it would corrupt the trace of `name`.
    let (value, _) = s.eval("t.name + (if t.color is Blue then \"!\" else \"?\")").unwrap();
    assert!(matches!(&value, froglang_core::state::FrogValue::Str(s) if s == "hello!"), "got {:?}", value);
}
