// Tests for Phase 3 (ERRORS.md implementation plan): boxing a value into
// an *anonymous* union (`Int | Str`, `T?`) so it's actually representable
// at runtime, not just type-checked.
//
// Before this, `Type::Union` never reached codegen except as a nominal
// union's own alias (`data X is A | B` — see `test_unions.rs`, Phase 2).
// An annotation like `let x: Int | Str = 5` type-checked but, if ever run,
// degenerated to a raw, untagged `i64` slot — GC-unsafe for any member
// that isn't already a heap pointer, and with no way to tell which member
// was actually stored. Scoped down from the original plan's 2-slot/
// `cond_mask` design (see the plan file): every non-`None` member is
// boxed the same way a nominal union's non-nullary member already is
// (`FrogVariant`, local tag = index in the union's own sorted member
// list), trading one extra allocation for a scalar member against not
// needing a global type-id registry or GC representation changes at all.
// `Str`/`List` members and widening an already-union-typed value into a
// different union are explicitly rejected for now (`TypeChecker::lower_widen`)
// rather than silently generating something wrong.

use froglang_core::codegen::compile_and_run;

fn type_error(src: &str) -> String {
    let ast = froglang_core::frontend::parser::Parser::parse(src).expect("parse error");
    let mut tc = froglang_core::frontend::typeck::TypeChecker::new();
    let err = tc.check_and_lower(ast).expect_err("expected a type error");
    format!("{:?}", err)
}

// ── T? (Int | None) ─────────────────────────────────────────────────────────

#[test]
fn test_optional_int_round_trips_through_a_function_boundary() {
    assert_eq!(compile_and_run(
        "func maybe(cond: Bool): Int? = if cond then 5 else none\n\
         match maybe(true) {\n\
         is Int(n) then n\n\
         is None then -1\n\
         } * 100 + match maybe(false) {\n\
         is Int(n) then n\n\
         is None then -1\n\
         }"
    ), 499);
}

#[test]
fn test_bare_is_type_test_on_optional_int() {
    assert_eq!(compile_and_run(
        "let x: Int? = 5\n\
         if x is Int then 1 else 0"
    ), 1);
    assert_eq!(compile_and_run(
        "let x: Int? = none\n\
         if x is Int then 1 else 0"
    ), 0);
}

#[test]
fn test_unannotated_function_returning_none_infers_optional() {
    // The body's own type (`Int | None`, from the `if`'s branch join) is
    // what the return slot unifies against — no annotation needed.
    assert_eq!(compile_and_run(
        "func maybe(cond: Bool) = if cond then 5 else none\n\
         if maybe(true) is Int then 1 else 0"
    ), 1);
}

// ── general anonymous unions: scalar and struct members ────────────────────

#[test]
fn test_scalar_or_struct_union_call_argument_widening() {
    assert_eq!(compile_and_run(
        "data Oops(msg: Str)\n\
         func classify(x: Int | Oops): Int = match x {\n\
         is Int(n) then n * 10\n\
         is Oops(o) then -1\n\
         }\n\
         classify(5) * 100 + classify(Oops(msg=\"bad\"))"
    ), 4999);
}

#[test]
fn test_struct_field_of_union_type() {
    assert_eq!(compile_and_run(
        "data Cell(v: Int | Str)\n\
         let c = Cell(v=5)\n\
         match c.v {\n\
         is Int(n) then n\n\
         is Str(s) then -1\n\
         }"
    ), 5);
}

#[test]
fn test_wildcard_bind_skips_narrowing_the_value() {
    assert_eq!(compile_and_run(
        "let x: Int | Bool = 7\n\
         match x {\n\
         is Int(_) then 1\n\
         is Bool(_) then 0\n\
         }"
    ), 1);
}

#[test]
fn test_scalar_union_member_survives_gc_pressure() {
    // Each loop iteration widens a fresh Int into `Int | Oops` (a real
    // heap allocation — see the module doc comment), forcing at least one
    // real collection before the loop ends. If the shadow-frame rooting
    // for `Widen`'s box (`for_each_heap_producer`'s matching arm) were out
    // of sync, the running `total` would corrupt partway through.
    assert_eq!(compile_and_run(
        "data Oops(msg: Str)\n\
         func classify(x: Int | Oops): Int = match x {\n\
         is Int(n) then n\n\
         is Oops(o) then -1\n\
         }\n\
         mut total = 0\n\
         for i in 0..3000 do {\n\
         let v: Int | Oops = i\n\
         total = total + classify(v)\n\
         }\n\
         total"
    ), 3000 * 2999 / 2);
}

// ── errors ───────────────────────────────────────────────────────────────────

#[test]
fn test_non_exhaustive_anonymous_match_is_rejected() {
    let err = type_error(
        "func f(x: Int?): Int = match x {\nis Int(n) then n\n}"
    );
    assert!(err.contains("Non-exhaustive"), "unexpected error: {}", err);
}

#[test]
fn test_is_on_a_non_union_value_is_rejected() {
    let err = type_error("5 is Str");
    assert!(err.contains("Can only use 'is' on a union value"), "unexpected error: {}", err);
}

// ── Str/List members ────────────────────────────────────────────────────────
//
// `Str`/`List` used to be rejected as union members (`TypeChecker::lower_widen`
// used to special-case them out) because nothing had actually verified the
// double-boxing story: a `Str`/`List` value is already its own heap object
// (`FrogStr`/`FrogList`, self-describing via `ObjKind`), so widening one into
// a union boxes it a *second* time — a `FrogVariant` whose single payload
// slot holds a pointer to the already-heap-allocated value — exactly the
// same `box_into_variant` path a plain struct member already used. That
// turned out to need no new codegen at all (`box_into_variant`,
// `compile_narrow`, and `for_each_heap_producer`'s `Widen` arm are all
// already generic over the boxed value's type), so only the `lower_widen`
// rejection itself was ever the blocker.

#[test]
fn test_str_member_round_trips() {
    assert_eq!(compile_and_run(
        "func describe(x: Int | Str): Int = match x {\n\
         is Int(n) then n\n\
         is Str(s) then -1\n\
         }\n\
         describe(\"hi\") * 1000 + describe(5)"
    ), -1000 + 5);
}

#[test]
fn test_list_member_round_trips() {
    // `Str | List<Int>` has no scalar (`Int`/`Float`/`Bool`) member, so this
    // is the one-slot boxed path, not the two-slot `cond_mask` one — both
    // members are boxed via `box_into_variant` exactly the same way.
    //
    // Discriminates via `is Str` only, not `is List<l>`: pattern-matching a
    // bare parametric type name is a separate, pre-existing gap
    // (`TypeChecker::check_type_pattern`'s `resolve_type_name` has no case
    // for `List`, `Function`, etc — unrelated to whether `List` is a legal
    // union member, which is what this test is actually about) — a
    // `List`-typed union member is fully constructible and discriminable by
    // tag today, just not yet destructurable by a `List<...>` pattern.
    assert_eq!(compile_and_run(
        "func is_str(x: Str | List<Int>): Int = if x is Str then 1 else 0\n\
         is_str(\"hi\") * 10 + is_str([1, 2, 3])"
    ), 10);
}

#[test]
fn test_str_union_member_survives_gc_pressure() {
    // Half the elements box a fresh `Str` (a real heap allocation each
    // time) into `Int | Str`'s single payload slot, forcing several real
    // collections while the list is still being built. A wrong `ptr_mask`
    // on the wrapping `FrogVariant` would either free a still-live boxed
    // string or crash trying to follow a raw `Int` payload as a pointer.
    assert_eq!(compile_and_run(
        "let xs: List<Int | Str> = [for i in 0..8000 do (if i - (i / 2) * 2 == 0 then i else \"hello\")]\n\
         mut total = 0\n\
         for i in 0..8000 do (total = total + match xs[i] {\n\
         is Int(n) then n\n\
         is Str(s) then 0\n\
         })\n\
         total"
    ), (0..8000i64).step_by(2).sum());
}

#[test]
fn test_assigning_into_a_union_typed_struct_field_widens() {
    // A field assignment places a value into a union-typed slot exactly as
    // much as `StructInit` does, so it needs the same `lower_widen` — the
    // field-assign branch of `Expression::Assign` used to skip it, storing
    // the raw immediate `9` over the boxed field and leaving the following
    // `TypeTag`/`Narrow` to dereference `9` as a `FrogVariant*`.
    assert_eq!(compile_and_run(
        "data Cell(v: Int | Bool)\n\
         mut c = Cell(v=5)\n\
         c.v = 9\n\
         match c.v {\n\
         is Int(n) then n\n\
         is Bool(b) then -1\n\
         }"
    ), 9);
}

// ── a scalar-carrying union as a list element ─────────────────────────────────
//
// A `List<Int | Point>` element is laid out inline (`codegen::UnionLayout`):
// a tagged pointer column plus a scalar column, no allocation for the `Int`
// case. Both columns' pointer-ness is static — the scalar column is never
// scanned, and the pointer column is scanned unconditionally, with the
// uniform `gc::is_heap_ptr` rule screening a tag-only word out. These tests
// exercise it end to end, including under real GC pressure, where getting
// the columns wrong would either free a still-live boxed member
// (read-after-free) or follow a raw `Int` payload as if it were a pointer.

#[test]
fn test_list_of_scalar_and_struct_union_round_trips() {
    assert_eq!(compile_and_run(
        "data Point(x: Int, y: Int)\n\
         let xs: List<Int | Point> = [1, Point(x=3, y=4), 5]\n\
         let a = match xs[0] { is Int(n) then n\nis Point(p) then -1\n }\n\
         let b = match xs[1] { is Int(n) then -1\nis Point(p) then p.x + p.y\n }\n\
         let c = match xs[2] { is Int(n) then n\nis Point(p) then -1\n }\n\
         a * 100 + b * 10 + c"
    ), 100 + 7 * 10 + 5);
}

#[test]
fn test_list_of_scalar_union_survives_gc_pressure() {
    // Half the elements box a `Point` (a real heap allocation each time),
    // forcing several real collections while the list is still growing via
    // the comprehension — if `cond_mask`/`boxed_tags` were wrong, either the
    // still-live `Point`s would get swept as garbage (the odd-index reads
    // below would read freed memory) or a raw `Int` payload would get
    // misread as a pointer during marking (a crash, not a wrong answer).
    assert_eq!(compile_and_run(
        "data Point(x: Int, y: Int)\n\
         let xs: List<Int | Point> = [for i in 0..6000 do (if i - (i / 2) * 2 == 0 then i else Point(x=i, y=i))]\n\
         mut total = 0\n\
         for i in 0..6000 do (total = total + match xs[i] {\n\
         is Int(n) then n\n\
         is Point(p) then p.x\n\
         })\n\
         total"
    ), 6000 * 5999 / 2);
}

#[test]
fn test_two_distinct_scalar_unions_in_one_struct() {
    // This combination used to be a *type error*: the old two-slot union
    // representation described a payload slot's pointer-ness with one
    // shared `(cond_mask, boxed_tags)` pair per GC-scanned aggregate, which
    // two different scalar-carrying union shapes in the same struct could
    // not both use, and `TypeChecker::check_scalar_union_consistency`
    // rejected them rather than miscompile.
    //
    // Under `codegen::UnionLayout` each union owns its own columns and each
    // column's pointer-ness is static, so there is nothing shared left to
    // conflict — the restriction, the check, and the error message are all
    // gone. Exercised through a list (which is where the aggregate scanning
    // actually happens) and under `FrogState`, since that is what runs
    // `validate_codegen_constraints`.
    use froglang_core::state::FrogState;
    let mut s = FrogState::new();
    let result = s.eval(
        "data Point(x: Int, y: Int)\n\
         data Line(a: Point, b: Point)\n\
         data Both(m: Int | Point, n: Float | Line)\n\
         let items: List<Both> = [Both(m=1, n=2.0), Both(m=Point(x=3, y=4), n=5.0)]\n\
         match items[1].m { is Int(k) then k\nis Point(p) then p.x + p.y\n }"
    );
    match result {
        Ok((v, _)) => assert_eq!(v.display_str(), "7"),
        other => panic!("expected 7, got {:?}", other),
    }
}

#[test]
fn test_for_loop_over_scalar_union_list_allocating_in_the_body() {
    // Regression, from the era of the old two-slot union representation:
    // `compile_for_loop` used to root each element leaf on `is_heap_ty(leaf)`
    // alone, which was `true` for a two-slot union's payload — so the
    // *scalar* payload of an `Int` element (a raw integer, not a pointer)
    // got rooted as if it were one. The collector dereferenced every
    // non-zero root unchecked, so any collection triggered from inside the
    // loop body dereferenced 4000004 as a heap object. `codegen::UnionLayout`
    // now makes a scalar column statically un-scannable, so there's nothing
    // left for this class of bug to trip over; `burn` allocates hard enough
    // to force real collections while the loop variable is live.
    assert_eq!(compile_and_run(
        "data Point(x: Int, y: Int)\n\
         func burn(k: Int): Int = [for i in 0..40000 do Point(x=i, y=i)][k].x\n\
         let xs: List<Int | Point> = [4000000, 4000002, 4000004]\n\
         mut total = 0\n\
         for x in xs do (total = total + burn(1) + match x {\n\
         is Int(n) then n\n\
         is Point(p) then p.x\n\
         })\n\
         total"
    ), 4000000 + 4000002 + 4000004 + 3);
}

#[test]
fn test_indexing_a_scalar_union_list_allocating_between_reads() {
    // The same mis-rooting as above, on the `xs[i]` path
    // (`TypedExprKind::Index`): the read below is interleaved with an
    // allocation heavy enough to collect while the indexed element is
    // still rooted.
    assert_eq!(compile_and_run(
        "data Point(x: Int, y: Int)\n\
         func burn(k: Int): Int = [for i in 0..40000 do Point(x=i, y=i)][k].x\n\
         let xs: List<Int | Point> = [4000000, 4000002, 4000004]\n\
         mut total = 0\n\
         for i in 0..3 do (total = total + match xs[i] {\n\
         is Int(n) then n + burn(1)\n\
         is Point(p) then p.x + burn(1)\n\
         })\n\
         total"
    ), 4000000 + 4000002 + 4000004 + 3);
}

// ── Checking a value into a union whose member is not a literal match ────────
//
// `unify`'s concrete-vs-union arm accepts a member only by *equality* and
// binds nothing, which left two shapes uncheckable: a union member that is a
// generic struct's own binder, and a member a value merely *unifies* with
// (an empty list literal, whose element type is still a variable). Every site
// that checks a value against an expected type now falls back to unifying it
// with the one union member it fits.

#[test]
fn an_empty_list_literal_widens_into_a_union_with_a_list_member() {
    // `[]` synthesizes `List(~t0)`, which equals no member of
    // `List<Str> | Int` but unifies with exactly one. Discriminated by
    // `is Int` rather than a `List<...>` pattern, which is a separate
    // pre-existing gap — see `test_list_member_round_trips`.
    assert_eq!(
        compile_and_run("let x: List<Str> | Int = []\nif x is Int then 0 else 1"),
        1,
    );
}

#[test]
fn a_union_annotated_slot_still_accepts_each_ordinary_member() {
    // The fallback must not have displaced the plain paths: an ordinary
    // member still goes through `unify`'s own equality arm.
    assert_eq!(compile_and_run("let y: List<Str> | Int = 5\nif y is Int then 0 else 1"), 0);
    assert_eq!(compile_and_run("let z: List<Str> | Int = [\"a\"]\nif z is Int then 0 else 1"), 1);
}

#[test]
fn ambiguous_membership_is_rejected_rather_than_guessed() {
    // `[]` unifies with *both* members, and picking one would silently pick a
    // runtime tag, so this has to fail rather than choose.
    let err = type_error("let x: List<Int> | List<Str> = []\nx");
    assert!(err.contains("Expected"), "{}", err);
}

#[test]
fn a_generic_structs_union_field_accepts_a_value_that_pins_the_binder() {
    // `data Box<A>(v: A | Str)` — `Int` equals neither `Str` nor the binder
    // `~t`, but unifies with the binder, which is what fixes `A = Int`.
    let src = "\
        data Box<A>(v: A | Str)\n\
        let b = Box(v=1)\n\
        match b.v {\n\
        is Int(n) then n\n\
        is Str(_) then -1\n\
        }";
    assert_eq!(compile_and_run(src), 1);
}
