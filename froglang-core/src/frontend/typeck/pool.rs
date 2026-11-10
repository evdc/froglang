//! `Pool<T>` and `Id<T>` — NOGC.md step 5: graph-shaped data addressed by
//! generational indices instead of pointers.
//!
//! A pool is an ordinary value. Its three prelude declarations (installed by
//! `stdlib::install`, see `POOL_PRELUDE`) are plain generic structs:
//!
//! ```text
//! data __PoolSlot<T>(gen: Int, next: Int, value: T)   // next: free-list link, -1 when live
//! data Id<T>(index: Int, gen: Int)              // plain data, Eq, position-independent
//! data Pool<T>(slots: List<__PoolSlot<T>>, head: Int) // head: first free slot, -1 when none
//! ```
//!
//! Nothing here is a runtime object. Every operation is a typeck-level
//! builtin that parses a small source template and lowers it through the
//! ordinary `if`/assign/index/place machinery, the same way `get`/`remove`
//! on `Dict` do. Codegen needs no new node. A generic `func` cannot touch a
//! struct's fields before instantiation, so these cannot be written as
//! prelude functions either.
//!
//! Operations, with `g` a bare `mut` binding of type `Pool<T>`:
//!
//! - `Pool()` — an empty pool (sugar for `Pool(slots=[], head=-1)`).
//! - `g.add(v)` — `Id<T>`. Reuses the head of the free list, or appends.
//! - `g.remove(id)` — frees the slot and bumps its generation. Panics on a
//!   stale `id`.
//! - `g.get(id)` — `T | None`; `None` when `id` is stale.
//! - `g[id]` — a place. Reads and writes panic on a stale `id`.
//!
//! The receiver must be a bare identifier (`g.add(...)`, `g[id]`), the same
//! v1 restriction `remove` has on `Dict`. A pool that sits inside a struct
//! must be bound to its own name first.
//!
//! Known limit: `remove` cannot release the freed slot's `T` (there is no
//! placeholder `T` to leave behind), so the old value stays in the slot until
//! that slot is reused or the pool is dropped.

use super::*;
use crate::frontend::parser::Parser;

/// The prelude declarations `stdlib::install` registers. Order matters:
/// `Pool` names `__PoolSlot`.
pub(crate) const POOL_PRELUDE: [&str; 3] = [
    "data __PoolSlot<T>(gen: Int, next: Int, value: T)",
    "data Id<T>(index: Int, gen: Int)",
    "data Pool<T>(slots: List<__PoolSlot<T>>, head: Int)",
];

pub(crate) const POOL_NAME: &str = "Pool";
pub(crate) const ID_NAME: &str = "Id";
pub(crate) const SLOT_NAME: &str = "__PoolSlot";

/// The argument-binding name every template refers to. The arg is lowered
/// once, bound here, and the template is lowered with this name in scope.
const ARG: &str = "__pool_arg";

/// Is the id valid for the pool? The index must be in range (negative
/// indices would wrap, and the check short-circuits so an out-of-range slot
/// is never indexed); a stale id's generation no longer matches its slot's. `{a}` is the id, `{g}` the pool.
const VALID: &str = "{a}.index >= 0 and {a}.index < len({g}.slots) and {a}.gen == {g}.slots[{a}.index].gen";

/// Each template is written against `{g}` (pool) and `{a}` (the bound arg).
const ADD: &str = "if {g}.head >= 0 then {\n\
    let __pool_i = {g}.head\n\
    {g}.head = {g}.slots[__pool_i].next\n\
    {g}.slots[__pool_i].next = -1\n\
    {g}.slots[__pool_i].value = {a}\n\
    Id(index=__pool_i, gen={g}.slots[__pool_i].gen)\n\
} else {\n\
    push(mut {g}.slots, __PoolSlot(gen=0, next=-1, value={a}))\n\
    Id(index=len({g}.slots) - 1, gen=0)\n\
}";

const REMOVE: &str = "if {valid} then {\n\
    {g}.slots[{a}.index].gen = {g}.slots[{a}.index].gen + 1\n\
    {g}.slots[{a}.index].next = {g}.head\n\
    {g}.head = {a}.index\n\
} else {\n\
    panic(\"stale Id\")\n\
}";

const GET: &str = "if {valid} then { {g}.slots[{a}.index].value } else { none }";
const READ: &str = "if {valid} then { {g}.slots[{a}.index].value } else { panic(\"stale Id\") }";
const CHECKED_INDEX: &str = "if {valid} then { {a}.index } else { panic(\"stale Id\") }";

fn fill(template: &str, root: &str) -> String {
    let valid = VALID.replace("{a}", ARG).replace("{g}", root);
    template
        .replace("{valid}", &valid)
        .replace("{a}", ARG)
        .replace("{g}", root)
}

/// The pool's element type when `ty` is `Pool<T>` (after `lookup`).
pub(crate) fn pool_elem(ty: &Type) -> Option<&Type> {
    match ty {
        Type::Named { name, args } if name == POOL_NAME => args.first(),
        _ => None,
    }
}

fn id_of(elem: &Type) -> Type {
    Type::Named { name: ID_NAME.to_string(), args: vec![elem.clone()] }
}

fn slot_of(elem: &Type) -> Type {
    Type::Named { name: SLOT_NAME.to_string(), args: vec![elem.clone()] }
}

impl TypeChecker {
    /// `Some((root, T))` when `expr` is a bare identifier bound to a `Pool<T>`.
    pub(super) fn pool_root(&mut self, expr: &Expression) -> Option<(String, Type)> {
        let name = expr.get_identifier()?;
        let ty = self.ctx.get(name)?.clone();
        let resolved = self.lookup(&ty);
        let elem = pool_elem(&resolved)?.clone();
        Some((name.to_string(), elem))
    }

    /// Parse a template. Its spans point into the template text, not the
    /// source, so a type error inside one names the template's position.
    fn parse_template(text: &str, span: Span) -> Result<Spanned<Expression>, Spanned<TypeError>> {
        let ast = Parser::parse(text).map_err(|_| Spanned::from(TypeError {
            msg: format!("internal: pool template did not parse: {text}")
        }, span))?;
        Ok(Spanned::from(ast.item, span))
    }

    /// Lower `arg`, bind it to `ARG`, then lower the template with `ARG` in
    /// scope. The result is a block: the arg's assignment, then the template.
    fn bind_and_lower(
        &mut self,
        arg: Spanned<Expression>,
        arg_check: impl FnOnce(&mut Self, &Type, Span) -> Result<(), Spanned<TypeError>>,
        template: &str,
        span: Span,
    ) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let arg_span = arg.span;
        let lowered = self.check_and_lower(arg)?;
        let arg_ty = lowered.item.ty.clone();
        arg_check(self, &arg_ty, arg_span)?;
        let assign = Spanned::from(TypedExpr {
            id: 0, ty: arg_ty.clone(),
            kind: TypedExprKind::Assign { name: ARG.to_string(), value: Box::new(lowered), decl: true },
        }, arg_span);
        let body = Self::parse_template(template, span)?;
        // Errors inside the template point into the template text, which
        // the user never wrote. Report them at the call instead.
        let body = self.with_context_mut(
            std::iter::once((ARG.to_string(), arg_ty, false)),
            |t| t.check_and_lower(body),
        ).map_err(|e| Spanned::from(e.item, span))?;
        let ty = body.item.ty.clone();
        Ok(Spanned::from(TypedExpr { id: 0, ty, kind: TypedExprKind::Block(vec![assign, body]) }, span))
    }

    /// The id argument must be an `Id<T>` for this pool's `T`.
    fn expect_id(&mut self, elem: &Type, got: &Type, span: Span) -> Result<(), Spanned<TypeError>> {
        if self.unify(got, &id_of(elem)) {
            return Ok(());
        }
        Err(Spanned::from(TypeError {
            msg: format!("a Pool<{}> takes an Id<{}>, got {}", self.lookup(elem), self.lookup(elem), self.lookup(got))
        }, span))
    }

    /// `g.add(v)`, `g.remove(id)`, `g.get(id)` on a `Pool<T>` binding `g`.
    pub(super) fn lower_pool_method(
        &mut self,
        root: &str,
        elem: &Type,
        method: &str,
        mut args: Vec<Spanned<Expression>>,
        span: Span,
    ) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        if args.len() != 1 {
            return Err(Spanned::from(TypeError {
                msg: format!("{method} takes exactly one argument, got {}", args.len())
            }, span));
        }
        let arg = args.remove(0);
        let elem = elem.clone();
        match method {
            "add" => {
                let template = fill(ADD, root);
                let elem_for_check = elem.clone();
                let lowered = self.bind_and_lower(arg, move |t, got, sp| {
                    if t.unify(got, &elem_for_check) {
                        Ok(())
                    } else {
                        Err(Spanned::from(TypeError {
                            msg: format!("Pool<{}>.add takes a {}, got {}", t.lookup(&elem_for_check), t.lookup(&elem_for_check), t.lookup(got))
                        }, sp))
                    }
                }, &template, span)?;
                let id = id_of(&elem);
                if !self.unify(&lowered.item.ty, &id) {
                    return Err(Spanned::from(TypeError { msg: "internal: pool add result is not an Id".to_string() }, span));
                }
                Ok(lowered)
            }
            "remove" => {
                let template = fill(REMOVE, root);
                let elem2 = elem.clone();
                self.bind_and_lower(arg, move |t, got, sp| t.expect_id(&elem2, got, sp), &template, span)
            }
            "get" => {
                self.reject_linear_pool_read(root, &elem, span)?;
                let template = fill(GET, root);
                let elem2 = elem.clone();
                self.bind_and_lower(arg, move |t, got, sp| t.expect_id(&elem2, got, sp), &template, span)
            }
            other => Err(Spanned::from(TypeError {
                msg: format!("a Pool has no method '{other}' (it has add, remove and get)")
            }, span)),
        }
    }

    /// Is `method` one of the `Pool` methods? Used to decide whether a call
    /// on a `Pool` receiver is ours to lower.
    pub(super) fn is_pool_method(name: &str) -> bool {
        matches!(name, "add" | "remove" | "get")
    }

    /// `g[id]` as a value: the element, or a panic on a stale id.
    pub(super) fn lower_pool_index(
        &mut self,
        root: &str,
        elem: &Type,
        index: Spanned<Expression>,
        span: Span,
    ) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        self.reject_linear_pool_read(root, elem, span)?;
        self.lower_pool_index_unchecked(root, elem, index, span)
    }

    /// Reading a whole `Linear` element out of a pool copies it while the
    /// pool still owns it, which `Linear` forbids (`frontend::linear`'s
    /// alias rule). Reading a field of it is fine, so `lower_field_access`
    /// calls `lower_pool_index_unchecked` directly.
    pub(super) fn reject_linear_pool_read(&self, root: &str, elem: &Type, span: Span) -> Result<(), Spanned<TypeError>> {
        if self.implements_linear(&self.lookup(elem)) {
            return Err(crate::frontend::linear::alias_error(&format!("{root}[..]"), span));
        }
        Ok(())
    }

    pub(super) fn lower_pool_index_unchecked(
        &mut self,
        root: &str,
        elem: &Type,
        index: Spanned<Expression>,
        span: Span,
    ) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let template = fill(READ, root);
        let elem2 = elem.clone();
        self.bind_and_lower(index, move |t, got, sp| t.expect_id(&elem2, got, sp), &template, span)
    }

    /// `g[id]` as a place's index step. Returns the checked slot index, an
    /// `Int` that panics on a stale id, and the `__PoolSlot<T>` element type the
    /// step addresses. The caller adds the `.slots` and `.value` steps.
    pub(super) fn lower_pool_place_index(
        &mut self,
        root: &str,
        elem: &Type,
        index: Spanned<Expression>,
        span: Span,
    ) -> Result<(TypedExprRef, Type), Spanned<TypeError>> {
        let template = fill(CHECKED_INDEX, root);
        let elem2 = elem.clone();
        let lowered = self.bind_and_lower(index, move |t, got, sp| t.expect_id(&elem2, got, sp), &template, span)?;
        if !self.unify(&lowered.item.ty, &Type::Int) {
            return Err(Spanned::from(TypeError { msg: "internal: pool index is not an Int".to_string() }, span));
        }
        Ok((Box::new(lowered), slot_of(elem)))
    }

    /// `Pool()`: fill in the empty fields the bare constructor leaves out.
    pub(super) fn pool_ctor_args(span: Span) -> Vec<Spanned<Expression>> {
        // `Parser::parse` wraps its output in a `Block`, so unwrap the one
        // statement it holds.
        let ast = Parser::parse("Pool(slots=[], head=-1)").ok().and_then(|b| match b.item {
            Expression::Block(mut stmts) if stmts.len() == 1 => Some(stmts.remove(0)),
            _ => None,
        });
        match ast {
            Some(Spanned { item: Expression::Call(c), .. }) => c.args
                .into_iter()
                .map(|a| Spanned::from(a.item, span))
                .collect(),
            _ => unreachable!("builtin Pool constructor template parses to a call"),
        }
    }
}
