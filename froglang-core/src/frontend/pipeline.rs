//! The passes between type-checking and codegen, in one place, so every way
//! of compiling a program (`FrogState`'s JIT entries, `frog build`, `frog
//! check`, `frog check --copies`) accepts exactly the same programs. They
//! used to be spelled out at each call site, and the native paths had
//! drifted: they skipped `validate_codegen_constraints` and `linear::check`.
//!
//! The caller supplies the borrowing callees (they depend on the codegen
//! backend's `imported_callees`) and gets back the final parameter
//! conventions to `publish`.

use std::collections::HashSet;

use crate::frontend::conventions::{self, Conventions, Signatures};
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
/// 4. parameter conventions, then `insert_aborts`, which needs them (it
///    aborts only parameters the callee owns) and liveness, so ids
/// 5. `number_nodes` again for the nodes `insert_aborts` added, and the
///    conventions again, since they are keyed by node id
/// 6. `linear::check`
///
/// `prior_exit_live` is every name an earlier REPL entry bound (empty for a
/// whole program); see `linear::check`. `borrowers` and `prior` are
/// `conventions::infer`'s.
pub fn lower_checked(
    tc: &mut TypeChecker,
    typed: &mut Spanned<TypedExpr>,
    prior_exit_live: &NameSet,
    borrowers: &HashSet<String>,
    prior: &Signatures,
) -> Result<Conventions, Spanned<TypeError>> {
    tc.validate_codegen_constraints(typed)?;
    tc.monomorphize_generics(typed)?;
    tc.lower_function_values(typed)?;
    tc.desugar_notation(typed)?;
    liveness::number_nodes(typed);
    let before_aborts = conventions::infer(typed, borrowers, prior);
    linear::insert_aborts(typed, tc, &before_aborts);
    liveness::number_nodes(typed);
    let conv = conventions::infer(typed, borrowers, prior);

    let stmts: &[Spanned<TypedExpr>] = match &typed.item.kind {
        TypedExprKind::Block(s) => s.as_slice(),
        _ => std::slice::from_ref(typed),
    };
    linear::check(stmts, prior_exit_live, tc)?;
    Ok(conv)
}
