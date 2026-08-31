//! `Linear` enforcement — `TRAITS.md` Part 7, the prerequisite `DATA.md`
//! Stage 4 (`Sink`) needs. `Linear` itself is just a marker (`typeck::Trait::
//! Linear`, granted via `provides`, exactly like `Error`); this module is
//! the actual check: "a `Copy`-classified read of a `Linear` value is a type
//! error, not a silent clone" and "a `Linear` binding must be consumed on
//! every branch of an `if`/`match` or none."
//!
//! Deliberately isolated from `typeck.rs` and called as its own pass, from
//! `FrogState::eval_with_base`, right after `liveness::number_nodes` and
//! before codegen ever sees the tree — see `TRAITS.md` Part 7, "The check
//! itself": "Emitted at typeck, before codegen ever sees it." That placement
//! is also what keeps this module swappable later: the day `Trait::Linear`
//! becomes a prelude declaration (TRAITS.md Stage 5/6) instead of a built-in
//! enum variant, `check`'s signature and the two rules below don't change —
//! only where `TypeChecker::implements_linear` gets its answer from.
//!
//! No new analysis: both rules ride on `liveness::analyze_body`/
//! `analyze_entry`'s existing `Ownership::Copy`/`Move` classification, per
//! `TRAITS.md` Part 7's own "no new algorithm class" note. `walk` below is a
//! bottom-up fold — mirroring `liveness.rs`'s own `transfer`/`dump_walk_kind`
//! shape, one more entry in that file's five-way repetition of the same
//! `TypedExprKind` match — that both applies rule 1 at every `Var`
//! occurrence and returns the set of `Linear`-typed names it moved, so rule
//! 2 can just compare the two branches' returned sets at each `Conditional`
//! with no second traversal.
//!
//! One consequence of riding on `Ownership` for free: a `Linear` value read
//! outside the two exemptions below (`Ctx::mut_exempt`, `suppress`) is now
//! *only* ever `Move`-classified in a program this pass accepts — every
//! other `Copy`-classified occurrence is rejected before codegen runs. So
//! `codegen`'s clone-on-`Copy` rule (`compile_expr_multi`'s `Var` arm) can
//! never actually fire there, which is exactly `TRAITS.md` Part 7's "a
//! `Linear` value's GC representation can be a genuine identity handle" — it
//! falls out of this check, not a separate codegen change.
//!
//! **Known gap, deliberately not solved here**: at top-level/REPL-entry
//! scope, `FrogState` treats every prior binding as live forever (`env`
//! re-roots all of it after every entry — see `check`'s own `exit_live`
//! comment below) so that a later entry can still reference it. That is
//! exactly the aliasing rule 1 exists to catch, so a top-level `Linear`
//! binding can never pass a `Copy`-classified read at all today — including
//! its own last, and only, read. Sound (it never lets a real alias through),
//! but unusably strict for anything above function-body scope. Fixing it
//! needs a cross-entry consumption story `TRAITS.md` doesn't have yet
//! (does moving a top-level `Linear` value invalidate it for every later
//! entry, and how would that even be reported at the point of the *later*
//! entry's read); revisit once `DATA.md` Stage 4 has a real top-level
//! `Sink` user to design that against. Inside a function body — the
//! realistic `Sink`/`mut self` shape — both rules work as intended; see
//! `tests/test_linear.rs`.

use std::collections::HashSet;

use crate::frontend::liveness::{self, Liveness, NameSet, Ownership};
use crate::frontend::tokens::{Span, Spanned};
use crate::frontend::typed_ast::{Arg, PlaceSeg, TypedExpr, TypedExprKind};
use crate::frontend::typeck::{TypeChecker, TypeError};

/// Read-only state threaded through every `walk` call.
struct Ctx<'a> {
    tc: &'a TypeChecker,
    /// Names exempt from rule 1 for the *whole* traversal — currently just
    /// the enclosing function's own `mut` parameter names (empty for the
    /// entry-level walk, which has no parameters). A `mut` parameter's
    /// final value is always copied out at every return
    /// (`liveness.rs`'s `analyze_body` doc comment), which makes
    /// `exit_live` include it unconditionally — so *every* read of it is
    /// `Copy`-classified by construction, not just an aliasing one. That is
    /// exactly the `mut self` pattern `TRAITS.md` Part 7 sanctions
    /// ("`mut` is already the language's spelling for an externally visible
    /// effect... what makes aliasing through it safe is `Linear`"), so it's
    /// exempted here rather than flagged — copy-in/copy-out is not a second
    /// live reference, it's the one binding threading through sequentially,
    /// and the existing `mut`-exclusivity check (`TypeChecker::lower_call`)
    /// already guarantees no other argument can alias it.
    mut_exempt: HashSet<String>,
}

/// Check every top-level `func`/lambda body, plus this entry's own main
/// body, for `Linear` violations. Mirrors the exact recursive shape
/// `codegen::Codegen::compile_entry`'s Pass 1 uses to find top-level
/// function bodies (only top-level `Assign { value: Function }` statements
/// — nested function literals aren't compiled as their own bodies today
/// either, so there's nothing more to recurse into), and
/// `build_main_body`'s own `exit_live` computation for the entry itself —
/// see the comment at that duplication below.
///
/// `prior_exit_live` is every name a previous entry bound (`FrogState::
/// env_types.keys()` at the call site) — the same set `build_main_body`
/// unions with this entry's own top-level bindings, since `FrogState::eval`
/// re-roots every `env` binding after every entry regardless of whether
/// this entry's code touches it again. See this module's own doc comment
/// for what that means for a top-level `Linear` binding today.
pub fn check(
    stmts: &[Spanned<TypedExpr>],
    prior_exit_live: &NameSet,
    tc: &TypeChecker,
) -> Result<(), Spanned<TypeError>> {
    for stmt in stmts {
        if let TypedExprKind::Assign { value, .. } = &stmt.item.kind {
            if let TypedExprKind::Function { params, body, .. } = &value.item.kind {
                let mut_exempt: HashSet<String> =
                    params.iter().filter(|(_, _, is_mut)| *is_mut).map(|(n, _, _)| n.clone()).collect();
                let body_liveness = liveness::analyze_body(body, &mut_exempt.iter().cloned().collect());
                let ctx = Ctx { tc, mut_exempt };
                walk(body, &body_liveness, &ctx, None)?;
            }
        }
    }

    // Same computation as `codegen::Codegen::build_main_body`'s `exit_live`
    // — kept in sync by hand since this pass runs before codegen exists to
    // ask. If that computation changes, this one needs to change with it.
    let mut exit_live = prior_exit_live.clone();
    for s in stmts {
        if let TypedExprKind::Assign { name, value } = &s.item.kind {
            if !matches!(value.item.kind, TypedExprKind::Function { .. }) {
                exit_live.insert(name.clone());
            }
        }
    }
    let entry_liveness = liveness::analyze_entry(stmts, &exit_live);
    let ctx = Ctx { tc, mut_exempt: HashSet::new() };
    for stmt in stmts {
        walk(stmt, &entry_liveness, &ctx, None)?;
    }
    Ok(())
}

fn alias_error(name: &str, span: Span) -> Spanned<TypeError> {
    Spanned::from(
        TypeError { msg: format!("'{}' is Linear; it can only be moved or passed `mut`, not aliased", name) },
        span,
    )
}

fn branch_join_error(name: &str, span: Span) -> Spanned<TypeError> {
    Spanned::from(
        TypeError {
            msg: format!(
                "'{}' is Linear and is consumed on only one branch of this if/match — \
                 it must be consumed on every branch or none",
                name
            ),
        },
        span,
    )
}

/// Union two moved-name sets. Never fails — kept as a plain function (not
/// `Result`-returning) so every call site composes without extra `?`
/// bookkeeping; `walk`'s own `Result` is only ever about a rule violation,
/// never about this.
fn union(mut a: HashSet<String>, b: HashSet<String>) -> HashSet<String> {
    a.extend(b);
    a
}

fn fold(
    exprs: &[Spanned<TypedExpr>],
    liveness: &Liveness,
    ctx: &Ctx,
    suppress: Option<&str>,
) -> Result<HashSet<String>, Spanned<TypeError>> {
    let mut moved = HashSet::new();
    for e in exprs {
        moved = union(moved, walk(e, liveness, ctx, suppress)?);
    }
    Ok(moved)
}

/// Bottom-up fold over `e`: applies rule 1 (a `Copy`-classified read of a
/// `Linear` name is an error) at every `Var` occurrence it visits, and
/// returns the set of `Linear`-typed names `e` moved (had at least one
/// `Move`-classified read of, outside the two exemptions) — which is
/// exactly what a `Conditional` parent needs to check rule 2 without a
/// second walk. Child order doesn't matter here (unlike `liveness::
/// transfer`, this never computes a live-set), so children are visited in
/// whatever order is simplest to write.
///
/// `suppress`, when `Some(root)`, is `PlaceAssign`'s doing (see that arm):
/// a read of `root` found while walking *its own* `value`/index
/// subexpressions is exempted from rule 1, the same way `Ctx::mut_exempt`
/// exempts a `mut` parameter — see that field's doc comment. `o.x = o.x + 1`
/// reads `o` to compute the new value and immediately writes it back into
/// the same binding — read-modify-write on one name, not a second live
/// reference to it (`liveness.rs`'s own `PlaceAssign` arm makes the
/// identical judgment call: `root` is "never killed" rather than
/// read-then-rebound). Codegen still clones the whole value under the hood
/// to do this (`compile_place_assign` reads the full struct before
/// overwriting one leaf) — a pre-existing codegen characteristic this pass
/// doesn't change, just declines to flag here.
fn walk(
    e: &Spanned<TypedExpr>,
    liveness: &Liveness,
    ctx: &Ctx,
    suppress: Option<&str>,
) -> Result<HashSet<String>, Spanned<TypeError>> {
    match &e.item.kind {
        TypedExprKind::IntLit(_)
        | TypedExprKind::FloatLit(_)
        | TypedExprKind::BoolLit(_)
        | TypedExprKind::StrLit(_)
        | TypedExprKind::NoneLit => Ok(HashSet::new()),

        TypedExprKind::Var(name) => {
            if suppress == Some(name.as_str()) || ctx.mut_exempt.contains(name) {
                return Ok(HashSet::new());
            }
            if ctx.tc.implements_linear(&e.item.ty) {
                match liveness.ownership(e.item.id) {
                    Ownership::Copy => return Err(alias_error(name, e.span)),
                    Ownership::Move => {
                        let mut moved = HashSet::new();
                        moved.insert(name.clone());
                        return Ok(moved);
                    }
                }
            }
            Ok(HashSet::new())
        }

        TypedExprKind::Unary { expr, .. }
        | TypedExprKind::Truthy(expr)
        | TypedExprKind::Coerce(expr)
        | TypedExprKind::Widen { value: expr, .. }
        | TypedExprKind::Narrow { value: expr, .. }
        | TypedExprKind::TypeTag { target: expr, .. }
        | TypedExprKind::IsVariant { target: expr, .. }
        | TypedExprKind::VariantField { target: expr, .. }
        | TypedExprKind::FieldAccess { target: expr, .. } => walk(expr, liveness, ctx, suppress),

        TypedExprKind::Binary { left, right, .. } =>
            Ok(union(walk(left, liveness, ctx, suppress)?, walk(right, liveness, ctx, suppress)?)),

        TypedExprKind::Index { target, index } =>
            Ok(union(walk(target, liveness, ctx, suppress)?, walk(index, liveness, ctx, suppress)?)),

        TypedExprKind::Slice { target, start, end } => {
            let mut moved = walk(target, liveness, ctx, suppress)?;
            if let Some(s) = start { moved = union(moved, walk(s, liveness, ctx, suppress)?); }
            if let Some(en) = end { moved = union(moved, walk(en, liveness, ctx, suppress)?); }
            Ok(moved)
        }

        TypedExprKind::Range { start, end } =>
            Ok(union(walk(start, liveness, ctx, suppress)?, walk(end, liveness, ctx, suppress)?)),

        TypedExprKind::List(elems) => fold(elems, liveness, ctx, suppress),

        TypedExprKind::Block(stmts) => fold(stmts, liveness, ctx, suppress),

        TypedExprKind::StructInit { fields, .. } | TypedExprKind::VariantInit { fields, .. } => {
            let mut moved = HashSet::new();
            for (_, v) in fields { moved = union(moved, walk(v, liveness, ctx, suppress)?); }
            Ok(moved)
        }

        // A function-valued `Assign` is never compiled as an ordinary
        // binding (`func_ids`, not `vars` — see `liveness.rs`'s matching
        // arm), and its body gets its own, separate `check` call from the
        // top level above — so there's nothing to fold in here.
        TypedExprKind::Assign { value, .. } => {
            if matches!(value.item.kind, TypedExprKind::Function { .. }) { Ok(HashSet::new()) } else { walk(value, liveness, ctx, suppress) }
        }
        TypedExprKind::Function { .. } => Ok(HashSet::new()),

        TypedExprKind::PlaceAssign { place, value } => {
            let root = place.root.as_str();
            let mut moved = walk(value, liveness, ctx, Some(root))?;
            for seg in &place.path {
                if let PlaceSeg::Index { index, .. } = seg {
                    moved = union(moved, walk(index, liveness, ctx, Some(root))?);
                }
            }
            Ok(moved)
        }

        // `callable` names a `func_ids` entry, not a binding — never
        // visited, matching `liveness::transfer`'s `Call` arm.
        // A `mut` argument is a place, not a value: its root is written
        // back after the call rather than consumed, so only its `[index]`
        // subexpressions participate here — the same treatment
        // `PlaceAssign`'s own arm gives them.
        TypedExprKind::Call { args, .. } => {
            let mut moved = HashSet::new();
            for a in args.iter().flat_map(Arg::subexprs) {
                moved = union(moved, walk(a, liveness, ctx, suppress)?);
            }
            Ok(moved)
        }

        TypedExprKind::Return(value) => match value {
            Some(v) => walk(v, liveness, ctx, suppress),
            None => Ok(HashSet::new()),
        },

        TypedExprKind::ForLoop { iterable, cond, body, .. }
        | TypedExprKind::Comprehension { iterable, cond, body, .. } => {
            let mut moved = union(walk(iterable, liveness, ctx, suppress)?, walk(body, liveness, ctx, suppress)?);
            if let Some(c) = cond { moved = union(moved, walk(c, liveness, ctx, suppress)?); }
            Ok(moved)
        }

        // Rule 2. Both branches are walked in full regardless of which one
        // errors first below, so a real bug in one arm is never masked by
        // an asymmetric-consumption report about the other.
        TypedExprKind::Conditional { cond, true_branch, false_branch } => {
            let true_moved = walk(true_branch, liveness, ctx, suppress)?;
            let false_moved = match false_branch {
                Some(fb) => walk(fb, liveness, ctx, suppress)?,
                None => HashSet::new(),
            };
            if let Some(name) = true_moved.symmetric_difference(&false_moved).next() {
                return Err(branch_join_error(name, e.span));
            }
            let joined = union(true_moved, false_moved);
            Ok(union(joined, walk(cond, liveness, ctx, suppress)?))
        }
    }
}
