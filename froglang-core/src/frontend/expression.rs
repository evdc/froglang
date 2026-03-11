// expression.rs

use std::{fmt, ops::Range};
use crate::frontend::{tokens::{Position, Spanned, Token}, typeck::Type};

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
    pub return_type: Option<Type>
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
    Annotated(AnnotatedExpr)
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

    pub fn call(func: Spanned<Expression>, args: Vec<Spanned<Expression>>) -> Expression {
        Expression::Call(CallExpr { callable: Box::new(func), args })
    }

    pub fn annotated(expr: Spanned<Expression>, ty: Spanned<Expression>) -> Expression {
        Expression::Annotated(AnnotatedExpr { expr: Box::new(expr), ty: Box::new(ty) })
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
        }
    }
}

