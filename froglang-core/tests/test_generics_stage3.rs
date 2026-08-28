//! End-to-end tests for `TRAITS.md` Stage 3a: `data Name<A, B>(...)`
//! generic struct declarations (plus the Stage 3b monomorphization cases
//! that only show up through them). Unlike a generic `func` (Stage 3b,
//! which mangles a separate compiled body per instantiation), a struct's "instantiation" is purely a layout computation
//! — `TypeChecker::instantiate_struct`/`materialize_struct` substitute
//! concrete type arguments into a stored field template on demand, with no
//! mangled symbol names or duplicated compiled bodies. These tests go
//! through the real binary (`common::run`), so they exercise the whole
//! pipeline: parsing `<A, B>`, typeck's per-call fresh instantiation, and
//! codegen's `struct_fields` looking up the concrete layout registered
//! under `Type::struct_key`.

mod common;

/// The plan's core acceptance case: one generic struct constructed and
/// field-accessed at two different concrete instantiations in the same
/// program. If the two instantiations shared one layout (or one clobbered
/// the other's registered field types), this would print the wrong values
/// or crash in codegen rather than printing distinct, correct results.
#[test]
fn a_generic_struct_used_at_two_instantiations_in_one_program() {
    let src = "\
        data Pair<A, B>(fst: A, snd: B)\n\
        let p = Pair(fst=1, snd=2)\n\
        let q = Pair(fst=\"a\", snd=3.5)\n\
        print(p.fst + p.snd)\n\
        print(q.fst)\n\
        print(q.snd)";
    assert_eq!(common::run(src), "3\na\n3.5\n");
}

/// A three-field generic struct nested inside another (non-generic)
/// struct's field, to exercise `struct_fields`' recursive flattening
/// picking up the substituted (not placeholder) field types at every
/// level, not just the top one.
#[test]
fn a_generic_struct_field_inside_another_struct_flattens_correctly() {
    let src = "\
        data Pair<A, B>(fst: A, snd: B)\n\
        data Wrapper(inner: Pair<Int, Int>, tag: Str)\n\
        let w = Wrapper(inner=Pair(fst=10, snd=20), tag=\"w\")\n\
        print(w.inner.fst + w.inner.snd)\n\
        print(w.tag)";
    assert_eq!(common::run(src), "30\nw\n");
}

/// `Pair<A, B>` printed via the builtin `print` — `codegen::print_value`
/// must print the bare declared name (`Pair(...)`) as the prefix, not the
/// mangled `structs` lookup key (`Pair(Int, Str)(...)`), while still
/// finding the concrete field layout to actually print the values.
#[test]
fn a_generic_struct_prints_with_its_bare_name_not_its_mangled_layout_key() {
    let src = "data Pair<A, B>(fst: A, snd: B)\nprint(Pair(fst=1, snd=\"x\"))";
    assert_eq!(common::run(src), "Pair(fst=1, snd=\"x\")\n");
}

/// `X<Y>=5` (no space before `=`) lexes as one `Token::GtEq` — the
/// `Grammar::expect_close_angle` split hazard the plan calls out. This
/// isn't about generic structs at all, just confirming the split doesn't
/// regress an ordinary annotated-`let` whose value happens to abut the
/// closing `>`.
#[test]
fn no_space_before_assign_after_a_generic_type_annotation_still_parses() {
    let src = "data Pair<A, B>(fst: A, snd: B)\nlet p: Pair<Int, Int>=Pair(fst=1, snd=2)\nprint(p.fst)";
    assert_eq!(common::run(src), "1\n");
}

/// A generic whose body calls *another* generic. The inner call's type is
/// only concrete once the outer body has been cloned and its binders
/// substituted, so collecting instantiations in a single pass over the
/// pre-monomorphization tree records `id` at `idpair`'s own unresolved
/// binder `TypeVar` and never emits `id$Int` at all — used to panic in
/// codegen with "no entry found for key". `monomorphize_generics` now
/// re-collects from each body it emits, to a fixed point.
#[test]
fn a_generic_calling_another_generic_instantiates_the_inner_one() {
    let src = "\
        func id(x) = x\n\
        func idpair(a) = id(a)\n\
        print(idpair(3))\n\
        print(idpair(\"hi\"))";
    assert_eq!(common::run(src), "3\nhi\n");
}

/// The same fixed point, three levels deep and at two distinct
/// instantiations — one round of re-collection isn't enough, the worklist
/// has to keep draining.
#[test]
fn a_three_deep_generic_call_chain_instantiates_every_level() {
    let src = "\
        func id(x) = x\n\
        func mid(x) = id(x)\n\
        func outer(x) = mid(x)\n\
        print(outer(7))\n\
        print(outer(\"s\"))";
    assert_eq!(common::run(src), "7\ns\n");
}

/// `==` on a generic struct instantiation. The desugaring looks the field
/// layout up through `materialize_struct`, not by the bare declared name:
/// the name alone keys the template, whose field types are still binder
/// `TypeVar`s, and codegen would compare a `Str` field as a raw `I64` —
/// reporting two identical `Pair`s unequal.
#[test]
fn equality_on_a_generic_struct_compares_str_fields_structurally() {
    let src = "\
        data Pair<A, B>(fst: A, snd: B)\n\
        let p = Pair(fst=\"ab\", snd=\"cd\")\n\
        let q = Pair(fst=\"ab\", snd=\"cd\")\n\
        let r = Pair(fst=\"ab\", snd=\"zz\")\n\
        print(p == q)\n\
        print(p == r)\n\
        print(p != r)";
    assert_eq!(common::run(src), "true\nfalse\ntrue\n");
}

/// The same, with a generic struct nested in a generic struct's field, so
/// `build_struct_eq`'s recursive step also has to carry the concrete type
/// down rather than the inner struct's bare name.
#[test]
fn equality_recurses_into_a_nested_generic_struct_field() {
    let src = "\
        data Inner<T>(v: T)\n\
        data Pair<A, B>(fst: A, snd: B)\n\
        let p = Pair(fst=\"ab\", snd=Inner(v=\"z\"))\n\
        let q = Pair(fst=\"ab\", snd=Inner(v=\"z\"))\n\
        let r = Pair(fst=\"ab\", snd=Inner(v=\"y\"))\n\
        print(p == q)\n\
        print(p == r)";
    assert_eq!(common::run(src), "true\nfalse\n");
}

/// Redefining a generic in the same entry. The old `stmts.retain` dropped
/// *every* top-level function assign whose name had ever been generalized,
/// including this second, monomorphic one — and then rewrote its call site
/// to the first declaration's instantiation, printing `1` twice with no
/// diagnostic. Stripping is keyed by the declaration's own template symbol
/// now, so an unrelated declaration that merely reuses the name survives.
#[test]
fn redefining_a_generic_with_a_monomorphic_binding_uses_the_new_body() {
    let src = "\
        let f = x -> x\n\
        print(f(1))\n\
        let f = x -> x + 100\n\
        print(f(1))";
    assert_eq!(common::run(src), "1\n101\n");
}

/// The same, where the redefinition is itself generic: two independent
/// templates, each with its own instantiations, both callable at several
/// types.
#[test]
fn redefining_a_generic_with_another_generic_keeps_them_separate() {
    let src = "\
        let f = x -> x\n\
        print(f(1))\n\
        print(f(\"a\"))\n\
        let f = x -> x + x\n\
        print(f(1))\n\
        print(f(1.5))";
    assert_eq!(common::run(src), "1\na\n2\n3.0\n");
}

/// A parameter that merely shares its name with a generic is not an
/// instantiation of it. Matching `Var` nodes by bare name recorded this
/// `id: Int` as one, mangled it to `id$None`, and rewrote the innocent
/// parameter reference to that symbol — an "unbound variable in codegen"
/// panic. Each `Var` now carries the template symbol its own scope
/// resolved to, or none at all.
#[test]
fn a_parameter_shadowing_a_generic_is_not_an_instantiation_of_it() {
    let src = "\
        func id(x) = x\n\
        func g(id: Int): Int = id * 2\n\
        print(g(4))\n\
        print(id(9))\n\
        print((7).id())";
    assert_eq!(common::run(src), "8\n9\n7\n");
}

/// A generic given a second name with `let`, then called at two distinct
/// types through the alias and at a third through the original name. The
/// alias carries the target's binders and template symbol, so all three
/// instantiate through one template.
#[test]
fn an_alias_of_a_generic_is_callable_at_several_types() {
    let src = "\
        func id(x) = x\n\
        let f = id\n\
        let h = f\n\
        print(h(3))\n\
        print(h(\"s\"))\n\
        print(id(1.5))\n\
        print((9).f())";
    assert_eq!(common::run(src), "3\ns\n1.5\n9\n");
}

/// An alias inherits the target's declared `mut` parameters — `lower_call`
/// looks those up by the *source* callee name, which at the call site is
/// the alias, so they have to be copied across at declaration time or the
/// receiver-mutation guard silently stops applying.
#[test]
fn an_alias_inherits_the_targets_mut_parameters() {
    let src = "\
        func addto(mut xs: List<Int>): None = xs.push(9)\n\
        let s = addto\n\
        mut ys = [1]\n\
        ys.s()\n\
        print(ys)";
    assert_eq!(common::run(src), "[1, 9]\n");
}

// ── Recursion ────────────────────────────────────────────────────────────────
//
// A function could only call itself when it was *fully* annotated, because
// that was the only case `lower_assign` pre-bound the name for. Generalization
// happens exactly when the annotations run out, so "generic" and "recursive"
// were mutually exclusive — which made the recursive case
// `monomorphize_generics` and `rewrite_call_sites` are written to handle
// unreachable. A not-fully-annotated function is now pre-bound to a
// placeholder built from fresh vars, so its body can call it at one type
// (monomorphic recursion) and the result is generalized afterwards.

/// The plain case: no annotations at all, one recursive call. This used to
/// be "Unbound variable fact".
#[test]
fn an_unannotated_function_can_call_itself() {
    let src = "\
        func fact(n) = if n <= 1 then 1 else n * fact(n - 1)\n\
        print(fact(5))";
    assert_eq!(common::run(src), "120\n");
}

/// A let-bound lambda is generalized by the same value-restriction rule as a
/// `func`, so it gets the same pre-bind and can recurse too.
#[test]
fn a_let_bound_lambda_can_call_itself() {
    let src = "\
        let fib = [n] -> if n < 2 then n else fib(n - 1) + fib(n - 2)\n\
        print(fib(10))";
    assert_eq!(common::run(src), "55\n");
}

/// Recursive *and* generic: the parameter `x` is never constrained, so this
/// generalizes to one binder and monomorphizes into two bodies, each of whose
/// recursive calls must resolve to its own instantiation rather than to the
/// stripped template.
#[test]
fn a_recursive_generic_is_monomorphized_at_every_instantiation() {
    let src = "\
        func count(x, n: Int) = if n == 0 then 0 else 1 + count(x, n - 1)\n\
        print(count(\"a\", 3))\n\
        print(count(1, 2))";
    assert_eq!(common::run(src), "3\n2\n");
}

/// The pre-bind must not cost the declaration its polymorphism: the
/// placeholder is in scope while the body is checked, so an environment scan
/// that saw it would find every one of the function's own vars "captured" and
/// generalize nothing.
#[test]
fn the_recursion_prebind_does_not_stop_a_function_generalizing() {
    let src = "\
        func id(x) = x\n\
        print(id(1))\n\
        print(id(\"s\"))\n\
        print(id(2.5))";
    assert_eq!(common::run(src), "1\ns\n2.5\n");
}

/// Polymorphic recursion — self-calls at two *different* types within one
/// body — is undecidable to infer, so it is reported rather than guessed at.
/// The pre-bind is what makes this reachable at all (before it, the name was
/// simply unbound); the placeholder binds `x` at the first self-call's type,
/// and the second one then fails the ordinary argument check, which reports
/// it more precisely than `lower_assign`'s own whole-signature backstop
/// would. Asserted as "rejected, naming the two types" rather than on one
/// exact message, since which of the two guards fires first is not the point.
#[test]
fn recursion_at_two_different_types_is_rejected() {
    let src = "\
        func f(x, n: Int) = if n == 0 then 0 else f(\"s\", n - 1) + f(1, n - 1)\n\
        print(f(1, 2))";
    let out = common::run_raw(src);
    assert_ne!(out.status, Some(0), "expected a type error, got: {}", out.stdout);
    let reported = format!("{}{}", out.stdout, out.stderr);
    assert!(reported.contains("Int") && reported.contains("Str"), "{}", reported);
}
