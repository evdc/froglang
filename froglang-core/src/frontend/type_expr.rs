// type_expr.rs
//
// The surface syntax of *types*, kept deliberately separate from
// `Expression`.
//
// Type annotations used to be carried as ordinary `Expression`s and sniffed
// apart in the type checker (`TypeChecker::resolve_annotation`), which had two
// costs. The small one: a type name was looked up in the *value* environment
// as a fallback, so a stray local could shadow a type. The large one: three of
// the four annotation sites — `func` params, `func` return types, and `data`
// fields — never went through the expression parser at all, they called
// `Parser::identifier()` directly, so an annotation had to be a single bare
// identifier. `AST | ParseError`, `Str?`, and even `List(Int)` were
// unspellable there.
//
// A type is not an expression and does not want the expression grammar's
// precedence table, so it gets its own small recursive-descent parser
// (`Grammar::type_expr`) and its own AST. Resolution to a `Type` is
// `TypeChecker::resolve_type_expr`.

use std::fmt;

use crate::frontend::tokens::Spanned;

/// Convenience alias, parallel to `ExprRef` in the untyped AST.
pub type TypeExprRef = Box<Spanned<TypeExpr>>;

#[derive(Debug, Clone, PartialEq)]
pub enum TypeExpr {
    /// `Int`, `Str`, `Shape` — a bare type name.
    Name(String),
    /// `List(Int)` — a named type applied to arguments. Parsed generally;
    /// which names actually accept arguments is a resolution-time question.
    Apply(String, Vec<Spanned<TypeExpr>>),
    /// `A | B | C`. Always at least two members — a one-member union is
    /// parsed as the member itself, not as a `Union`.
    Union(Vec<Spanned<TypeExpr>>),
    /// `T?` — sugar for `T | None`, expanded during resolution rather than
    /// parsing so that error messages can still say `T?`.
    Optional(TypeExprRef),
    /// `(A, B -> C)`. The parentheses are required; a bare `->` in
    /// annotation position is `ParseError::FunctionTypeNeedsParens`.
    Func(Vec<Spanned<TypeExpr>>, TypeExprRef),
}

impl fmt::Display for TypeExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeExpr::Name(name) => write!(f, "{}", name),
            TypeExpr::Apply(name, args) => {
                write!(f, "{}(", name)?;
                for (i, a) in args.iter().enumerate() {
                    if i > 0 { write!(f, ", ")?; }
                    write!(f, "{}", a.item)?;
                }
                write!(f, ")")
            }
            TypeExpr::Union(members) => {
                for (i, m) in members.iter().enumerate() {
                    if i > 0 { write!(f, " | ")?; }
                    write!(f, "{}", m.item)?;
                }
                Ok(())
            }
            TypeExpr::Optional(inner) => write!(f, "{}?", inner.item),
            TypeExpr::Func(params, result) => {
                write!(f, "(")?;
                for (i, p) in params.iter().enumerate() {
                    if i > 0 { write!(f, ", ")?; }
                    write!(f, "{}", p.item)?;
                }
                write!(f, " -> {})", result.item)
            }
        }
    }
}

impl TypeExpr {
    /// Every type *name* mentioned anywhere in this expression, as mutable
    /// references, in source order. `crate::frontend::modules` uses this to
    /// mangle type references across module boundaries without needing its
    /// own recursive walk over the type grammar.
    pub fn names_mut(&mut self) -> Vec<&mut String> {
        let mut out = Vec::new();
        self.collect_names_mut(&mut out);
        out
    }

    fn collect_names_mut<'a>(&'a mut self, out: &mut Vec<&'a mut String>) {
        match self {
            TypeExpr::Name(name) => out.push(name),
            TypeExpr::Apply(name, args) => {
                out.push(name);
                for a in args { a.item.collect_names_mut(out); }
            }
            TypeExpr::Union(members) => {
                for m in members { m.item.collect_names_mut(out); }
            }
            TypeExpr::Optional(inner) => inner.item.collect_names_mut(out),
            TypeExpr::Func(params, result) => {
                for p in params { p.item.collect_names_mut(out); }
                result.item.collect_names_mut(out);
            }
        }
    }
}
