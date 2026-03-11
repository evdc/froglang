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

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    None,
    Int,
    Float,
    Bool,
    Str,
    Function { params: Vec<Type>, result: Box<Type>},
    TypeVar { name: String }
}

impl Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Type::Int => write!(f, "Int"),
            Type::Float => write!(f, "Float"),
            Type::Bool => write!(f, "Bool"),
            Type::Str => write!(f, "Str"),
            Type::None => write!(f, "None"),
            Type::Function { params, result} => write!(f, "{} -> {}", format_vec(params), result),
            Type::TypeVar { name } => write!(f, "~{}", name)
        }
    }
}

pub struct TypeChecker {
    // Variable (at the value level) name -> Type
    ctx: HashMap<String, Type>,
    // TypeVar name -> Type
    substitutions: HashMap<String, Type>,
    next_id: u32
}

// two ways to handle Dynamic
// - just allow it to pass through, let it fail at runtime
// - treat it as a base type, make it a TypeError to pass Dynamic to something expecting another type;
//   insert runtime checks as necessary.

impl TypeChecker {
    pub fn empty() -> Self {
        TypeChecker { ctx: HashMap::new(), substitutions: HashMap::new(), next_id: 0 }
    }

    pub fn new() -> Self {
        TypeChecker { ctx: TypeChecker::default_context(), substitutions: HashMap::new(), next_id: 0 }
    }

    pub fn fresh_var(&mut self) -> Type {
        let v = Type::TypeVar { name: format!("t{}", self.next_id) };
        self.next_id += 1;
        return v
    }

    pub fn add_ctx(mut self, ctx: impl Iterator<Item=(String, Type)>) -> Self {
        let _ = ctx.map(|(k, v)| self.ctx.insert(k, v));
        self
    }

    pub fn context(self) -> HashMap<String, Type> {
        self.ctx
    }

    fn default_context() -> HashMap<String, Type> {
        // Arithmetic/logical operators are treated as calls to fake "functions" for type checking purposes,
        // e.g. +: Int, Int -> Int
        // so far, no polymorphism of any kind ...
        HashMap::from([
            ("<unaryminus>".to_string(), Type::Function { params: vec![Type::Int], result: Box::new(Type::Int) }),
            ("not".to_string(), Type::Function { params: vec![Type::Bool], result: Box::new(Type::Bool) }),
            ("+".to_string(), Type::Function { params: vec![Type::Int, Type::Int], result: Box::new(Type::Int) }),
            ("-".to_string(), Type::Function { params: vec![Type::Int, Type::Int], result: Box::new(Type::Int) }),
            ("*".to_string(), Type::Function { params: vec![Type::Int, Type::Int], result: Box::new(Type::Int) }),
            ("/".to_string(), Type::Function { params: vec![Type::Float, Type::Float], result: Box::new(Type::Float) }),
            ("==".to_string(), Type::Function { params: vec![Type::Int, Type::Int], result: Box::new(Type::Bool) }),
            ("!=".to_string(), Type::Function { params: vec![Type::Int, Type::Int], result: Box::new(Type::Bool) }),
            (">=".to_string(), Type::Function { params: vec![Type::Int, Type::Int], result: Box::new(Type::Bool) }),
            ("<=".to_string(), Type::Function { params: vec![Type::Int, Type::Int], result: Box::new(Type::Bool) }),
            ("<".to_string(), Type::Function { params: vec![Type::Int, Type::Int], result: Box::new(Type::Bool) }),
            (">".to_string(), Type::Function { params: vec![Type::Int, Type::Int], result: Box::new(Type::Bool) }),
        ])
    }

    pub fn infer(&mut self, expr: &Spanned<Expression>) -> TypeResult {
        match &expr.item {
            Expression::Literal(inner) => inner.infer(self, expr.span),
            Expression::Unary(inner) => inner.infer(self, expr.span),
            Expression::Binary(inner) => inner.infer(self, expr.span),
            Expression::Conditional(inner) => inner.infer(self, expr.span),
            Expression::Assign(inner) => inner.infer(self, expr.span),
            Expression::Function(inner) => inner.infer(self, expr.span),
            Expression::Call(inner) => self.infer_call(&inner.callable, &inner.args),
            _ => return Err(Spanned::from(TypeError { msg: format!("Unhandled expression {}", expr) }, expr.span))
        }
    }

    pub fn check(&mut self, expr: &Spanned<Expression>, expected_ty: &Type) -> TypeResult {
        if let Expression::Function(func) = &expr.item {
            if let Type::Function { params, result } = &expected_ty {
                if func.params.len() != params.len() {
                    return Err(Spanned::from(TypeError { msg: format!("Wrong number of arguments, expected {}, got {}", params.len(), func.params.len()) }, expr.span));
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
                return Err(Spanned::from(TypeError { msg: "Expected function type".to_string() }, expr.span))
            }
        };

        // General case. todo replace equals check with subtype check (or unify?)
        let inferred = self.infer(expr)?;
        if inferred == *expected_ty {
            Ok(inferred)
        } else {
            Err(Spanned::from(TypeError { msg: format!("Expected {} got {}", expected_ty, inferred) }, expr.span))
        }
    }

    fn infer_call(&mut self, callable: &Spanned<Expression>, args: &Vec<Spanned<Expression>>) -> TypeResult {
        println!("=== Callable: {:?}, Args: {:?} ===", callable.item, args);
        let func_type = self.infer(callable)?;
        println!("Inferred func type: {:?}", func_type);

        if let Type::Function { params, result } = func_type {
            if args.len() != params.len() {
                return Err(Spanned::from(TypeError { msg: format!("Wrong number of arguments, expected {}, got {}", params.len(), args.len()) }, callable.span));
            }
            for (arg, param) in args.iter().zip(params) {
                let argt = self.infer(arg)?;
                if !self.unify(&argt, &param) {
                    return Err(Spanned::from(TypeError { msg: format!("Can't unify {:?} and {:?}", arg, param) }, arg.span));
                }
            };
            println!("Context after unifying args: {:?}", self.substitutions);
            return Ok(self.lookup(&result));
        } else {
            return Err(Spanned::from(TypeError { msg: format!("Not callable: {}", callable.item) }, callable.span))
        };
    }

    fn fake_call(&mut self, fn_name: String, span: Span, args: Vec<&Spanned<Expression>>) -> TypeResult {
        // Skip some of the lookup steps but use the same core
        let func_type = self.get(&fn_name, span)?;
         if let Type::Function { params, result } = func_type {
            for (arg, param) in args.iter().zip(params) {
                let argt = self.infer(arg)?;
                if !self.unify(&argt, &param) {
                    return Err(Spanned::from(TypeError { msg: format!("Can't unify {:?} and {:?}", arg, param) }, arg.span));
                }
            };
            return Ok(self.lookup(&result));
        } else {
            unreachable!("should only have function types in this builtin map")
        }
    }

    // fn fresh_var(&mut self) -> Type {
    //     let t = Type::TypeVar { name: format!("t{}", self.next_id) };
    //     self.next_id += 1;
    //     t
    // }

    fn lookup(&self, ty: &Type) -> Type {
        match ty {
            Type::TypeVar { name } => {
                if let Some(found) = self.substitutions.get(name) {
                    self.lookup(found)
                } else {
                    ty.clone()
                }
            },
            Type::Function { params, result } => {
                Type::Function { 
                    params: params.iter().map(|ty| self.lookup(ty)).collect(), 
                    result: Box::new(self.lookup(&result))
                }
            },
            _ => ty.clone()
        }
    }

    fn get(&self, name: &str, span: Span) -> TypeResult {
        let ty = self.ctx.get(name).ok_or(
            Spanned::from(TypeError { msg: format!("Unbound variable {}", name)}, span)
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
        };

        match (&t1, &t2) {
            (Type::TypeVar { name }, _) => {
                self.substitutions.insert(name.clone(), t2.clone());
                return true;
            },
            (_, Type::TypeVar { name }) => {
                self.substitutions.insert(name.clone(), t1.clone());
                return true;
            },
            (Type::Function { params: p1, result: r1 }, Type::Function { params: p2, result: r2 }) => {
                if p1.len() != p2.len() {
                    return false;
                }
                p1.iter().zip(p2.iter()).all(|(l, r)| self.unify(l, r)) &&
                self.unify(&r1, &r2)
            },
            (_, _) => return false
        };
        return false;
    }

    fn resolve_annotation(&self, annotation: &Expression, span: Span) -> TypeResult {
        match annotation {
            Expression::Literal(lit) => match &lit.token {
                Token::Identifier(name) => self.get(&name, span),
                _ => return Err(Spanned::from(
                    TypeError { msg: format!("Expected identifier for type, got {}", annotation) }, 
                    span
                ))
            },
            _ => return Err(Spanned::from(
                TypeError { msg: format!("Invalid type expression: {}", annotation) }, 
                span
            ))
        }
    }
}

pub trait Infer {
    fn infer(&self, tc: &mut TypeChecker, span: Span) -> TypeResult;
}

impl Infer for LiteralExpr {
    fn infer(&self, tc: &mut TypeChecker, span: Span) -> TypeResult {
        let ty = match &self.token {
            Token::Float(_) => Type::Float,
            Token::Int(_) => Type::Int,
            Token::String(_) => Type::Str, 
            Token::False => Type::Bool,
            Token::True => Type::Bool,
            Token::Identifier(nm) => tc.get(&nm, span)?,
            _ => unreachable!("weird literal")
        };
        Ok(ty)
    }
}

impl Infer for UnaryExpr {
    fn infer(&self, tc: &mut TypeChecker, span: Span) -> TypeResult {
        let ty = match self.op {
            Token::Minus => tc.fake_call("<unaryminus>".to_string(), span, vec![&*self.expr])?,
            Token::Not => tc.fake_call("not".to_string(), span, vec![&*self.expr])?,
            _ => unreachable!("weird unary")
        };
        Ok(ty)
    }
}

impl Infer for BinaryExpr {
    fn infer(&self, tc: &mut TypeChecker, span: Span) -> TypeResult {
        let fn_name = format!("{}", self.op);
        let ty = tc.fake_call(fn_name, span, vec![&*self.left, &*self.right])?;
        Ok(ty)
    }
}

impl Infer for AssignExpr {
    fn infer(&self, tc: &mut TypeChecker, span: Span) -> TypeResult {
        // extract the name from target, which currently can only be a single identifier
        let name = &self.target.item.get_identifier().expect("should have validated in parsing");
        // If a type annotation is provided, check the value against it
        let ty = if let Some(annotation) = &self.typ {
            let annotated_ty = tc.resolve_annotation(&annotation.item, span)?;
            tc.check(&*self.value, &annotated_ty)?
        } else {
            // If a type annotation is not provided, infer the value's type
            tc.infer(&self.value)?
        };
        // Then add this name/type to context for future checking
        tc.ctx.insert(name.to_string(), ty.clone());
        Ok(ty)
    }
}

impl Infer for ConditionalExpr {
    fn infer(&self, tc: &mut TypeChecker, span: Span) -> TypeResult {
        let cond_type = tc.infer(&self.cond)?;
        if let Type::Bool = cond_type {
            // todo - we can do narrowing in here, no?
            let true_type = tc.infer(&self.true_branch)?;
            let false_type = if let Some(fb) = &self.false_branch {
                tc.infer(fb)?
            } else {
                Type::None
            };
            // todo - when we have unions, the type of an if expr is the union of its branches
            // for now they must unify
            if !tc.unify(&true_type, &false_type) {
                return Err(Spanned::from(TypeError { msg: "Conditional branches must both unify".to_string() }, span))
            }
            Ok(true_type)
        } else {
            return Err(Spanned::from(TypeError { msg: format!("Incorrect type {:?} for conditional, should be bool", cond_type) }, span));
        }
    }
}

// // This variant infers function param/return types, like ML-family whole-program inference
// Expression::Function(func) => {
//     // todo, there's too much string cloning in here
//     let prev_ctx = self.ctx.clone();
//     let mut body_ctx = prev_ctx.clone();
//     let mut param_types = Vec::new();
//     for p in &func.params {
//         let v = self.fresh_var();
//         body_ctx.insert(p.name.clone(), v.clone());
//         param_types.push(v);
//     }
//     self.ctx = body_ctx;
//     let body_type = self.infer( &func.body)?;
//     self.ctx = prev_ctx;
//     Type::Function { params: param_types, result: Box::new(body_type) }
// }

impl Infer for FunctionExpr {
    fn infer(&self, tc: &mut TypeChecker, span: Span) -> TypeResult {
        let mut param_types = Vec::new();
        let mut param_bindings = Vec::new();

        for p in &self.params {
            let param_ty = if let Some(annotation) = &p.ty { 
                tc.resolve_annotation(&annotation.item, span)?
            } else { tc.fresh_var() };
            param_bindings.push((p.name.clone(), param_ty.clone()));
            param_types.push(param_ty);
        }

        let body_type = tc.with_context(param_bindings.into_iter(), |t| {
            // If a result type is given, check against it (with param types as context)
            // If not, infer a result type (with param types as context)
            if let Some(ret_ann) = &self.return_type {
                t.check(&self.body, ret_ann)
            } else {
                t.infer(&self.body)
            }
        })?;

        Ok(Type::Function { params: param_types, result: Box::new(body_type) })
    }
}

