//! GC rooting under a forced collection on every allocation.
//!
//! Every test here runs its program twice over — once normally, where the
//! 1 MB collection threshold means no GC ever runs and any rooting bug is
//! invisible, and once under `FROG_GC_STRESS=1`, where a value that lost
//! its only root is freed at the first opportunity and its cell recycled
//! into some later allocation. The two runs must agree; a bug shows up as
//! stress-mode output that is truncated, empty, or holding some other
//! string's bytes.
//!
//! The first two tests here predate RUNTIME.md Part 2 and probe the shadow
//! stack's structural defect: a root slot was owned by *producer site*, not
//! by binding, so `let s = <producer>` gave `s` no root of its own — it
//! borrowed the producer's, and anything that later overwrote that slot
//! while `s` was still live dropped the value on the floor. Cranelift's
//! stack maps (which now provide GC roots — see `gc.rs`, "Precise roots")
//! tie a root to the `Variable`'s own live range instead, so both now pass;
//! they stay as regression tests for the class of bug, not as documentation
//! of a known hole. The third test guards Part 2's own migration defect —
//! see its own doc comment.

mod common;
use common::{run, run_gc_stress};

/// A value produced in one arm of an `if` and assigned to a binding
/// declared outside the enclosing loop.
///
/// Regression test for the reverted `Conditional` slot sharing: both arms
/// used to write the same slot indices, on the argument that only one arm
/// ever runs. True within a single execution, false across a loop
/// back-edge — iteration 3 taking the `else` arm overwrote the root
/// iteration 0 had left in that slot for `best`, and the `"skip"` concat
/// then recycled the freed cell, so this printed `B`.
#[test]
fn conditional_arms_do_not_share_roots_across_a_back_edge() {
    let src = r#"
func go(n: Int): Str = {
  mut best = "none"
  for i in 0..n do {
    if i < 3 then { best = "keep" + "A"
                    0 }
    else { print("skip" + "B")
           0 }
  }
  best
}
print(go(6))
"#;
    assert_eq!(run(src).lines().last(), Some("keepA"));
    assert_eq!(run_gc_stress(src).lines().last(), Some("keepA"));
}

/// The same escape without any conditional slot sharing involved: the
/// producer is in a loop body and re-runs, overwriting its own slot, while
/// the binding it fed outlives the loop.
///
/// Used to be KNOWN FAILING under the shadow-stack representation — a loop
/// body has always reused its slot across the back-edge, and fixing it
/// needed roots owned per binding rather than per producer site. RUNTIME.md
/// Part 2 (Cranelift stack maps) gives every binding exactly that: a
/// `Variable`'s root tracks its own live range, not some producer site's.
/// Prints `keepAA` correctly now — see RUNTIME.md's "Sequencing" for the
/// un-ignore this test used to carry.
#[test]
fn loop_body_producer_does_not_clobber_an_escaping_binding() {
    let src = r#"
func go(): Str = {
  let parts = ["AA", "BB", "CC", "DD", "EE", "FF"]
  mut best = "none"
  for i in 0..6 do {
    let s = "keep" + parts[i]
    if i < 1 then { best = s
                    0 }
    else { 0 }
  }
  best
}
print(go())
"#;
    assert_eq!(run(src).lines().last(), Some("keepAA"));
    assert_eq!(run_gc_stress(src).lines().last(), Some("keepAA"));
}

// ── the top-level out_ptr buffer is itself a root, not just a source of roots ──

#[test]
fn top_level_binding_survives_a_collection_triggered_before_eval_returns() {
    let src = r#"
data Product(name: Str, price: Int)
data LineItem(product: Product, qty: Int)
func line_total(item: LineItem): Int = item.product.price * item.qty
let catalog = [Product(name="Coffee", price=450), Product(name="Tea", price=350)]
let cart = [LineItem(product=catalog[0], qty=2), LineItem(product=catalog[1], qty=1)]
let bulk_lines = [for item in cart do line_total(item)]
print(bulk_lines[0])
"#;
    assert_eq!(run(src).lines().last(), Some("900"));
    assert_eq!(run_gc_stress(src).lines().last(), Some("900"));
}
