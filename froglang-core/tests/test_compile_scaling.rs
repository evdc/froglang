//! Guards against the front end going accidentally superlinear.
//!
//! Nothing else in the suite would notice: every other test compiles a
//! program small enough that even an exponential algorithm finishes
//! instantly, and `benches/` measures a *fixed* program's runtime, not how
//! compilation scales with program size.
//!
//! That gap hid a real bug. The operator arm for `+` inferred its left
//! operand to test for string concatenation and then handed the same
//! operands to the shared operand-join helper, which inferred them all over
//! again — so a
//! left-nested `a + b + c + ...` cost `T(n) = 2·T(n-1)`. Measured on the
//! release binary before the fix: 20 terms took 330 ms, 24 terms took 5.2 s,
//! and 30 terms would have taken minutes. `a - b - c` and `a * b * c`,
//! which never take that arm, stayed flat — which is exactly the asymmetry
//! `arithmetic_chains_scale_the_same_for_every_operator` below pins down.
//!
//! Each case runs under a deliberately loose wall-clock budget. These are
//! not micro-benchmarks and are not meant to catch a 2x slowdown; they're
//! meant to catch a change in *complexity class*, where the gap between
//! "fine" and "broken" is several orders of magnitude. A budget that
//! generous can't flake on a loaded machine, and the work is done on a
//! worker thread so a genuine hang fails the test instead of wedging the
//! run forever.

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use froglang_core::frontend::expression::Expression;
use froglang_core::frontend::modules;
use froglang_core::frontend::tokens::{Span, Spanned};
use froglang_core::frontend::typeck::TypeChecker;

/// Generous enough that machine load can never explain a failure, small
/// enough that an exponential blowup can never sneak under it.
const BUDGET: Duration = Duration::from_secs(10);

/// Parse and type-check `src` on a worker thread, failing if it doesn't
/// finish within `BUDGET`. `label` names the case in the failure message.
///
/// A timeout leaks the worker thread — it may still be spinning inside an
/// exponential inference. That's deliberate: the alternative is no timeout
/// at all, and the test process is about to fail and exit anyway.
///
/// The worker gets an explicit large stack. Both the parser and the type
/// checker recurse once per level of expression nesting, and a spawned
/// thread's 2 MiB default is a fraction of the ~8 MiB main thread the real
/// `froglang-core` binary runs on — without this the deep-nesting cases
/// would abort on stack exhaustion in a debug build (where frames are
/// fattest) and report a *depth* limit this test isn't measuring. The
/// release binary handles 500-deep nesting on its ordinary main stack.
fn check_within_budget(label: &str, src: String) {
    let (tx, rx) = mpsc::channel();
    let worker_src = src;
    let worker = thread::Builder::new()
        .stack_size(128 * 1024 * 1024)
        .name("compile-scaling".to_string());
    worker.spawn(move || {
        let _ = tx.send(check_once(&worker_src).map(|_| ()));
    }).expect("failed to spawn compile-scaling worker");

    match rx.recv_timeout(BUDGET) {
        Ok(Ok(())) => {}
        Ok(Err(msg)) => panic!("{} failed to compile: {}", label, msg),
        Err(_) => panic!(
            "{} did not finish type-checking within {:?} — the front end has almost \
             certainly gone superlinear in program size",
            label, BUDGET,
        ),
    }
}

/// Parse and type-check `src` on the calling thread, returning how long the
/// type-checking alone took (parsing is excluded: it is the same work for
/// both programs in a ratio comparison, and including it would dilute the
/// signal).
fn check_once(src: &str) -> Result<Duration, String> {
    let base = std::env::temp_dir().join("<compile-scaling>");
    let stmts = modules::resolve_source(src, &base)
        .map_err(|e| format!("module error: {}", e))?;
    let span = match (stmts.first(), stmts.last()) {
        (Some(f), Some(l)) => f.span.merge(l.span),
        _ => Span::new((0, 0), (0, 0)),
    };
    let ast = Spanned::from(Expression::Block(stmts), span);
    let start = Instant::now();
    TypeChecker::new()
        .check_and_lower(ast)
        .map_err(|e| format!("type error: {}", e))?;
    Ok(start.elapsed())
}

/// Fastest of a few type-check runs of `src`, on a worker thread with a
/// large stack (see `check_within_budget`). The minimum, not the mean:
/// scheduling noise can only ever make a run *slower*, so the fastest
/// observation is the closest to the work actually required.
fn fastest_check(label: &str, src: String) -> Duration {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .stack_size(128 * 1024 * 1024)
        .name("compile-scaling".to_string())
        .spawn(move || {
            let result = (|| {
                // The first run is a warm-up (allocator, page faults) and is
                // discarded; the best of the next three is the answer.
                check_once(&src)?;
                let mut best = check_once(&src)?;
                for _ in 0..2 {
                    best = best.min(check_once(&src)?);
                }
                Ok::<Duration, String>(best)
            })();
            let _ = tx.send(result);
        })
        .expect("failed to spawn compile-scaling worker");

    match rx.recv_timeout(BUDGET) {
        Ok(Ok(d)) => d,
        Ok(Err(msg)) => panic!("{} failed to compile: {}", label, msg),
        Err(_) => panic!("{} did not finish type-checking within {:?}", label, BUDGET),
    }
}

/// `1 <op> 1 <op> ... ` with `n` terms — a left-nested binary tree of depth
/// `n`, the shape that makes re-inferring a subtree per level expensive.
fn chain(op: &str, n: usize) -> String {
    let terms: Vec<&str> = std::iter::repeat("1").take(n).collect();
    format!("let x = {}\n", terms.join(&format!(" {} ", op)))
}

#[test]
fn arithmetic_chains_scale_the_same_for_every_operator() {
    // 40 terms is ~65,000x the work of the 24-term case that took 5.2s
    // before the fix, so the `+` row here is decisive on its own. The other
    // operators are included because their staying flat while `+` blew up
    // is what localised the bug to the `+` arm in the first place — if a
    // future change makes them slow too, the cause is somewhere else and
    // the test should say so.
    for op in ["+", "-", "*", "/"] {
        check_within_budget(&format!("40-term '{}' chain", op), chain(op, 40));
    }
}

#[test]
fn long_string_concatenations_scale() {
    // The `+` arm's `Str` special case is the branch that used to double
    // the work; exercise it on strings too, not just the numeric fallthrough.
    let terms: Vec<&str> = std::iter::repeat("\"a\"").take(40).collect();
    check_within_budget(
        "40-term string concatenation",
        format!("let s = {}\n", terms.join(" + ")),
    );
}

#[test]
fn comparison_chains_scale() {
    // `<` routes through `join_operand_types` too, via a different caller.
    let cmp: Vec<String> = (0..60).map(|i| format!("({} < {})", i, i + 1)).collect();
    check_within_budget(
        "60 comparisons combined with 'and'",
        format!("let b = {}\n", cmp.join(" and ")),
    );
}

#[test]
fn long_statement_blocks_scale() {
    // Program *width* rather than depth: 800 sibling bindings, each
    // referring to the previous one so they can't be checked independently.
    let mut src = String::from("let v0 = 0\n");
    for i in 1..800 {
        src.push_str(&format!("let v{} = v{} + 1\n", i, i - 1));
    }
    check_within_budget("800 sequential let-bindings", src);
}

#[test]
fn entering_a_scope_does_not_cost_the_size_of_the_environment() {
    // Many bindings in scope *and* many scopes opened over them — the
    // product that used to be quadratic. Each function body opens a scope,
    // and the environment it was entered with was deep-cloned to save and
    // restore it (twice per body, since `infer` and `check_and_lower` are
    // separate passes over the same body). Note
    // `long_statement_blocks_scale` above would not have caught this: plain
    // top-level `let`s open no scopes at all, so nothing was ever cloned.
    //
    // Measured on the release binary at half this size (1500 + 1500):
    // 1048 ms before the scope stack, 81 ms after. At the size below,
    // before was on the order of 17 s and after is ~112 ms.
    let n = 6000;
    let mut src = String::new();
    for i in 0..n {
        src.push_str(&format!("let v{i} = {i}\n"));
    }
    for i in 0..n {
        src.push_str(&format!("func g{i}(x: Int): Int = x + {i}\n"));
    }
    src.push_str("g0(1)\n");
    check_within_budget("6000 bindings in scope over 6000 function bodies", src);
}

#[test]
fn many_top_level_declarations_scale() {
    // The same quadratic seen through program *width* rather than through
    // one long environment: interleaved `data` and `func` declarations, so
    // the environment grows as the file is walked. 3200 declarations took
    // 7.0 s before the scope stack and 40 ms after.
    let mut src = String::new();
    for i in 0..3200 {
        src.push_str(&format!(
            "data S{i}(a: Int, b: Str)\nfunc f{i}(x: Int): Int = if x > {i} then x - {i} else x + {i}\n"
        ));
    }
    src.push_str("f0(1)\n");
    check_within_budget("3200 interleaved data/func declarations", src);
}

#[test]
fn deeply_nested_conditionals_scale() {
    // Nesting depth through a construct that joins branch types at every
    // level (`join_types`), which is a second place a per-node re-walk
    // would show up.
    check_within_budget("800-deep if/else nesting", nested_ifs(800));
}

#[test]
fn nesting_depth_costs_no_more_than_the_same_work_laid_out_flat() {
    // The single-pass property, stated as something observable.
    //
    // The front end used to type-check in two passes: `check_and_lower`
    // began by calling `infer` on the whole node, then recursed with
    // `check_and_lower` into each child — which inferred *that* child's
    // entire subtree all over again. Cost was therefore O(n · depth): fine
    // for a wide, shallow program, quadratic for a deeply nested one. Now
    // each node's type is computed from its already-lowered children, and
    // no subtree is ever walked twice.
    //
    // Both programs below contain the same number of `if` expressions over
    // the same variable, so they are the same amount of work to check —
    // they differ only in *shape*: one is 800 conditionals nested inside
    // one another, the other 800 laid out side by side at depth 1. A
    // per-node re-walk shows up as the nested one costing on the order of
    // `depth` times more than the flat one; single-pass keeps the ratio
    // near 1. Measured against the release binary at these sizes: the
    // two-pass front end took 20 ms at depth 200, 90 ms at 400 and 360 ms
    // at 800 — clean quadratic — where single-pass takes 0.5 / 0.7 / 1.9 ms.
    //
    // A *ratio* rather than a wall-clock budget because it's what actually
    // identifies this bug and because it needs no calibration to the
    // machine: both halves are measured back to back in the same process.
    // The threshold has roughly 5x headroom over the ratio this shape
    // actually produces, so load can't explain a failure — only a return
    // of depth-dependent cost can.
    const N: usize = 800;
    const MAX_RATIO: f64 = 8.0;

    let deep = fastest_check("nested if/else", nested_ifs(N));

    let mut flat = String::from("let x = 0\n");
    for i in 0..N {
        flat.push_str(&format!("let a{} = if x > 0 then 1 else 0\n", i));
    }
    let flat = fastest_check("sequential if/else", flat);

    let ratio = deep.as_secs_f64() / flat.as_secs_f64().max(f64::EPSILON);
    assert!(
        ratio < MAX_RATIO,
        "{} nested conditionals took {:?}, the same {} laid out flat took {:?} — a {:.1}x \
         ratio. Nesting depth is not supposed to cost anything extra; this is the signature \
         of the front end re-walking each subtree per level again.",
        N, deep, N, flat, ratio,
    );
}

/// `if x > 0 then ... else 0`, `n` levels deep — one conditional per level,
/// over a variable bound outside so every level is real work.
fn nested_ifs(n: usize) -> String {
    let mut src = String::from("let x = 0\n");
    for _ in 0..n {
        src.push_str("if x > 0 then\n");
    }
    src.push_str("1\n");
    for _ in 0..n {
        src.push_str("else 0\n");
    }
    src
}

#[test]
fn wide_matches_scale() {
    // Many arms over one nominal union — `lower_match` clones per arm, so
    // this is where an arm-count blowup would surface.
    let variants: Vec<String> = (0..60).map(|i| format!("V{}", i)).collect();
    let mut src = format!("data Wide is {}\n", variants.join(" | "));
    src.push_str("func f(w: Wide): Int = match w {\n");
    for (i, v) in variants.iter().enumerate() {
        src.push_str(&format!("  is {} then {}\n", v, i));
    }
    src.push_str("}\n");
    check_within_budget("60-arm match", src);
}
