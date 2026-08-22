//! Shadow-stack rooting under a forced collection on every allocation.
//!
//! Every test here runs its program twice over — once normally, where the
//! 1 MB collection threshold means no GC ever runs and any rooting bug is
//! invisible, and once under `FROG_GC_STRESS=1`, where a value that lost
//! its only root is freed at the first opportunity and its cell recycled
//! into some later allocation. The two runs must agree; a bug shows up as
//! stress-mode output that is truncated, empty, or holding some other
//! string's bytes.
//!
//! The shape all of these probe is a *heap value that escapes the
//! construct that produced it*. Shadow-stack slots are owned by producer
//! site rather than by binding (see `codegen::max_heap_slots`), so
//! `let s = <producer>` gives `s` no root of its own — it borrows the
//! producer's. Anything that then overwrites that slot while the binding
//! is still live drops the value on the floor.

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
/// KNOWN FAILING, pre-existing and unrelated to the `Conditional` revert —
/// loop bodies have always reused their slots across the back-edge. Fixing
/// it needs roots owned per binding rather than per producer site
/// (MUTABILITY.md stage 6); until then this documents the hole rather than
/// guarding it. Prints `keep` instead of `keepAA` under stress.
#[test]
#[ignore = "known unsound: loop back-edge overwrites a root the escaping binding still needs"]
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
