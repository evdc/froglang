//! Tests for the inline (tagged-pointer) union representation — RUNTIME.md
//! Part 1.
//!
//! A union whose members fit in the tag space and which isn't
//! self-referential is flattened into `codegen::UnionLayout`'s columns
//! instead of being boxed: pointer columns first (slot 0 carrying the member
//! tag in its low 3 bits), then scalar columns. Every column's pointer-ness
//! is then a static property of the column, which is what lets the collector
//! run one uniform `gc::is_heap_ptr` rule over every word in the system.
//!
//! What is worth testing here is the boundaries, since the middle of the
//! range is covered by every other union test in the suite:
//!
//!   * a payload-carrying nominal union no longer allocates (the
//!     `roadmap.md` perf item) but still round-trips;
//!   * the boxed fallback past `MAX_INLINE_UNION_MEMBERS` is *reachable*,
//!     not merely present — RUNTIME.md's third open question;
//!   * a member whose first pointer leaf is itself tagged gets the tag a
//!     column of its own instead of corrupting that leaf — RUNTIME.md's
//!     second open question;
//!   * a self-referential union still works, via the boxed path.
//!
//! Each runs under `FROG_GC_STRESS=1` as well as normally: a collection on
//! every allocation is what turns a mis-encoded word from a latent bug into
//! a failure, and the encoding is the whole point of the change.

mod common;
use common::{run, run_gc_stress};

fn both(src: &str, expected: &str) {
    assert_eq!(run(src), expected, "normal run");
    assert_eq!(run_gc_stress(src), expected, "under FROG_GC_STRESS=1");
}

// ── inline layout: scalars only ───────────────────────────────────────────────

/// `Discount` is `roadmap.md`'s named case: four members, the widest
/// carrying two `Int`s, none carrying a pointer. It lays out as one tag
/// column plus two scalar columns and allocates nothing at all, where it
/// used to cost one `frog_alloc_variant` per value.
#[test]
fn nominal_union_of_scalar_members_round_trips() {
    both(
        r#"data Discount is NoDiscount | Percent(pct: Int) | Flat(amount: Int) | BulkOver(min_qty: Int, pct: Int)

func apply(d: Discount, gross: Int): Int = match d {
  is NoDiscount then gross
  is Percent(pct) then gross - gross * pct / 100
  is Flat(amount) then if gross > amount then gross - amount else 0
  is BulkOver(min_qty, pct) then if gross > min_qty then gross - gross * pct / 100 else gross
}

print(apply(NoDiscount, 100))
print(apply(Percent(pct=10), 100))
print(apply(Flat(amount=25), 100))
print(apply(BulkOver(min_qty=50, pct=20), 100))
print(Percent(pct=7))
"#,
        "100\n90\n75\n80\nDiscount.Percent(pct=7)\n",
    );
}

/// A payload-less member of an inline union is no longer an immediate at
/// all — it is simply its tag, sitting in slot 0 with the pointer part
/// zero, so it masks to `0` and the collector correctly declines to follow
/// it. Allocating hard between constructing and reading it is what makes
/// that observable.
#[test]
fn payload_less_member_of_an_inline_union_is_just_its_tag() {
    both(
        r#"data Category is Food | Book | Electronics | Toy

func name(c: Category): Str = match c {
  is Food then "food"
  is Book then "book"
  is Electronics then "electronics"
  is Toy then "toy"
}

let cs = [Food, Book, Electronics, Toy, Book]
for c in cs do {
  let churn = [for i in 0..500 do "x" + "y"]
  print(name(c))
}
"#,
        "food\nbook\nelectronics\ntoy\nbook\n",
    );
}

// ── inline layout: pointer column shared with the tag ─────────────────────────

/// The common shape: one member contributes a `Str` (a plain, 8-aligned
/// pointer with three spare low bits) and another an `Int`. The tag rides in
/// the `Str`'s low bits, so the union is two slots wide — the same width the
/// old two-slot representation used, but with the pointer column statically
/// scannable instead of conditionally so.
#[test]
fn tag_rides_in_the_low_bits_of_a_str_member() {
    both(
        r#"data Tagged is Named(who: Str, n: Int) | Anon

func show(t: Tagged): Str = match t {
  is Named(who, n) then who
  is Anon then "-"
}

let ts = [Named(who="ada", n=1), Anon, Named(who="grace", n=2)]
for t in ts do {
  let churn = [for i in 0..500 do "a" + "b"]
  print(show(t))
}
print(Named(who="ada", n=3))
"#,
        "ada\n-\ngrace\nTagged.Named(who=\"ada\", n=3)\n",
    );
}

/// An anonymous `Int | Str` is the same shape reached from the other
/// direction — no `data` declaration, tags assigned by position in the
/// normalized member list.
#[test]
fn anonymous_scalar_and_pointer_union_round_trips() {
    both(
        r#"let xs: List(Int | Str) = [1, "two", 3, "four"]
mut n = 0
for x in xs do {
  let churn = [for i in 0..300 do "p" + "q"]
  n = n + match x { is Int(k) then k
                    is Str(s) then 0 }
  print(x)
}
print(n)
"#,
        "1\n\"two\"\n3\n\"four\"\n4\n",
    );
}

// ── inline layout: the tag needs a column of its own ──────────────────────────

/// RUNTIME.md's second open question, answered by `UnionLayout`'s
/// `dedicated_tag`. `Outer.Wrap`'s first pointer leaf is an `Inner`, whose
/// own slot 0 already carries *its* tag — OR-ing `Outer`'s tag into the same
/// word would corrupt both. `union_layout` detects that (`overlay_safe` is
/// false for a union leaf) and gives the tag a column of its own, costing
/// one slot rather than correctness.
#[test]
fn a_union_nested_in_a_union_member_gets_a_dedicated_tag_column() {
    both(
        r#"data Inner is P(k: Int) | Q(t: Str)
data Outer is Wrap(i: Inner, m: Int) | Empty

func inner(i: Inner): Int = match i {
  is P(k) then k
  is Q(t) then if t == "xyz" then 3 else 0
}
func outer(x: Outer): Int = match x {
  is Wrap(i, m) then m + inner(i)
  is Empty then -1
}

let os = [Wrap(i=P(k=5), m=10), Empty, Wrap(i=Q(t="xyz"), m=10)]
mut total = 0
for o in os do {
  let churn = [for i in 0..300 do "u" + "v"]
  total = total + outer(o)
}
print(total)
print(Wrap(i=Q(t="xyz"), m=1))
"#,
        "27\nOuter.Wrap(i=Inner.Q(t=\"xyz\"), m=1)\n",
    );
}

// ── the boxed fallback ────────────────────────────────────────────────────────

/// RUNTIME.md's third open question: the boxed fallback past
/// `codegen::MAX_INLINE_UNION_MEMBERS` (six — tags `1..=6`, with `0`
/// reserved for a plain pointer and `7` for immediates) must be *reachable*,
/// not merely present. Seven members put this union over the line, so it
/// keeps the one-slot `FrogVariant` representation, and its payload-less
/// members keep an immediate — re-encoded as `(tag << 3) | 7` so it lands in
/// the reserved `111` class rather than colliding with a tagged pointer.
#[test]
fn a_seven_member_union_falls_back_to_boxing() {
    both(
        r#"data Wide is A | B | C(n: Int) | D | E | F | G(s: Str)

func w(x: Wide): Int = match x {
  is A then 1
  is B then 2
  is C(n) then 100 + n
  is D then 4
  is E then 5
  is F then 6
  is G(s) then if s == "abcd" then 1004 else 1000
}

let ws = [A, C(n=7), G(s="abcd"), F, B, D, E]
for x in ws do {
  let churn = [for i in 0..300 do "m" + "n"]
  print(w(x))
}
print(C(n=9))
"#,
        "1\n107\n1004\n6\n2\n4\n5\nWide.C(n=9)\n",
    );
}

/// A self-referential union cannot be flattened into a finite slot count, so
/// `union_is_inline` rejects it and it keeps the boxed representation —
/// exactly what it had before. The point of the test is that the *detection*
/// works: getting it wrong is an infinite recursion in `union_layout` at
/// compile time, not a wrong answer at run time.
#[test]
fn a_self_referential_union_stays_boxed() {
    both(
        r#"data Tree is Leaf | Node(v: Int, l: Tree, r: Tree)

func sum(t: Tree): Int = match t {
  is Leaf then 0
  is Node(v, l, r) then v + sum(l) + sum(r)
}

print(sum(Node(v=1, l=Node(v=2, l=Leaf, r=Leaf), r=Node(v=3, l=Leaf, r=Leaf))))
"#,
        "6\n",
    );
}

/// A union reachable from itself only *through a struct field* is still
/// self-referential and must still box — the cycle does not have to be
/// direct. `List(Branchy)` deliberately does not count, being a pointer that
/// breaks the cycle, so this uses a plain struct instead.
#[test]
fn a_union_that_cycles_through_a_struct_stays_boxed() {
    both(
        r#"data Pair(l: Chain, r: Chain)
data Chain is Stop | Link(v: Int, p: Pair)

func total(c: Chain): Int = match c {
  is Stop then 0
  is Link(v, p) then v + total(p.l) + total(p.r)
}

print(total(Link(v=5, p=Pair(l=Link(v=2, p=Pair(l=Stop, r=Stop)), r=Stop))))
"#,
        "7\n",
    );
}

// ── two distinct union shapes in one aggregate ────────────────────────────────

/// The restriction the old representation forced (one scalar-carrying union
/// shape per GC-scanned aggregate, enforced by the now-deleted
/// `TypeChecker::check_scalar_union_consistency`) is gone: each union owns
/// its own columns, so there is nothing shared left to conflict over.
#[test]
fn two_distinct_union_shapes_can_share_one_list_element() {
    both(
        r#"data Point(x: Int, y: Int)
data Line(a: Point, b: Point)
data Both(m: Int | Point, n: Float | Line)

let items = [Both(m=1, n=2.0), Both(m=Point(x=3, y=4), n=5.0)]
mut total = 0
for it in items do {
  let churn = [for i in 0..300 do "c" + "d"]
  total = total + match it.m { is Int(k) then k
                               is Point(p) then p.x + p.y }
}
print(total)
"#,
        "8\n",
    );
}
