//! Liveness analysis over the typed AST — see `MUTABILITY.md` stage 6.
//!
//! Phase 1 (this file, so far): node identity. Liveness results are keyed to
//! a specific *occurrence* of a node, not to its structural shape — lowering
//! clones subtrees (a guarded match arm's `tail`, a `catch` handler inlined
//! per `Error`-providing member — see `TypeChecker::build_catch_arms`), so
//! two `Var("x")` nodes with identical spans can be genuinely different
//! occurrences on different control-flow paths.
//!
//! `number_nodes` stamps every node with a fresh `NodeId`, in one pass run
//! *after* lowering finishes (`TypeChecker::check_and_lower_entry` /
//! `check_and_lower`, called from `FrogState::eval_with_base`). It must run
//! after, not during, lowering — assigning ids as nodes are constructed
//! would give every clone of a subtree the same id, defeating the point.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::frontend::tokens::Spanned;
use crate::frontend::typed_ast::{Arg, NodeId, PlaceSeg, TypedExpr, TypedExprKind, TypedExprRef};

/// Stamp every node reachable from `expr` (including nested `Function`
/// bodies — unlike `codegen::for_each_heap_producer`, which skips them
/// because they're compiled as separate top-level functions, this pass
/// numbers them too: they get their own, independent liveness analysis
/// later, but still need real ids) with a fresh, densely-packed id starting
/// at 1. Returns the number of nodes numbered (the highest id assigned) so
/// a caller can size a `NodeId`-indexed table without a second walk.
///
/// 0 is never assigned — it stays reserved for "unnumbered", so a stray
/// lookup against a node this pass didn't reach fails loudly instead of
/// colliding with a real node.
pub fn number_nodes(expr: &mut Spanned<TypedExpr>) -> NodeId {
    let mut next: NodeId = 1;
    number(expr, &mut next);
    next - 1
}

fn number(expr: &mut Spanned<TypedExpr>, next: &mut NodeId) {
    expr.item.id = *next;
    *next += 1;
    number_kind(&mut expr.item.kind, next);
}

fn number_opt(expr: &mut Option<Box<Spanned<TypedExpr>>>, next: &mut NodeId) {
    if let Some(e) = expr {
        number(e, next);
    }
}

fn number_kind(kind: &mut TypedExprKind, next: &mut NodeId) {
    match kind {
        TypedExprKind::IntLit(_)
        | TypedExprKind::FloatLit(_)
        | TypedExprKind::BoolLit(_)
        | TypedExprKind::StrLit(_)
        | TypedExprKind::NoneLit
        | TypedExprKind::Var(_) => {}

        TypedExprKind::Unary { expr, .. } => number(expr, next),
        TypedExprKind::Binary { left, right, .. } => {
            number(left, next);
            number(right, next);
        }
        TypedExprKind::Conditional { cond, true_branch, false_branch } => {
            number(cond, next);
            number(true_branch, next);
            number_opt(false_branch, next);
        }
        TypedExprKind::Assign { value, .. } => number(value, next),
        TypedExprKind::Function { body, .. } => number(body, next),
        TypedExprKind::Call { callable, args, .. } => {
            number(callable, next);
            for a in args.iter_mut().flat_map(Arg::subexprs_mut) { number(a, next); }
        }
        TypedExprKind::Index { target, index } => {
            number(target, next);
            number(index, next);
        }
        TypedExprKind::Slice { target, start, end } => {
            number(target, next);
            number_opt(start, next);
            number_opt(end, next);
        }
        TypedExprKind::Range { start, end } => {
            number(start, next);
            number(end, next);
        }
        TypedExprKind::List(elems) => {
            for e in elems { number(e, next); }
        }
        TypedExprKind::Dict(pairs) => {
            for (k, v) in pairs { number(k, next); number(v, next); }
        }
        TypedExprKind::Block(stmts) => {
            for s in stmts { number(s, next); }
        }
        TypedExprKind::ForLoop { iterable, cond, body, .. }
        | TypedExprKind::Comprehension { iterable, cond, body, .. } => {
            number(iterable, next);
            number_opt(cond, next);
            number(body, next);
        }
        TypedExprKind::StructInit { fields, .. } => {
            for (_, v) in fields { number(v, next); }
        }
        TypedExprKind::FieldAccess { target, .. } => number(target, next),
        TypedExprKind::PlaceAssign { place, value } => {
            for seg in place.path.iter_mut() {
                if let PlaceSeg::Index { index, .. } = seg {
                    number(index, next);
                }
            }
            number(value, next);
        }
        TypedExprKind::VariantInit { fields, .. } => {
            for (_, v) in fields { number(v, next); }
        }
        TypedExprKind::IsVariant { target, .. } => number(target, next),
        TypedExprKind::VariantField { target, .. } => number(target, next),
        TypedExprKind::Return(value) => number_opt(value, next),
        TypedExprKind::Widen { value, .. } => number(value, next),
        TypedExprKind::Narrow { value, .. } => number(value, next),
        TypedExprKind::TypeTag { target, .. } => number(target, next),
        TypedExprKind::Truthy(value) => number(value, next),
        TypedExprKind::Coerce(value) => number(value, next),
    }
}

// ── Phase 2: the liveness analysis itself ───────────────────────────────────
//
// A backward, name-level (never per-leaf — see `Liveness`'s own doc comment)
// dataflow analysis over the typed AST. There is no CFG on the froglang side
// (codegen builds Cranelift's implicitly, as it walks the tree — see
// `codegen::compile_expr_multi`), so this is a structured fold rather than a
// worklist over basic blocks: each `transfer` arm computes live-in from
// live-out by walking children in the *reverse* of the order
// `compile_expr_multi`'s matching arm evaluates them, since a name last read
// late in evaluation order is exactly the one whose earlier reads are not
// last uses. Getting an arm's child order backwards from codegen's is the
// single easiest way to produce an unsound answer, so every arm below is
// commented with the `codegen/mod.rs` arm it mirrors.
//
// The one place this *isn't* a straight structural mirror is the `for`-loop
// back-edge — see `transfer_loop`.

/// Binding names live at some program point. Always a plain `String` set,
/// never a per-leaf one: `TypedExprKind::Var` reads every flattened leaf of
/// its type in one instruction (`codegen/mod.rs:1431`), so a struct with
/// nine scalar leaves and one heap leaf is live or dead as a single unit —
/// this mirrors that, deliberately, rather than chasing precision the
/// codegen representation can't use anyway. It also mirrors
/// `codegen`'s `vars: HashMap<String, Variable>` keying, which conflates a
/// shadowing inner binding with its outer namesake into one Cranelift
/// `Variable` (`var_key`) — see `Ownership`'s own note on shadowing.
pub type NameSet = BTreeSet<String>;

/// Whether a `Var` occurrence's value can be moved out of its binding
/// (transferred, not copied) because nothing reachable after it in program
/// order can observe the old binding's contents. `Move` is recorded when the
/// name is *not* already in the occurrence's `live_out` — i.e. this read is
/// the name's last one on every path forward from here.
///
/// This is name-based, so it inherits the one piece of imprecision that
/// choice buys: the typed AST does not resolve shadowing (`let x = 1` then,
/// in an inner scope, another `let x = 2`, are both just `Var("x")`/
/// `Assign{name: "x", ..}` — see `codegen::var_key`'s doc comment for why
/// codegen already conflates them into one `Variable`). A liveness result
/// computed over plain names is sound for that representation and would
/// need scope-qualified names — which nothing downstream understands yet —
/// to do any better.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership { Copy, Move }

/// Liveness facts computed for one function body (`analyze_body`) or one
/// REPL/top-level entry's statement list (`analyze_entry`).
pub struct Liveness {
    /// `Var` node ids that are the name's last use on every path forward.
    last_use: HashSet<NodeId>,
    /// Statement node id -> names whose live range ends immediately after
    /// that statement (computed at `Block`/entry statement boundaries as
    /// `live_in(stmt) \ live_out(stmt)`).
    dead_after: HashMap<NodeId, Vec<String>>,
}

impl Liveness {
    fn empty() -> Self {
        Liveness { last_use: HashSet::new(), dead_after: HashMap::new() }
    }

    /// `Ownership::Move` iff the `Var` node `var_node` (its `TypedExpr::id`)
    /// is this name's last use on every path forward from it. Panics-free
    /// for an id this analysis never numbered — such an id simply isn't in
    /// `last_use`, so `Copy` (the conservative answer) comes back.
    pub fn ownership(&self, var_node: NodeId) -> Ownership {
        if self.last_use.contains(&var_node) { Ownership::Move } else { Ownership::Copy }
    }

    /// Names whose live range ends right after the `Block`/entry statement
    /// with this node id — empty for a node that isn't a statement
    /// position, or one after which nothing dies.
    pub fn dead_after(&self, stmt: NodeId) -> &[String] {
        self.dead_after.get(&stmt).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// Read-only state threaded through every `transfer` call — currently just
/// the names live at every `return` (see `transfer`'s `Return` arm).
struct Ctx {
    /// Names live at the end of the body being analyzed — i.e. what a
    /// `return` (or falling off the end) must keep alive. See
    /// `analyze_body`/`analyze_entry`'s doc comments for what belongs here;
    /// getting this wrong for `analyze_entry` is a real memory-safety bug,
    /// not just lost precision (`FrogState::eval` rebuilds the GC root set
    /// from `env` after every REPL entry — `state.rs:299-335` — so a name
    /// this analysis fails to keep alive across an entry gets its root
    /// cleared while `env` still points at it).
    exit_live: NameSet,
}

/// Analyze one `func` body. `exit_live` must be exactly its `mut` parameter
/// names (`TypedExprKind::Function.params`, the `bool` field) — those are
/// read back at every `return_`-emitting site by `mut_param_copyout`
/// (`codegen/mod.rs:282`), so they're live no matter how the body exits, and
/// nothing else the caller doesn't already own is.
pub fn analyze_body(body: &Spanned<TypedExpr>, exit_live: &NameSet) -> Liveness {
    let ctx = Ctx { exit_live: exit_live.clone() };
    let mut out = Liveness::empty();
    transfer(body, exit_live, &ctx, &mut out);
    out
}

/// Analyze one top-level/REPL entry's statement list (`build_main_body`'s
/// `stmts`, not wrapped in a `Block` node — the entry has none).
///
/// `exit_live` must include **every name in `env_types`** (every binding any
/// prior entry made, not just this one's own), because `FrogState::eval`
/// treats every `env` binding as live after every entry
/// (`state.rs:305-335`) whether or not this entry's code happens to use it.
/// Passing just this entry's own `Assign` names here would let a name used
/// once, early, in this entry get its GC root cleared before the entry
/// finishes — the entry still runs to completion holding a dangling
/// pointer's *slot*, but the object behind it may already be swept.
pub fn analyze_entry(stmts: &[Spanned<TypedExpr>], exit_live: &NameSet) -> Liveness {
    let ctx = Ctx { exit_live: exit_live.clone() };
    let mut out = Liveness::empty();
    let mut live_out = exit_live.clone();
    for s in stmts.iter().rev() {
        let live_in = transfer(s, &live_out, &ctx, &mut out);
        record_dead_after(&mut out, s.item.id, &live_in, &live_out);
        live_out = live_in;
    }
    out
}

/// Print every `Var` occurrence in `body` (a `func` body — see
/// `analyze_body`) to stderr as `name @ span -> Copy|Move`, one line each,
/// in source order — gated on `FROG_DUMP_LIVENESS` at the two call sites in
/// `codegen::build_func_body`/`build_main_body`, since walking the whole
/// tree a second time only to throw the result away isn't free. This is
/// currently the *only* consumer of `Ownership`: nothing in codegen acts on
/// a `Move` mark yet — GC roots are now Cranelift's own stack maps
/// (RUNTIME.md Part 2), tied to each `Variable`'s real live range, so the
/// per-binding-slot problem this analysis was designed against no longer
/// exists on the GC side. A future consumer (move-on-last-use for a
/// non-GC resource, or a mid-level IR) still has to define its own
/// ownership discipline; nothing here presumes one. So this dump is the only way
/// to inspect the analysis today, and it's what a future semantic consumer
/// (copy elision, `mut` container operations) should be checked against.
pub fn dump_body(name: &str, body: &Spanned<TypedExpr>, liveness: &Liveness) {
    eprintln!("-- liveness: {} --", name);
    dump_walk(body, liveness);
}

/// Same as `dump_body`, for one REPL/top-level entry's bare statement list
/// (see `analyze_entry`).
pub fn dump_entry(name: &str, stmts: &[Spanned<TypedExpr>], liveness: &Liveness) {
    eprintln!("-- liveness: {} --", name);
    for s in stmts { dump_walk(s, liveness); }
}

fn dump_walk(expr: &Spanned<TypedExpr>, liveness: &Liveness) {
    if let TypedExprKind::Var(name) = &expr.item.kind {
        let mark = match liveness.ownership(expr.item.id) {
            Ownership::Copy => "Copy",
            Ownership::Move => "Move",
        };
        eprintln!("  {} @ {} -> {}", name, expr.span, mark);
    }
    dump_walk_kind(&expr.item.kind, liveness);
}
fn dump_walk_opt(expr: &Option<TypedExprRef>, liveness: &Liveness) {
    if let Some(e) = expr { dump_walk(e, liveness); }
}
fn dump_walk_kind(kind: &TypedExprKind, liveness: &Liveness) {
    match kind {
        TypedExprKind::IntLit(_) | TypedExprKind::FloatLit(_) | TypedExprKind::BoolLit(_)
        | TypedExprKind::StrLit(_) | TypedExprKind::NoneLit | TypedExprKind::Var(_) => {}
        TypedExprKind::Unary { expr, .. } => dump_walk(expr, liveness),
        TypedExprKind::Binary { left, right, .. } => { dump_walk(left, liveness); dump_walk(right, liveness); }
        TypedExprKind::Conditional { cond, true_branch, false_branch } => {
            dump_walk(cond, liveness); dump_walk(true_branch, liveness); dump_walk_opt(false_branch, liveness);
        }
        TypedExprKind::Assign { value, .. } => {
            if !matches!(value.item.kind, TypedExprKind::Function { .. }) { dump_walk(value, liveness); }
        }
        TypedExprKind::Function { .. } => {}
        TypedExprKind::Call { args, .. } => { for a in args.iter().flat_map(Arg::subexprs) { dump_walk(a, liveness); } }
        TypedExprKind::Index { target, index } => { dump_walk(target, liveness); dump_walk(index, liveness); }
        TypedExprKind::Slice { target, start, end } => {
            dump_walk(target, liveness); dump_walk_opt(start, liveness); dump_walk_opt(end, liveness);
        }
        TypedExprKind::Range { start, end } => { dump_walk(start, liveness); dump_walk(end, liveness); }
        TypedExprKind::List(elems) => for e in elems { dump_walk(e, liveness); },
        TypedExprKind::Dict(pairs) => for (k, v) in pairs { dump_walk(k, liveness); dump_walk(v, liveness); },
        TypedExprKind::Block(stmts) => for s in stmts { dump_walk(s, liveness); },
        TypedExprKind::ForLoop { iterable, cond, body, .. }
        | TypedExprKind::Comprehension { iterable, cond, body, .. } => {
            dump_walk(iterable, liveness); dump_walk_opt(cond, liveness); dump_walk(body, liveness);
        }
        TypedExprKind::StructInit { fields, .. } => for (_, v) in fields { dump_walk(v, liveness); },
        TypedExprKind::FieldAccess { target, .. } => dump_walk(target, liveness),
        TypedExprKind::PlaceAssign { place, value } => {
            for seg in &place.path {
                if let PlaceSeg::Index { index, .. } = seg { dump_walk(index, liveness); }
            }
            dump_walk(value, liveness);
        }
        TypedExprKind::VariantInit { fields, .. } => for (_, v) in fields { dump_walk(v, liveness); },
        TypedExprKind::IsVariant { target, .. } => dump_walk(target, liveness),
        TypedExprKind::VariantField { target, .. } => dump_walk(target, liveness),
        TypedExprKind::Return(value) => dump_walk_opt(value, liveness),
        TypedExprKind::Widen { value, .. } => dump_walk(value, liveness),
        TypedExprKind::Narrow { value, .. } => dump_walk(value, liveness),
        TypedExprKind::TypeTag { target, .. } => dump_walk(target, liveness),
        TypedExprKind::Truthy(value) => dump_walk(value, liveness),
        TypedExprKind::Coerce(value) => dump_walk(value, liveness),
    }
}

/// `dead_after(stmt) = live_in(stmt) \ live_out(stmt)` — names this
/// statement's execution was the last thing that needed, so they die
/// immediately after it.
fn record_dead_after(out: &mut Liveness, stmt_id: NodeId, live_in: &NameSet, live_out: &NameSet) {
    let dead: Vec<String> = live_in.difference(live_out).cloned().collect();
    if !dead.is_empty() {
        out.dead_after.entry(stmt_id).or_default().extend(dead);
    }
}

/// Fold a plain expression list backward (`List`'s elements) — codegen
/// evaluates them left to right (`compile_list_lit`), so the backward walk
/// processes them right to left.
fn fold_reverse(exprs: &[Spanned<TypedExpr>], live_out: &NameSet, ctx: &Ctx, out: &mut Liveness) -> NameSet {
    let mut lo = live_out.clone();
    for e in exprs.iter().rev() {
        lo = transfer(e, &lo, ctx, out);
    }
    lo
}

/// Fold a `(name, value)` field list backward (`StructInit`/`VariantInit`) —
/// same evaluation-order reasoning as `fold_reverse`, just over the tuples
/// `compile_expr_multi`'s matching arms iterate.
fn fold_fields_reverse(fields: &[(String, TypedExprRef)], live_out: &NameSet, ctx: &Ctx, out: &mut Liveness) -> NameSet {
    let mut lo = live_out.clone();
    for (_, v) in fields.iter().rev() {
        lo = transfer(v, &lo, ctx, out);
    }
    lo
}

/// Live-in of `e` given `live_out` (live immediately after `e` finishes).
/// Records `last_use`/`dead_after` facts into `out` as a side effect. See
/// this file's module-level comment for the ordering discipline every arm
/// must follow.
fn transfer(e: &Spanned<TypedExpr>, live_out: &NameSet, ctx: &Ctx, out: &mut Liveness) -> NameSet {
    match &e.item.kind {
        // ── Leaves: nothing to propagate ──
        TypedExprKind::IntLit(_)
        | TypedExprKind::FloatLit(_)
        | TypedExprKind::BoolLit(_)
        | TypedExprKind::StrLit(_)
        | TypedExprKind::NoneLit => live_out.clone(),

        // A read. If `name` isn't already needed later, this occurrence is
        // its last use.
        TypedExprKind::Var(name) => {
            if !live_out.contains(name) {
                out.last_use.insert(e.item.id);
            }
            let mut live_in = live_out.clone();
            live_in.insert(name.clone());
            live_in
        }

        // ── Single-child pass-through (mirrors compile_expr_multi's
        // single `compile_expr(inner, ...)` call in each of these arms) ──
        TypedExprKind::Unary { expr: inner, .. }
        | TypedExprKind::Truthy(inner)
        | TypedExprKind::Coerce(inner)
        | TypedExprKind::Widen { value: inner, .. }
        | TypedExprKind::Narrow { value: inner, .. }
        | TypedExprKind::TypeTag { target: inner, .. }
        | TypedExprKind::IsVariant { target: inner, .. }
        | TypedExprKind::VariantField { target: inner, .. } => transfer(inner, live_out, ctx, out),

        // `FieldAccess`'s `enum_name: Some` arm reads out of heap memory via
        // `target`'s value and never touches `target`'s own leaves
        // (`compile_expr_multi:1596`); the `None` (plain struct) arm reads
        // `target`'s full flattened value (`:1601`) then slices it. Either
        // way `target` is the only child that can hold a live name.
        TypedExprKind::FieldAccess { target, .. } => transfer(target, live_out, ctx, out),

        // `compile_binary` evaluates `left` then `right`
        // (`compile_binary:1674` on the `Str` path, and the numeric path
        // right after) — including `And`/`Or`, where `right` is only
        // *conditionally* evaluated (`compile_binary:1720`). Treating it as
        // unconditional here is a deliberate over-approximation: sound
        // (never marks a name dead that a taken path still needs), just not
        // maximally precise on the untaken short-circuit path.
        TypedExprKind::Binary { left, right, .. } => {
            let after_right = transfer(right, live_out, ctx, out);
            transfer(left, &after_right, ctx, out)
        }

        // `compile_expr_multi:1465` evaluates `target` then `index`.
        TypedExprKind::Index { target, index } => {
            let after_index = transfer(index, live_out, ctx, out);
            transfer(target, &after_index, ctx, out)
        }

        // `compile_expr_multi:1487` evaluates `target`, then `start`
        // (if present), then `end` (if present).
        TypedExprKind::Slice { target, start, end } => {
            let mut lo = live_out.clone();
            if let Some(e2) = end { lo = transfer(e2, &lo, ctx, out); }
            if let Some(s2) = start { lo = transfer(s2, &lo, ctx, out); }
            transfer(target, &lo, ctx, out)
        }

        // `compile_expr_multi:1509` evaluates `start` then `end`.
        TypedExprKind::Range { start, end } => {
            let after_end = transfer(end, live_out, ctx, out);
            transfer(start, &after_end, ctx, out)
        }

        // `compile_list_lit` pushes each element in source order.
        TypedExprKind::List(elems) => fold_reverse(elems, live_out, ctx, out),

        // `compile_dict_lit` evaluates each pair's key then value, pairs in
        // source order — reverse that here the same way `fold_reverse` does
        // for a flat list.
        TypedExprKind::Dict(pairs) => {
            let mut lo = live_out.clone();
            for (k, v) in pairs.iter().rev() {
                lo = transfer(v, &lo, ctx, out);
                lo = transfer(k, &lo, ctx, out);
            }
            lo
        }

        // Both compile in declared-field order (`compile_expr_multi:1583`
        // for `StructInit`; `compile_variant_init` for `VariantInit`).
        TypedExprKind::StructInit { fields, .. } | TypedExprKind::VariantInit { fields, .. } =>
            fold_fields_reverse(fields, live_out, ctx, out),

        // `TypedExprKind::Block(Vec<..>)` (`compile_expr_multi:1536`) folds
        // its statements in source order, keeping only the last one's
        // value — but every statement, tail or not, still runs and can be
        // the last use of a name, so this records a `dead_after` boundary
        // at each one, exactly like `analyze_entry` does for a bare
        // statement list.
        TypedExprKind::Block(stmts) => {
            let mut lo = live_out.clone();
            for s in stmts.iter().rev() {
                let li = transfer(s, &lo, ctx, out);
                record_dead_after(out, s.item.id, &li, &lo);
                lo = li;
            }
            lo
        }

        // A function-valued `Assign` is skipped by codegen entirely
        // (`compile_expr_multi:1521-1524` never compiles `value`; function
        // bindings live in the separate `func_ids` map, never in `vars`) —
        // so `name` never enters this analysis's namespace and `value`
        // (a `Function` node, itself never descended into either — see
        // below) contributes nothing.
        TypedExprKind::Assign { name, value } => {
            if matches!(value.item.kind, TypedExprKind::Function { .. }) {
                live_out.clone()
            } else {
                let mut lo = live_out.clone();
                lo.remove(name);
                transfer(value, &lo, ctx, out)
            }
        }

        // Reached only for a `Function` node that isn't the RHS of a
        // top-level `Assign` (codegen's own arm, `compile_expr_multi:1544`,
        // likewise never descends into it: function bodies are compiled
        // separately, as their own top-level Cranelift functions with
        // their own `analyze_body` call — see `build_func_body`).
        TypedExprKind::Function { .. } => live_out.clone(),

        // `root` is read-modify-write (a place write reads the rest of the
        // binding's other leaves implicitly, by only rebinding the touched
        // ones — `compile_place_assign`'s no-index arm, `codegen/mod.rs:1354`),
        // so it's added to `live_out`, never removed. `compile_place_assign`
        // evaluates any `Index` step's subexpression *before* `value`
        // (`codegen/mod.rs:1379` then `:1387` — the index is needed to
        // compute the write address first), so backward, `value` is
        // processed before the path's index expressions.
        // `Push` is the same shape: a read-modify-write of `place`'s root
        // (`MUTABILITY.md` Stage 8), with its own value evaluated after the
        // path's index expressions.
        TypedExprKind::PlaceAssign { place, value } => {
            let mut lo = live_out.clone();
            lo.insert(place.root.clone());
            lo = transfer(value, &lo, ctx, out);
            for seg in place.path.iter().rev() {
                if let PlaceSeg::Index { index, .. } = seg {
                    lo = transfer(index, &lo, ctx, out);
                }
            }
            lo
        }

        // `compile_conditional` evaluates `cond` first, then exactly one of
        // `true_branch`/`false_branch` at runtime — but statically, either
        // could run, so live-in of `cond` must cover both. A missing
        // `false_branch` contributes `live_out` unchanged, matching
        // `compile_conditional`'s own synthesized-`else` behavior
        // (`codegen/mod.rs:1876` inserts an implicit `0`/no-op arm).
        //
        // A branch whose own `.ty` is `Type::Never` has no edge to the join
        // at all (`compile_conditional` skips emitting its jump —
        // `codegen/mod.rs:1876`, `:1889`) — folding it in via `live_out`
        // here anyway is a deliberate, sound over-approximation (v1; see
        // `MUTABILITY.md`/this analysis's plan). It can only make a name
        // look live where it's actually dead, never the reverse.
        TypedExprKind::Conditional { cond, true_branch, false_branch } => {
            let true_live = transfer(true_branch, live_out, ctx, out);
            let false_live = match false_branch {
                Some(fb) => transfer(fb, live_out, ctx, out),
                None => live_out.clone(),
            };
            let joined: NameSet = true_live.union(&false_live).cloned().collect();
            transfer(cond, &joined, ctx, out)
        }

        // `return`/`return value` never falls through — whatever's live
        // after it in the surrounding `Block` is irrelevant; what matters
        // is `ctx.exit_live`, the same set every other exit path
        // (falling off the end) must also satisfy.
        TypedExprKind::Return(value) => match value {
            Some(v) => transfer(v, &ctx.exit_live, ctx, out),
            None => ctx.exit_live.clone(),
        },

        // `compile_call` (`codegen/mod.rs:1956`) evaluates `args` left to
        // right, then — only *after* the call returns — rebinds each
        // `mut`-marked argument's root from the callee's copy-out
        // (`:2114-2130`). Backward, that copy-out def is processed first:
        // removing the root name means the argument's own `Var` occurrence
        // (visited next, below) sees its old value as unneeded afterward,
        // so it's correctly marked a last use — `bump(mut a)` really does
        // consume the caller's `a` and hand back a fresh one.
        // `TypeChecker::lower_call` guarantees no other argument shares a
        // `mut` argument's root name (the exclusivity check), so these
        // removals can't interact with each other.
        //
        // `callable` is always `TypedExprKind::Var(function_name)`
        // (`codegen/mod.rs:1957` panics otherwise) naming a `func_ids`
        // entry, not a `vars` binding — recursing into it the way an
        // ordinary `Var` child would be treated would wrongly add the
        // function's name to this analysis's binding namespace, so it's
        // deliberately never visited.
        TypedExprKind::Call { args, .. } => {
            let mut lo = live_out.clone();
            // A `mut` argument's root is read-modify-write, exactly like a
            // `PlaceAssign`'s: the call reads the place's current value and
            // the copy-out writes a new one back, so the root is live going
            // in. Anything that aliased it earlier must therefore see a
            // `Copy` and mark — `mut b = a; f(mut a)` where the callee
            // pushes must not be visible through `b`.
            //
            // Before places, this arm *removed* the root instead, and was
            // still correct only because the argument was a `Var` node whose
            // own transfer added it straight back; the removal existed to
            // make that read a last use, so it would not clone. Reading a
            // place creates no `Var` occurrence at all, so there is nothing
            // left to classify — the elision is structural now
            // (`codegen::emit_place_ref` never marks), and this is a plain
            // read-modify-write.
            for arg in args.iter() {
                if let Arg::Mut(place) = arg { lo.insert(place.root.clone()); }
            }
            for a in args.iter().rev().flat_map(Arg::subexprs) {
                lo = transfer(a, &lo, ctx, out);
            }
            lo
        }

        TypedExprKind::ForLoop { var, iterable, cond, body, .. }
        | TypedExprKind::Comprehension { var, iterable, cond, body, .. } =>
            transfer_loop(var, iterable, cond, body, live_out, ctx, out),
    }
}

/// The only back-edge in the language (`compile_for_loop`,
/// `codegen/mod.rs:2496`: `header_bb` branches to `body_bb` or `exit_bb`;
/// `body_bb` — after an optional guard, which itself has a second path
/// straight back to the header when false — jumps back to `header_bb`).
/// `iterable` is evaluated exactly once, before the loop
/// (`compile_for_loop:2483`) and never referenced again by name inside the
/// loop — the per-iteration element read is raw memory access keyed by
/// index, not a typed-AST `Var` — so it is the one child *not* part of the
/// cycle.
///
/// Because a name used at the top of `body` is also needed at the bottom
/// (the previous iteration's tail flows into it across the back-edge), the
/// header's own live set is a fixpoint, not a single backward pass: this
/// iterates until the set entering `header_bb` stops growing, then commits
/// real `last_use`/`dead_after` marks only for that converged answer.
///
/// `var` is bound fresh every iteration from the current list element
/// (`compile_for_loop:2537-2541`), so it is never itself carried across the
/// back-edge — it's removed from the header's accumulated set on every
/// round, exactly like a normal local declared at the top of a loop body.
fn transfer_loop(
    var: &str,
    iterable: &Spanned<TypedExpr>,
    cond: &Option<TypedExprRef>,
    body: &Spanned<TypedExpr>,
    live_out: &NameSet,
    ctx: &Ctx,
    out: &mut Liveness,
) -> NameSet {
    // One round: propagate `b` (the header's current live set) through the
    // back-edge — `body`, then (if present) the guard, which has two
    // successors of its own: into `body` (live-out `l`, body's own
    // live-in) or straight back to the header on a false guard (live-out
    // `b` itself) — before dropping `var`, which the header never carries.
    let round = |b: &NameSet, out: &mut Liveness| -> NameSet {
        let mut l = transfer(body, b, ctx, out);
        if let Some(c) = cond {
            let mut guard_live_out = l.clone();
            guard_live_out.extend(b.iter().cloned());
            l = transfer(c, &guard_live_out, ctx, out);
        }
        l.remove(var);
        let mut b_next = l;
        b_next.extend(live_out.iter().cloned());
        b_next
    };

    // Discovery: iterate on a throwaway `Liveness` so no `last_use`/
    // `dead_after` mark from a non-converged round leaks into the real
    // result — a `Var` use inside `body` can look like a last use on an
    // early round and then turn out to be live across the back-edge once
    // the fixpoint actually settles; recording it early would be a
    // use-after-move once move-on-last-use has a consumer.
    let mut scratch = Liveness::empty();
    let mut b = live_out.clone();
    let mut rounds = 0usize;
    loop {
        let b_next = round(&b, &mut scratch);
        if b_next == b { break; }
        b = b_next;
        rounds += 1;
        debug_assert!(
            rounds < 10_000,
            "liveness fixpoint over a for-loop failed to converge after {} rounds \
             — a transfer function above is non-monotone",
            rounds,
        );
    }

    // Confirmation: re-run exactly the same computation once more with the
    // converged `b`, this time recording into the real `out`.
    round(&b, out);

    transfer(iterable, &b, ctx, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontend::parser::Parser;
    use crate::frontend::typeck::TypeChecker;

    fn lower(src: &str) -> Spanned<TypedExpr> {
        let ast = Parser::parse(src).expect("parse error");
        let mut tc = TypeChecker::new();
        tc.check_and_lower_entry(ast).expect("type error")
    }

    /// Collect every id reachable from `expr`, in the same order `number`
    /// visits them — used to assert both "every node got a nonzero id" and
    /// "no two nodes share one" without hand-writing a second walk.
    fn collect_ids(expr: &Spanned<TypedExpr>, out: &mut Vec<NodeId>) {
        out.push(expr.item.id);
        collect_kind_ids(&expr.item.kind, out);
    }
    fn collect_opt_ids(expr: &Option<Box<Spanned<TypedExpr>>>, out: &mut Vec<NodeId>) {
        if let Some(e) = expr { collect_ids(e, out); }
    }
    fn collect_kind_ids(kind: &TypedExprKind, out: &mut Vec<NodeId>) {
        match kind {
            TypedExprKind::IntLit(_) | TypedExprKind::FloatLit(_) | TypedExprKind::BoolLit(_)
            | TypedExprKind::StrLit(_) | TypedExprKind::NoneLit | TypedExprKind::Var(_) => {}
            TypedExprKind::Unary { expr, .. } => collect_ids(expr, out),
            TypedExprKind::Binary { left, right, .. } => { collect_ids(left, out); collect_ids(right, out); }
            TypedExprKind::Conditional { cond, true_branch, false_branch } => {
                collect_ids(cond, out); collect_ids(true_branch, out); collect_opt_ids(false_branch, out);
            }
            TypedExprKind::Assign { value, .. } => collect_ids(value, out),
            TypedExprKind::Function { body, .. } => collect_ids(body, out),
            TypedExprKind::Call { callable, args, .. } => {
                collect_ids(callable, out);
                for a in args.iter().flat_map(Arg::subexprs) { collect_ids(a, out); }
            }
            TypedExprKind::Index { target, index } => { collect_ids(target, out); collect_ids(index, out); }
            TypedExprKind::Slice { target, start, end } => {
                collect_ids(target, out); collect_opt_ids(start, out); collect_opt_ids(end, out);
            }
            TypedExprKind::Range { start, end } => { collect_ids(start, out); collect_ids(end, out); }
            TypedExprKind::List(elems) => for e in elems { collect_ids(e, out); },
            TypedExprKind::Dict(pairs) => for (k, v) in pairs { collect_ids(k, out); collect_ids(v, out); },
            TypedExprKind::Block(stmts) => for s in stmts { collect_ids(s, out); },
            TypedExprKind::ForLoop { iterable, cond, body, .. }
            | TypedExprKind::Comprehension { iterable, cond, body, .. } => {
                collect_ids(iterable, out); collect_opt_ids(cond, out); collect_ids(body, out);
            }
            TypedExprKind::StructInit { fields, .. } => for (_, v) in fields { collect_ids(v, out); },
            TypedExprKind::FieldAccess { target, .. } => collect_ids(target, out),
            TypedExprKind::PlaceAssign { place, value } => {
                for seg in &place.path {
                    if let PlaceSeg::Index { index, .. } = seg { collect_ids(index, out); }
                }
                collect_ids(value, out);
            }
            TypedExprKind::VariantInit { fields, .. } => for (_, v) in fields { collect_ids(v, out); },
            TypedExprKind::IsVariant { target, .. } => collect_ids(target, out),
            TypedExprKind::VariantField { target, .. } => collect_ids(target, out),
            TypedExprKind::Return(value) => collect_opt_ids(value, out),
            TypedExprKind::Widen { value, .. } => collect_ids(value, out),
            TypedExprKind::Narrow { value, .. } => collect_ids(value, out),
            TypedExprKind::TypeTag { target, .. } => collect_ids(target, out),
            TypedExprKind::Truthy(value) => collect_ids(value, out),
            TypedExprKind::Coerce(value) => collect_ids(value, out),
        }
    }

    fn assert_dense_unique(mut typed: Spanned<TypedExpr>, expect_source: &str) {
        let n = number_nodes(&mut typed);
        let mut ids = Vec::new();
        collect_ids(&typed, &mut ids);
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "duplicate id assigned for {:?}", expect_source);
        assert!(ids.iter().all(|&id| id != 0), "unnumbered (id == 0) node in {:?}", expect_source);
        assert_eq!(sorted, (1..=n).collect::<Vec<_>>(), "ids not densely packed 1..=n for {:?}", expect_source);
    }

    #[test]
    fn numbers_simple_expr() {
        assert_dense_unique(lower("1 + 2"), "1 + 2");
    }

    #[test]
    fn numbers_conditional() {
        assert_dense_unique(lower("if true then 1 else 2"), "conditional");
    }

    #[test]
    fn numbers_for_loop() {
        assert_dense_unique(lower("mut acc = 0\nfor i in 0..5 do { acc = acc + i }\nacc"), "for loop");
    }

    #[test]
    fn numbers_function_body() {
        // Function bodies must be reached too (unlike for_each_heap_producer).
        assert_dense_unique(lower("func f(x: Int): Int = x + 1\nf(2)"), "function");
    }

    #[test]
    fn numbers_match_desugaring() {
        let src = "data Shape is Circle(r: Int) | Square(s: Int)\n\
                    let sh = Circle(r=2)\n\
                    match sh {\n\
                    is Circle(r) then r\n\
                    is Square(s) then s\n\
                    }";
        assert_dense_unique(lower(src), "match");
    }

    #[test]
    fn numbers_catch_desugaring_with_clones() {
        // build_catch_arms clones handler_body into every Error-providing
        // arm — this is the case that motivates numbering after lowering
        // rather than during construction.
        let src = "data E(msg: Str) provides Error\n\
                    func f(x: Int): Int | E = if x < 0 then E(msg=\"bad\") else x\n\
                    f(1) catch 0";
        assert_dense_unique(lower(src), "catch");
    }

    #[test]
    fn numbers_place_assign_with_index() {
        let src = "mut xs = [1, 2, 3]\nxs[0] = 9";
        assert_dense_unique(lower(src), "place assign with index");
    }

    // ── Phase 2: the analysis itself ──────────────────────────────────────

    fn lower_numbered(src: &str) -> Spanned<TypedExpr> {
        let mut t = lower(src);
        number_nodes(&mut t);
        t
    }

    /// Every test source is multi-statement, so `check_and_lower_entry`
    /// always wraps it in `TypedExprKind::Block` (see `lower`'s own doc
    /// comment above) — unwrap that into the bare statement list
    /// `analyze_entry` expects (an entry has no enclosing `Block` node of
    /// its own; `FrogState::eval`/`build_main_body` work from the list
    /// directly).
    fn entry_stmts(typed: Spanned<TypedExpr>) -> Vec<Spanned<TypedExpr>> {
        match typed.item.kind {
            TypedExprKind::Block(stmts) => stmts,
            other => vec![Spanned::from(TypedExpr { id: typed.item.id, ty: typed.item.ty, kind: other }, typed.span)],
        }
    }

    /// Every `Var(name)` node's id, in the same traversal order `number`
    /// visits them (i.e. source order) — lets a test address "the Nth
    /// occurrence of `x`" without hand-computing ids.
    fn collect_var_ids(expr: &Spanned<TypedExpr>, name: &str, out: &mut Vec<NodeId>) {
        if let TypedExprKind::Var(n) = &expr.item.kind {
            if n == name { out.push(expr.item.id); }
        }
        collect_var_ids_kind(&expr.item.kind, name, out);
    }
    fn collect_var_ids_opt(expr: &Option<Box<Spanned<TypedExpr>>>, name: &str, out: &mut Vec<NodeId>) {
        if let Some(e) = expr { collect_var_ids(e, name, out); }
    }
    fn collect_var_ids_kind(kind: &TypedExprKind, name: &str, out: &mut Vec<NodeId>) {
        match kind {
            TypedExprKind::IntLit(_) | TypedExprKind::FloatLit(_) | TypedExprKind::BoolLit(_)
            | TypedExprKind::StrLit(_) | TypedExprKind::NoneLit | TypedExprKind::Var(_) => {}
            TypedExprKind::Unary { expr, .. } => collect_var_ids(expr, name, out),
            TypedExprKind::Binary { left, right, .. } => { collect_var_ids(left, name, out); collect_var_ids(right, name, out); }
            TypedExprKind::Conditional { cond, true_branch, false_branch } => {
                collect_var_ids(cond, name, out); collect_var_ids(true_branch, name, out); collect_var_ids_opt(false_branch, name, out);
            }
            TypedExprKind::Assign { value, .. } => collect_var_ids(value, name, out),
            TypedExprKind::Function { body, .. } => collect_var_ids(body, name, out),
            TypedExprKind::Call { callable, args, .. } => {
                collect_var_ids(callable, name, out);
                for a in args.iter().flat_map(Arg::subexprs) { collect_var_ids(a, name, out); }
            }
            TypedExprKind::Index { target, index } => { collect_var_ids(target, name, out); collect_var_ids(index, name, out); }
            TypedExprKind::Slice { target, start, end } => {
                collect_var_ids(target, name, out); collect_var_ids_opt(start, name, out); collect_var_ids_opt(end, name, out);
            }
            TypedExprKind::Range { start, end } => { collect_var_ids(start, name, out); collect_var_ids(end, name, out); }
            TypedExprKind::List(elems) => for e in elems { collect_var_ids(e, name, out); },
            TypedExprKind::Dict(pairs) => for (k, v) in pairs { collect_var_ids(k, name, out); collect_var_ids(v, name, out); },
            TypedExprKind::Block(stmts) => for s in stmts { collect_var_ids(s, name, out); },
            TypedExprKind::ForLoop { iterable, cond, body, .. }
            | TypedExprKind::Comprehension { iterable, cond, body, .. } => {
                collect_var_ids(iterable, name, out); collect_var_ids_opt(cond, name, out); collect_var_ids(body, name, out);
            }
            TypedExprKind::StructInit { fields, .. } => for (_, v) in fields { collect_var_ids(v, name, out); },
            TypedExprKind::FieldAccess { target, .. } => collect_var_ids(target, name, out),
            TypedExprKind::PlaceAssign { place, value } => {
                for seg in &place.path {
                    if let PlaceSeg::Index { index, .. } = seg { collect_var_ids(index, name, out); }
                }
                collect_var_ids(value, name, out);
            }
            TypedExprKind::VariantInit { fields, .. } => for (_, v) in fields { collect_var_ids(v, name, out); },
            TypedExprKind::IsVariant { target, .. } => collect_var_ids(target, name, out),
            TypedExprKind::VariantField { target, .. } => collect_var_ids(target, name, out),
            TypedExprKind::Return(value) => collect_var_ids_opt(value, name, out),
            TypedExprKind::Widen { value, .. } => collect_var_ids(value, name, out),
            TypedExprKind::Narrow { value, .. } => collect_var_ids(value, name, out),
            TypedExprKind::TypeTag { target, .. } => collect_var_ids(target, name, out),
            TypedExprKind::Truthy(value) => collect_var_ids(value, name, out),
            TypedExprKind::Coerce(value) => collect_var_ids(value, name, out),
        }
    }

    /// Corner case 1 (see the liveness plan): the only back-edge in the
    /// language. `count` is read-and-written every iteration, so its use
    /// *inside* the loop body must never be a last use — only the trailing
    /// read, after the loop, with nothing live past it, is.
    #[test]
    fn loop_back_edge_keeps_body_use_alive() {
        // `xs` is read every iteration and never reassigned. A naive
        // single backward pass — using only the set live *after* the
        // whole loop (empty here; nothing follows) as if it were already
        // the header's converged set — would see nothing forcing `xs` to
        // stay live and mark this occurrence a last use. A future
        // move-on-last-use consumer acting on that would have nothing to
        // read on iteration 2. The fixpoint is what discovers that the
        // body's *own* need for `xs` must feed back into the header's live
        // set before the body itself is analyzed for real.
        let src = "let xs = [1, 2, 3]\nfor i in xs do {\nprint(xs)\n}";
        let typed = lower_numbered(src);
        let mut ids = Vec::new();
        collect_var_ids(&typed, "xs", &mut ids);
        // One occurrence as the loop's `iterable` (evaluated once, before
        // the loop — the one child of `ForLoop` outside the back-edge),
        // one inside the body.
        assert_eq!(ids.len(), 2, "expected 2 occurrences of `xs`, found {}", ids.len());
        let liveness = analyze_entry(&entry_stmts(typed), &NameSet::new());
        assert_eq!(liveness.ownership(ids[1]), Ownership::Copy,
            "xs is read again next iteration — this body occurrence must not look like a last use");
    }

    /// Contrast with the above: a loop-carried *accumulator*
    /// (`count = count + i`) reassigns the name every iteration, so its
    /// own read genuinely *is* a last use of the pre-assignment value —
    /// nothing, not even the next iteration, ever reads that specific
    /// value again (the next iteration reads the value this one just
    /// wrote). This is the same reasoning as
    /// `self_assignment_reads_the_old_value_as_a_last_use`, just recurring
    /// every iteration instead of once — recorded here so the two loop
    /// tests aren't read as contradicting each other.
    #[test]
    fn loop_accumulator_reassignment_is_a_last_use_each_iteration() {
        let src = "mut count = 0\nfor i in 0..3 do {\ncount = count + i\n}\ncount";
        let typed = lower_numbered(src);
        let mut ids = Vec::new();
        collect_var_ids(&typed, "count", &mut ids);
        assert_eq!(ids.len(), 2, "one Var(count) on the accumulator's RHS, one trailing");
        let liveness = analyze_entry(&entry_stmts(typed), &NameSet::new());
        assert_eq!(liveness.ownership(ids[0]), Ownership::Move,
            "count = count + i's RHS read is the last use of that iteration's pre-update value");
        assert_eq!(liveness.ownership(ids[1]), Ownership::Move,
            "the trailing count has nothing live after it in this entry");
    }

    /// Corner case 2: a guarded loop has a *second* back-edge (the
    /// guard-false path jumps straight back to the header) — the fixpoint
    /// must still converge, and a read-only guard variable (never
    /// reassigned, exactly like `xs` above) must stay live rather than
    /// being marked a last use on its first evaluation.
    #[test]
    fn guarded_loop_converges_and_keeps_guard_var_alive() {
        let src = "mut count = 0\nlet limit = 2\nfor i in 0..5 if i > limit do {\ncount = count + i\n}\ncount";
        let typed = lower_numbered(src);
        let mut limit_ids = Vec::new();
        collect_var_ids(&typed, "limit", &mut limit_ids);
        assert_eq!(limit_ids.len(), 1);
        let liveness = analyze_entry(&entry_stmts(typed), &NameSet::new());
        assert_eq!(liveness.ownership(limit_ids[0]), Ownership::Copy,
            "limit is read again by the guard on the next iteration — never reassigned, \
             so (like `xs` in the unguarded-loop test) it must never look like a last use");
    }

    /// Corner case 5/6: a `mut` call argument is both a use (of the old
    /// value) and a def (of the copy-out).
    ///
    /// Since `MUTABILITY.md` Stage 8 it is a `Place`, not an expression, so
    /// it produces **no `Var` occurrence at all** — there is nothing left to
    /// classify, and the clone-elision this used to encode (marking the
    /// argument a last use so it would not clone) is structural now:
    /// `codegen::emit_place_ref` reads the place directly and never marks.
    ///
    /// What still has to hold is that the root is *live into* the call, so
    /// an earlier alias of it is a `Copy` and marks. Getting this wrong is
    /// a soundness bug, not a missed optimization: with the root removed
    /// from `live_out`, `mut b = a` below reads as a last use, nothing marks
    /// the list, and a callee that pushes mutates it where `b` can see.
    #[test]
    fn mut_argument_keeps_its_root_live_into_the_call() {
        // The body ends with an explicit `none` literal rather than the
        // bare assignment `p = p + 1`: assignment expressions type to the
        // *assigned value's* type, not `Type::None` (a pre-existing
        // quirk, unrelated to mutability or liveness — see the
        // `MUTABILITY.md` stage-4 implementation notes), so a body whose
        // tail is a bare reassignment fails to type-check against a
        // declared `: None` return.
        let src = "func bump(mut p: Int): None = {\np = p + 1\nnone\n}\nmut a = 1\nmut b = a\nbump(mut a)\na";
        let typed = lower_numbered(src);
        let mut ids = Vec::new();
        collect_var_ids(&typed, "a", &mut ids);
        // Two occurrences, and neither is the argument: the read in
        // `mut b = a`, and the trailing `a` reading whatever `bump`'s
        // copy-out rebound the name to. `bump(mut a)` contributes none.
        assert_eq!(ids.len(), 2, "expected 2 occurrences of `a`, found {}", ids.len());
        let liveness = analyze_entry(&entry_stmts(typed), &NameSet::new());
        assert_eq!(liveness.ownership(ids[0]), Ownership::Copy,
            "`a` is read again by `bump(mut a)` below, so aliasing it here is not a last use");
        assert_eq!(liveness.ownership(ids[1]), Ownership::Move,
            "the trailing `a` has nothing live after it in this entry");
    }

    /// Corner case 7: `x = x + 1` reads the old value of `x` on its RHS
    /// before defining the new one — that read is correctly a last use of
    /// the old value even though `x` (the name) is about to be redefined.
    #[test]
    fn self_assignment_reads_the_old_value_as_a_last_use() {
        let src = "mut x = 1\nx = x + 1\nx";
        let typed = lower_numbered(src);
        let mut ids = Vec::new();
        collect_var_ids(&typed, "x", &mut ids);
        assert_eq!(ids.len(), 2, "one Var(x) on the assignment's RHS, one trailing");
        let liveness = analyze_entry(&entry_stmts(typed), &NameSet::new());
        assert_eq!(liveness.ownership(ids[0]), Ownership::Move,
            "the RHS read is the last use of x's pre-assignment value");
        assert_eq!(liveness.ownership(ids[1]), Ownership::Move,
            "the trailing x is also a last use — nothing live after this entry");
    }

    /// Corner case 8: a `PlaceAssign`'s root is read-modify-write, never
    /// killed — `o` must still be live (and its later use a genuine last
    /// use, not an artifact of the write appearing to "declare" `o` fresh).
    #[test]
    fn place_assign_root_is_not_killed() {
        let src = "data P(x: Int)\nmut o = P(x=1)\no.x = 5\no.x";
        let typed = lower_numbered(src);
        let mut ids = Vec::new();
        collect_var_ids(&typed, "o", &mut ids);
        // `o.x = 5` (a `PlaceAssign`) names its root as a bare `String`,
        // not a sub-expression — no `Var("o")` node there. The trailing
        // `o.x` (an ordinary read) is a `FieldAccess` whose `target` *is*
        // `Var("o")`: a struct read is always a whole-binding read, sliced
        // down to one field afterward (`compile_expr_multi`'s
        // `FieldAccess`/`enum_name: None` arm, `codegen/mod.rs:1601`). So
        // there is exactly one `Var("o")` node in this program.
        assert_eq!(ids.len(), 1);
        let stmts = entry_stmts(typed);
        let liveness = analyze_entry(&stmts, &NameSet::new());
        assert_eq!(liveness.ownership(ids[0]), Ownership::Move,
            "the trailing `o.x` is o's only Var read, with nothing live after this entry");
        // The real point of this test: `o` must not be listed as dying
        // right after the `o.x = 5` statement, since the trailing `o.x`
        // still needs it — a `PlaceAssign`'s root is read-modify-write,
        // never killed, unlike an ordinary `Assign`.
        let place_assign_stmt = stmts.iter().find(|s| matches!(s.item.kind, TypedExprKind::PlaceAssign { .. }))
            .expect("expected a PlaceAssign statement");
        assert!(!liveness.dead_after(place_assign_stmt.item.id).contains(&"o".to_string()),
            "`o` is read again by the trailing `o.x` — it must not die right after `o.x = 5`");
    }

    /// Corner case 3/4: `catch` inlines its handler body once per
    /// `Error`-providing arm (`TypeChecker::build_catch_arms` clones it) —
    /// each clone is a distinct node with its own id, and the analysis must
    /// treat them independently rather than conflating marks across clones.
    #[test]
    fn catch_handler_clones_are_analyzed_independently() {
        let src = "data E(msg: Str) provides Error\n\
                    func f(x: Int): Int | E = if x < 0 then E(msg=\"bad\") else x\n\
                    f(1) catch 0";
        let typed = lower_numbered(src);
        // No assertion beyond "doesn't panic and produces a result" —
        // the desugaring only clones the *handler*, and this handler is a
        // literal `0`, so there's nothing to conflate; the point is that
        // `analyze_entry` tolerates the cloned-subtree shape at all
        // (distinct ids from `number_nodes`, per the Phase 1 tests, are
        // what make this safe).
        let _ = analyze_entry(&entry_stmts(typed), &NameSet::new());
    }

    /// Cross-entry (REPL) rule: a binding from a prior entry must be kept
    /// alive across an entry that only reads it once, early — this is
    /// exactly `exit_live`'s job (see `analyze_entry`'s doc comment) and a
    /// wrong answer here is a GC-root bug, not just lost precision.
    #[test]
    fn exit_live_keeps_a_prior_entry_binding_alive_past_its_only_use() {
        // A real REPL session shares one `TypeChecker` across entries
        // (`FrogState::eval_with_base` reuses `self.tc`, `state.rs:227`),
        // so `s` is genuinely bound by the time entry 2 is checked — an
        // isolated `TypeChecker::new()` would (correctly) reject `s` as
        // unbound, exactly as it does for a real second REPL entry that
        // ran without a first.
        let mut tc = TypeChecker::new();
        let ast1 = Parser::parse("let s = \"hi\"").expect("parse error");
        tc.check_and_lower_entry(ast1).expect("type error");

        // Entry 2's own code only reads `s` once, at the top, but `s`
        // must still be reported live for the *entire* entry, because
        // `FrogState::eval` keeps rooting every `env` binding (from every
        // prior entry, not just this one) after every entry regardless of
        // whether this entry touches it again (`state.rs:305-335`).
        let ast2 = Parser::parse("print(s)\nlet t = 1\nt").expect("parse error");
        let mut typed = tc.check_and_lower_entry(ast2).expect("type error");
        number_nodes(&mut typed);
        let mut ids = Vec::new();
        collect_var_ids(&typed, "s", &mut ids);
        assert_eq!(ids.len(), 1);

        let mut exit_live = NameSet::new();
        exit_live.insert("s".to_string());
        let liveness = analyze_entry(&entry_stmts(typed.clone()), &exit_live);
        assert_eq!(liveness.ownership(ids[0]), Ownership::Copy,
            "s must stay live for the whole entry because it's in exit_live, \
             even though nothing in this entry reads it again");

        // Contrast: without `s` in `exit_live` (as if it were a genuinely
        // fresh, entry-local binding), the same read is correctly a last
        // use — this isolates that the *only* thing making the first case
        // `Copy` is `exit_live`, not some other property of the source.
        let liveness_local = analyze_entry(&entry_stmts(typed), &NameSet::new());
        assert_eq!(liveness_local.ownership(ids[0]), Ownership::Move);
    }

    /// A `func` body's `exit_live` is its `mut` parameters only. Uses a
    /// body that never reassigns `acc` (self-assignment is a separate,
    /// already-covered case — see `self_assignment_reads_the_old_value_as_a_last_use`,
    /// where reading the *old* value right before rebinding the name
    /// correctly *is* a last use even for a `mut` binding): here nothing
    /// ever kills `acc`, so `exit_live` is the only thing keeping its one
    /// read alive. `x`, an ordinary parameter not in `exit_live`, used
    /// once, is a genuine last use by contrast.
    #[test]
    fn function_body_exit_live_is_its_mut_params_only() {
        let src = "func f(mut acc: Int, x: Int): Int = {\nlet y = acc + x\ny\n}";
        let typed = lower_numbered(src);
        // Dig out the Function node's body.
        let TypedExprKind::Block(stmts) = &typed.item.kind else { panic!("expected top-level Block") };
        let TypedExprKind::Assign { value, .. } = &stmts[0].item.kind else { panic!("expected Assign") };
        let TypedExprKind::Function { body, .. } = &value.item.kind else { panic!("expected Function") };

        let mut acc_ids = Vec::new();
        collect_var_ids(body, "acc", &mut acc_ids);
        let mut x_ids = Vec::new();
        collect_var_ids(body, "x", &mut x_ids);
        assert_eq!(acc_ids.len(), 1);
        assert_eq!(x_ids.len(), 1);

        let mut exit_live = NameSet::new();
        exit_live.insert("acc".to_string());
        let liveness = analyze_body(body, &exit_live);
        assert_eq!(liveness.ownership(acc_ids[0]), Ownership::Copy,
            "acc is `mut` and never reassigned — its copy-out reads the same \
             binding this occurrence reads, so it must not look like a last use");
        assert_eq!(liveness.ownership(x_ids[0]), Ownership::Move,
            "x is a plain parameter, not in exit_live, and used exactly once");
    }
}
