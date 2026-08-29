//! Traits — `plans/TRAITS.md` Stage 5, plus the declaration half of Stage 6.
//!
//! What this covers:
//!
//!   * `trait Name { ... }` declarations, with member signatures and default
//!     bodies, and `Self` as a binder over the implementing type.
//!   * `provides` with a body, in both positions: inline on a `data`
//!     declaration, and standalone (`provides T for Int { ... }`) for a type
//!     you don't own.
//!   * Resolution step 2 (`TRAITS.md` Part 1): `x.f()` finds a member of an
//!     impl for `typeof(x)`, after a field and before a free function.
//!   * The trait-qualified prefix form, `Trait.member(x)`.
//!   * Zero-`Self` members (`zero(): Self`), resolved from the expected type.
//!   * The coherence and partitioning rules `register_impl` enforces at the
//!     declaration, which is what keeps step 2 a hash lookup.
//!   * `<T: Bound>` binders on `func` and `data`, and the requirement that
//!     declared bounds cover the inferred ones.
//!
//! Deliberately *not* covered, because it isn't built (see TRAITS.md Stage 6):
//! operators desugaring to member calls, structural `Show`, `Error::message`,
//! and user impls of `Num`. Those keep their current intrinsic paths.

mod common;
use common::run;

/// A type error's rendered message. Mirrors `tests/test_errors.rs`'s helper —
/// the checker is enough for every rejection here, since all of them are
/// raised before codegen.
fn type_error(src: &str) -> String {
    let ast = froglang_core::frontend::parser::Parser::parse(src).expect("parse error");
    let mut tc = froglang_core::frontend::typeck::TypeChecker::new();
    let err = tc.check_and_lower(ast).expect_err("expected a type error");
    err.item.msg
}

// ── Declaring and implementing ───────────────────────────────────────────

#[test]
fn a_member_is_called_through_the_receiver() {
    let src = r#"
trait Shape {
    func area(s: Self): Int
}
data Circle(r: Int) provides Shape {
    func area(c: Circle): Int = c.r * c.r
}
print(Circle(r=3).area())
"#;
    assert_eq!(run(src), "9\n");
}

#[test]
fn the_prefix_form_is_trait_qualified() {
    let src = r#"
trait Shape {
    func area(s: Self): Int
}
data Circle(r: Int) provides Shape {
    func area(c: Circle): Int = c.r * c.r
}
print(Shape.area(Circle(r=4)))
"#;
    assert_eq!(run(src), "16\n");
}

#[test]
fn a_standalone_impl_works_on_a_primitive() {
    // The `provides T for Type { ... }` form exists for types whose
    // declaration you don't own — a primitive being the sharpest case, and
    // the one that proves the impl table isn't keyed on `data` names only.
    let src = r#"
trait Describe {
    func describe(s: Self): Str
}
provides Describe for Int {
    func describe(n: Int): Str = "an int"
}
print((7).describe())
"#;
    assert_eq!(run(src), "an int\n");
}

#[test]
fn a_two_self_member_dispatches_on_its_first_argument() {
    // Both parameters being `Self` is why members are plain functions rather
    // than methods with a receiver (`TRAITS.md` Part 1) — a receiver model
    // would have to nominate one of the two as special.
    let src = r#"
trait Pick {
    func bigger(a: Self, b: Self): Self
}
data Box(n: Int) provides Pick {
    func bigger(a: Box, b: Box): Box = if a.n > b.n then a else b
}
print(Pick.bigger(Box(n=2), Box(n=9)).n)
"#;
    assert_eq!(run(src), "9\n");
}

#[test]
fn a_mut_self_member_needs_no_marker_at_the_dot_call() {
    // The receiver exemption (`TRAITS.md` Part 2): the marker exists to make
    // a hidden mutated operand visible, and the receiver is the least hidden
    // position there is. The declaration-site mutability check still applies.
    let src = r#"
trait Counter {
    func bump(mut s: Self): None
}
data C(n: Int) provides Counter {
    func bump(mut s: C): None = { s.n = s.n + 1 }
}
mut c = C(n=1)
c.bump()
c.bump()
print(c.n)
"#;
    assert_eq!(run(src), "3\n");
}

#[test]
fn a_member_may_omit_annotations_and_may_spell_self() {
    // An omitted annotation is filled in from the trait's own declaration
    // with `Self` substituted, so all three spellings mean the same thing.
    let src = r#"
trait Shape {
    func area(s: Self): Int
}
data A(r: Int) provides Shape { func area(c) = c.r }
data B(r: Int) provides Shape { func area(c: Self): Int = c.r }
data C(r: Int) provides Shape { func area(c: C): Int = c.r }
print(A(r=1).area() + B(r=2).area() + C(r=3).area())
"#;
    assert_eq!(run(src), "6\n");
}

// ── Default bodies ───────────────────────────────────────────────────────

#[test]
fn a_default_body_is_used_when_an_impl_omits_the_member() {
    let src = r#"
trait Shape {
    func area(s: Self): Int
    func describe(s: Self): Str = "a shape"
}
data Circle(r: Int) provides Shape {
    func area(c: Circle): Int = c.r
}
data Sq(w: Int) provides Shape {
    func area(s: Sq): Int = s.w
    func describe(s: Sq): Str = "a square"
}
print(Circle(r=1).describe())
print(Sq(w=1).describe())
"#;
    assert_eq!(run(src), "a shape\na square\n");
}

#[test]
fn a_default_body_calls_another_member_and_specializes_per_type() {
    // The default is re-checked and re-emitted per implementing type, so
    // `s.area()` inside it resolves to *that* type's impl — monomorphization
    // by construction, with no template kept.
    let src = r#"
trait Shape {
    func area(s: Self): Int
    func doubled(s: Self): Int = s.area() * 2
}
data Circle(r: Int) provides Shape { func area(c: Circle): Int = c.r * c.r }
data Sq(w: Int) provides Shape { func area(s: Sq): Int = s.w }
print(Circle(r=3).doubled())
print(Sq(w=5).doubled())
"#;
    assert_eq!(run(src), "18\n10\n");
}

// ── Zero-`Self` members ──────────────────────────────────────────────────

#[test]
fn a_zero_self_member_resolves_from_the_expected_type() {
    let src = r#"
trait Zero {
    func zero(): Self
}
data Money(cents: Int) provides Zero {
    func zero(): Money = Money(cents=0)
}
provides Zero for Int {
    func zero(): Int = 0
}
let m: Money = zero()
let n = Zero.zero(): Int
print(m.cents + n)
"#;
    assert_eq!(run(src), "0\n");
}

#[test]
fn a_zero_self_member_without_an_expected_type_says_to_annotate() {
    let src = "\
        trait Zero { func zero(): Self }\n\
        data Money(cents: Int) provides Zero { func zero(): Money = Money(cents=0) }\n\
        let m = zero()\n\
        m";
    assert_eq!(
        type_error(src),
        "cannot infer which 'zero' is meant; annotate the expected type"
    );
}

#[test]
fn a_member_with_a_self_parameter_does_not_claim_the_bare_name() {
    // Only a zero-`Self` member has nothing to dispatch on, so only it may be
    // resolved from the expected type. A member that takes `Self` dispatches
    // on its argument, and must leave the bare name to whatever else owns
    // it — here the `len` builtin, and `Bag` construction.
    let src = r#"
trait Sized { func len(s: Self): Int }
data Bag(n: Int) provides Sized { func len(b: Bag): Int = b.n }
print(len([1, 2, 3]))
print(Bag(n=9).len())
"#;
    assert_eq!(run(src), "3\n9\n");
}

// ── Unions ───────────────────────────────────────────────────────────────

#[test]
fn a_member_call_on_a_union_dispatches_to_each_members_impl() {
    // A union satisfies a trait iff every member does (`TRAITS.md` Part 1,
    // "Traits and unions"), and the call becomes the dispatch `match` that
    // `?`/`catch` already build from the same pieces.
    let src = r#"
trait Shape {
    func area(s: Self): Int
}
data Sh is Circle(r: Int) | Sq(w: Int)
provides Shape for Sh.Circle { func area(c: Sh.Circle): Int = c.r * 2 }
provides Shape for Sh.Sq { func area(s: Sh.Sq): Int = s.w * s.w }
func report(x: Sh): Int = x.area()
print(report(Sh.Circle(r=5)))
print(report(Sh.Sq(w=4)))
"#;
    assert_eq!(run(src), "10\n16\n");
}

#[test]
fn a_union_member_call_falls_through_when_one_member_has_no_impl() {
    // Not an error of its own: it falls back to step 3, so the message is the
    // ordinary "no such member or function" one.
    let src = "\
        trait Shape { func area(s: Self): Int }\n\
        data Sh is Circle(r: Int) | Sq(w: Int)\n\
        provides Shape for Sh.Circle { func area(c: Sh.Circle): Int = c.r }\n\
        func report(x: Sh): Int = x.area()\n\
        report(Sh.Circle(r=1))";
    assert!(
        type_error(src).contains("there's no function 'area' to call as a method"),
        "unexpected: {}", type_error(src)
    );
}

#[test]
fn a_nominal_union_may_be_implemented_as_one_type() {
    // The impl's `Self` is the union itself — one body, taking the union —
    // but a *value* of it is always one of the variants, so it is registered
    // under each variant's key, where dispatch actually looks.
    let src = r#"
trait Shape { func area(s: Self): Int }
data Sh is Circle(r: Int) | Sq(w: Int) provides Shape {
  func area(s: Sh): Int = match s {
    is Circle(r) then r * 2
    is Sq(w) then w * w
  }
}
let a: Sh = Circle(r=5)
let b: Sh = Sq(w=4)
print(a.area())
print(b.area())
print(Shape.area(b))
"#;
    assert_eq!(run(src), "10\n16\n16\n");
}

#[test]
fn the_prefix_forms_trait_is_honoured_on_a_union_receiver() {
    // The union dispatch builds arms that re-lower as *unqualified* calls, so
    // the named trait has to be checked before they're built — otherwise a
    // wrong-trait call is accepted on a union while the same call on one of
    // its members is rejected.
    let src = "\
        trait A { func f(s: Self): Int }\n\
        trait B { func f(s: Self): Int }\n\
        data X(n: Int) provides A { func f(x: X): Int = x.n }\n\
        data Y(n: Int) provides A { func f(y: Y): Int = y.n * 10 }\n\
        let u: X | Y = Y(n=7)\n\
        B.f(u)";
    assert!(
        type_error(src).contains("implements 'f' from trait 'A', not 'B'"),
        "unexpected: {}", type_error(src)
    );
}

// ── Resolution order ─────────────────────────────────────────────────────

#[test]
fn a_field_still_wins_over_a_same_named_member() {
    // Step 1 before step 2, unconditionally — so no existing program can
    // change meaning when a trait is introduced.
    let src = r#"
trait T { func n(s: Self): Int }
data D(n: Int) provides T { func n(s: D): Int = 99 }
print(D(n=5).n)
"#;
    assert_eq!(run(src), "5\n");
}

#[test]
fn a_member_wins_over_a_same_named_free_function() {
    // Step 2 before step 3.
    let src = r#"
trait T { func size(s: Self): Int }
data D(v: Int) provides T { func size(s: D): Int = 1 }
func size(d: D): Int = 2
print(D(v=0).size())
"#;
    assert_eq!(run(src), "1\n");
}

#[test]
fn reading_a_member_without_calling_it_says_to_call_it() {
    let src = "\
        trait T { func f(s: Self): Int }\n\
        data D(n: Int) provides T { func f(s: D): Int = 1 }\n\
        D(n=1).f";
    assert_eq!(
        type_error(src),
        "'f' is a member of trait 'T', not a field — call it: x.f(...)"
    );
}

// ── Coherence and declaration-time rules ─────────────────────────────────

#[test]
fn one_type_cannot_implement_one_trait_twice() {
    let src = "\
        trait Shape { func area(s: Self): Int }\n\
        data Circle(r: Int) provides Shape { func area(c: Circle): Int = 1 }\n\
        provides Shape for Circle { func area(c: Circle): Int = 2 }\n\
        1";
    assert_eq!(
        type_error(src),
        "'Circle' already implements 'Shape' — one impl per trait per type"
    );
}

#[test]
fn two_traits_declaring_one_member_name_collide_at_the_impl_not_the_call() {
    // The one ambiguity resolution step 2 could still have, answered where
    // the error can name both impls rather than at some later call site.
    let src = "\
        trait A { func f(s: Self): Int }\n\
        trait B { func f(s: Self): Int }\n\
        data D(n: Int) provides A { func f(s: D): Int = 1 }\n\
        provides B for D { func f(s: D): Int = 2 }\n\
        1";
    assert_eq!(
        type_error(src),
        "'D' already has a member 'f' from trait 'A'; 'B' can't also provide it"
    );
}

#[test]
fn a_missing_member_with_no_default_is_rejected() {
    let src = "\
        trait Shape { func area(s: Self): Int }\n\
        data Circle(r: Int) provides Shape\n\
        1";
    assert_eq!(
        type_error(src),
        "'Circle' provides 'Shape' but doesn't implement member 'area'"
    );
}

#[test]
fn a_member_signature_must_match_the_trait() {
    let src = "\
        trait Shape { func area(s: Self): Int }\n\
        data Circle(r: Int) provides Shape { func area(c: Circle): Str = \"x\" }\n\
        1";
    assert_eq!(
        type_error(src),
        "member 'area' returns Str, but the trait says Int"
    );
}

#[test]
fn a_supplied_member_must_belong_to_a_listed_trait() {
    let src = "\
        trait Shape { func area(s: Self): Int }\n\
        data Circle(r: Int) provides Shape { func area(c: Circle): Int = 1\n\
        func nope(c: Circle): Int = 2 }\n\
        1";
    assert_eq!(type_error(src), "'nope' is not a member of Shape");
}

#[test]
fn a_trait_member_must_mention_self() {
    // Otherwise nothing could dispatch it and no impl could vary it — it is
    // an ordinary function that happens to be written inside a trait.
    let src = "trait T { func f(x: Int): Int }\n1";
    assert!(
        type_error(src).contains("mentions Self nowhere in its signature"),
        "unexpected: {}", type_error(src)
    );
}

#[test]
fn implementing_a_trait_for_a_generic_type_is_rejected_explicitly() {
    let src = "\
        trait Shape { func area(s: Self): Int }\n\
        data Box<A>(v: A) provides Shape { func area(b: Box<A>): Int = 1 }\n\
        1";
    assert!(
        type_error(src).contains("is generic; implementing a trait for a generic type isn't supported yet"),
        "unexpected: {}", type_error(src)
    );
}

#[test]
fn a_generic_type_may_still_be_granted_a_marker_trait() {
    // The rejection above is about *members*, which would have to be generic
    // over binders that aren't in scope in an impl body. A bare marker grant
    // supplies none, so it stays legal on a generic type.
    let src = r#"
data Box<T>(v: T) provides Error
print(Box(v=3).v)
"#;
    assert_eq!(run(src), "3\n");
}

// ── The builtin traits, as registry entries ──────────────────────────────

#[test]
fn the_builtin_traits_are_declarations_in_the_same_registry() {
    // `Trait` is no longer a closed set of two spellable names: `provides`
    // resolves every trait through one table, so redeclaring a built-in is a
    // named error rather than "already declared" or silent shadowing.
    assert_eq!(
        type_error("trait Eq { func f(s: Self): Int }\n1"),
        "'Eq' is a built-in trait and can't be redeclared"
    );
    assert_eq!(
        type_error("data X(n: Int) provides Nope\n1"),
        "Unknown trait 'Nope'"
    );
}

#[test]
fn truthy_stays_internal_and_says_why() {
    // `TRAITS.md` Part 5's exception: `Truthy` is a coercion relation
    // consulted by `check_condition`, not a callable interface — making it
    // implementable would let any impl redefine what `if x` means.
    assert!(
        type_error("data X(n: Int) provides Truthy\n1")
            .contains("compiler-internal coercion relation"),
        "unexpected message"
    );
}

#[test]
fn a_builtin_trait_can_be_granted_but_not_given_a_body() {
    let src = "data E(m: Str) provides Error { func f(e: E): Int = 1 }\n1";
    assert!(
        type_error(src).contains("can be granted but not implemented with a body"),
        "unexpected: {}", type_error(src)
    );
}

#[test]
fn a_builtin_can_be_granted_alongside_a_trait_that_has_a_body() {
    // A built-in declares no members, so nothing in the body can belong to
    // it — listing one next to a real trait says "grant this too", and must
    // not be read as trying to implement it.
    let src = r#"
trait Shape { func area(s: Self): Int }
data Boom(n: Int) provides Error, Shape { func area(b: Boom): Int = b.n }
print(Boom(n=5).area())
"#;
    assert_eq!(run(src), "5\n");
}

#[test]
fn error_still_works_as_the_marker_it_always_was() {
    // The registry change must not disturb `ERRORS.md`'s machinery: `error`
    // sugar, `provides Error`, and `catch` all key off the same trait.
    let src = r#"
error Bad(msg: Str)
func risky(n: Int): Int | Bad = if n > 0 then n else Bad(msg="negative")
print(risky(-1) catch [e] -> 0)
print(risky(7) catch [e] -> 0)
"#;
    assert_eq!(run(src), "0\n7\n");
}

// ── Explicit bounds ──────────────────────────────────────────────────────

#[test]
fn a_declared_bound_lets_one_generic_run_at_two_types() {
    let src = r#"
func twice<T: Num>(x: T): T = x + x
print(twice(3))
print(twice(1.5))
"#;
    assert_eq!(run(src), "6\n3.0\n");
}

#[test]
fn an_inferred_bound_must_be_written() {
    // "Infer, then require the inferred bounds to be written" — a caller
    // reads the signature, not the body.
    assert_eq!(
        type_error("func twice<T>(x: T): T = x + x\ntwice(3)"),
        "type parameter 'T' of 'twice' is used as Num, but is declared without that bound — write <T: Num>"
    );
}

#[test]
fn an_unused_binder_and_an_unused_bound_are_both_fine() {
    let src = r#"
func k<T: Ord>(x: Int): Int = x
print(k(3))
"#;
    assert_eq!(run(src), "3\n");
}

#[test]
fn a_data_declaration_may_bound_its_binders() {
    let src = r#"
data Box<A: Eq>(v: A)
print(Box(v=3).v)
"#;
    assert_eq!(run(src), "3\n");
}

#[test]
fn a_member_is_called_through_a_bound() {
    // What a bound buys, beyond documentation: the body is checked once
    // against the *trait's* signature, and monomorphization picks the impl
    // per instantiation. `report` is one declaration compiled twice here.
    let src = r#"
trait Shape {
    func area(s: Self): Int
}
data Circle(r: Int) provides Shape { func area(c: Circle): Int = c.r * c.r }
provides Shape for Int { func area(n: Int): Int = n }

func report<T: Shape>(x: T): Int = x.area()
print(report(Circle(r=3)))
print(report(7))
"#;
    assert_eq!(run(src), "9\n7\n");
}

#[test]
fn a_bound_member_call_takes_arguments_and_may_return_self() {
    let src = r#"
trait Scalable {
    func scale(s: Self, k: Int): Self
}
data Vec(x: Int) provides Scalable {
    func scale(v: Vec, k: Int): Vec = Vec(x = v.x * k)
}
func twice<T: Scalable>(x: T): T = x.scale(2).scale(3)
print(twice(Vec(x=2)).x)
"#;
    assert_eq!(run(src), "12\n");
}

#[test]
fn a_bound_member_call_reaches_a_default_body() {
    // The impl that omitted `name` and the impl that overrode it are both
    // reached through the same generic — the default is specialized per
    // implementing type, so there is a symbol to resolve to either way.
    let src = r#"
trait Named {
    func area(s: Self): Int
    func name(s: Self): Str = "thing"
}
data Vec(x: Int) provides Named {
    func area(v: Vec): Int = v.x
    func name(v: Vec): Str = "vec"
}
data Len(n: Int) provides Named {
    func area(l: Len): Int = l.n
}
func label<T: Named>(x: T): Str = x.name()
print(label(Vec(x=1)))
print(label(Len(n=1)))
"#;
    assert_eq!(run(src), "vec\nthing\n");
}

#[test]
fn a_generic_calling_another_generic_resolves_both_members() {
    // The nested case: `outer`'s own binder is only concrete once *its*
    // instantiation is emitted, and that is what makes `inner`'s member call
    // resolvable. The fixed point in `monomorphize_generics` is what gets
    // there; resolution runs after it, over everything it emitted.
    let src = r#"
trait Shape {
    func area(s: Self): Int
}
data Circle(r: Int) provides Shape { func area(c: Circle): Int = c.r * c.r }
func inner<T: Shape>(x: T): Int = x.area()
func outer<T: Shape>(x: T): Int = inner(x) + x.area()
print(outer(Circle(r=3)))
"#;
    assert_eq!(run(src), "18\n");
}

#[test]
fn a_mutating_member_is_called_through_a_bound() {
    // The receiver exemption (`TRAITS.md` Part 2) applies here too: no `mut`
    // marker at the dot call, but the root still has to be mutable — and the
    // copy-out reaches the caller's binding through the generic.
    let src = r#"
trait Counter {
    func bump(mut s: Self): Int
}
data C(n: Int) provides Counter {
    func bump(mut c: C): Int = { c.n = c.n + 1; c.n }
}
func go<T: Counter>(mut x: T): Int = x.bump() + x.bump()
mut c = C(n=0)
print(go(mut c))
print(c.n)
"#;
    assert_eq!(run(src), "3\n2\n");
}

#[test]
fn a_generic_declared_but_never_called_needs_no_impl_to_resolve() {
    // Nothing is instantiated, so nothing is resolved — the declaration is
    // stripped before codegen like any other uninstantiated generic, and no
    // pending callee survives it.
    let src = r#"
trait Shape { func area(s: Self): Int }
func report<T: Shape>(x: T): Int = x.area()
print(1)
"#;
    assert_eq!(run(src), "1\n");
}

#[test]
fn a_member_the_bound_doesnt_declare_is_not_found() {
    let src = "\
        trait Shape { func area(s: Self): Int }\n\
        func report<T: Shape>(x: T): Int = x.perimeter()\n\
        report(1)";
    assert!(
        type_error(src).contains("no function 'perimeter'"),
        "unexpected: {}", type_error(src)
    );
}

#[test]
fn an_unbounded_type_parameter_has_no_members() {
    let src = "\
        trait Shape { func area(s: Self): Int }\n\
        func report<T>(x: T): Int = x.area()\n\
        report(1)";
    assert!(
        type_error(src).contains("no function 'area'"),
        "unexpected: {}", type_error(src)
    );
}

#[test]
fn a_bound_member_call_is_arity_checked_at_the_declaration() {
    // Checked once, against the trait's signature — not once per
    // instantiation, and not left to whichever impl happens to be compiled.
    let src = "\
        trait Scalable { func scale(s: Self, k: Int): Self }\n\
        func twice<T: Scalable>(x: T): T = x.scale()\n\
        print(1)";
    assert!(
        type_error(src).contains("expected 1, got 0"),
        "unexpected: {}", type_error(src)
    );
}

#[test]
fn a_type_that_doesnt_satisfy_the_bound_is_rejected_at_the_call() {
    let src = "\
        trait Shape { func area(s: Self): Int }\n\
        data Circle(r: Int) provides Shape { func area(c: Circle): Int = c.r }\n\
        func report<T: Shape>(x: T): Int = x.area()\n\
        report(1)";
    let msg = type_error(src);
    assert!(msg.contains("Shape"), "unexpected: {}", msg);
}

#[test]
fn a_declaration_inside_a_generic_body_keeps_the_binders_in_scope() {
    // Hoisting a nested `data` or `trait` installs its own type-parameter
    // scope; it has to put back the one it found, or the enclosing
    // function's `<T>` is gone for the rest of its body.
    let src = r#"
func f<T: Num>(x: T): T = {
  data Local(a: Int)
  let z: T = x
  z + z
}
func g<T: Num>(x: T): T = {
  trait Tiny { func tiny(s: Self): Int }
  let z: T = x
  z + z
}
print(f(4))
print(g(5))
"#;
    assert_eq!(run(src), "8\n10\n");
}
