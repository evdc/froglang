use crate::frontend::tokens::{Spanned, Token};
use crate::frontend::typeck::Type;

/// Convenience alias, parallel to `ExprRef` in the untyped AST.
pub type TypedExprRef = Box<Spanned<TypedExpr>>;

/// Semantic content of a type-checked node.
/// Wrap in `Spanned<TypedExpr>` for a located value.
/// All `TypeVar`s reachable from `ty` are fully resolved by `check_and_lower`.
#[derive(Debug, Clone, PartialEq)]
pub struct TypedExpr {
    pub ty:   Type,
    pub kind: TypedExprKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypedExprKind {
    IntLit(i64),
    FloatLit(f64),
    BoolLit(bool),
    StrLit(String),
    /// Variable reference — name resolved from environment.
    Var(String),

    Unary  { op: Token, expr: TypedExprRef },
    Binary { op: Token, left: TypedExprRef, right: TypedExprRef },

    Conditional {
        cond:         TypedExprRef,
        true_branch:  TypedExprRef,
        false_branch: Option<TypedExprRef>,
    },

    /// `let name = value`.  The type annotation expression (if any) is
    /// absorbed into `TypedExpr.ty`; it is not stored here.
    Assign {
        name:  String,
        value: TypedExprRef,
    },

    /// Parameters are `(name, resolved_type)` — the `Option<ExprRef>` annotation
    /// expressions from the untyped AST are gone; types are resolved.
    Function {
        params:      Vec<(String, Type)>,
        return_type: Type,
        body:        TypedExprRef,
    },

    Call { callable: TypedExprRef, args: Vec<Spanned<TypedExpr>> },

    /// `target[index]` — list element access.
    Index { target: TypedExprRef, index: TypedExprRef },

    /// `target[start..end]` — list slice; either bound may be omitted
    /// (`target[..end]`, `target[start..]`, `target[..]`).
    Slice { target: TypedExprRef, start: Option<TypedExprRef>, end: Option<TypedExprRef> },

    /// `start..end` — standalone range, eagerly materialized as `List(Int)`.
    Range { start: TypedExprRef, end: TypedExprRef },

    /// Homogeneous list (parsed as `[a, b, c]`, inferred as `List(T)`).
    List(Vec<Spanned<TypedExpr>>),

    Block(Vec<Spanned<TypedExpr>>),

    /// `for var in iterable (if cond)? body`, run for effect. Result type
    /// is always `Type::None`.
    ForLoop {
        var:      String,
        iterable: TypedExprRef,
        cond:     Option<TypedExprRef>,
        body:     TypedExprRef,
    },

    /// `[for var in iterable (if cond)? body]` — same shape as `ForLoop`,
    /// but each `body` evaluation is collected into a `List` instead of
    /// discarded. Result type is `List(body's type)`.
    Comprehension {
        var:      String,
        iterable: TypedExprRef,
        cond:     Option<TypedExprRef>,
        body:     TypedExprRef,
    },
}
