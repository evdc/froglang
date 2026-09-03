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
//! Two later sections cover the *dispatch* over that representation rather
//! than the representation itself, since both bugs they pin down are only
//! reachable through a `match`:
//!
//!   * a `Float` member rides in a scalar column like any other primitive,
//!     including through the `F64` merge block a `match` on it joins into;
//!   * an exhaustive `match` emits no tag test for its rightmost arm, and
//!     the arms around it still bind, guard and fall through correctly.
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
        r#"let xs: List<Int | Str> = [1, "two", 3, "four"]
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
/// direct. `List<Branchy>` deliberately does not count, being a pointer that
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

// ── a `Float` member's column ─────────────────────────────────────────────────

/// A union's scalar columns are `I64`-typed words (`struct_fields` labels
/// them `Type::Int`), so a `Float` member's payload is `bitcast` in and out
/// of one. The merge block a `match` on such a union joins into is
/// `F64`-typed, though, and every value flowing into it — including the
/// placeholder for a branch that can't be taken — has to be `F64` too.
/// Codegen used to materialise that placeholder with `iconst`, which is
/// integer-only: any `match`/`?`/`!`/`catch` on a union with a `Float`
/// member aborted in Cranelift's verifier before ever running. `Int` and
/// `Bool` members were unaffected, which is why this went unnoticed —
/// so the assertion worth making is that all three primitive members now
/// work through the same path.
#[test]
fn a_float_member_matches_and_catches() {
    both(
        r#"error Bad(msg: Str)
func f(x: Int): Float | Bad = if x < 0 then Bad(msg="neg") else 1.5
func g(x: Int): Float | Bad = f(x)? + 1.0

print(f(1) catch 0.0)
print(f(0 - 1) catch 0.0)
print(g(1) catch 0.0)
print(f(1)!)
print(match f(1) {
  is Float(v) then v
  is Bad(e) then 0.0
})
"#,
        "1.5\n0.0\n2.5\n1.5\n1.5\n",
    );
}

/// The same for a nominal union whose members carry `Float`s, including one
/// that mixes a `Float` and an `Int` (so the value rides in the second
/// scalar column rather than the first) and one with no payload at all.
#[test]
fn a_nominal_union_of_float_members_round_trips() {
    both(
        r#"data V is A(x: Float) | B(y: Float, z: Int) | Zero

func val(v: V): Float = match v {
  is A(x) then x
  is B(y, z) then y + 1.0
  is Zero then 0.0
}

for v in [A(x=1.5), B(y=2.5, z=1), Zero] do {
  let churn = [for i in 0..300 do "e" + "f"]
  print(val(v))
}
print(A(x=1.5))
"#,
        "1.5\n3.5\n0.0\nV.A(x=1.5)\n",
    );
}

// ── the rightmost arm carries no tag test ─────────────────────────────────────

/// An exhaustive `match` with no `else` needs no tag test on its last arm:
/// the fold in `fold_match_arm` runs right-to-left, so a `None` tail means
/// every other member is handled to this arm's left and the subject can
/// only hold this one. Dropping the test is what makes a two-member union —
/// `Int | E`, the shape `?`/`!`/`catch` desugar to — cost *one* branch per
/// dispatch rather than two.
///
/// The behaviour that has to survive it: the dropped-test arm still runs
/// its pattern extraction, so its binds are live in its body.
#[test]
fn the_last_arm_of_an_exhaustive_match_still_binds_its_pattern() {
    both(
        r#"data Shape is Circle(r: Int) | Rect(w: Int, h: Int)

func area(s: Shape): Int = match s {
  is Circle(r) then r * r * 3
  is Rect(w, h) then w * h
}

print(area(Circle(r=2)))
print(area(Rect(w=3, h=4)))

let xs: List<Int | Str> = ["ab", 7]
for x in xs do {
  print(match x {
    is Int(k) then k
    is Str(s) then len(s)
  })
}
"#,
        "12\n12\n2\n7\n",
    );
}

/// Guards are what make the last arm's dropped test load-bearing rather
/// than cosmetic: a guarded arm never counts toward exhaustiveness, so
/// control genuinely *falls through* it at runtime and lands on an untested
/// last arm — which must therefore still be the right one, and must still
/// bind. And the guard on the way past has to actually run: `tail` being
/// `None` for the arm to its right says nothing about whether an earlier
/// arm's guard is observable, and here it prints.
///
/// `Pos(n=5)` takes the guarded arm; `Pos(n=0)` evaluates the same guard,
/// fails it, and falls through to the untested `is Pos(n)`; `Neg` never
/// reaches the guard at all.
#[test]
fn a_failed_guard_falls_through_to_the_untested_last_arm() {
    both(
        r#"data Sign is Neg(n: Int) | Pos(n: Int)

func noisy(n: Int): Bool = {
  print("guard")
  n > 0
}

func f(s: Sign): Int = match s {
  is Neg(n) then 0 - n
  is Pos(n) and noisy(n) then n
  is Pos(n) then n - 100
}

print(f(Pos(n=5)))
print(f(Pos(n=0)))
print(f(Neg(n=3)))
"#,
        "guard\n5\nguard\n-100\n-3\n",
    );
}
