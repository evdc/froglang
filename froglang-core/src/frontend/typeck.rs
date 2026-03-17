use std::{collections::HashMap, fmt::Display, vec};

use crate::frontend::{
    expression::{AssignExpr, BinaryExpr, ConditionalExpr, Expression, FunctionExpr, LiteralExpr, UnaryExpr},
    tokens::{Span, Spanned, Token},
};
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
    match tr {
        Trait::Num => matches!(ty, Type::Int | Type::Float),
        Trait::Eq  => matches!(ty, Type::Int | Type::Float | Type::Bool | Type::Str),
        Trait::Ord => matches!(ty, Type::Int | Type::Float | Type::Str),
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
                seen.sort_by(|a, b| format!("{}", a).cmp(&format!("{}", b)));
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
        }
    }
}

pub struct TypeChecker {
    // Variable (value level) name -> Type
    ctx: HashMap<String, Type>,
    // TypeVar name -> Type
    substitutions: HashMap<String, Type>,
    next_id: u32,
}

impl TypeChecker {
    pub fn empty() -> Self {
        TypeChecker { ctx: HashMap::new(), substitutions: HashMap::new(), next_id: 0 }
    }

    pub fn new() -> Self {
        TypeChecker { ctx: TypeChecker::default_context(), substitutions: HashMap::new(), next_id: 0 }
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

    pub fn add_ctx(mut self, ctx: impl Iterator<Item=(String, Type)>) -> Self {
        for (k, v) in ctx { self.ctx.insert(k, v); }
        self
    }

    pub fn context(self) -> HashMap<String, Type> {
        self.ctx
    }

    fn default_context() -> HashMap<String, Type> {
        // Polymorphic operators are handled in infer_builtin_op; this context
        // holds only user-visible bindings added via assignment or add_ctx.
        HashMap::new()
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
            Expression::Block(stmts) => {
                if stmts.is_empty() {
                    return Ok(Type::None);
                }
                let mut last = Type::None;
                for stmt in stmts {
                    last = self.infer(stmt)?;
                }
                Ok(last)
            },
            Expression::Tuple(elems) => {
                if elems.is_empty() {
                    return Ok(Type::List(Box::new(Type::TypeVar { name: "a".to_string(), bounds: vec![] })));
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
        }
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
                let t = self.fresh_bounded_var(vec![Trait::Num]);
                for arg in args {
                    let argt = self.infer(arg)?;
                    if !self.unify(&argt, &t) {
                        let resolved_t    = self.lookup(&t);
                        let resolved_argt = self.lookup(&argt);
                        let msg = if matches!(resolved_t, Type::TypeVar { .. }) {
                            format!("Operator '{}' requires Num, got {}", op, resolved_argt)
                        } else {
                            format!("Operator '{}' got incompatible types: expected {}, got {}", op, resolved_t, resolved_argt)
                        };
                        return Err(Spanned::from(TypeError { msg }, arg.span));
                    }
                }
                Ok(self.lookup(&t))
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
                let t = self.fresh_bounded_var(vec![Trait::Ord]);
                for arg in args {
                    let argt = self.infer(arg)?;
                    if !self.unify(&argt, &t) {
                        let resolved_t    = self.lookup(&t);
                        let resolved_argt = self.lookup(&argt);
                        let msg = if matches!(resolved_t, Type::TypeVar { .. }) {
                            format!("Operator '{}' requires Ord, got {}", op, resolved_argt)
                        } else {
                            format!("Operator '{}' got incompatible types: expected {}, got {}", op, resolved_t, resolved_argt)
                        };
                        return Err(Spanned::from(TypeError { msg }, arg.span));
                    }
                }
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

    fn with_context<F, R>(
        &mut self,
        update_ctx: impl Iterator<Item = (String, Type)>,
        mut closure: F,
    ) -> R where F: FnMut(&mut Self) -> R,
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

    fn resolve_annotation(&self, annotation: &Expression, span: Span) -> TypeResult {
        match annotation {
            Expression::Literal(lit) => match &lit.token {
                Token::Identifier(name) => match name.as_str() {
                    "Int"   => Ok(Type::Int),
                    "Float" => Ok(Type::Float),
                    "Bool"  => Ok(Type::Bool),
                    "Str"   => Ok(Type::Str),
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
            let true_type  = tc.infer(&self.true_branch)?;
            let false_type = if let Some(fb) = &self.false_branch {
                tc.infer(fb)?
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
