//! The passes between type-checking and codegen, in one place, so every way
//! of compiling a program (`FrogState`'s JIT entries, `frog build`, `frog
//! check`, `frog check --copies`) accepts exactly the same programs. They
//! used to be spelled out at each call site, and the native paths had
//! drifted: they skipped `validate_codegen_constraints` and `linear::check`.
//!
//! Parameter conventions are left to the caller, since what counts as a
//! borrowing callee depends on the codegen backend (`imported_callees`).

use crate::frontend::liveness::{self, NameSet};
use crate::frontend::linear;
use crate::frontend::tokens::Spanned;
use crate::frontend::typeck::{TypeChecker, TypeError};
use crate::frontend::typed_ast::{TypedExpr, TypedExprKind};

/// Run every pass after `check_and_lower`/`check_and_lower_entry` that can
/// reject a program, plus the rewrites codegen relies on, in order:
///
/// 1. `validate_codegen_constraints`
/// 2. `monomorphize_generics`, then `lower_function_values` (needs every
///    type substituted), then `desugar_notation` (whose synthesized `Var`
///    callables `lower_function_values` must not see)
/// 3. `number_nodes`, which must follow both, so cloned and synthesized
///    nodes get ids
/// 4. `insert_aborts` (reads liveness, so needs ids; adds nodes, so
///    `number_nodes` runs again)
/// 5. `linear::check`
///
/// `prior_exit_live` is every name an earlier REPL entry bound (empty for a
/// whole program); see `linear::check`.
pub fn lower_checked(
    tc: &mut TypeChecker,
    typed: &mut Spanned<TypedExpr>,
    prior_exit_live: &NameSet,
) -> Result<(), Spanned<TypeError>> {
    tc.validate_codegen_constraints(typed)?;
    tc.monomorphize_generics(typed)?;
    tc.lower_function_values(typed)?;
    tc.desugar_notation(typed)?;
    liveness::number_nodes(typed);
    linear::insert_aborts(typed, tc);
    liveness::number_nodes(typed);

    let stmts: &[Spanned<TypedExpr>] = match &typed.item.kind {
        TypedExprKind::Block(s) => s.as_slice(),
        _ => std::slice::from_ref(typed),
    };
    linear::check(stmts, prior_exit_live, tc)
}
