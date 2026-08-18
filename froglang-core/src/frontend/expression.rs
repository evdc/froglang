// expression.rs

use std::{fmt, ops::Range};
use crate::frontend::tokens::{Position, Spanned, Token};

// alias for conciseness
type ExprRef = Box<Spanned<Expression>>;

#[derive(Debug, Clone, PartialEq)]
pub struct LiteralExpr {
    pub token: Token
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnaryExpr {
    pub op: Token,
    pub expr: ExprRef
}

#[derive(Debug, Clone, PartialEq)]
pub struct BinaryExpr  {
    pub op: Token,
    pub left: ExprRef,
    pub right: ExprRef
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConditionalExpr {
    pub cond: ExprRef,
    pub true_branch: ExprRef,
    pub false_branch: Option<ExprRef>
}

#[derive(Debug, Clone, PartialEq)]
pub struct AssignExpr {
    pub target: ExprRef,
    pub typ: Option<ExprRef>,
    pub value: ExprRef
}

#[derive(Debug, Clone, PartialEq)]
pub struct Parameter { 
    pub name: String,
    // Type annotations are carried as expressions
    // until type checking, since only the TypeChecker has enough context to resolve them to a Type.
    pub ty: Option<ExprRef>
}

#[derive(Debug, Clone, PartialEq)]
pub struct FunctionExpr {
    pub params: Vec<Parameter>,
    pub body: ExprRef,
    pub return_type: Option<ExprRef>
}

#[derive(Debug, Clone, PartialEq)]
pub struct CallExpr {
    pub callable: ExprRef,
    pub args: Vec<Spanned<Expression>>
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnnotatedExpr {
    pub expr: ExprRef,
    pub ty: ExprRef     // Will be resolved into an actual Type in the checker
}

#[derive(Debug, Clone, PartialEq)]
pub struct IndexExpr {
    pub target: ExprRef,
    pub index: ExprRef
}

#[derive(Debug, Clone, PartialEq)]
pub struct SliceExpr {
    pub target: ExprRef,
    pub start: Option<ExprRef>,
    pub end: Option<ExprRef>
}

/// `start..end` used as a standalone expression (not as an index): both
/// bounds are required, and it eagerly materializes a `List(Int)`.
/// See `SliceExpr` for the `target[start..end]` form, which allows either
/// bound to be omitted.
#[derive(Debug, Clone, PartialEq)]
pub struct RangeExpr {
    pub start: ExprRef,
    pub end: ExprRef
}

/// `for var in iterable (if cond)? do body`. As a bare statement it runs for
/// effect and evaluates to `Type::None`; wrapped in `[...]` (see
/// `Expression::Comprehension`) it collects each `body` evaluation into a
/// `List` instead. Both forms share this same node.
#[derive(Debug, Clone, PartialEq)]
pub struct ForLoopExpr {
    pub var: String,
    pub iterable: ExprRef,
    pub cond: Option<ExprRef>,
    pub body: ExprRef
}

/// `data Name(field: Type, ...)`. Field type annotations stay as unresolved
/// expressions (see `Parameter`) until type checking, mirroring function
/// params exactly — reuses `Parameter` rather than a new struct.
#[derive(Debug, Clone, PartialEq)]
pub struct DataDeclExpr {
    pub name:   String,
    pub fields: Vec<Parameter>,
}

/// `target.field` — struct field access. Also used, with `target.field`
/// on the left of `=`, as the desugaring target for the `alice.age = 43`
/// rebind-sugar (see `Grammar::assign`'s target validation, and
/// `TypedExprKind::FieldAssign`).
#[derive(Debug, Clone, PartialEq)]
pub struct FieldAccessExpr {
    pub target: ExprRef,
    pub field:  String,
}

/// `import "./path.frog" { a, b }` or `import "./path.frog" as alias`.
/// Resolved and removed entirely by `crate::frontend::modules` before the
/// AST ever reaches the type checker — see that module for the merge
/// algorithm. Never appears in a `TypedExpr`.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportExpr {
    pub path: String,
    pub kind: ImportKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ImportKind {
    Named(Vec<String>),
    Qualified(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expression {
    Literal(LiteralExpr),
    Unary(UnaryExpr),
    Binary(BinaryExpr),
    Conditional(ConditionalExpr),
    Assign(AssignExpr),
    Function(FunctionExpr),
    Call(CallExpr),
    Tuple(Vec<Spanned<Expression>>),
    Block(Vec<Spanned<Expression>>),
    Annotated(AnnotatedExpr),
    Index(IndexExpr),
    Slice(SliceExpr),
    Range(RangeExpr),
    ForLoop(ForLoopExpr),
    /// `[for var in iterable (if cond)? body]` — a `ForLoop` wrapped in
    /// brackets, which changes its meaning from "discard, evaluate to
    /// None" to "collect each body value into a List". Kept as a distinct
    /// variant (rather than a flag on `ForLoopExpr`) so typeck/codegen can
    /// match on it directly instead of a match-inside-a-match.
    Comprehension(ExprRef),
    DataDecl(DataDeclExpr),
    FieldAccess(FieldAccessExpr),
    Import(ImportExpr),
}


impl Expression {
    pub fn at(self, span: Range<(u32, u32)>) -> Spanned<Expression> {
        Spanned::new(self, Position {line: span.start.0, col: span.start.1}, Position {line: span.end.0, col: span.end.1})
    }

    pub fn literal( token: Token) -> Expression  {
        Expression::Literal(LiteralExpr { token })
    }

    pub fn unary(t: Token, expr: Spanned<Expression>) -> Expression {
        Expression::Unary(UnaryExpr { op: t, expr: Box::new(expr) })
    }

    pub fn binary(t: Token, l: Spanned<Expression>, r: Spanned<Expression>) -> Expression {
        Expression::Binary(BinaryExpr { op: t, left: Box::new(l), right: Box::new(r) })
    }

    pub fn conditional(cond: Spanned<Expression>, true_branch: Spanned<Expression>, false_branch: Option<Spanned<Expression>>) -> Expression {
        Expression::Conditional(ConditionalExpr { cond: Box::new(cond), true_branch: Box::new(true_branch), false_branch: false_branch.map(Box::new) })
    }

    pub fn assign(target: Spanned<Expression>, typ: Option<Spanned<Expression>>, value: Spanned<Expression>) -> Expression {
        Expression::Assign(AssignExpr { target: Box::new(target), typ: typ.map(Box::new), value: Box::new(value) })
    }

    pub fn function(params: Vec<Parameter>, body: Spanned<Expression>) -> Expression {
        Expression::Function(FunctionExpr { params, body: Box::new(body), return_type: None })
    }

    pub fn function_with_return(params: Vec<Parameter>, body: Spanned<Expression>, return_type: Option<Spanned<Expression>>) -> Expression {
        Expression::Function(FunctionExpr { params, body: Box::new(body), return_type: return_type.map(Box::new) })
    }

    pub fn call(func: Spanned<Expression>, args: Vec<Spanned<Expression>>) -> Expression {
        Expression::Call(CallExpr { callable: Box::new(func), args })
    }

    pub fn annotated(expr: Spanned<Expression>, ty: Spanned<Expression>) -> Expression {
        Expression::Annotated(AnnotatedExpr { expr: Box::new(expr), ty: Box::new(ty) })
    }

    pub fn index(target: Spanned<Expression>, index: Spanned<Expression>) -> Expression {
        Expression::Index(IndexExpr { target: Box::new(target), index: Box::new(index) })
    }

    pub fn slice(target: Spanned<Expression>, start: Option<Spanned<Expression>>, end: Option<Spanned<Expression>>) -> Expression {
        Expression::Slice(SliceExpr { target: Box::new(target), start: start.map(Box::new), end: end.map(Box::new) })
    }

    pub fn range(start: Spanned<Expression>, end: Spanned<Expression>) -> Expression {
        Expression::Range(RangeExpr { start: Box::new(start), end: Box::new(end) })
    }

    pub fn for_loop(var: String, iterable: Spanned<Expression>, cond: Option<Spanned<Expression>>, body: Spanned<Expression>) -> Expression {
        Expression::ForLoop(ForLoopExpr { var, iterable: Box::new(iterable), cond: cond.map(Box::new), body: Box::new(body) })
    }

    pub fn comprehension(for_loop: Spanned<Expression>) -> Expression {
        Expression::Comprehension(Box::new(for_loop))
    }

    pub fn field_access(target: Spanned<Expression>, field: String) -> Expression {
        Expression::FieldAccess(FieldAccessExpr { target: Box::new(target), field })
    }

    pub fn get_identifier(&self) -> Option<&str> {
        match self {
            Expression::Literal(lit) => match &lit.token {
                Token::Identifier(name) => Some(&name),
                _ => None
            },
            _ => None
        }
    }
}


impl fmt::Display for Expression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expression::Literal(lit) => {
                write!(f, "{}", lit.token)
            }
            
            Expression::Unary(unary) => {
                write!(f, "{}{}", unary.op, unary.expr)
            }
            
            Expression::Binary(binary) => {
                write!(f, "({} {} {})", binary.left, binary.op, binary.right)
            }
            
            Expression::Conditional(cond) => {
                match &cond.false_branch {
                    Some(false_branch) => {
                        write!(f, "if {} then {} else {}", 
                               cond.cond, 
                               cond.true_branch, 
                               false_branch)
                    }
                    None => {
                        write!(f, "if {} then {}", 
                               cond.cond, 
                               cond.true_branch)
                    }
                }
            }

            Expression::Assign(assign) => {
                write!(f, "{} = {}", assign.target, assign.value)
            }
            
            Expression::Function(func) => {
                write!(f, "fn {:?} -> {}", func.params, func.body)
            }
            
            Expression::Call(call) => {
                if call.args.is_empty() {
                    write!(f, "{}()", call.callable)
                } else if call.args.len() == 1 {
                    write!(f, "{}({})", call.callable, call.args[0])
                } else {
                    write!(f, "{}(", call.callable)?;
                    for (i, arg) in call.args.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}", arg)?;
                    }
                    write!(f, ")")
                }
            }
            
            Expression::Tuple(elements) => {
                if elements.is_empty() {
                    write!(f, "()")
                } else if elements.len() == 1 {
                    // Single element tuple needs trailing comma to distinguish from parentheses
                    write!(f, "({},)", elements[0])
                } else {
                    write!(f, "(")?;
                    for (i, elem) in elements.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}", elem)?;
                    }
                    write!(f, ")")
                }
            }
            
            Expression::Block(statements) => {
                if statements.is_empty() {
                    write!(f, "{{}}")
                } else if statements.len() == 1 {
                    write!(f, "{{ {} }}", statements[0])
                } else {
                    write!(f, "{{ ")?;
                    for (i, stmt) in statements.iter().enumerate() {
                        if i > 0 {
                            write!(f, "; ")?;
                        }
                        write!(f, "{}", stmt)?;
                    }
                    write!(f, " }}")
                }
            }

            Expression::Annotated(inner) => {
                write!(f, "{} : {}", inner.expr, inner.ty)
            }

            Expression::Index(idx) => {
                write!(f, "{}[{}]", idx.target, idx.index)
            }

            Expression::Slice(s) => {
                write!(f, "{}[", s.target)?;
                if let Some(start) = &s.start { write!(f, "{}", start)?; }
                write!(f, "..")?;
                if let Some(end) = &s.end { write!(f, "{}", end)?; }
                write!(f, "]")
            }

            Expression::Range(r) => {
                write!(f, "{}..{}", r.start, r.end)
            }

            Expression::ForLoop(fl) => {
                write!(f, "for {} in {}", fl.var, fl.iterable)?;
                if let Some(cond) = &fl.cond {
                    write!(f, " if {}", cond)?;
                }
                write!(f, " do {}", fl.body)
            }

            Expression::Comprehension(inner) => {
                write!(f, "[{}]", inner)
            }

            Expression::DataDecl(d) => {
                write!(f, "data {}(", d.name)?;
                for (i, p) in d.fields.iter().enumerate() {
                    if i > 0 { write!(f, ", ")?; }
                    match &p.ty {
                        Some(ty) => write!(f, "{}: {}", p.name, ty)?,
                        None => write!(f, "{}", p.name)?,
                    }
                }
                write!(f, ")")
            }

            Expression::FieldAccess(fa) => {
                write!(f, "{}.{}", fa.target, fa.field)
            }

            Expression::Import(i) => {
                match &i.kind {
                    ImportKind::Named(names) => write!(f, "import {:?} {{ {} }}", i.path, names.join(", ")),
                    ImportKind::Qualified(alias) => write!(f, "import {:?} as {}", i.path, alias),
                }
            }
        }
    }
}

