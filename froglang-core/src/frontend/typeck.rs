use std::{collections::HashMap, fmt::Display, vec};

use crate::frontend::{
    expression::{AssignExpr, BinaryExpr, ConditionalExpr, Expression, FieldAccessExpr, ForLoopExpr, FunctionExpr, LiteralExpr, UnaryExpr},
    tokens::{Span, Spanned, Token},
};
use crate::frontend::typed_ast::{TypedExpr, TypedExprKind};
use crate::utils::format_vec;

#[derive(Debug)]
pub struct TypeError {
    pub msg: String
}

type TypeResult = Result<Type, Spanned<TypeError>>;

/// Traits constrain type variables. A type must implement a trait to be bound
/// to a TypeVar that carries that bound.
#[derive(Debug, Clone, PartialEq)]
pub enum Trait {
    Num,   // Int, Float — arithmetic operators
    Eq,    // Int, Float, Bool, Str — == and !=
    Ord,   // Int, Float, Str — <, >, <=, >=
}

impl Display for Trait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Trait::Num => write!(f, "Num"),
            Trait::Eq  => write!(f, "Eq"),
            Trait::Ord => write!(f, "Ord"),
        }
    }
}

/// Check whether a concrete type implements the given trait.
/// Only makes sense for non-TypeVar types; TypeVar-TypeVar unification is
/// handled separately so we never call this on a TypeVar.
fn type_implements(ty: &Type, tr: &Trait) -> bool {
    match ty {
        // A union satisfies a trait iff every variant does.
        Type::Union(variants) => variants.iter().all(|v| type_implements(v, tr)),
        // Structs get structural `==`/`!=` (desugared into a per-field
        // conjunction at lowering time — see `TypeChecker::desugar_struct_eq`
        // in `check_and_lower`'s `Binary` arm), so they satisfy `Eq`. A
        // struct with a field type that itself doesn't implement `Eq` (e.g.
        // a `List` field — lists don't support `==` at all currently) will
        // fail type-checking when the desugared per-field comparison is
        // itself inferred, which is the correct place for that error to
        // surface, not here.
        Type::Struct(_) if *tr == Trait::Eq => true,
        _ => match tr {
            Trait::Num => matches!(ty, Type::Int | Type::Float),
            Trait::Eq  => matches!(ty, Type::Int | Type::Float | Type::Bool | Type::Str),
            Trait::Ord => matches!(ty, Type::Int | Type::Float | Type::Str),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
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
}

pub struct TypeCheckerCheckpoint {
    ctx: HashMap<String, Type>,
    substitutions: HashMap<String, Type>,
    next_id: u32,
    struct_defs: StructDefs,
}

impl TypeChecker {
    pub fn empty() -> Self {
        TypeChecker { ctx: HashMap::new(), substitutions: HashMap::new(), next_id: 0, struct_defs: HashMap::new() }
    }

    pub fn new() -> Self {
        TypeChecker { ctx: TypeChecker::default_context(), substitutions: HashMap::new(), next_id: 0, struct_defs: HashMap::new() }
    }

    /// Field layout for every registered struct, in declaration order.
    /// Threaded into `Codegen::compile_entry` so codegen can flatten
    /// struct-typed values into their leaf fields — see `struct_fields`
    /// in `codegen/mod.rs`.
    pub fn struct_defs(&self) -> &StructDefs {
        &self.struct_defs
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
        }
    }

    pub fn restore(&mut self, cp: TypeCheckerCheckpoint) {
        self.ctx = cp.ctx;
        self.substitutions = cp.substitutions;
        self.next_id = cp.next_id;
        self.struct_defs = cp.struct_defs;
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
                let annotated_ty = self.resolve_annotation(&inner.ty.item, expr.span)?;
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
        }
    }

    fn infer_field_access(&mut self, fa: &FieldAccessExpr, span: Span) -> TypeResult {
        let target_ty = self.infer(&fa.target)?;
        let resolved = self.lookup(&target_ty);
        match &resolved {
            Type::Struct(sname) => {
                let field_defs = self.struct_defs.get(sname).cloned().unwrap_or_default();
                field_defs.iter().find(|(n, _)| n == &fa.field).map(|(_, t)| t.clone())
                    .ok_or_else(|| Spanned::from(TypeError {
                        msg: format!("Struct {} has no field '{}'", sname, fa.field)
                    }, span))
            },
            _ => Err(Spanned::from(TypeError {
                msg: format!("Can't access field '{}' on {}, expected a struct", fa.field, resolved)
            }, fa.target.span)),
        }
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
                if self.struct_defs.contains_key(&d.name) {
                    return Err(Spanned::from(TypeError {
                        msg: format!("Struct '{}' is already declared", d.name)
                    }, s.span));
                }
                self.struct_defs.insert(d.name.clone(), Vec::new());
            }
        }
        for s in stmts {
            if let Expression::DataDecl(d) = &s.item {
                let mut fields = Vec::with_capacity(d.fields.len());
                for p in &d.fields {
                    let ann = p.ty.as_ref().expect("data-decl fields always carry a type annotation — see Grammar::data_decl");
                    let ty = self.resolve_annotation(&ann.item, s.span)?;
                    fields.push((p.name.clone(), ty));
                }
                self.struct_defs.insert(d.name.clone(), fields);
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
    /// infinite-size cycle the way a direct field can).
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

    /// Type-check a `Name(field=value, ...)` struct construction call —
    /// intercepted in `infer_call` before the generic function-call path.
    fn infer_struct_init(&mut self, name: &str, field_defs: &[(String, Type)], args: &[Spanned<Expression>], span: Span) -> TypeResult {
        let mut seen: HashMap<String, Span> = HashMap::new();
        for arg in args {
            let (fname, value_expr) = match &arg.item {
                Expression::Assign(a) => {
                    let fname = a.target.item.get_identifier()
                        .ok_or_else(|| Spanned::from(TypeError {
                            msg: "Struct field name must be a plain identifier".to_string()
                        }, arg.span))?
                        .to_string();
                    (fname, &*a.value)
                },
                _ => return Err(Spanned::from(TypeError {
                    msg: format!("Struct construction requires named fields, e.g. {}(field=value)", name)
                }, arg.span)),
            };
            if let Some(_prev) = seen.get(&fname) {
                return Err(Spanned::from(TypeError {
                    msg: format!("Duplicate field '{}' in construction of {}", fname, name)
                }, arg.span));
            }
            let field_ty = field_defs.iter().find(|(n, _)| n == &fname)
                .map(|(_, t)| t.clone())
                .ok_or_else(|| Spanned::from(TypeError {
                    msg: format!("Struct {} has no field '{}'", name, fname)
                }, arg.span))?;
            let value_ty = self.infer(value_expr)?;
            let resolved_value_ty = self.lookup(&value_ty);
            let resolved_field_ty = self.lookup(&field_ty);
            if !(widens_to(&resolved_value_ty, &resolved_field_ty) || self.unify(&value_ty, &field_ty)) {
                return Err(Spanned::from(TypeError {
                    msg: format!("Field '{}' of {} expects {}, got {}", fname, name, resolved_field_ty, resolved_value_ty)
                }, value_expr.span));
            }
            seen.insert(fname, arg.span);
        }
        for (fname, _) in field_defs {
            if !seen.contains_key(fname) {
                return Err(Spanned::from(TypeError {
                    msg: format!("Missing field '{}' in construction of {}", fname, name)
                }, span));
            }
        }
        Ok(Type::Struct(name.to_string()))
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
                let res = self.check(&func.body, &*result)?;
                self.ctx = prev_ctx;
                return Ok(res);
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
    ///   T ≤ T | U  (T is a member of any union it belongs to)
    ///   T | U ≤ V  iff T ≤ V and U ≤ V
    pub fn is_subtype(&self, sub: &Type, sup: &Type) -> bool {
        let sub = self.lookup(sub);
        let sup = self.lookup(sup);
        if sub == sup { return true; }
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
            if !matches!(&resolved, Type::TypeVar { .. }) && !type_implements(&resolved, &tr) {
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
                if bounds.iter().all(|b| variants.iter().all(|v| type_implements(v, b))) {
                    self.substitutions.insert(name.clone(), t1.clone());
                    true
                } else {
                    false
                }
            },
            (Type::TypeVar { name, bounds }, Type::Union(variants)) => {
                if bounds.iter().all(|b| variants.iter().all(|v| type_implements(v, b))) {
                    self.substitutions.insert(name.clone(), t2.clone());
                    true
                } else {
                    false
                }
            },
            // Bounded TypeVar on left, concrete type on right.
            (Type::TypeVar { name, bounds }, _) => {
                if !bounds.iter().all(|b| type_implements(&t2, b)) {
                    return false;
                }
                self.substitutions.insert(name.clone(), t2.clone());
                true
            },
            // Concrete type on left, bounded TypeVar on right.
            (_, Type::TypeVar { name, bounds }) => {
                if !bounds.iter().all(|b| type_implements(&t1, b)) {
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
                Token::Identifier(nm) => TypedExprKind::Var(nm),
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
                let cond = self.check_and_lower(*c.cond)?;

                // Each branch is its own scope (see the matching note on
                // `ConditionalExpr::infer`): a `let` inside one arm must not
                // remain bound once we're back outside the conditional.
                let prev_ctx = self.ctx.clone();
                let true_result = self.check_and_lower(*c.true_branch);
                self.ctx = prev_ctx;
                let true_branch = true_result?;

                let false_branch = match c.false_branch {
                    Some(fb) => {
                        let prev_ctx = self.ctx.clone();
                        let false_result = self.check_and_lower(*fb);
                        self.ctx = prev_ctx;
                        Some(Box::new(false_result?))
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
                let target_item = a.target.item;
                if let Expression::FieldAccess(fa) = target_item {
                    let base = fa.target.item.get_identifier()
                        .expect("parser only allows a bare identifier as a field-assign base")
                        .to_string();
                    let value = self.check_and_lower(*a.value)?;
                    TypedExprKind::FieldAssign { base, field: fa.field, value: Box::new(value) }
                } else {
                    let name  = target_item.get_identifier()
                        .expect("assignment target must be identifier").to_string();
                    let value = self.check_and_lower(*a.value)?;
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
                let body_result = self.check_and_lower(*f.body);
                self.ctx = prev_ctx;
                let body = body_result?;

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
                    for (fname, _) in &field_defs {
                        let idx = fields.iter().position(|(n, _)| n == fname)
                            .expect("field presence validated during infer");
                        ordered.push(fields.remove(idx));
                    }
                    TypedExprKind::StructInit { name, fields: ordered }
                } else {
                    let callable = self.check_and_lower(*c.callable)?;
                    let mut args = Vec::with_capacity(c.args.len());
                    for arg in c.args {
                        args.push(self.check_and_lower(arg)?);
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
                TypedExprKind::FieldAccess { target: Box::new(target), field: fa.field }
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
        Ok((fl.var, Box::new(iterable), cond, Box::new(body)))
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
            let lf = Spanned::from(TypedExpr { ty: fty.clone(), kind: TypedExprKind::FieldAccess { target: Box::new(l.clone()), field: fname.clone() } }, span);
            let rf = Spanned::from(TypedExpr { ty: fty.clone(), kind: TypedExprKind::FieldAccess { target: Box::new(r.clone()), field: fname.clone() } }, span);
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

    fn resolve_annotation(&self, annotation: &Expression, span: Span) -> TypeResult {
        match annotation {
            Expression::Literal(lit) => match &lit.token {
                Token::Identifier(name) => match name.as_str() {
                    "Int"   => Ok(Type::Int),
                    "Float" => Ok(Type::Float),
                    "Bool"  => Ok(Type::Bool),
                    "Str"   => Ok(Type::Str),
                    _ if self.struct_defs.contains_key(name) => Ok(Type::Struct(name.clone())),
                    _       => self.get(name, span),
                },
                _ => Err(Spanned::from(
                    TypeError { msg: format!("Expected identifier for type, got {}", annotation) },
                    span
                )),
            },
            // `(T -> U)` in a type annotation parses as a FunctionExpr.
            // Parameter *names* are the input type names; body is the return type.
            // e.g. `(Int -> Int)` → params=[Parameter{name:"Int"}], body=Literal("Int")
            // e.g. `(Int -> Int -> Bool)` → right-associative nesting is handled recursively.
            Expression::Function(func) => {
                let param_types: Result<Vec<Type>, _> = func.params.iter()
                    .map(|p| self.resolve_annotation(
                        &Expression::Literal(crate::frontend::expression::LiteralExpr {
                            token: Token::Identifier(p.name.clone())
                        }),
                        span
                    ))
                    .collect();
                let result_type = self.resolve_annotation(&func.body.item, span)?;
                Ok(Type::Function { params: param_types?, result: Box::new(result_type) })
            },
            _ => Err(Spanned::from(
                TypeError { msg: format!("Invalid type expression: {}", annotation) },
                span
            )),
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
            Token::Identifier(nm) => tc.get(&nm, span)?,
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
            let annotated_ty = tc.resolve_annotation(&annotation.item, span)?;
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
                        .map(|p| tc.resolve_annotation(&p.ty.as_ref().unwrap().item, span))
                        .collect();
                    let ret_ty = tc.resolve_annotation(&func.return_type.as_ref().unwrap().item, span)?;
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
                Ok(Type::Union(vec![true_type, false_type]))
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
                tc.resolve_annotation(&annotation.item, span)?
            } else {
                tc.fresh_var()
            };
            param_bindings.push((p.name.clone(), param_ty.clone()));
            param_types.push(param_ty);
        }

        let body_type = tc.with_context(param_bindings.into_iter(), |t| {
            if let Some(ret_ann) = &self.return_type {
                let ret_ty = t.resolve_annotation(&ret_ann.item, span)?;
                t.check(&self.body, &ret_ty)
            } else {
                t.infer(&self.body)
            }
        })?;

        let func_ty = Type::Function { params: param_types, result: Box::new(body_type) };
        Ok(tc.lookup(&func_ty))
    }
}
