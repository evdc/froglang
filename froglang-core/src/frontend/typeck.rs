use std::{collections::HashMap, fmt::Display, vec};

use crate::frontend::{
    expression::{AssignExpr, BinaryExpr, ConditionalExpr, Expression, FieldAccessExpr, ForLoopExpr, FunctionExpr, LiteralExpr, MatchArm, Pattern, UnaryExpr},
    tokens::{Span, Spanned, Token},
};
use crate::frontend::type_expr::TypeExpr;
use crate::frontend::typed_ast::{TypedExpr, TypedExprKind};
use crate::utils::format_vec;

#[derive(Debug)]
pub struct TypeError {
    pub msg: String
}

type TypeResult = Result<Type, Spanned<TypeError>>;

/// Traits constrain type variables. A type must implement a trait to be bound
/// to a TypeVar that carries that bound.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Trait {
    Num,   // Int, Float — arithmetic operators
    Eq,    // Int, Float, Bool, Str — == and !=
    Ord,   // Int, Float, Str — <, >, <=, >=
    /// Marker trait for fallible-function error types (`ERRORS.md`). Unlike
    /// the other three traits, no type implements this structurally — it's
    /// granted per-struct-name by a `provides Error` clause on a `data`
    /// declaration (or implied by the `error X(...)` shorthand), recorded in
    /// `TypeChecker.provides` and consulted only through `type_implements`.
    Error,
}

impl Display for Trait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Trait::Num   => write!(f, "Num"),
            Trait::Eq    => write!(f, "Eq"),
            Trait::Ord   => write!(f, "Ord"),
            Trait::Error => write!(f, "Error"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Type {
    None,
    Int,
    Float,
    Bool,
    Str,
    Function { params: Vec<Type>, result: Box<Type> },
    /// Type variable with optional trait bounds.
    /// Empty bounds = unconstrained (used for lambda parameters).
    TypeVar { name: String, bounds: Vec<Trait> },
    List(Box<Type>),
    /// Sum / union type: a value whose type is one of the variants.
    /// Produced by if-expressions whose branches have incompatible types.
    Union(Vec<Type>),
    /// A `data Name(...)` struct type. Nominal: only the name is compared
    /// (derived `PartialEq`/`unify`'s `t1 == t2` fast path already give
    /// this for free — two structs unify iff their names match). Field
    /// names/types live in `TypeChecker.struct_defs`, not here, so cloning
    /// a `Type::Struct` stays cheap regardless of field count.
    Struct(String),
    /// The bottom type: no value of this type is ever produced. `return`'s
    /// own type (see `TypeChecker::return_types`) — it unifies with
    /// anything and vanishes from any union it appears in (`normalize`,
    /// `is_subtype`), so `if c then return 1 else 2` types as plain `Int`,
    /// not `Never | Int`.
    Never,
}

impl Type {
    /// Normalize a union type: flatten nested unions, deduplicate, and sort
    /// variants into a canonical order. Non-union types are returned unchanged.
    ///
    /// Examples:
    ///   `Int | Str | Int`       → `Int | Str`
    ///   `Str | Int`             → `Int | Str`
    ///   `Int | (Str | Bool)`    → `Bool | Int | Str`
    pub fn normalize(self) -> Type {
        match self {
            Type::Union(variants) => {
                // 1. Recursively normalize and flatten nested unions.
                let mut flat: Vec<Type> = Vec::new();
                for v in variants {
                    match v.normalize() {
                        Type::Union(inner) => flat.extend(inner),
                        other => flat.push(other),
                    }
                }
                // 2. Deduplicate, preserving first occurrence.
                let mut seen: Vec<Type> = Vec::new();
                for ty in flat {
                    if !seen.contains(&ty) {
                        seen.push(ty);
                    }
                }
                // 2b. `Never` never widens a union — it exists to vanish as
                // soon as it stands next to a real value (that's the whole
                // point of a `return` unifying with its surroundings).
                // Kept only when it is the union's sole remaining member, so
                // `Never | Never` still normalizes to `Never` rather than to
                // `None`.
                if seen.len() > 1 {
                    seen.retain(|t| *t != Type::Never);
                }
                // 3. Sort canonically by display string (stable, readable).
                // `sort_by_cached_key` renders each element's key once, not
                // on every comparison the sort makes.
                seen.sort_by_cached_key(|t| t.to_string());
                match seen.len() {
                    0 => Type::None,
                    1 => seen.remove(0),
                    _ => Type::Union(seen),
                }
            }
            other => other,
        }
    }
}

impl Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Type::Int    => write!(f, "Int"),
            Type::Float  => write!(f, "Float"),
            Type::Bool   => write!(f, "Bool"),
            Type::Str    => write!(f, "Str"),
            Type::None   => write!(f, "None"),
            Type::Function { params, result } =>
                write!(f, "{} -> {}", format_vec(params), result),
            Type::TypeVar { name, bounds } => {
                if bounds.is_empty() {
                    write!(f, "~{}", name)
                } else {
                    let bs = bounds.iter()
                        .map(|b| format!("{}", b))
                        .collect::<Vec<_>>()
                        .join(" + ");
                    write!(f, "~{}:{}", name, bs)
                }
            },
            Type::List(inner) => write!(f, "[{}]", inner),
            Type::Union(variants) => {
                let strs: Vec<String> = variants.iter().map(|t| format!("{}", t)).collect();
                write!(f, "{}", strs.join(" | "))
            }
            Type::Struct(name) => write!(f, "{}", name),
            Type::Never => write!(f, "Never"),
        }
    }
}

/// Returns true iff `from` can be implicitly widened to `to`.
/// Directed and acyclic — NOT symmetric. Only safe (non-lossy) promotions.
/// To add Int32/Int64/UInt8 etc, add cases here only.
pub fn widens_to(from: &Type, to: &Type) -> bool {
    matches!(
        (from, to),
        (Type::Int, Type::Float)
        // Future: (Type::Int32, Type::Int64)
        //         (Type::Int32, Type::Float)
        //         (Type::Int64, Type::Float)
    )
}

/// Least upper bound of t1 and t2 in the widening lattice.
/// Returns Some(join) if compatible, None if incompatible (type error at call site).
pub fn numeric_join(t1: &Type, t2: &Type) -> Option<Type> {
    if t1 == t2           { return Some(t1.clone()); }
    if widens_to(t1, t2)  { return Some(t2.clone()); }
    if widens_to(t2, t1)  { return Some(t1.clone()); }
    None
}

/// Ordered field list for one declared struct: `(field_name, field_type)`
/// pairs in declaration order — order matters for construction-argument
/// reordering and for flattened codegen layout (see `struct_fields` in
/// `codegen/mod.rs`).
pub type StructDefs = HashMap<String, Vec<(String, Type)>>;

/// One registered `data Name(common...) is A(...) | B(...) | ...` nominal
/// union (the language's only sum type — see `ERRORS.md`): its common
/// fields (readable on any member without matching) and its members in
/// declaration order (declaration order fixes each member's runtime tag —
/// see `Codegen`/`runtime::gc::FrogVariant`). `ty` is the resolved
/// `Type::Union` this declaration's name stands for — every member is a
/// nominal marker `Type::Struct("Name.Member")`, sorted the same way
/// `Type::normalize()` would sort them, so it can be looked up in
/// `TypeChecker.union_names` in the other direction.
#[derive(Debug, Clone)]
pub struct UnionDef {
    pub common:   Vec<(String, Type)>,
    pub variants: Vec<(String, Vec<(String, Type)>)>,
    pub ty:       Type,
}

impl UnionDef {
    pub fn variant_index(&self, variant: &str) -> Option<usize> {
        self.variants.iter().position(|(n, _)| n == variant)
    }

    /// All-nullary check used by codegen/typeck to decide whether a union
    /// can be represented as a bare tag instead of a boxed pointer.
    pub fn is_unit_enum(&self) -> bool {
        self.common.is_empty() && self.variants.iter().all(|(_, fs)| fs.is_empty())
    }
}

pub type UnionDefs = HashMap<String, UnionDef>;

pub struct TypeChecker {
    // Variable (value level) name -> Type
    ctx: HashMap<String, Type>,
    // TypeVar name -> Type
    substitutions: HashMap<String, Type>,
    next_id: u32,
    /// Registered `data Name(...)` declarations — see `hoist_data_decls`.
    /// Never scoped/popped: once a struct name is registered it stays
    /// visible for the rest of the program, including from later
    /// independent blocks. A known simplification, not a hard limit.
    struct_defs: StructDefs,
    /// Registered `data Name(...) is ...` nominal-union declarations — see
    /// `hoist_data_decls`. Same never-scoped lifetime as `struct_defs`.
    union_defs: UnionDefs,
    /// Reverse of `union_defs`: a union's normalized, sorted member-type
    /// vector (the same shape `Type::normalize()` produces) -> its declared
    /// name. Lets any `Type::Union` value that happens to be a registered
    /// nominal union be resolved back to its `UnionDef` — see
    /// `TypeChecker::resolve_union`.
    union_names: HashMap<Vec<Type>, String>,
    /// variant name -> names of every union declaring it. Used to resolve a
    /// bare (unqualified) variant constructor/pattern: unique -> that
    /// union, ambiguous -> require `Union.Variant` qualification.
    variant_owners: HashMap<String, Vec<String>>,
    /// Stack of enclosing functions' return types, innermost last. `return`
    /// (in `infer`'s `Expression::Return` arm) checks its value against
    /// `.last()` and errors if the stack is empty — "return outside a
    /// function". Pushed/popped around a function body's inference and,
    /// separately, its lowering (`FunctionExpr::infer`, `check_and_lower`'s
    /// `Function` arm, and `check`'s lambda-against-`Type::Function` case) —
    /// `infer` and `check_and_lower` are two independent passes over the
    /// same body, so each needs its own push.
    return_types: Vec<Type>,
    /// Trait names granted to a struct or union-member by name — populated
    /// from a `data`/`error` declaration's trailing `provides Clause`
    /// (`hoist_data_decls`). Keyed by the same qualified name `struct_defs`
    /// uses for a union member (`"Shape.Circle"`) or the bare name for a
    /// plain struct. Consulted only by `type_implements`.
    provides: HashMap<String, Vec<Trait>>,
}

pub struct TypeCheckerCheckpoint {
    ctx: HashMap<String, Type>,
    substitutions: HashMap<String, Type>,
    next_id: u32,
    struct_defs: StructDefs,
    union_defs: UnionDefs,
    union_names: HashMap<Vec<Type>, String>,
    variant_owners: HashMap<String, Vec<String>>,
    return_types: Vec<Type>,
    provides: HashMap<String, Vec<Trait>>,
}

impl TypeChecker {
    pub fn empty() -> Self {
        TypeChecker { ctx: HashMap::new(), substitutions: HashMap::new(), next_id: 0, struct_defs: HashMap::new(), union_defs: HashMap::new(), union_names: HashMap::new(), variant_owners: HashMap::new(), return_types: Vec::new(), provides: HashMap::new() }
    }

    pub fn new() -> Self {
        TypeChecker { ctx: TypeChecker::default_context(), substitutions: HashMap::new(), next_id: 0, struct_defs: HashMap::new(), union_defs: HashMap::new(), union_names: HashMap::new(), variant_owners: HashMap::new(), return_types: Vec::new(), provides: HashMap::new() }
    }

    /// Check whether a concrete type implements the given trait. Only makes
    /// sense for non-TypeVar types; TypeVar-TypeVar unification is handled
    /// separately so this is never called on a TypeVar.
    fn type_implements(&self, ty: &Type, tr: &Trait) -> bool {
        match ty {
            // A union satisfies a trait iff every variant does.
            Type::Union(variants) => variants.iter().all(|v| self.type_implements(v, tr)),
            // Structs get structural `==`/`!=` (desugared into a per-field
            // conjunction at lowering time — see `TypeChecker::desugar_struct_eq`
            // in `check_and_lower`'s `Binary` arm), so they satisfy `Eq`. A
            // struct with a field type that itself doesn't implement `Eq` (e.g.
            // a `List` field — lists don't support `==` at all currently) will
            // fail type-checking when the desugared per-field comparison is
            // itself inferred, which is the correct place for that error to
            // surface, not here.
            Type::Struct(_) if *tr == Trait::Eq => true,
            // `Error` is granted, not structural — see `provides`.
            Type::Struct(name) if *tr == Trait::Error => {
                self.provides.get(name).map(|ts| ts.contains(tr)).unwrap_or(false)
            },
            _ => match tr {
                Trait::Num   => matches!(ty, Type::Int | Type::Float),
                Trait::Eq    => matches!(ty, Type::Int | Type::Float | Type::Bool | Type::Str),
                Trait::Ord   => matches!(ty, Type::Int | Type::Float | Type::Str),
                Trait::Error => false,
            }
        }
    }

    /// A statement-position value may not silently discard a possible
    /// `Error` — the must-handle rule (`ERRORS.md`). Applies to every
    /// non-tail `Block` statement and every `for`-loop body; a tail
    /// position is exempt because its value propagates to the caller,
    /// where the same value is (recursively) subject to this same rule.
    /// `let x = ...`/`obj.f = ...` are exempt too: binding a name isn't
    /// discarding — `x` still carries the union type onward to wherever it
    /// (recursively) needs to be handled, exactly like a tail value does.
    fn check_must_handle(&self, stmt: &Spanned<TypedExpr>) -> Result<(), Spanned<TypeError>> {
        if matches!(stmt.item.kind, TypedExprKind::Assign { .. } | TypedExprKind::FieldAssign { .. }) {
            return Ok(());
        }
        let ty = &stmt.item.ty;
        let span = stmt.span;
        let offender = match ty {
            Type::Union(members) => members.iter().find(|m| self.type_implements(m, &Trait::Error)),
            other if self.type_implements(other, &Trait::Error) => Some(other),
            _ => None,
        };
        if let Some(m) = offender {
            return Err(Spanned::from(TypeError {
                msg: format!(
                    "unhandled error: this statement's value has type '{}', which may be '{}' \
                     (declared to provide Error) — bind it, match it, or otherwise handle it \
                     instead of discarding it",
                    ty, m
                )
            }, span));
        }
        Ok(())
    }

    /// Field layout for every registered struct, in declaration order.
    /// Threaded into `Codegen::compile_entry` so codegen can flatten
    /// struct-typed values into their leaf fields — see `struct_fields`
    /// in `codegen/mod.rs`.
    pub fn struct_defs(&self) -> &StructDefs {
        &self.struct_defs
    }

    /// Layout for every registered nominal union, in declaration order.
    /// Threaded into `Codegen` alongside `struct_defs`.
    pub fn union_defs(&self) -> &UnionDefs {
        &self.union_defs
    }

    /// Fresh unconstrained type variable (used for unannotated lambda parameters).
    pub fn fresh_var(&mut self) -> Type {
        let v = Type::TypeVar { name: format!("t{}", self.next_id), bounds: vec![] };
        self.next_id += 1;
        v
    }

    /// Fresh type variable with trait bounds (used for polymorphic operators).
    fn fresh_bounded_var(&mut self, bounds: Vec<Trait>) -> Type {
        let v = Type::TypeVar { name: format!("t{}", self.next_id), bounds };
        self.next_id += 1;
        v
    }

    pub fn checkpoint(&self) -> TypeCheckerCheckpoint {
        TypeCheckerCheckpoint {
            ctx: self.ctx.clone(),
            substitutions: self.substitutions.clone(),
            next_id: self.next_id,
            struct_defs: self.struct_defs.clone(),
            union_defs: self.union_defs.clone(),
            union_names: self.union_names.clone(),
            variant_owners: self.variant_owners.clone(),
            return_types: self.return_types.clone(),
            provides: self.provides.clone(),
        }
    }

    pub fn restore(&mut self, cp: TypeCheckerCheckpoint) {
        self.ctx = cp.ctx;
        self.substitutions = cp.substitutions;
        self.next_id = cp.next_id;
        self.struct_defs = cp.struct_defs;
        self.union_defs = cp.union_defs;
        self.union_names = cp.union_names;
        self.variant_owners = cp.variant_owners;
        self.return_types = cp.return_types;
        self.provides = cp.provides;
    }

    pub fn add_ctx(mut self, ctx: impl Iterator<Item=(String, Type)>) -> Self {
        for (k, v) in ctx { self.ctx.insert(k, v); }
        self
    }

    pub fn context(self) -> HashMap<String, Type> {
        self.ctx
    }

    fn default_context() -> HashMap<String, Type> {
        let mut ctx = HashMap::new();
        ctx.insert("print".to_string(), Type::Function {
            params: vec![Type::Str],
            result: Box::new(Type::None),
        });
        ctx.insert("gc_dump".to_string(), Type::Function {
            params: vec![],
            result: Box::new(Type::None),
        });
        ctx
    }

    pub fn infer(&mut self, expr: &Spanned<Expression>) -> TypeResult {
        match &expr.item {
            Expression::Literal(inner)    => inner.infer(self, expr.span),
            Expression::Unary(inner)      => inner.infer(self, expr.span),
            Expression::Binary(inner)     => inner.infer(self, expr.span),
            Expression::Conditional(inner)=> inner.infer(self, expr.span),
            Expression::Assign(inner)     => inner.infer(self, expr.span),
            Expression::Function(inner)   => inner.infer(self, expr.span),
            Expression::Call(inner)       => self.infer_call(&inner.callable, &inner.args),
            // A block is its own lexical scope: bindings made by a `let`
            // inside it (directly, or via a nested block/conditional branch)
            // must not leak to whatever follows the block. Without this,
            // codegen can be asked to reference an SSA value that only
            // exists on one control-flow path (e.g. one arm of an `if`),
            // which is invalid IR, not just a stale-name bug.
            Expression::Block(stmts) => {
                self.hoist_data_decls(stmts)?;
                self.with_context(std::iter::empty(), |t| {
                    let mut last = Type::None;
                    for stmt in stmts {
                        if matches!(stmt.item, Expression::DataDecl(_)) { continue; }
                        last = t.infer(stmt)?;
                    }
                    Ok(last)
                })
            },
            Expression::Tuple(elems) => {
                if elems.is_empty() {
                    return Ok(Type::List(Box::new(self.fresh_var())));
                }
                let first_ty = self.infer(&elems[0])?;
                for elem in &elems[1..] {
                    let elem_ty = self.infer(elem)?;
                    if !self.unify(&first_ty, &elem_ty) {
                        return Err(Spanned::from(TypeError {
                            msg: format!("List elements must have the same type, got {} and {}", first_ty, elem_ty)
                        }, elem.span));
                    }
                }
                Ok(Type::List(Box::new(self.lookup(&first_ty))))
            },
            Expression::Annotated(inner) => {
                let annotated_ty = self.resolve_type_expr(&inner.ty)?;
                self.check(&inner.expr, &annotated_ty)
            }

            Expression::Index(idx) => {
                let target_ty = self.infer(&idx.target)?;
                let resolved_target = self.lookup(&target_ty);
                let elem_ty = match &resolved_target {
                    Type::List(inner) => (**inner).clone(),
                    Type::TypeVar { .. } => {
                        let elem = self.fresh_var();
                        if !self.unify(&target_ty, &Type::List(Box::new(elem.clone()))) {
                            return Err(Spanned::from(TypeError {
                                msg: format!("Can't index into {}", resolved_target)
                            }, idx.target.span));
                        }
                        elem
                    },
                    _ => return Err(Spanned::from(TypeError {
                        msg: format!("Can't index into {}, expected a List", resolved_target)
                    }, idx.target.span)),
                };

                let index_ty = self.infer(&idx.index)?;
                if !self.unify(&index_ty, &Type::Int) {
                    return Err(Spanned::from(TypeError {
                        msg: format!("List index must be Int, got {}", self.lookup(&index_ty))
                    }, idx.index.span));
                }

                Ok(self.lookup(&elem_ty))
            }

            Expression::Slice(s) => {
                let target_ty = self.infer(&s.target)?;
                let resolved_target = self.lookup(&target_ty);
                let list_ty = match &resolved_target {
                    Type::List(_) => resolved_target.clone(),
                    Type::TypeVar { .. } => {
                        let elem = self.fresh_var();
                        let list_ty = Type::List(Box::new(elem));
                        if !self.unify(&target_ty, &list_ty) {
                            return Err(Spanned::from(TypeError {
                                msg: format!("Can't slice {}", resolved_target)
                            }, s.target.span));
                        }
                        list_ty
                    },
                    _ => return Err(Spanned::from(TypeError {
                        msg: format!("Can't slice {}, expected a List", resolved_target)
                    }, s.target.span)),
                };

                for bound in [&s.start, &s.end].into_iter().flatten() {
                    let bound_ty = self.infer(bound)?;
                    if !self.unify(&bound_ty, &Type::Int) {
                        return Err(Spanned::from(TypeError {
                            msg: format!("Slice bound must be Int, got {}", self.lookup(&bound_ty))
                        }, bound.span));
                    }
                }

                Ok(self.lookup(&list_ty))
            }

            Expression::Range(r) => {
                let start_ty = self.infer(&r.start)?;
                if !self.unify(&start_ty, &Type::Int) {
                    return Err(Spanned::from(TypeError {
                        msg: format!("Range start must be Int, got {}", self.lookup(&start_ty))
                    }, r.start.span));
                }
                let end_ty = self.infer(&r.end)?;
                if !self.unify(&end_ty, &Type::Int) {
                    return Err(Spanned::from(TypeError {
                        msg: format!("Range end must be Int, got {}", self.lookup(&end_ty))
                    }, r.end.span));
                }
                Ok(Type::List(Box::new(Type::Int)))
            }

            Expression::ForLoop(fl) => {
                self.infer_for_loop(fl)?;
                Ok(Type::None)
            }

            Expression::Comprehension(inner) => {
                let fl = match &inner.item {
                    Expression::ForLoop(fl) => fl,
                    _ => unreachable!("Comprehension always wraps a ForLoop — see Grammar::tuple"),
                };
                let body_ty = self.infer_for_loop(fl)?;
                Ok(Type::List(Box::new(body_ty)))
            }

            // Already registered by `hoist_data_decls` (called from the
            // enclosing `Block`) by the time this is ever reached directly.
            Expression::DataDecl(_) => Ok(Type::None),

            Expression::FieldAccess(fa) => self.infer_field_access(fa, expr.span),

            Expression::Match(m) => self.infer_match(&m.subject, &m.arms, &m.default, expr.span),

            Expression::IsPattern(ip) => self.infer_is_pattern(ip, expr.span),

            Expression::Import(_) => unreachable!(
                "Expression::Import must be resolved and stripped by frontend::modules before typeck ever sees it"
            ),

            Expression::Return(value) => self.infer_return(value, expr.span),
        }
    }

    fn infer_field_access(&mut self, fa: &FieldAccessExpr, span: Span) -> TypeResult {
        let target_ty = self.infer(&fa.target)?;
        let resolved = self.lookup(&target_ty);
        if let Type::Struct(sname) = &resolved {
            let field_defs = self.struct_defs.get(sname).cloned().unwrap_or_default();
            return field_defs.iter().find(|(n, _)| n == &fa.field).map(|(_, t)| t.clone())
                .ok_or_else(|| Spanned::from(TypeError {
                    msg: format!("Struct {} has no field '{}'", sname, fa.field)
                }, span));
        }
        // Only common fields (declared on the union head) are readable
        // without matching — a variant-only field requires a `match`/
        // `is` to narrow the value first (see DESIGN.md's `shape.r`
        // example).
        if let Some((ename, def)) = self.resolve_union(&resolved) {
            let ename = ename.to_string();
            let def = def.clone();
            if let Some((_, t)) = def.common.iter().find(|(n, _)| n == &fa.field) {
                return Ok(t.clone());
            }
            return if def.variants.iter().any(|(_, fs)| fs.iter().any(|(n, _)| n == &fa.field)) {
                Err(Spanned::from(TypeError {
                    msg: format!("'{}' is a variant-specific field of {} — match on it to access it", fa.field, ename)
                }, span))
            } else {
                Err(Spanned::from(TypeError {
                    msg: format!("{} has no field '{}'", ename, fa.field)
                }, span))
            };
        }
        Err(Spanned::from(TypeError {
            msg: format!("Can't access field '{}' on {}, expected a struct or union", fa.field, resolved)
        }, fa.target.span))
    }

    /// If `ty` is a `Type::Union` matching a registered nominal union
    /// (`data X is A | B`), return its declared name and definition. `None`
    /// for any other type, including an anonymous structural union with no
    /// matching declaration.
    fn resolve_union(&self, ty: &Type) -> Option<(&str, &UnionDef)> {
        if let Type::Union(members) = ty {
            if let Some(name) = self.union_names.get(members) {
                return self.union_defs.get(name).map(|d| (name.as_str(), d));
            }
        }
        None
    }

    /// Resolve a bare type *name* — a builtin, a registered struct, or a
    /// registered nominal union — to a `Type`. Shared by `resolve_type_expr`
    /// (a full `TypeExpr::Name`) and by pattern resolution (`is <Name>` on
    /// an anonymous union — see `infer_is_pattern`/`infer_match`), which
    /// only ever has a bare `String` to work with, not a parsed `TypeExpr`.
    fn resolve_type_name(&self, name: &str) -> Option<Type> {
        match name {
            "Int"   => Some(Type::Int),
            "Float" => Some(Type::Float),
            "Bool"  => Some(Type::Bool),
            "Str"   => Some(Type::Str),
            "None"  => Some(Type::None),
            _ if self.struct_defs.contains_key(name) => Some(Type::Struct(name.to_string())),
            _ if self.union_defs.contains_key(name)  => Some(self.union_defs[name].ty.clone()),
            _ => None,
        }
    }

    /// If `lowered`'s type doesn't already match `target`, and `target` is
    /// an *anonymous* `Type::Union` (see `resolve_union`) that `lowered`'s
    /// type is a flat member of, wrap `lowered` in an explicit `Widen` node
    /// so codegen actually boxes it into the union's runtime
    /// representation. Subtyping is otherwise invisible at runtime —
    /// `check`/`unify` accept a narrower value for a wider expected type,
    /// but nothing coerces it — so every lowering site that places a
    /// `check`-validated value into a `Type::Union`-typed slot (an
    /// annotated `let`, a function's declared return type, a call
    /// argument, a struct/variant field) must route the lowered value
    /// through here. A no-op for any other case (already the right type,
    /// or `target` isn't a union — those are handled elsewhere, e.g. the
    /// ambient `Int -> Float` widening).
    ///
    /// `Str`/`List` members and widening a value that's *already*
    /// union-typed (nominal or anonymous) into a different union aren't
    /// supported yet — rejected with a clear error rather than silently
    /// generating an incorrect box. Scalars (`Int`/`Float`/`Bool`), plain
    /// structs, and `None` are the supported member types.
    fn lower_widen(&self, lowered: Spanned<TypedExpr>, target: &Type) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let from = lowered.item.ty.clone();
        if from == *target || from == Type::Never {
            return Ok(lowered);
        }
        let Type::Union(members) = target else { return Ok(lowered); };
        let Some(tag) = members.iter().position(|m| *m == from) else { return Ok(lowered); };
        let span = lowered.span;
        if matches!(from, Type::Union(_)) {
            return Err(Spanned::from(TypeError {
                msg: format!("widening a union-typed value ({}) into a different union ({}) is not yet supported", from, target)
            }, span));
        }
        if matches!(from, Type::Str | Type::List(_)) {
            return Err(Spanned::from(TypeError {
                msg: format!("{} is not yet supported as a member of a union that needs boxing", from)
            }, span));
        }
        Ok(Spanned::from(
            TypedExpr { ty: target.clone(), kind: TypedExprKind::Widen { value: Box::new(lowered), tag: tag as u32 } },
            span,
        ))
    }

    /// Register every `data Name(field: Type, ...)` declaration found
    /// directly in `stmts` into `self.struct_defs`, in three phases so
    /// declarations can reference each other regardless of source order:
    /// (1) register every name, so forward references resolve; (2) resolve
    /// every field list to concrete `Type`s; (3) check the resulting
    /// field-type graph for direct/transitive self-reference, which would
    /// make an unboxed struct infinite size.
    fn hoist_data_decls(&mut self, stmts: &[Spanned<Expression>]) -> Result<(), Spanned<TypeError>> {
        for s in stmts {
            if let Expression::DataDecl(d) = &s.item {
                if self.struct_defs.contains_key(&d.name) || self.union_defs.contains_key(&d.name) {
                    return Err(Spanned::from(TypeError {
                        msg: format!("'{}' is already declared", d.name)
                    }, s.span));
                }
                if d.variants.is_empty() {
                    self.struct_defs.insert(d.name.clone(), Vec::new());
                } else {
                    for v in &d.variants {
                        self.variant_owners.entry(v.name.clone()).or_default().push(d.name.clone());
                    }
                    // Only the member *names* are needed to build the
                    // nominal marker types below — field types aren't
                    // resolved until the second pass, which is what lets a
                    // union reference itself recursively (`data Tree is
                    // Leaf | Node(l: Tree, r: Tree)`): by the time a
                    // member's fields are resolved, `Tree`'s own `Type`
                    // (and its `union_names` reverse entry) already exist.
                    let mut member_types: Vec<Type> = d.variants.iter()
                        .map(|v| Type::Struct(format!("{}.{}", d.name, v.name)))
                        .collect();
                    member_types.sort_by_cached_key(|t| t.to_string());
                    let ty = Type::Union(member_types.clone());
                    self.union_names.insert(member_types, d.name.clone());
                    self.union_defs.insert(d.name.clone(), UnionDef { common: Vec::new(), variants: Vec::new(), ty });
                }
            }
        }
        for s in stmts {
            if let Expression::DataDecl(d) = &s.item {
                let mut fields = Vec::with_capacity(d.fields.len());
                for p in &d.fields {
                    let ann = p.ty.as_ref().expect("data-decl fields always carry a type annotation — see Grammar::data_decl");
                    let ty = self.resolve_type_expr(ann)?;
                    fields.push((p.name.clone(), ty));
                }
                if d.variants.is_empty() {
                    self.struct_defs.insert(d.name.clone(), fields);
                } else {
                    let mut seen_variants: HashMap<String, Span> = HashMap::new();
                    let mut variants = Vec::with_capacity(d.variants.len());
                    for v in &d.variants {
                        if seen_variants.contains_key(&v.name) {
                            return Err(Spanned::from(TypeError {
                                msg: format!("Variant '{}' is declared twice in {}", v.name, d.name)
                            }, s.span));
                        }
                        seen_variants.insert(v.name.clone(), s.span);
                        let mut vfields = Vec::with_capacity(v.fields.len());
                        for p in &v.fields {
                            let ann = p.ty.as_ref().expect("variant fields always carry a type annotation — see Grammar::data_decl");
                            let ty = self.resolve_type_expr(ann)?;
                            vfields.push((p.name.clone(), ty));
                        }
                        variants.push((v.name.clone(), vfields));
                    }
                    let def = self.union_defs.get_mut(&d.name).expect("registered in the first pass, above");
                    def.common = fields;
                    def.variants = variants;
                }

                if !d.provides.is_empty() {
                    let mut traits = Vec::with_capacity(d.provides.len());
                    for name in &d.provides {
                        match name.as_str() {
                            "Error" => traits.push(Trait::Error),
                            other => return Err(Spanned::from(TypeError {
                                msg: format!("Unknown trait '{}' in provides clause", other)
                            }, s.span)),
                        }
                    }
                    // A union's `provides` grants every variant the trait
                    // (the doc's "a union satisfies a trait iff every
                    // member does" rule then makes the alias itself
                    // satisfy it too, for free, via `type_implements`) —
                    // there's no `Type::Struct` for the alias name itself
                    // to key `provides` off of.
                    if d.variants.is_empty() {
                        self.provides.entry(d.name.clone()).or_default().extend(traits);
                    } else {
                        for v in &d.variants {
                            self.provides.entry(format!("{}.{}", d.name, v.name)).or_default().extend(traits.clone());
                        }
                    }
                }
            }
        }
        for s in stmts {
            if let Expression::DataDecl(d) = &s.item {
                self.check_struct_acyclic(&d.name, &mut Vec::new(), s.span)?;
            }
        }
        Ok(())
    }

    /// DFS over the struct field-type graph, following only direct
    /// `Type::Struct` fields (a `List(Struct(_))` field is fine — a list is
    /// a heap pointer, not inline storage, so it can't create an
    /// infinite-size cycle the way a direct field can). A `Type::Union`
    /// field is always fine too — a nominal union's member is a single
    /// boxed pointer (see `runtime::gc::FrogVariant`), so it can't create
    /// an infinite-size cycle either; this is what makes a recursive union
    /// (e.g. a binary tree, `data Tree is Leaf | Node(l: Tree, r: Tree)`)
    /// legal even though a recursive struct is not.
    fn check_struct_acyclic(&self, name: &str, path: &mut Vec<String>, span: Span) -> Result<(), Spanned<TypeError>> {
        if path.iter().any(|n| n == name) {
            path.push(name.to_string());
            return Err(Spanned::from(TypeError {
                msg: format!("Struct type contains itself: {}", path.join(" -> "))
            }, span));
        }
        path.push(name.to_string());
        if let Some(fields) = self.struct_defs.get(name).cloned() {
            for (_, fty) in &fields {
                if let Type::Struct(inner) = fty {
                    self.check_struct_acyclic(inner, path, span)?;
                }
            }
        }
        path.pop();
        Ok(())
    }

    /// Type-check a `Kind(field=value, ...)` construction call's arguments
    /// against `field_defs` — shared by struct construction and enum
    /// variant construction (`kind_name` is only used for error text, e.g.
    /// `"Person"` or `"Shape.Circle"`).
    fn check_record_args(&mut self, kind_name: &str, field_defs: &[(String, Type)], args: &[Spanned<Expression>], span: Span) -> Result<(), Spanned<TypeError>> {
        let mut seen: HashMap<String, Span> = HashMap::new();
        for arg in args {
            let (fname, value_expr) = match &arg.item {
                Expression::Assign(a) => {
                    let fname = a.target.item.get_identifier()
                        .ok_or_else(|| Spanned::from(TypeError {
                            msg: "Field name must be a plain identifier".to_string()
                        }, arg.span))?
                        .to_string();
                    (fname, &*a.value)
                },
                _ => return Err(Spanned::from(TypeError {
                    msg: format!("Construction requires named fields, e.g. {}(field=value)", kind_name)
                }, arg.span)),
            };
            if let Some(_prev) = seen.get(&fname) {
                return Err(Spanned::from(TypeError {
                    msg: format!("Duplicate field '{}' in construction of {}", fname, kind_name)
                }, arg.span));
            }
            let field_ty = field_defs.iter().find(|(n, _)| n == &fname)
                .map(|(_, t)| t.clone())
                .ok_or_else(|| Spanned::from(TypeError {
                    msg: format!("{} has no field '{}'", kind_name, fname)
                }, arg.span))?;
            let value_ty = self.infer(value_expr)?;
            let resolved_value_ty = self.lookup(&value_ty);
            let resolved_field_ty = self.lookup(&field_ty);
            if !(widens_to(&resolved_value_ty, &resolved_field_ty) || self.unify(&value_ty, &field_ty)) {
                return Err(Spanned::from(TypeError {
                    msg: format!("Field '{}' of {} expects {}, got {}", fname, kind_name, resolved_field_ty, resolved_value_ty)
                }, value_expr.span));
            }
            seen.insert(fname, arg.span);
        }
        for (fname, _) in field_defs {
            if !seen.contains_key(fname) {
                return Err(Spanned::from(TypeError {
                    msg: format!("Missing field '{}' in construction of {}", fname, kind_name)
                }, span));
            }
        }
        Ok(())
    }

    /// Type-check a `Name(field=value, ...)` struct construction call —
    /// intercepted in `infer_call` before the generic function-call path.
    fn infer_struct_init(&mut self, name: &str, field_defs: &[(String, Type)], args: &[Spanned<Expression>], span: Span) -> TypeResult {
        self.check_record_args(name, field_defs, args, span)?;
        Ok(Type::Struct(name.to_string()))
    }

    /// Type-check a `Variant(field=value, ...)` enum variant construction
    /// call (bare or `Enum.Variant`-qualified) — intercepted in
    /// `infer_call` alongside struct construction. `field_defs` for the
    /// call is the enum's common fields followed by the variant's own.
    fn infer_variant_init(&mut self, enum_name: &str, variant: &str, args: &[Spanned<Expression>], span: Span) -> TypeResult {
        let def = self.union_defs.get(enum_name).cloned()
            .expect("enum_name resolved via variant_owners/union_defs, must be registered");
        let variant_fields = def.variants.iter().find(|(n, _)| n == variant)
            .map(|(_, fs)| fs.clone())
            .expect("variant resolved via variant_owners/union_defs, must be registered");
        let mut field_defs = def.common.clone();
        field_defs.extend(variant_fields);
        let kind_name = format!("{}.{}", enum_name, variant);
        self.check_record_args(&kind_name, &field_defs, args, span)?;
        Ok(def.ty.clone())
    }

    /// Resolve a call's callee to `(enum_name, variant_name)` if it names
    /// an enum variant constructor — either bare (`Circle(...)`, valid
    /// only if `Circle` is declared by exactly one enum) or qualified
    /// (`Shape.Circle(...)`). `Ok(None)` means "not a variant constructor
    /// at all" (an ordinary call, or struct construction — handled by the
    /// caller before/after this).
    fn resolve_variant_callee(&self, callable: &Expression) -> Result<Option<(String, String)>, String> {
        match callable {
            Expression::Literal(LiteralExpr { token: Token::Identifier(name) }) => {
                match self.variant_owners.get(name) {
                    None => Ok(None),
                    Some(owners) if owners.len() == 1 => Ok(Some((owners[0].clone(), name.clone()))),
                    Some(owners) => Err(format!(
                        "Variant '{}' is ambiguous between {} — qualify it, e.g. {}.{}(...)",
                        name, owners.join(", "), owners[0], name
                    )),
                }
            }
            Expression::FieldAccess(fa) => {
                if let Some(enum_name) = fa.target.item.get_identifier() {
                    if let Some(def) = self.union_defs.get(enum_name) {
                        return if def.variant_index(&fa.field).is_some() {
                            Ok(Some((enum_name.to_string(), fa.field.clone())))
                        } else {
                            Err(format!("{} has no variant '{}'", enum_name, fa.field))
                        };
                    }
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// Infer a bare identifier that isn't a bound variable as a nullary
    /// (parenthesis-free) enum variant construction, e.g. `Red` for
    /// `data Color is Red | Green | Blue`. Only legal when the variant (and
    /// its enum's common fields) declare no fields at all — anything with
    /// fields must be constructed with call syntax, even if the variant
    /// itself is empty (to supply the common fields).
    fn infer_bare_variant(&self, name: &str, span: Span) -> TypeResult {
        match self.variant_owners.get(name) {
            None => Err(Spanned::from(TypeError { msg: format!("Unbound variable {}", name) }, span)),
            Some(owners) if owners.len() > 1 => Err(Spanned::from(TypeError {
                msg: format!("Variant '{}' is ambiguous between {} — qualify it, e.g. {}.{}(...)", name, owners.join(", "), owners[0], name)
            }, span)),
            Some(owners) => {
                let enum_name = &owners[0];
                let def = self.union_defs.get(enum_name).expect("registered");
                let variant_fields = def.variants.iter().find(|(n, _)| n == name)
                    .map(|(_, fs)| fs).expect("registered");
                if !def.common.is_empty() || !variant_fields.is_empty() {
                    return Err(Spanned::from(TypeError {
                        msg: format!("Variant '{}' has fields and must be constructed with {}(...)", name, name)
                    }, span));
                }
                Ok(def.ty.clone())
            }
        }
    }

    /// Validate a match/`is` pattern against `enum_name`'s definition:
    /// the optional qualifier (if present) must name the same enum, the
    /// variant must exist, and — if any binds are given at all — their
    /// count must exactly match the variant's field arity. Returns the
    /// variant's declaration index (its runtime tag).
    fn check_pattern(&self, pattern: &Pattern, enum_name: &str, def: &UnionDef, span: Span) -> Result<usize, Spanned<TypeError>> {
        if let Some(path) = &pattern.path {
            if path != enum_name {
                return Err(Spanned::from(TypeError {
                    msg: format!("Pattern '{}.{}' does not match subject type {}", path, pattern.variant, enum_name)
                }, span));
            }
        }
        let idx = def.variant_index(&pattern.variant).ok_or_else(|| Spanned::from(TypeError {
            msg: format!("{} has no variant '{}'", enum_name, pattern.variant)
        }, span))?;
        let arity = def.variants[idx].1.len();
        if !pattern.binds.is_empty() && pattern.binds.len() != arity {
            return Err(Spanned::from(TypeError {
                msg: format!("Pattern for variant '{}' expects {} binding(s), got {}", pattern.variant, arity, pattern.binds.len())
            }, span));
        }
        Ok(idx)
    }

    /// `subject is Pattern` used as an ordinary `Bool`-valued expression
    /// (not the entire condition of an `if` — that case is intercepted
    /// earlier, in `ConditionalExpr::infer`, and desugars through
    /// `infer_match`/`lower_match` instead so the pattern's binds are
    /// actually reachable). Binds are rejected here since there is no
    /// `then`-scope for them to enter.
    fn infer_is_pattern(&mut self, ip: &crate::frontend::expression::IsPatternExpr, span: Span) -> TypeResult {
        let subject_ty = self.infer(&ip.subject)?;
        let resolved = self.lookup(&subject_ty);
        let enum_name = match self.resolve_union(&resolved) {
            Some((name, _)) => name.to_string(),
            None => {
                let Type::Union(members) = &resolved else {
                    return Err(Spanned::from(TypeError {
                        msg: format!("Can only use 'is' on a union value, got {}", resolved)
                    }, ip.subject.span));
                };
                self.check_type_pattern(&ip.pattern, members, span)?;
                if !ip.pattern.binds.is_empty() {
                    return Err(Spanned::from(TypeError {
                        msg: "pattern bindings with 'is' are only allowed as the entire condition of an 'if'".to_string()
                    }, span));
                }
                return Ok(Type::Bool);
            },
        };
        let def = self.union_defs.get(&enum_name).cloned().expect("registered");
        self.check_pattern(&ip.pattern, &enum_name, &def, span)?;
        if !ip.pattern.binds.is_empty() {
            return Err(Spanned::from(TypeError {
                msg: "pattern bindings with 'is' are only allowed as the entire condition of an 'if'".to_string()
            }, span));
        }
        Ok(Type::Bool)
    }

    /// Type-check a `match subject { arms... (else default)? }`. Also used
    /// (with a single synthesized arm) to type-check `if subject is P then
    /// ... (else ...)?` — see `ConditionalExpr::infer`. Each arm's bound
    /// fields are in scope for its own guard and body only. An unguarded
    /// arm counts toward exhaustiveness; a guarded one never does (the
    /// guard might not hold at runtime). Arm/default bodies are joined the
    /// same way `if` branches are: unify if possible, else fall back to a
    /// `Union` — this is what lets `lower_match` desugar into ordinary
    /// nested `Conditional`s with no dedicated codegen of its own.
    fn infer_match(&mut self, subject: &Spanned<Expression>, arms: &[MatchArm], default: &Option<Box<Spanned<Expression>>>, span: Span) -> TypeResult {
        let subject_ty = self.infer(subject)?;
        let resolved_subject = self.lookup(&subject_ty);
        let enum_name = match self.resolve_union(&resolved_subject) {
            Some((name, _)) => name.to_string(),
            None => return if matches!(&resolved_subject, Type::Union(_)) {
                self.infer_anon_match(resolved_subject, arms, default, span)
            } else {
                Err(Spanned::from(TypeError {
                    msg: format!("Can only match on a union value, got {}", resolved_subject)
                }, subject.span))
            },
        };
        let def = self.union_defs.get(&enum_name).cloned().expect("registered");

        let mut covered: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut result_ty: Option<Type> = None;

        for arm in arms {
            let idx = self.check_pattern(&arm.pattern, &enum_name, &def, arm.body.span)?;
            let variant_fields = &def.variants[idx].1;
            let bindings: Vec<(String, Type)> = if arm.pattern.binds.is_empty() {
                Vec::new()
            } else {
                arm.pattern.binds.iter().zip(variant_fields.iter())
                    .filter(|(b, _)| b.as_str() != "_")
                    .map(|(b, (_, ty))| (b.clone(), ty.clone()))
                    .collect()
            };

            let arm_ty = self.with_context(bindings.into_iter(), |t| -> TypeResult {
                if let Some(g) = &arm.guard {
                    let gt = t.infer(g)?;
                    if !t.unify(&gt, &Type::Bool) {
                        return Err(Spanned::from(TypeError {
                            msg: format!("match guard must be Bool, got {}", t.lookup(&gt))
                        }, g.span));
                    }
                }
                t.infer(&arm.body)
            })?;

            if arm.guard.is_none() {
                covered.insert(idx);
            }

            result_ty = Some(match result_ty {
                None => arm_ty,
                Some(prev) => {
                    if self.unify(&prev, &arm_ty) {
                        self.lookup(&prev)
                    } else {
                        Type::Union(vec![prev, arm_ty]).normalize()
                    }
                }
            });
        }

        if let Some(d) = default {
            let dt = self.with_context(std::iter::empty(), |t| t.infer(d))?;
            result_ty = Some(match result_ty {
                None => dt,
                Some(prev) => if self.unify(&prev, &dt) { self.lookup(&prev) } else { Type::Union(vec![prev, dt]).normalize() },
            });
        } else if covered.len() < def.variants.len() {
            let missing: Vec<&str> = def.variants.iter().enumerate()
                .filter(|(i, _)| !covered.contains(i))
                .map(|(_, (n, _))| n.as_str())
                .collect();
            return Err(Spanned::from(TypeError {
                msg: format!("Non-exhaustive match on {}: missing {} (add an 'else' arm to handle the rest)", enum_name, missing.join(", "))
            }, span));
        }

        Ok(result_ty.unwrap_or(Type::None))
    }

    /// Validate a match/`is` pattern naming a bare type (`is Int`) against
    /// an *anonymous* union's flat, sorted member list — the counterpart
    /// of `check_pattern` for a nominal union's declared variant name.
    /// Returns the member's index in `members` (its runtime tag, used
    /// consistently by `Widen`/`Narrow`/`TypeTag` for the same union) and
    /// its resolved `Type`.
    fn check_type_pattern(&self, pattern: &Pattern, members: &[Type], span: Span) -> Result<(usize, Type), Spanned<TypeError>> {
        if pattern.path.is_some() {
            return Err(Spanned::from(TypeError {
                msg: "a qualifier ('X.Y') only applies to a nominal union's variant name".to_string()
            }, span));
        }
        let member_ty = self.resolve_type_name(&pattern.variant).ok_or_else(|| Spanned::from(TypeError {
            msg: format!("Unknown type '{}'", pattern.variant)
        }, span))?;
        let idx = members.iter().position(|m| *m == member_ty).ok_or_else(|| Spanned::from(TypeError {
            msg: format!("{} is not a member of {}", member_ty, Type::Union(members.to_vec()))
        }, span))?;
        if pattern.binds.len() > 1 {
            return Err(Spanned::from(TypeError {
                msg: format!("Pattern for '{}' expects at most 1 binding, got {}", member_ty, pattern.binds.len())
            }, span));
        }
        Ok((idx, member_ty))
    }

    /// `match`/`if ... is` on an *anonymous* union subject (`resolved_subject`
    /// already inferred and known to be `Type::Union` by the caller —
    /// `infer_match`). Structurally the same algorithm as `infer_match`'s
    /// own nominal-union body just above (arm-by-arm, join branch types,
    /// exhaustiveness over every member unless there's a default) — kept
    /// as a separate function rather than interleaved branches because the
    /// two subject kinds resolve their pattern (`check_pattern` vs
    /// `check_type_pattern`), their exhaustiveness set (variant count vs
    /// member count), and their bind's bound type (a variant field vs the
    /// whole narrowed member) differently enough that sharing one body
    /// would need more branching than it would save.
    fn infer_anon_match(&mut self, resolved_subject: Type, arms: &[MatchArm], default: &Option<Box<Spanned<Expression>>>, span: Span) -> TypeResult {
        let members = match &resolved_subject {
            Type::Union(members) => members.clone(),
            _ => unreachable!("caller already checked resolved_subject is a Union"),
        };

        let mut covered: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut result_ty: Option<Type> = None;

        for arm in arms {
            let (idx, member_ty) = self.check_type_pattern(&arm.pattern, &members, arm.body.span)?;
            let bindings: Vec<(String, Type)> = arm.pattern.binds.iter()
                .filter(|b| b.as_str() != "_")
                .map(|b| (b.clone(), member_ty.clone()))
                .collect();

            let arm_ty = self.with_context(bindings.into_iter(), |t| -> TypeResult {
                if let Some(g) = &arm.guard {
                    let gt = t.infer(g)?;
                    if !t.unify(&gt, &Type::Bool) {
                        return Err(Spanned::from(TypeError {
                            msg: format!("match guard must be Bool, got {}", t.lookup(&gt))
                        }, g.span));
                    }
                }
                t.infer(&arm.body)
            })?;

            if arm.guard.is_none() {
                covered.insert(idx);
            }

            result_ty = Some(match result_ty {
                None => arm_ty,
                Some(prev) => {
                    if self.unify(&prev, &arm_ty) {
                        self.lookup(&prev)
                    } else {
                        Type::Union(vec![prev, arm_ty]).normalize()
                    }
                }
            });
        }

        if let Some(d) = default {
            let dt = self.with_context(std::iter::empty(), |t| t.infer(d))?;
            result_ty = Some(match result_ty {
                None => dt,
                Some(prev) => if self.unify(&prev, &dt) { self.lookup(&prev) } else { Type::Union(vec![prev, dt]).normalize() },
            });
        } else if covered.len() < members.len() {
            let missing: Vec<String> = members.iter().enumerate()
                .filter(|(i, _)| !covered.contains(i))
                .map(|(_, m)| m.to_string())
                .collect();
            return Err(Spanned::from(TypeError {
                msg: format!("Non-exhaustive match on {}: missing {} (add an 'else' arm to handle the rest)", resolved_subject, missing.join(", "))
            }, span));
        }

        Ok(result_ty.unwrap_or(Type::None))
    }

    /// Shared inference for `for var in iterable (if cond)? body`: unifies
    /// `iterable` against `List(elem)`, binds `var: elem` for `cond`/`body`
    /// (popped afterward via `with_context`), and returns `body`'s type —
    /// used directly by `Comprehension`, ignored (replaced with `Type::None`)
    /// by a bare `ForLoop`.
    fn infer_for_loop(&mut self, fl: &ForLoopExpr) -> TypeResult {
        let iter_ty = self.infer(&fl.iterable)?;
        let resolved_iter = self.lookup(&iter_ty);
        let elem_ty = match &resolved_iter {
            Type::List(inner) => (**inner).clone(),
            Type::TypeVar { .. } => {
                let elem = self.fresh_var();
                if !self.unify(&iter_ty, &Type::List(Box::new(elem.clone()))) {
                    return Err(Spanned::from(TypeError {
                        msg: format!("Can't iterate over {}", resolved_iter)
                    }, fl.iterable.span));
                }
                elem
            },
            _ => return Err(Spanned::from(TypeError {
                msg: format!("Can't iterate over {}, expected a List", resolved_iter)
            }, fl.iterable.span)),
        };

        self.with_context(std::iter::once((fl.var.clone(), elem_ty)), |t| {
            if let Some(cond) = &fl.cond {
                let cond_ty = t.infer(cond)?;
                if !t.unify(&cond_ty, &Type::Bool) {
                    return Err(Spanned::from(TypeError {
                        msg: format!("for-loop condition must be Bool, got {}", t.lookup(&cond_ty))
                    }, cond.span));
                }
            }
            t.infer(&fl.body)
        })
    }

    /// The post-inference half of `check`: given an already-known `actual`
    /// type (rather than an expression to infer one from), accept it if
    /// it's a subtype of `expected`, or unify if `actual` is still an open
    /// type variable. Used by a bare `return` (no expression to hand
    /// `check`) checking its implicit `None` against the enclosing
    /// function's return type.
    fn check_ty(&mut self, actual: Type, expected: &Type, span: Span) -> TypeResult {
        let resolved = self.lookup(&actual);
        if self.is_subtype(&resolved, expected) {
            Ok(resolved)
        } else if matches!(resolved, Type::TypeVar { .. }) && self.unify(&resolved, expected) {
            Ok(self.lookup(&resolved))
        } else {
            Err(Spanned::from(TypeError {
                msg: format!("Expected {} got {}", expected, resolved)
            }, span))
        }
    }

    /// `return`, or `return value`. Always typed `Never` — see
    /// `Type::Never` and `TypeChecker::return_types`.
    ///
    /// When the enclosing function has a declared return type, `check` is
    /// used — it gives subtype acceptance (returning an `Int` into a
    /// declared `Int | Str` works) and pushes expected types down into an
    /// unannotated lambda literal. But an *unannotated* function's return
    /// type is a fresh, still-unbound type var (see `FunctionExpr::infer`),
    /// and `check` assumes its `expected_ty` argument is already concrete —
    /// so for that case this unifies directly instead, exactly like any
    /// other site that pins down a fresh var from an inferred type.
    fn infer_return(&mut self, value: &Option<Box<Spanned<Expression>>>, span: Span) -> TypeResult {
        let return_ty = self.return_types.last().cloned().ok_or_else(|| Spanned::from(
            TypeError { msg: "'return' used outside of a function".to_string() }, span
        ))?;
        let resolved_return_ty = self.lookup(&return_ty);
        let still_unbound = matches!(resolved_return_ty, Type::TypeVar { .. });

        match value {
            Some(v) if still_unbound => {
                let value_ty = self.infer(v)?;
                if !self.unify(&value_ty, &return_ty) {
                    return Err(Spanned::from(TypeError {
                        msg: format!("Function's return statements disagree: {} vs {}", self.lookup(&return_ty), value_ty)
                    }, span));
                }
            }
            Some(v) => { self.check(v, &resolved_return_ty)?; }
            None if still_unbound => {
                if !self.unify(&Type::None, &return_ty) {
                    return Err(Spanned::from(TypeError {
                        msg: format!("Function's return statements disagree: {} vs None", self.lookup(&return_ty))
                    }, span));
                }
            }
            None => { self.check_ty(Type::None, &resolved_return_ty, span)?; }
        }
        Ok(Type::Never)
    }

    pub fn check(&mut self, expr: &Spanned<Expression>, expected_ty: &Type) -> TypeResult {
        // Special-case for functions: push down expected parameter types.
        if let Expression::Function(func) = &expr.item {
            if let Type::Function { params, result } = &expected_ty {
                if func.params.len() != params.len() {
                    return Err(Spanned::from(TypeError {
                        msg: format!("Wrong number of arguments, expected {}, got {}", params.len(), func.params.len())
                    }, expr.span));
                }
                let prev_ctx = self.ctx.clone();
                let mut body_ctx = prev_ctx.clone();
                for (p, pty) in func.params.iter().zip(params.iter()) {
                    body_ctx.insert(p.name.clone(), pty.clone());
                }
                self.ctx = body_ctx;
                self.return_types.push((**result).clone());
                // Pop before propagating: an error inside the body must not
                // leave a stale frame on `return_types`, or a later `infer`
                // on this same checker would accept a top-level `return`.
                let res = self.check(&func.body, &*result);
                self.return_types.pop();
                self.ctx = prev_ctx;
                return Ok(res?);
            } else {
                return Err(Spanned::from(TypeError { msg: "Expected function type".to_string() }, expr.span));
            }
        };

        let inferred = self.infer(expr)?;
        let resolved = self.lookup(&inferred);
        if self.is_subtype(&resolved, expected_ty) {
            Ok(resolved)
        } else if matches!(resolved, Type::TypeVar { .. }) && self.unify(&resolved, expected_ty) {
            // TypeVar inferred for the expression: unify it with the expected type
            // rather than a subtype check, which would always fail for unbound vars.
            Ok(self.lookup(&resolved))
        } else {
            Err(Spanned::from(TypeError {
                msg: format!("Expected {} got {}", expected_ty, resolved)
            }, expr.span))
        }
    }

    /// Subtype relation:
    ///   T ≤ T
    ///   Never ≤ T  (the bottom type is a subtype of everything)
    ///   T ≤ T | U  (T is a member of any union it belongs to)
    ///   T | U ≤ V  iff T ≤ V and U ≤ V
    pub fn is_subtype(&self, sub: &Type, sup: &Type) -> bool {
        let sub = self.lookup(sub);
        let sup = self.lookup(sup);
        if sub == sup { return true; }
        if sub == Type::Never { return true; }
        match (&sub, &sup) {
            // Union on left: every variant must be a subtype of sup.
            // This arm must come first so it takes priority over the next arm
            // when both sides are unions.
            (Type::Union(sub_variants), _) => sub_variants.iter().all(|v| self.is_subtype(v, &sup)),
            // Scalar on left, union on right: sub must fit at least one variant.
            (_, Type::Union(variants))     => variants.iter().any(|v| self.is_subtype(&sub, v)),
            _ => false,
        }
    }

    fn infer_call(&mut self, callable: &Spanned<Expression>, args: &Vec<Spanned<Expression>>) -> TypeResult {
        // Struct construction: `Person(name="Alice", age=42)` looks like an
        // ordinary call syntactically (there's no dedicated construction
        // grammar — see `Grammar::data_decl`'s doc comment), so it's
        // disambiguated here, before the generic function-call path, by
        // checking whether the callee name is a registered struct.
        if let Expression::Literal(LiteralExpr { token: Token::Identifier(name) }) = &callable.item {
            if let Some(field_defs) = self.struct_defs.get(name).cloned() {
                return self.infer_struct_init(name, &field_defs, args, callable.span);
            }
        }

        // Enum variant construction: `Circle(r=4)` (bare, unique owner) or
        // `Shape.Circle(r=4)` (qualified) — same syntactic shape as an
        // ordinary call, disambiguated the same way struct construction is.
        match self.resolve_variant_callee(&callable.item) {
            Ok(Some((enum_name, variant))) => return self.infer_variant_init(&enum_name, &variant, args, callable.span),
            Ok(None) => {},
            Err(msg) => return Err(Spanned::from(TypeError { msg }, callable.span)),
        }

        // `print` is a builtin conversion: unlike ordinary functions, its
        // argument is accepted at any type and is formatted as text by the
        // code generator/runtime.  Still infer the argument so errors inside
        // it are reported normally.
        if matches!(&callable.item, Expression::Literal(LiteralExpr { token: Token::Identifier(name) }) if name == "print") {
            if args.len() != 1 {
                return Err(Spanned::from(TypeError {
                    msg: format!("Wrong number of arguments, expected 1, got {}", args.len())
                }, callable.span));
            }
            self.infer(&args[0])?;
            return Ok(Type::None);
        }

        let raw_type = self.infer(callable)?;
        let func_type = self.lookup(&raw_type);

        // If the callee is an unbound TypeVar (e.g. a lambda parameter used as a function),
        // bind it to a fresh function type whose arity matches this call site.
        let func_type = if let Type::TypeVar { name, .. } = &func_type {
            let param_types: Vec<Type> = args.iter().map(|_| self.fresh_var()).collect();
            let result_type = self.fresh_var();
            let fn_ty = Type::Function { params: param_types, result: Box::new(result_type) };
            self.substitutions.insert(name.clone(), fn_ty.clone());
            fn_ty
        } else {
            func_type
        };

        if let Type::Function { params, result } = func_type {
            if args.len() != params.len() {
                return Err(Spanned::from(TypeError {
                    msg: format!("Wrong number of arguments, expected {}, got {}", params.len(), args.len())
                }, callable.span));
            }
            for (arg, param) in args.iter().zip(params) {
                let argt = self.infer(arg)?;
                let resolved_argt  = self.lookup(&argt);
                let resolved_param = self.lookup(&param);
                // Allow implicit widening coercions at call sites (e.g. Int→Float).
                if widens_to(&resolved_argt, &resolved_param) {
                    continue;
                }
                if !self.unify(&argt, &param) {
                    return Err(Spanned::from(TypeError {
                        msg: format!("Can't unify {:?} and {:?}", argt, param)
                    }, arg.span));
                }
            };
            return Ok(self.lookup(&result));
        } else {
            return Err(Spanned::from(TypeError {
                msg: format!("Not callable: {}", callable.item)
            }, callable.span));
        };
    }

    /// Shared logic for polymorphic operators bounded by a single trait:
    /// arithmetic (`Num`) and ordering (`Ord`) both (a) infer each operand,
    /// requiring it to satisfy `tr` unless it's still an unbound TypeVar,
    /// then (b) either fold the concrete operands through the widening
    /// lattice, or — if any operand is an unresolved TypeVar or a union —
    /// unify every operand against one fresh `tr`-bounded type variable
    /// instead. Returns the joined/unified operand type; `Ord` callers
    /// always want `Bool` instead, so they call this for its
    /// type-checking/unification side effects and discard the result.
    fn join_operands(&mut self, op: &str, tr: Trait, args: &[&Spanned<Expression>], span: Span) -> TypeResult {
        let mut arg_types: Vec<Type> = Vec::new();
        for arg in args {
            let argt = self.infer(arg)?;
            let resolved = self.lookup(&argt);
            if !matches!(&resolved, Type::TypeVar { .. }) && !self.type_implements(&resolved, &tr) {
                return Err(Spanned::from(TypeError {
                    msg: format!("Operator '{}' requires {}, got {}", op, tr, resolved)
                }, arg.span));
            }
            arg_types.push(resolved);
        }
        let has_union = arg_types.iter().any(|t| matches!(t, Type::Union(..)));
        let concrete: Vec<&Type> = arg_types.iter().filter(|t| !matches!(t, Type::TypeVar { .. })).collect();

        if concrete.is_empty() || has_union {
            // All TypeVars, or a union operand present — unify via one fresh
            // tr-bounded var. Unions satisfy `tr` iff every variant does
            // (handled by unify).
            let t = self.fresh_bounded_var(vec![tr.clone()]);
            for (arg, argt) in args.iter().zip(&arg_types) {
                if !self.unify(argt, &t) {
                    return Err(Spanned::from(TypeError {
                        msg: format!("Operator '{}' requires {}, got {}", op, tr, argt)
                    }, arg.span));
                }
            }
            return Ok(self.lookup(&t));
        }

        // All scalar operands — fold through the widening lattice.
        let mut join = concrete[0].clone();
        for ty in &concrete[1..] {
            join = numeric_join(&join, ty).ok_or_else(|| Spanned::from(TypeError {
                msg: format!("Operator '{}' got incompatible types: {} and {}", op, join, ty)
            }, span))?;
        }
        // Bind any TypeVars to the join type so lambda params get concrete types.
        for argt in &arg_types {
            if matches!(argt, Type::TypeVar { .. }) { self.unify(argt, &join); }
        }
        Ok(join)
    }

    /// Infer the result type of a built-in operator.
    ///
    /// Arithmetic (+, -, *, /, unary-): require `Num` — works for Int and Float.
    /// Equality (==, !=): require `Eq`  — works for Int, Float, Bool, Str.
    /// Ordering (<, >, <=, >=): require `Ord` — works for Int, Float, Str.
    /// Logical (and, or, not): Bool only.
    fn infer_builtin_op(&mut self, op: &str, args: &[&Spanned<Expression>], span: Span) -> TypeResult {
        match op {
            "not" => {
                let argt = self.infer(args[0])?;
                if !self.unify(&argt, &Type::Bool) {
                    return Err(Spanned::from(TypeError {
                        msg: format!("'not' requires Bool, got {}", self.lookup(&argt))
                    }, args[0].span));
                }
                Ok(Type::Bool)
            },

            "and" | "or" => {
                for arg in args {
                    let argt = self.infer(arg)?;
                    if !self.unify(&argt, &Type::Bool) {
                        return Err(Spanned::from(TypeError {
                            msg: format!("'{}' requires Bool, got {}", op, self.lookup(&argt))
                        }, arg.span));
                    }
                }
                Ok(Type::Bool)
            },

            "+" | "-" | "*" | "/" | "<unaryminus>" => {
                // String concatenation: `Str + Str -> Str`
                if op == "+" && args.len() == 2 {
                    let left_ty = self.infer(args[0])?;
                    let resolved_left = self.lookup(&left_ty);
                    if resolved_left == Type::Str {
                        let right_ty = self.infer(args[1])?;
                        let resolved_right = self.lookup(&right_ty);
                        if resolved_right == Type::Str {
                            return Ok(Type::Str);
                        }
                        return Err(Spanned::from(TypeError {
                            msg: format!("Operator '+' on Str requires Str on both sides, got {}", resolved_right)
                        }, args[1].span));
                    }
                }

                self.join_operands(op, Trait::Num, args, span)
            },

            "==" | "!=" => {
                let t = self.fresh_bounded_var(vec![Trait::Eq]);
                for arg in args {
                    let argt = self.infer(arg)?;
                    if !self.unify(&argt, &t) {
                        let resolved_t    = self.lookup(&t);
                        let resolved_argt = self.lookup(&argt);
                        let msg = if matches!(resolved_t, Type::TypeVar { .. }) {
                            format!("Operator '{}' requires Eq, got {}", op, resolved_argt)
                        } else {
                            format!("Operator '{}' got incompatible types: expected {}, got {}", op, resolved_t, resolved_argt)
                        };
                        return Err(Spanned::from(TypeError { msg }, arg.span));
                    }
                }
                Ok(Type::Bool)
            },

            "<" | ">" | "<=" | ">=" => {
                // Comparisons always yield Bool regardless of the operand
                // type; `join_operands` is called purely for its
                // type-checking/unification side effects here.
                self.join_operands(op, Trait::Ord, args, span)?;
                Ok(Type::Bool)
            },

            _ => Err(Spanned::from(TypeError {
                msg: format!("Unknown operator: {}", op)
            }, span)),
        }
    }

    fn lookup(&self, ty: &Type) -> Type {
        match ty {
            Type::TypeVar { name, .. } => {
                if let Some(found) = self.substitutions.get(name) {
                    self.lookup(found)
                } else {
                    ty.clone()
                }
            },
            Type::Function { params, result } => Type::Function {
                params: params.iter().map(|ty| self.lookup(ty)).collect(),
                result: Box::new(self.lookup(&result)),
            },
            Type::List(inner)      => Type::List(Box::new(self.lookup(inner))),
            Type::Union(variants)  => Type::Union(variants.iter().map(|t| self.lookup(t)).collect()),
            _ => ty.clone(),
        }
    }

    fn get(&self, name: &str, span: Span) -> TypeResult {
        let ty = self.ctx.get(name).ok_or(
            Spanned::from(TypeError { msg: format!("Unbound variable {}", name) }, span)
        )?;
        Ok(ty.clone())
    }

    /// Run `closure` with `update_ctx` merged into `self.ctx`, then restore
    /// `self.ctx` to its pre-call state — regardless of whether `closure`
    /// succeeded, so a scope's bindings (including ones made *inside*
    /// `closure`, e.g. a nested `let`) never leak to the caller.
    fn with_context<F, R>(
        &mut self,
        update_ctx: impl Iterator<Item = (String, Type)>,
        closure: F,
    ) -> R where F: FnOnce(&mut Self) -> R,
    {
        let prev_ctx = self.ctx.clone();
        self.ctx.extend(update_ctx);
        let res = closure(self);
        self.ctx = prev_ctx;
        res
    }

    fn unify(&mut self, t1: &Type, t2: &Type) -> bool {
        let t1 = self.lookup(t1);
        let t2 = self.lookup(t2);

        if t1 == t2 {
            return true;
        }

        match (&t1, &t2) {
            // Both TypeVars: bind the less-bounded to the more-bounded, so the
            // canonical representative carries the constraints. This ensures bounds
            // propagate when a bounded operator TypeVar unifies with an unconstrained
            // lambda parameter TypeVar.
            (Type::TypeVar { name: n1, bounds: b1 }, Type::TypeVar { name: n2, bounds: b2 }) => {
                if b2.len() > b1.len() {
                    // t2 has more bounds: bind t1 -> t2
                    self.substitutions.insert(n1.clone(), t2.clone());
                } else {
                    // t1 has more (or equal) bounds: bind t2 -> t1
                    self.substitutions.insert(n2.clone(), t1.clone());
                }
                true
            },
            // Union ↔ bounded TypeVar: accept if every union member satisfies every bound,
            // e.g. `Int | Float` satisfies `Num`. Must come before the generic TypeVar arms.
            (Type::Union(variants), Type::TypeVar { name, bounds }) => {
                if bounds.iter().all(|b| variants.iter().all(|v| self.type_implements(v, b))) {
                    self.substitutions.insert(name.clone(), t1.clone());
                    true
                } else {
                    false
                }
            },
            (Type::TypeVar { name, bounds }, Type::Union(variants)) => {
                if bounds.iter().all(|b| variants.iter().all(|v| self.type_implements(v, b))) {
                    self.substitutions.insert(name.clone(), t2.clone());
                    true
                } else {
                    false
                }
            },
            // Bounded TypeVar on left, concrete type on right.
            (Type::TypeVar { name, bounds }, _) => {
                if !bounds.iter().all(|b| self.type_implements(&t2, b)) {
                    return false;
                }
                self.substitutions.insert(name.clone(), t2.clone());
                true
            },
            // Concrete type on left, bounded TypeVar on right.
            (_, Type::TypeVar { name, bounds }) => {
                if !bounds.iter().all(|b| self.type_implements(&t1, b)) {
                    return false;
                }
                self.substitutions.insert(name.clone(), t1.clone());
                true
            },
            // Concrete type on left, Union on right:
            // accept (without binding) if the concrete is a member of the union.
            // This handles e.g. the second operand of `(Int|Float) + 2` after the first
            // operand already bound the operator TypeVar to the union.
            (_, Type::Union(variants)) => variants.iter().any(|v| t1 == *v),
            // Structural unification for functions.
            (Type::Function { params: p1, result: r1 }, Type::Function { params: p2, result: r2 }) => {
                if p1.len() != p2.len() {
                    return false;
                }
                p1.iter().zip(p2.iter()).all(|(l, r)| self.unify(l, r)) &&
                self.unify(&r1, &r2)
            },
            _ => false,
        }
    }

    /// Type-check and lower a top-level program or REPL entry.
    ///
    /// The parser always wraps its output in a `Block` (see `Parser::block`),
    /// but unlike a *nested* `{ ... }` block expression, the outermost list
    /// of statements is not its own lexical scope: a top-level `let` must
    /// remain visible to later statements in the same entry **and** to later
    /// `eval` calls on the same `FrogState` (which reuses this `TypeChecker`
    /// across entries). Plain `check_and_lower` scopes every `Block` it
    /// sees, so it can only be used here on the unwrapped statement list.
    pub fn check_and_lower_entry(
        &mut self,
        expr: Spanned<Expression>,
    ) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let span = expr.span;
        match expr.item {
            Expression::Block(stmts) => {
                self.hoist_data_decls(&stmts)?;
                let mut lowered = Vec::with_capacity(stmts.len());
                let mut ty = Type::None;
                for s in stmts {
                    if matches!(s.item, Expression::DataDecl(_)) { continue; }
                    let t = self.check_and_lower(s)?;
                    ty = t.item.ty.clone();
                    lowered.push(t);
                }
                if let Some((_, rest)) = lowered.split_last() {
                    for t in rest {
                        self.check_must_handle(t)?;
                    }
                }
                Ok(Spanned::from(TypedExpr { ty, kind: TypedExprKind::Block(lowered) }, span))
            },
            other => self.check_and_lower(Spanned::from(other, span)),
        }
    }

    /// Type-check and lower an untyped `Spanned<Expression>` into a
    /// `Spanned<TypedExpr>`, consuming the source node by move.
    ///
    /// Every node in the output carries a fully-resolved `Type` (no unbound
    /// `TypeVar`s at leaf positions once concrete call-sites constrain them).
    ///
    /// A `Block` encountered here is always a *nested* block (an `if`
    /// branch, a function body, an explicit `{ ... }` subexpression) and is
    /// therefore scoped — see `check_and_lower_entry` for the top-level entry
    /// point, which is not.
    pub fn check_and_lower(
        &mut self,
        expr: Spanned<Expression>,
    ) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let span = expr.span;
        // Borrow expr for inference, then consume it for lowering.
        let ty          = self.infer(&expr)?;
        let resolved_ty = self.lookup(&ty);

        let kind = match expr.item {
            Expression::Literal(lit) => match lit.token {
                Token::Int(n)         => TypedExprKind::IntLit(n),
                Token::Float(f)       => TypedExprKind::FloatLit(f),
                Token::String(s)      => TypedExprKind::StrLit(s),
                Token::True           => TypedExprKind::BoolLit(true),
                Token::False          => TypedExprKind::BoolLit(false),
                Token::None           => TypedExprKind::NoneLit,
                // A bound value takes priority; otherwise this is a
                // bindless nullary variant construction — see
                // `TypeChecker::infer_bare_variant`.
                Token::Identifier(nm) => if self.ctx.contains_key(&nm) {
                    TypedExprKind::Var(nm)
                } else {
                    let enum_name = self.variant_owners.get(&nm).and_then(|owners| owners.first()).cloned()
                        .expect("bare identifier validated as a nullary variant during infer");
                    let def = self.union_defs.get(&enum_name).expect("registered");
                    let tag = def.variant_index(&nm).expect("registered") as u32;
                    TypedExprKind::VariantInit { enum_name, variant: nm, tag, fields: Vec::new() }
                },
                _ => unreachable!("unexpected literal token"),
            },

            Expression::Unary(u) => {
                let inner = self.check_and_lower(*u.expr)?;
                TypedExprKind::Unary { op: u.op, expr: Box::new(inner) }
            },

            Expression::Binary(b) => {
                let left  = self.check_and_lower(*b.left)?;
                let right = self.check_and_lower(*b.right)?;
                if matches!(b.op, Token::EqEq | Token::NotEq) {
                    if let Type::Struct(name) = left.item.ty.clone() {
                        self.desugar_struct_eq(b.op, left, right, &name, span)
                    } else {
                        TypedExprKind::Binary { op: b.op, left: Box::new(left), right: Box::new(right) }
                    }
                } else {
                    TypedExprKind::Binary { op: b.op, left: Box::new(left), right: Box::new(right) }
                }
            },

            Expression::Conditional(c) => {
                // `if subject is Pattern then A (else B)?` — see the matching
                // note on `ConditionalExpr::infer`. Desugars entirely through
                // `lower_match`, which returns a fully-formed typed node, so
                // its `.kind` is used directly (its `.ty` should already
                // equal `resolved_ty`, computed above via `infer_match`).
                if matches!(&c.cond.item, Expression::IsPattern(_)) {
                    let ip = match c.cond.item {
                        Expression::IsPattern(ip) => ip,
                        _ => unreachable!(),
                    };
                    let arms = vec![crate::frontend::expression::MatchArm {
                        pattern: ip.pattern,
                        guard: None,
                        body: c.true_branch,
                    }];
                    return Ok(self.lower_match(ip.subject, arms, c.false_branch, span)?);
                }

                let cond = self.check_and_lower(*c.cond)?;

                // Each branch is its own scope (see the matching note on
                // `ConditionalExpr::infer`): a `let` inside one arm must not
                // remain bound once we're back outside the conditional.
                let prev_ctx = self.ctx.clone();
                let true_result = self.check_and_lower(*c.true_branch);
                self.ctx = prev_ctx;
                let true_branch = self.lower_widen(true_result?, &resolved_ty)?;

                let false_branch = match c.false_branch {
                    Some(fb) => {
                        let prev_ctx = self.ctx.clone();
                        let false_result = self.check_and_lower(*fb);
                        self.ctx = prev_ctx;
                        Some(Box::new(self.lower_widen(false_result?, &resolved_ty)?))
                    },
                    None => None,
                };
                TypedExprKind::Conditional {
                    cond:         Box::new(cond),
                    true_branch:  Box::new(true_branch),
                    false_branch,
                }
            },

            Expression::Assign(a) => {
                // Resolved before `a.target`/`a.value` are moved out below
                // — needed to widen the value when the annotation is a
                // union that its natural type doesn't already match (see
                // `lower_widen`; subtyping is otherwise invisible at
                // runtime).
                let annotation_ty = match &a.typ {
                    Some(ann) => Some(self.resolve_type_expr(ann)?),
                    None => None,
                };
                let target_item = a.target.item;
                if let Expression::FieldAccess(fa) = target_item {
                    let base = fa.target.item.get_identifier()
                        .expect("parser only allows a bare identifier as a field-assign base")
                        .to_string();
                    let value = self.check_and_lower(*a.value)?;
                    // The assigned field is exactly as much a union-typed
                    // slot as a `StructInit` argument is, so it needs the
                    // same widening — without it, `c.v = 9` would overwrite
                    // a boxed `Int | Bool` field with the raw immediate `9`
                    // and the next `TypeTag`/`Narrow` would dereference it
                    // as a `FrogVariant*`. The declared field type comes
                    // from `struct_defs` (via the base's own type), never
                    // from `a.typ` — a field assignment carries no
                    // annotation of its own.
                    let field_ty = self.ctx.get(&base).map(|t| self.lookup(t))
                        .and_then(|t| match t {
                            Type::Struct(name) => self.struct_defs.get(&name)
                                .and_then(|fs| fs.iter().find(|(n, _)| *n == fa.field))
                                .map(|(_, fty)| fty.clone()),
                            _ => None,
                        });
                    let value = match &field_ty {
                        Some(t) => self.lower_widen(value, t)?,
                        None => value,
                    };
                    TypedExprKind::FieldAssign { base, field: fa.field, value: Box::new(value) }
                } else {
                    let name  = target_item.get_identifier()
                        .expect("assignment target must be identifier").to_string();
                    let value = self.check_and_lower(*a.value)?;
                    let value = match &annotation_ty {
                        Some(t) => self.lower_widen(value, t)?,
                        None => value,
                    };
                    TypedExprKind::Assign { name, value: Box::new(value) }
                }
            },

            Expression::Function(f) => {
                let (param_types, return_type) = match &resolved_ty {
                    Type::Function { params, result } => (params.clone(), (**result).clone()),
                    _ => unreachable!("function expression must have Function type"),
                };
                let params: Vec<(String, Type)> = f.params.iter()
                    .zip(param_types.iter())
                    .map(|(p, ty)| (p.name.clone(), ty.clone()))
                    .collect();

                // Temporarily bind parameters so the body can look them up.
                let prev_ctx = self.ctx.clone();
                for (name, ty) in &params {
                    self.ctx.insert(name.clone(), ty.clone());
                }
                // Independent pass from `FunctionExpr::infer`'s own push —
                // `check_and_lower` re-walks the body from scratch to lower
                // it, so `return`'s lowering arm (below) needs the stack
                // populated here too, not just during inference.
                self.return_types.push(return_type.clone());
                let body_result = self.check_and_lower(*f.body);
                self.return_types.pop();
                self.ctx = prev_ctx;
                let body = body_result?;
                // The body's own tail value needs widening exactly like any
                // other site that places a narrower value into a
                // union-typed slot — `return` statements inside the body
                // are a separate site, handled by `Expression::Return`'s
                // own lowering arm below.
                let body = self.lower_widen(body, &return_type)?;

                TypedExprKind::Function { params, return_type, body: Box::new(body) }
            },

            Expression::Call(c) => {
                let struct_name = c.callable.item.get_identifier()
                    .filter(|n| self.struct_defs.contains_key(*n))
                    .map(|n| n.to_string());
                if let Some(name) = struct_name {
                    let field_defs = self.struct_defs.get(&name).cloned().unwrap_or_default();
                    let mut fields: Vec<(String, Box<Spanned<TypedExpr>>)> = Vec::with_capacity(c.args.len());
                    for arg in c.args {
                        match arg.item {
                            Expression::Assign(a) => {
                                let fname = a.target.item.get_identifier()
                                    .expect("validated during infer").to_string();
                                let value = self.check_and_lower(*a.value)?;
                                fields.push((fname, Box::new(value)));
                            },
                            _ => unreachable!("struct construction args validated as Assign during infer"),
                        }
                    }
                    // Reorder into declared-field order so codegen's flattened
                    // leaf layout (`struct_fields` in codegen/mod.rs) lines up
                    // regardless of the source's argument order.
                    let mut ordered = Vec::with_capacity(field_defs.len());
                    for (fname, fty) in &field_defs {
                        let idx = fields.iter().position(|(n, _)| n == fname)
                            .expect("field presence validated during infer");
                        let (fname, value) = fields.remove(idx);
                        let value = Box::new(self.lower_widen(*value, fty)?);
                        ordered.push((fname, value));
                    }
                    TypedExprKind::StructInit { name, fields: ordered }
                } else if let Ok(Some((enum_name, variant))) = self.resolve_variant_callee(&c.callable.item) {
                    let def = self.union_defs.get(&enum_name).cloned().expect("validated during infer");
                    let variant_fields = def.variants.iter().find(|(n, _)| n == &variant)
                        .map(|(_, fs)| fs.clone()).expect("validated during infer");
                    let mut field_defs = def.common.clone();
                    field_defs.extend(variant_fields);

                    let mut fields: Vec<(String, Box<Spanned<TypedExpr>>)> = Vec::with_capacity(c.args.len());
                    for arg in c.args {
                        match arg.item {
                            Expression::Assign(a) => {
                                let fname = a.target.item.get_identifier()
                                    .expect("validated during infer").to_string();
                                let value = self.check_and_lower(*a.value)?;
                                fields.push((fname, Box::new(value)));
                            },
                            _ => unreachable!("variant construction args validated as Assign during infer"),
                        }
                    }
                    let mut ordered = Vec::with_capacity(field_defs.len());
                    for (fname, fty) in &field_defs {
                        let idx = fields.iter().position(|(n, _)| n == fname)
                            .expect("field presence validated during infer");
                        let (fname, value) = fields.remove(idx);
                        let value = Box::new(self.lower_widen(*value, fty)?);
                        ordered.push((fname, value));
                    }
                    let tag = def.variant_index(&variant).expect("validated during infer") as u32;
                    TypedExprKind::VariantInit { enum_name, variant, tag, fields: ordered }
                } else {
                    let callable = self.check_and_lower(*c.callable)?;
                    let param_types = match &callable.item.ty {
                        Type::Function { params, .. } => Some(params.clone()),
                        _ => None,
                    };
                    let mut args = Vec::with_capacity(c.args.len());
                    for arg in c.args {
                        let lowered = self.check_and_lower(arg)?;
                        let lowered = match &param_types {
                            Some(params) if args.len() < params.len() => self.lower_widen(lowered, &params[args.len()])?,
                            _ => lowered,
                        };
                        args.push(lowered);
                    }
                    TypedExprKind::Call { callable: Box::new(callable), args }
                }
            },

            Expression::Tuple(elems) => {
                let mut items = Vec::with_capacity(elems.len());
                for e in elems {
                    items.push(self.check_and_lower(e)?);
                }
                TypedExprKind::List(items)
            },

            // A block is its own scope — see the matching note on `infer`'s
            // `Expression::Block` arm. `check_and_lower` is only ever called
            // directly (not via `check_and_lower_entry`) on a *nested* block,
            // since the top-level program/REPL entry goes through
            // `check_and_lower_entry` instead, which does not scope.
            Expression::Block(stmts) => {
                // Hoisting already ran — see `infer`'s `Expression::Block`
                // arm, which always runs first (`check_and_lower` infers
                // the whole expression before this match) — so only the
                // skip (not another `hoist_data_decls` call) is needed here.
                let prev_ctx = self.ctx.clone();
                let mut lowered = Vec::with_capacity(stmts.len());
                let mut err = None;
                for s in stmts {
                    if matches!(s.item, Expression::DataDecl(_)) { continue; }
                    match self.check_and_lower(s) {
                        Ok(t) => lowered.push(t),
                        Err(e) => { err = Some(e); break; },
                    }
                }
                self.ctx = prev_ctx;
                if let Some(e) = err { return Err(e); }
                // Must-handle: every non-tail statement's value is
                // discarded, so none may be a possible Error — see
                // `check_must_handle`. The tail is exempt; its value
                // propagates to whatever position this Block itself sits
                // in, which is checked there instead.
                if let Some((_, rest)) = lowered.split_last() {
                    for t in rest {
                        self.check_must_handle(t)?;
                    }
                }
                TypedExprKind::Block(lowered)
            },

            // The annotation expression is absorbed into `resolved_ty`.
            // Lower the inner expression and reuse its kind directly;
            // the outer Spanned<TypedExpr> carries the annotated type.
            Expression::Annotated(a) => {
                self.check_and_lower(*a.expr)?.item.kind
            },

            Expression::Index(idx) => {
                let target = self.check_and_lower(*idx.target)?;
                let index  = self.check_and_lower(*idx.index)?;
                TypedExprKind::Index { target: Box::new(target), index: Box::new(index) }
            },

            Expression::Slice(s) => {
                let target = self.check_and_lower(*s.target)?;
                let start = match s.start {
                    Some(e) => Some(Box::new(self.check_and_lower(*e)?)),
                    None => None,
                };
                let end = match s.end {
                    Some(e) => Some(Box::new(self.check_and_lower(*e)?)),
                    None => None,
                };
                TypedExprKind::Slice { target: Box::new(target), start, end }
            },

            Expression::Range(r) => {
                let start = self.check_and_lower(*r.start)?;
                let end   = self.check_and_lower(*r.end)?;
                TypedExprKind::Range { start: Box::new(start), end: Box::new(end) }
            },

            Expression::ForLoop(fl) => {
                let (var, iterable, cond, body) = self.lower_for_loop(fl)?;
                TypedExprKind::ForLoop { var, iterable, cond, body }
            },

            Expression::Comprehension(inner) => {
                let fl = match inner.item {
                    Expression::ForLoop(fl) => fl,
                    _ => unreachable!("Comprehension always wraps a ForLoop — see Grammar::tuple"),
                };
                let (var, iterable, cond, body) = self.lower_for_loop(fl)?;
                TypedExprKind::Comprehension { var, iterable, cond, body }
            },

            // Handled entirely by `hoist_data_decls` — never reaches codegen.
            Expression::DataDecl(_) => TypedExprKind::IntLit(0),

            Expression::FieldAccess(fa) => {
                let target = self.check_and_lower(*fa.target)?;
                let enum_name = self.resolve_union(&target.item.ty).map(|(name, _)| name.to_string());
                TypedExprKind::FieldAccess { target: Box::new(target), field: fa.field, enum_name }
            },

            Expression::Match(m) => return self.lower_match(m.subject, m.arms, m.default, span),

            // Standalone (non-if-condition) `subject is Variant`/`subject is
            // Type` — a plain tag test; see `infer_is_pattern`.
            Expression::IsPattern(ip) => {
                let target = self.check_and_lower(*ip.subject)?;
                match self.resolve_union(&target.item.ty) {
                    Some((name, _)) => {
                        let enum_name = name.to_string();
                        let def = self.union_defs.get(&enum_name).expect("registered");
                        let tag = def.variant_index(&ip.pattern.variant).expect("validated during infer") as u32;
                        TypedExprKind::IsVariant { target: Box::new(target), enum_name, variant: ip.pattern.variant, tag }
                    },
                    None => {
                        let members = match &target.item.ty {
                            Type::Union(members) => members.clone(),
                            other => unreachable!("is-pattern subject must be a union after inference, got {}", other),
                        };
                        let tag = members.iter().position(|m| self.resolve_type_name(&ip.pattern.variant).as_ref() == Some(m))
                            .expect("validated during infer") as u32;
                        TypedExprKind::TypeTag { target: Box::new(target), tag }
                    },
                }
            },

            Expression::Import(_) => unreachable!(
                "Expression::Import must be resolved and stripped by frontend::modules before typeck ever sees it"
            ),

            Expression::Return(value) => {
                let value = match value {
                    Some(v) => {
                        let lowered = self.check_and_lower(*v)?;
                        let lowered = match self.return_types.last().cloned() {
                            Some(ret_ty) => self.lower_widen(lowered, &self.lookup(&ret_ty))?,
                            None => lowered,
                        };
                        Some(Box::new(lowered))
                    },
                    None => None,
                };
                TypedExprKind::Return(value)
            },
        };

        Ok(Spanned::from(TypedExpr { ty: resolved_ty, kind }, span))
    }

    /// Shared lowering for `for var in iterable (if cond)? body`, used by
    /// both `ForLoop` and `Comprehension`. `var` is bound to the iterable's
    /// element type for `cond`/`body` only, then popped — mirrors the
    /// manual save/restore `self.ctx` pattern used for `Block`/`Function`
    /// above (rather than `infer`'s `with_context`, since lowering needs
    /// `?` to propagate through multiple steps before restoring).
    fn lower_for_loop(&mut self, fl: ForLoopExpr) -> Result<
        (String, Box<Spanned<TypedExpr>>, Option<Box<Spanned<TypedExpr>>>, Box<Spanned<TypedExpr>>),
        Spanned<TypeError>
    > {
        let iterable = self.check_and_lower(*fl.iterable)?;
        let elem_ty = match &iterable.item.ty {
            Type::List(inner) => (**inner).clone(),
            other => unreachable!("for-loop iterable must be List after inference, got {}", other),
        };

        let prev_ctx = self.ctx.clone();
        self.ctx.insert(fl.var.clone(), elem_ty);
        let result = (|| -> Result<_, Spanned<TypeError>> {
            let cond = match fl.cond {
                Some(c) => Some(Box::new(self.check_and_lower(*c)?)),
                None => None,
            };
            let body = self.check_and_lower(*fl.body)?;
            Ok((cond, body))
        })();
        self.ctx = prev_ctx;

        let (cond, body) = result?;
        // Each iteration discards the body's value exactly like a non-tail
        // Block statement does — same must-handle rule.
        self.check_must_handle(&body)?;
        Ok((fl.var, Box::new(iterable), cond, Box::new(body)))
    }

    /// Lower `match subject { arms... (else default)? }` — and, via
    /// `ConditionalExpr`'s special-case, `if subject is Pattern then A
    /// (else B)?` too (a single synthesized arm). `match` has no runtime
    /// representation of its own: it desugars entirely into ordinary
    /// `Conditional`/`Assign`/`IsVariant`/`VariantField` nodes, built
    /// right-to-left so each arm's "else" is the chain already built for
    /// the arms after it. `subject` is bound to a temporary first (mirrors
    /// `desugar_struct_eq`) so a side-effecting subject expression is only
    /// evaluated once, not once per arm's tag test.
    fn lower_match(&mut self, subject: Box<Spanned<Expression>>, arms: Vec<MatchArm>, default: Option<Box<Spanned<Expression>>>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let subject = self.check_and_lower(*subject)?;
        let Some((enum_name, _)) = self.resolve_union(&subject.item.ty).map(|(n, d)| (n.to_string(), d.clone())) else {
            return self.lower_anon_match(subject, arms, default, span);
        };
        let def = self.union_defs.get(&enum_name).cloned().expect("registered");

        let subject_name = format!("__match_subject_{}", self.next_id); self.next_id += 1;
        let subject_ty = subject.item.ty.clone();
        let subject_assign = Spanned::from(
            TypedExpr { ty: subject_ty.clone(), kind: TypedExprKind::Assign { name: subject_name.clone(), value: Box::new(subject) } },
            span,
        );

        let mut tail: Option<Spanned<TypedExpr>> = match default {
            Some(d) => {
                let prev_ctx = self.ctx.clone();
                let result = self.check_and_lower(*d);
                self.ctx = prev_ctx;
                Some(result?)
            }
            None => None,
        };

        for arm in arms.into_iter().rev() {
            let idx = def.variant_index(&arm.pattern.variant).expect("validated during infer");
            let variant_fields = def.variants[idx].1.clone();

            let subject_var = Spanned::from(
                TypedExpr { ty: subject_ty.clone(), kind: TypedExprKind::Var(subject_name.clone()) },
                span,
            );
            let base_cond = Spanned::from(
                TypedExpr { ty: Type::Bool, kind: TypedExprKind::IsVariant {
                    target: Box::new(subject_var.clone()), enum_name: enum_name.clone(), variant: arm.pattern.variant.clone(), tag: idx as u32,
                } },
                span,
            );

            let mut prelude = Vec::new();
            let mut bindings = Vec::new();
            if !arm.pattern.binds.is_empty() {
                for (bind, (fname, fty)) in arm.pattern.binds.iter().zip(variant_fields.iter()) {
                    if bind == "_" { continue; }
                    let value = Spanned::from(
                        TypedExpr { ty: fty.clone(), kind: TypedExprKind::VariantField {
                            target: Box::new(subject_var.clone()), enum_name: enum_name.clone(), variant: arm.pattern.variant.clone(), field: fname.clone(),
                        } },
                        span,
                    );
                    prelude.push(Spanned::from(
                        TypedExpr { ty: fty.clone(), kind: TypedExprKind::Assign { name: bind.clone(), value: Box::new(value) } },
                        span,
                    ));
                    bindings.push((bind.clone(), fty.clone()));
                }
            }

            // A guard's binds must be extracted (the `prelude`) *before*
            // the guard itself runs — they can't be folded into a single
            // `base_cond and guard` boolean the way a bindless guard could,
            // since extraction is a statement, not an expression. So a
            // guarded arm nests one level deeper: the tag test's true
            // branch runs the prelude, then re-tests the guard, only
            // falling through to `tail` (cloned — it's the else of both
            // the tag test and, on guard failure, the inner check too) if
            // that also fails.
            let prev_ctx = self.ctx.clone();
            for (n, t) in &bindings { self.ctx.insert(n.clone(), t.clone()); }
            let lowered = (|| -> Result<_, Spanned<TypeError>> {
                let body = self.check_and_lower(*arm.body)?;
                let guard = match arm.guard {
                    Some(g) => Some(self.check_and_lower(*g)?),
                    None => None,
                };
                Ok((guard, body))
            })();
            self.ctx = prev_ctx;
            let (guard, body) = lowered?;

            let true_inner = match guard {
                None => body,
                Some(g) => {
                    let true_ty = body.item.ty.clone();
                    // `tail` is only ever `None` here once every remaining
                    // arm/default has been folded in already (the loop runs
                    // right-to-left) — and `infer_match` already rejected a
                    // non-exhaustive match with no default before lowering
                    // ever starts, so a missing `tail` at this point means
                    // this guard's failure path is genuinely unreachable,
                    // not "produces None". `Never` (not `None`) is the
                    // correct placeholder: it vanishes from the union join
                    // below instead of forcing every guarded arm's type to
                    // widen to `T | None`.
                    let false_ty = tail.as_ref().map(|t| t.item.ty.clone()).unwrap_or(Type::Never);
                    let result_ty = if self.unify(&true_ty, &false_ty) {
                        self.lookup(&true_ty)
                    } else {
                        Type::Union(vec![true_ty, false_ty]).normalize()
                    };
                    // Widen each branch to the joined type when it turned
                    // out to be a union — mirrors the matching fix in
                    // `check_and_lower`'s own `Conditional` arm; this
                    // hand-built `Conditional` needs the same treatment.
                    let body = self.lower_widen(body, &result_ty)?;
                    let false_branch = match tail.clone() {
                        Some(t) => Some(Box::new(self.lower_widen(t, &result_ty)?)),
                        None => None,
                    };
                    Spanned::from(
                        TypedExpr { ty: result_ty, kind: TypedExprKind::Conditional {
                            cond: Box::new(g), true_branch: Box::new(body), false_branch,
                        } },
                        span,
                    )
                }
            };

            let true_branch = if prelude.is_empty() {
                true_inner
            } else {
                let inner_ty = true_inner.item.ty.clone();
                let mut stmts = prelude;
                stmts.push(true_inner);
                Spanned::from(TypedExpr { ty: inner_ty, kind: TypedExprKind::Block(stmts) }, span)
            };

            let true_ty = true_branch.item.ty.clone();
            // See the matching comment above: a missing `tail` here means
            // this arm's tag-test-false path is unreachable (the match is
            // already known exhaustive), not that it produces `None`.
            let false_ty = tail.as_ref().map(|t| t.item.ty.clone()).unwrap_or(Type::Never);
            let result_ty = if self.unify(&true_ty, &false_ty) {
                self.lookup(&true_ty)
            } else {
                Type::Union(vec![true_ty, false_ty]).normalize()
            };
            // See the matching comment in the guard case above.
            let true_branch = self.lower_widen(true_branch, &result_ty)?;
            let false_branch = match tail {
                Some(t) => Some(Box::new(self.lower_widen(t, &result_ty)?)),
                None => None,
            };

            tail = Some(Spanned::from(
                TypedExpr { ty: result_ty, kind: TypedExprKind::Conditional {
                    cond: Box::new(base_cond), true_branch: Box::new(true_branch), false_branch,
                } },
                span,
            ));
        }

        let chain = tail.expect("infer_match already rejected an empty match with no arms and no default");
        let chain_ty = chain.item.ty.clone();
        Ok(Spanned::from(TypedExpr { ty: chain_ty, kind: TypedExprKind::Block(vec![subject_assign, chain]) }, span))
    }

    /// `lower_match`'s counterpart for an *anonymous* union subject
    /// (`subject` already lowered, and already known not to be a nominal
    /// union — see `lower_match`). Same nested-`Conditional` desugaring,
    /// `TypeTag`/`Narrow` in place of `IsVariant`/`VariantField`, and a
    /// bind (there's at most one, checked by `check_type_pattern`) is the
    /// whole narrowed member rather than one of its fields.
    fn lower_anon_match(&mut self, subject: Spanned<TypedExpr>, arms: Vec<MatchArm>, default: Option<Box<Spanned<Expression>>>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let members = match &subject.item.ty {
            Type::Union(members) => members.clone(),
            other => unreachable!("lower_anon_match subject must be an anonymous union, got {}", other),
        };

        let subject_name = format!("__match_subject_{}", self.next_id); self.next_id += 1;
        let subject_ty = subject.item.ty.clone();
        let subject_assign = Spanned::from(
            TypedExpr { ty: subject_ty.clone(), kind: TypedExprKind::Assign { name: subject_name.clone(), value: Box::new(subject) } },
            span,
        );

        let mut tail: Option<Spanned<TypedExpr>> = match default {
            Some(d) => {
                let prev_ctx = self.ctx.clone();
                let result = self.check_and_lower(*d);
                self.ctx = prev_ctx;
                Some(result?)
            }
            None => None,
        };

        for arm in arms.into_iter().rev() {
            let (idx, member_ty) = self.check_type_pattern(&arm.pattern, &members, arm.body.span)?;

            let subject_var = Spanned::from(
                TypedExpr { ty: subject_ty.clone(), kind: TypedExprKind::Var(subject_name.clone()) },
                span,
            );
            let base_cond = Spanned::from(
                TypedExpr { ty: Type::Bool, kind: TypedExprKind::TypeTag {
                    target: Box::new(subject_var.clone()), tag: idx as u32,
                } },
                span,
            );

            let mut prelude = Vec::new();
            let mut bindings = Vec::new();
            if let Some(bind) = arm.pattern.binds.first() {
                if bind != "_" {
                    let value = Spanned::from(
                        TypedExpr { ty: member_ty.clone(), kind: TypedExprKind::Narrow {
                            value: Box::new(subject_var.clone()), tag: idx as u32,
                        } },
                        span,
                    );
                    prelude.push(Spanned::from(
                        TypedExpr { ty: member_ty.clone(), kind: TypedExprKind::Assign { name: bind.clone(), value: Box::new(value) } },
                        span,
                    ));
                    bindings.push((bind.clone(), member_ty.clone()));
                }
            }

            let prev_ctx = self.ctx.clone();
            for (n, t) in &bindings { self.ctx.insert(n.clone(), t.clone()); }
            let lowered = (|| -> Result<_, Spanned<TypeError>> {
                let body = self.check_and_lower(*arm.body)?;
                let guard = match arm.guard {
                    Some(g) => Some(self.check_and_lower(*g)?),
                    None => None,
                };
                Ok((guard, body))
            })();
            self.ctx = prev_ctx;
            let (guard, body) = lowered?;

            let true_inner = match guard {
                None => body,
                Some(g) => {
                    let true_ty = body.item.ty.clone();
                    let false_ty = tail.as_ref().map(|t| t.item.ty.clone()).unwrap_or(Type::Never);
                    let result_ty = if self.unify(&true_ty, &false_ty) {
                        self.lookup(&true_ty)
                    } else {
                        Type::Union(vec![true_ty, false_ty]).normalize()
                    };
                    // Widen each branch to the joined type when it turned
                    // out to be a union — mirrors the matching fix in
                    // `check_and_lower`'s own `Conditional` arm; this
                    // hand-built `Conditional` needs the same treatment.
                    let body = self.lower_widen(body, &result_ty)?;
                    let false_branch = match tail.clone() {
                        Some(t) => Some(Box::new(self.lower_widen(t, &result_ty)?)),
                        None => None,
                    };
                    Spanned::from(
                        TypedExpr { ty: result_ty, kind: TypedExprKind::Conditional {
                            cond: Box::new(g), true_branch: Box::new(body), false_branch,
                        } },
                        span,
                    )
                }
            };

            let true_branch = if prelude.is_empty() {
                true_inner
            } else {
                let inner_ty = true_inner.item.ty.clone();
                let mut stmts = prelude;
                stmts.push(true_inner);
                Spanned::from(TypedExpr { ty: inner_ty, kind: TypedExprKind::Block(stmts) }, span)
            };

            let true_ty = true_branch.item.ty.clone();
            let false_ty = tail.as_ref().map(|t| t.item.ty.clone()).unwrap_or(Type::Never);
            let result_ty = if self.unify(&true_ty, &false_ty) {
                self.lookup(&true_ty)
            } else {
                Type::Union(vec![true_ty, false_ty]).normalize()
            };
            // See the matching comment in the guard case above.
            let true_branch = self.lower_widen(true_branch, &result_ty)?;
            let false_branch = match tail {
                Some(t) => Some(Box::new(self.lower_widen(t, &result_ty)?)),
                None => None,
            };

            tail = Some(Spanned::from(
                TypedExpr { ty: result_ty, kind: TypedExprKind::Conditional {
                    cond: Box::new(base_cond), true_branch: Box::new(true_branch), false_branch,
                } },
                span,
            ));
        }

        let chain = tail.expect("infer_match already rejected an empty match with no arms and no default");
        let chain_ty = chain.item.ty.clone();
        Ok(Spanned::from(TypedExpr { ty: chain_ty, kind: TypedExprKind::Block(vec![subject_assign, chain]) }, span))
    }

    /// Desugar `left == right` / `left != right` (both already lowered,
    /// same `Type::Struct(name)`) into a per-field structural comparison.
    /// `left`/`right` are bound to fresh temporaries first so a
    /// side-effecting operand (e.g. a function call returning a struct)
    /// is only evaluated once, not once per field.
    fn desugar_struct_eq(&mut self, op: Token, left: Spanned<TypedExpr>, right: Spanned<TypedExpr>, name: &str, span: Span) -> TypedExprKind {
        let l_name = format!("__struct_eq_l{}", self.next_id); self.next_id += 1;
        let r_name = format!("__struct_eq_r{}", self.next_id); self.next_id += 1;
        let left_ty = left.item.ty.clone();
        let right_ty = right.item.ty.clone();

        let l_assign = Spanned::from(TypedExpr { ty: left_ty.clone(), kind: TypedExprKind::Assign { name: l_name.clone(), value: Box::new(left) } }, span);
        let r_assign = Spanned::from(TypedExpr { ty: right_ty.clone(), kind: TypedExprKind::Assign { name: r_name.clone(), value: Box::new(right) } }, span);
        let l_var = Spanned::from(TypedExpr { ty: left_ty, kind: TypedExprKind::Var(l_name) }, span);
        let r_var = Spanned::from(TypedExpr { ty: right_ty, kind: TypedExprKind::Var(r_name) }, span);

        let eq_expr = self.build_struct_eq(name, l_var, r_var, span);
        let result = if op == Token::NotEq {
            Spanned::from(TypedExpr { ty: Type::Bool, kind: TypedExprKind::Unary { op: Token::Not, expr: Box::new(eq_expr) } }, span)
        } else {
            eq_expr
        };

        TypedExprKind::Block(vec![l_assign, r_assign, result])
    }

    /// Build `l.f1 == r.f1 and l.f2 == r.f2 and ...` for every field of
    /// struct `name`, recursing for nested-struct fields. `l`/`r` are
    /// assumed cheap to duplicate (a `Var` or `FieldAccess` chain — never
    /// something that could re-run a side effect).
    fn build_struct_eq(&self, name: &str, l: Spanned<TypedExpr>, r: Spanned<TypedExpr>, span: Span) -> Spanned<TypedExpr> {
        let fields = self.struct_defs.get(name).cloned().unwrap_or_default();
        let mut chain: Option<Spanned<TypedExpr>> = None;
        for (fname, fty) in &fields {
            let lf = Spanned::from(TypedExpr { ty: fty.clone(), kind: TypedExprKind::FieldAccess { target: Box::new(l.clone()), field: fname.clone(), enum_name: None } }, span);
            let rf = Spanned::from(TypedExpr { ty: fty.clone(), kind: TypedExprKind::FieldAccess { target: Box::new(r.clone()), field: fname.clone(), enum_name: None } }, span);
            let sub = match fty {
                Type::Struct(inner) => self.build_struct_eq(inner, lf, rf, span),
                _ => Spanned::from(TypedExpr { ty: Type::Bool, kind: TypedExprKind::Binary { op: Token::EqEq, left: Box::new(lf), right: Box::new(rf) } }, span),
            };
            chain = Some(match chain {
                None => sub,
                Some(prev) => Spanned::from(TypedExpr { ty: Type::Bool, kind: TypedExprKind::Binary { op: Token::And, left: Box::new(prev), right: Box::new(sub) } }, span),
            });
        }
        chain.unwrap_or_else(|| Spanned::from(TypedExpr { ty: Type::Bool, kind: TypedExprKind::BoolLit(true) }, span))
    }

    /// Resolve a parsed type annotation (`crate::frontend::type_expr`) into a
    /// `Type`. Total over the type grammar — every `TypeExpr` variant is
    /// handled here, so an unresolvable annotation is a *name* problem, never
    /// a shape problem.
    ///
    /// Note that a type name is looked up only among the built-ins and the
    /// declared data types. It deliberately does *not* fall back to the value
    /// environment, which the old `Expression`-sniffing version did — that let
    /// a local variable shadow a type name.
    fn resolve_type_expr(&self, ann: &Spanned<TypeExpr>) -> TypeResult {
        let span = ann.span;
        match &ann.item {
            TypeExpr::Name(name) => self.resolve_type_name(name)
                .ok_or_else(|| Spanned::from(TypeError { msg: format!("Unknown type '{}'", name) }, span)),

            TypeExpr::Apply(name, args) => {
                let arg_types: Vec<Type> = args.iter()
                    .map(|a| self.resolve_type_expr(a))
                    .collect::<Result<_, _>>()?;
                match (name.as_str(), arg_types.len()) {
                    ("List", 1) => Ok(Type::List(Box::new(arg_types.into_iter().next().expect("len checked")))),
                    ("List", n) => Err(Spanned::from(
                        TypeError { msg: format!("List takes exactly 1 type argument, got {}", n) }, span)),
                    _ => Err(Spanned::from(
                        TypeError { msg: format!("Type '{}' does not take type arguments", name) }, span)),
                }
            }

            TypeExpr::Union(members) => {
                let member_types: Vec<Type> = members.iter()
                    .map(|m| self.resolve_type_expr(m))
                    .collect::<Result<_, _>>()?;
                Ok(Type::Union(member_types).normalize())
            }

            // `T?` is `T | None` and nothing more — the abbreviation exists
            // for readability, not as a distinct type.
            TypeExpr::Optional(inner) => {
                let inner_ty = self.resolve_type_expr(inner)?;
                Ok(Type::Union(vec![inner_ty, Type::None]).normalize())
            }

            TypeExpr::Func(params, result) => {
                let param_types: Vec<Type> = params.iter()
                    .map(|p| self.resolve_type_expr(p))
                    .collect::<Result<_, _>>()?;
                let result_type = self.resolve_type_expr(result)?;
                Ok(Type::Function { params: param_types, result: Box::new(result_type) })
            }
        }
    }
}

pub trait Infer {
    fn infer(&self, tc: &mut TypeChecker, span: Span) -> TypeResult;
}

impl Infer for LiteralExpr {
    fn infer(&self, tc: &mut TypeChecker, span: Span) -> TypeResult {
        let ty = match &self.token {
            Token::Float(_)       => Type::Float,
            Token::Int(_)         => Type::Int,
            Token::String(_)      => Type::Str,
            Token::False          => Type::Bool,
            Token::True           => Type::Bool,
            Token::None           => Type::None,
            // A bound value takes priority; otherwise this might be a
            // nullary enum variant used without call syntax (`Red` for
            // `data Color is Red | ...`) — see `infer_bare_variant`.
            Token::Identifier(nm) => match tc.ctx.get(nm) {
                Some(t) => t.clone(),
                None => tc.infer_bare_variant(nm, span)?,
            },
            _ => unreachable!("weird literal"),
        };
        Ok(ty)
    }
}

impl Infer for UnaryExpr {
    fn infer(&self, tc: &mut TypeChecker, span: Span) -> TypeResult {
        let op = match self.op {
            Token::Minus => "<unaryminus>",
            Token::Not   => "not",
            _ => unreachable!("weird unary"),
        };
        tc.infer_builtin_op(op, &[&*self.expr], span)
    }
}

impl Infer for BinaryExpr {
    fn infer(&self, tc: &mut TypeChecker, span: Span) -> TypeResult {
        let op = format!("{}", self.op);
        tc.infer_builtin_op(&op, &[&*self.left, &*self.right], span)
    }
}

impl Infer for AssignExpr {
    fn infer(&self, tc: &mut TypeChecker, span: Span) -> TypeResult {
        // `alice.age = 43` — rebind-sugar for struct "mutation". `Grammar::assign`
        // only lets this parse when the FieldAccess's own target is a bare
        // identifier, so `get_identifier` below is guaranteed to succeed.
        if let Expression::FieldAccess(fa) = &self.target.item {
            let base_name = fa.target.item.get_identifier()
                .expect("parser only allows a bare identifier as a field-assign base")
                .to_string();
            let base_ty = tc.get(&base_name, span)?;
            let resolved_base = tc.lookup(&base_ty);
            let struct_name = match &resolved_base {
                Type::Struct(n) => n.clone(),
                _ => return Err(Spanned::from(TypeError {
                    msg: format!("Can't assign field '{}' on {}, expected a struct", fa.field, resolved_base)
                }, span)),
            };
            let field_defs = tc.struct_defs.get(&struct_name).cloned().unwrap_or_default();
            let field_ty = field_defs.iter().find(|(n, _)| n == &fa.field)
                .map(|(_, t)| t.clone())
                .ok_or_else(|| Spanned::from(TypeError {
                    msg: format!("Struct {} has no field '{}'", struct_name, fa.field)
                }, span))?;
            return tc.check(&self.value, &field_ty);
        }

        let name = &self.target.item.get_identifier().expect("should have validated in parsing");
        let ty = if let Some(annotation) = &self.typ {
            let annotated_ty = tc.resolve_type_expr(annotation)?;
            // Validate the value against the annotation, then store the annotation
            // type (not the check return value). For function types this matters:
            // check() returns the body type, but the variable's type is the full
            // function type declared in the annotation.
            tc.check(&*self.value, &annotated_ty)?;
            annotated_ty
        } else {
            // Pre-bind fully-annotated functions so the body can reference the
            // function by name (enabling recursion).
            if let Expression::Function(func) = &self.value.item {
                if func.return_type.is_some() && func.params.iter().all(|p| p.ty.is_some()) {
                    let param_tys: Result<Vec<Type>, _> = func.params.iter()
                        .map(|p| tc.resolve_type_expr(p.ty.as_ref().expect("all params annotated — checked above")))
                        .collect();
                    let ret_ty = tc.resolve_type_expr(func.return_type.as_ref().expect("return type present — checked above"))?;
                    let func_ty = Type::Function { params: param_tys?, result: Box::new(ret_ty) };
                    tc.ctx.insert(name.to_string(), func_ty);
                }
            }
            tc.infer(&self.value)?
        };
        tc.ctx.insert(name.to_string(), ty.clone());
        Ok(ty)
    }
}

impl Infer for ConditionalExpr {
    fn infer(&self, tc: &mut TypeChecker, span: Span) -> TypeResult {
        // `if subject is Pattern then A (else B)?` — a binding pattern is
        // only meaningful with a `then`-scope to bind into, so this is
        // intercepted here (before the pattern ever reaches the generic
        // `IsPattern` path, which rejects binds) and handled as sugar for
        // a single-arm `match` — see `infer_match`/`lower_match`.
        if let Expression::IsPattern(ip) = &self.cond.item {
            let arms = vec![crate::frontend::expression::MatchArm {
                pattern: ip.pattern.clone(),
                guard: None,
                body: self.true_branch.clone(),
            }];
            return tc.infer_match(&ip.subject, &arms, &self.false_branch, span);
        }
        let cond_type = tc.infer(&self.cond)?;
        if let Type::Bool = cond_type {
            // Each branch is its own scope, whether or not it's written with
            // `{ }` — `if c then let y = 5 else 0` must not leave `y` bound
            // afterward, any more than `if c then { let y = 5 } else 0` does.
            let true_type = tc.with_context(std::iter::empty(), |t| t.infer(&self.true_branch))?;
            let false_type = if let Some(fb) = &self.false_branch {
                tc.with_context(std::iter::empty(), |t| t.infer(fb))?
            } else {
                Type::None
            };
            if tc.unify(&true_type, &false_type) {
                Ok(tc.lookup(&true_type))
            } else {
                // Branches have incompatible types: produce a union.
                // Normalizing matters beyond tidiness here — it's what
                // drops `Never` (a `return`'d branch) out of the result
                // entirely, so `if c then return 1 else 2` types as plain
                // `Int` rather than `Never | Int`.
                Ok(Type::Union(vec![true_type, false_type]).normalize())
            }
        } else {
            Err(Spanned::from(TypeError {
                msg: format!("Incorrect type {:?} for conditional, should be bool", cond_type)
            }, span))
        }
    }
}

impl Infer for FunctionExpr {
    fn infer(&self, tc: &mut TypeChecker, span: Span) -> TypeResult {
        let mut param_types   = Vec::new();
        let mut param_bindings = Vec::new();

        for p in &self.params {
            let param_ty = if let Some(annotation) = &p.ty {
                tc.resolve_type_expr(annotation)?
            } else {
                tc.fresh_var()
            };
            param_bindings.push((p.name.clone(), param_ty.clone()));
            param_types.push(param_ty);
        }

        // `return`'s type rule needs to know what it's returning into, even
        // when the body has no `: RetType` annotation at all — a fresh,
        // unbound type var serves as that slot in the unannotated case, and
        // gets pinned down the same way any other inferred type does: every
        // `return e` unifies against it via `check`, and so does the body's
        // own tail value below.
        let declared_ret = match &self.return_type {
            Some(ann) => Some(tc.resolve_type_expr(ann)?),
            None => None,
        };
        let return_slot = declared_ret.clone().unwrap_or_else(|| tc.fresh_var());

        let body_type = tc.with_context(param_bindings.into_iter(), |t| {
            t.return_types.push(return_slot.clone());
            let result = match &declared_ret {
                Some(ret_ty) => t.check(&self.body, ret_ty),
                None => t.infer(&self.body),
            };
            t.return_types.pop();
            result
        })?;

        let result_ty = if declared_ret.is_some() {
            return_slot
        } else if body_type == Type::Never {
            // The body ends in an unconditional `return`, so it never falls
            // through to a final value — the `return` statements alone
            // determine the result type, and there is nothing to unify with.
            // (`Never` unifies with nothing, so without this every
            // unannotated function ending in `return` would fail here.)
            tc.lookup(&return_slot)
        } else if tc.unify(&return_slot, &body_type) {
            tc.lookup(&return_slot)
        } else {
            return Err(Spanned::from(TypeError {
                msg: format!(
                    "Function's return statements disagree with its final value: {} vs {}",
                    tc.lookup(&return_slot), body_type
                )
            }, span));
        };

        let func_ty = Type::Function { params: param_types, result: Box::new(result_ty) };
        Ok(tc.lookup(&func_ty))
    }
}
