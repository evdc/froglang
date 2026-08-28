// expression.rs

use std::{fmt, ops::Range};
use crate::frontend::tokens::{Position, Spanned, Token};
use crate::frontend::type_expr::TypeExpr;

// Type annotations are carried as `TypeExpr` (see `type_expr.rs`) rather than
// as `Expression`s: types have their own grammar, and only the TypeChecker
// has enough context to resolve one to a `Type`.

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

/// Whether an `AssignExpr` declares a fresh binding, and if so, whether
/// that binding accepts later reassignment. `None` means this node is an
/// assignment to an *existing* binding (`x = ...`, no `let`/`mut` keyword),
/// which `TypeChecker::lower_assign` resolves by lookup rather than by
/// introducing a name — see `MUTABILITY.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutability {
    Immutable,
    Mutable,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AssignExpr {
    pub target: ExprRef,
    pub typ: Option<Spanned<TypeExpr>>,
    pub value: ExprRef,
    /// `Some(Immutable)` for `let`, `Some(Mutable)` for `mut`, `None` for
    /// a plain `target = value` assignment to a binding declared earlier.
    pub decl: Option<Mutability>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Parameter {
    pub name: String,
    pub ty: Option<Spanned<TypeExpr>>,
    /// `func f(mut p: T)` — `p` writes back to the caller's argument at
    /// this call, rather than being an ordinary by-value copy. See
    /// `MUTABILITY.md`. Never set by lambda parameter syntax, which has
    /// no room for a `mut` marker.
    pub mutable: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FunctionExpr {
    pub params: Vec<Parameter>,
    pub body: ExprRef,
    pub return_type: Option<Spanned<TypeExpr>>
}

#[derive(Debug, Clone, PartialEq)]
pub struct CallExpr {
    pub callable: ExprRef,
    pub args: Vec<Spanned<Expression>>
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnnotatedExpr {
    pub expr: ExprRef,
    pub ty: Spanned<TypeExpr>   // Resolved into an actual Type in the checker
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
/// bounds are required, and it eagerly materializes a `List<Int>`.
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

/// One field in a struct/variant field list: `name: Type` (a named field)
/// or a bare `Type` (a positional/"tuple struct" field, `name: None`) —
/// see `Grammar::field_list`. Unlike `Parameter` (function params, where
/// the type is optional and the name never is), a field's type is always
/// required and its name is the part that may be absent — the two are
/// different enough shapes that reusing `Parameter` here would just make
/// both look partially-optional. A single field list is always
/// all-named or all-positional, never mixed (`Grammar::field_list`
/// rejects a mix at parse time) — checked per list, so e.g. a union's
/// common fields and a variant's own fields may independently pick either
/// style.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldDecl {
    pub name: Option<String>,
    pub ty:   Spanned<TypeExpr>,
}

/// One variant of an enum declaration: `Circle(r: Int)`, a positional
/// `Circle(Int)`, or a nullary `Red`. Fields (if any) are on top of
/// whatever common fields the enclosing `DataDeclExpr` declares.
#[derive(Debug, Clone, PartialEq)]
pub struct VariantDecl {
    pub name:   String,
    pub fields: Vec<FieldDecl>,
}

/// `data Name(field: Type, ...)` for a struct, or
/// `data Name(common: Type, ...) is A(...) | B(...)` for an enum.
/// An empty `variants` list means this is a plain struct declaration;
/// `fields` holds the struct's own fields, or the enum's common fields.
/// `provides` is the trait-name list from a trailing `provides X, Y`
/// clause (`ERRORS.md`'s must-handle mechanism) — `error X(...)` is sugar
/// that implies `provides Error` here without needing its own AST node.
/// For a union declaration, `provides` grants every variant the trait, not
/// the union alias itself (see `TypeChecker::hoist_data_decls`).
#[derive(Debug, Clone, PartialEq)]
pub struct DataDeclExpr {
    pub name:     String,
    /// `<A, B>` binder names declared right after the data name, before any
    /// field list — `TRAITS.md` Stage 3a. Empty for an ordinary
    /// (non-generic) declaration; never bound to a `TypeVar` here — that
    /// happens in `TypeChecker::hoist_data_decls`, which mints one fresh
    /// placeholder per name and resolves `fields`'s `TypeExpr`s against
    /// them.
    pub type_params: Vec<String>,
    pub fields:   Vec<FieldDecl>,
    pub variants: Vec<VariantDecl>,
    pub provides: Vec<String>,
}

/// A pattern matched against an enum value: `Circle(r)`, `Shape.Circle(r)`,
/// `Circle(_)`, or a bindless `Circle`. `binds` names the variant's
/// declared fields positionally in declaration order; `"_"` means "don't
/// bind this field". `path`, if present, qualifies `variant` with its
/// owning enum's name (`Shape.Circle`); `None` means the bare, inferred
/// form.
#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    pub path:    Option<String>,
    pub variant: String,
    pub binds:   Vec<String>,
    /// Set only by the type checker's own internally-synthesized patterns
    /// (`TypeChecker::union_entries`'s anonymous-union branch, used to
    /// desugar `?`/`!`/`catch`) — the pattern's member index into the
    /// subject's already-known `Type::Union` member list, bypassing
    /// `variant`'s name-based lookup (`resolve_type_name`) entirely. That
    /// lookup can only resolve a bare nominal/primitive name, but `variant`
    /// there is `Type::to_string()`'s *display* form, which for a compound
    /// member (`List<Int>`, a function type, a nested union) isn't a valid
    /// name at all (surface `is`-pattern syntax can't spell one either —
    /// `Grammar::pattern` only ever parses a single identifier) — so
    /// resolving it back by name was a lossy round-trip through `Display`,
    /// not a real lookup. Always `None` for a pattern the parser produced,
    /// since real source text has no way to populate it.
    pub resolved_member: Option<usize>,
}

/// One arm of a `match` expression: `is Pattern (and guard)? then body`.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub guard:   Option<ExprRef>,
    pub body:    ExprRef,
}

/// `match subject { is P1 then e1  is P2 and guard then e2  else e3 }`.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchExpr {
    pub subject: ExprRef,
    pub arms:    Vec<MatchArm>,
    pub default: Option<ExprRef>,
}

/// `subject is Pattern` — used standalone as a `Bool` test, or (when it is
/// the entire condition of an `if`) as sugar for a two-armed `Match` that
/// also binds the pattern's fields in the `then` branch. See
/// `TypeChecker::check_and_lower`'s `Conditional` arm.
#[derive(Debug, Clone, PartialEq)]
pub struct IsPatternExpr {
    pub subject: ExprRef,
    pub pattern: Pattern,
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
    /// `return`, or `return value`. Typed `Never` — see `frontend::typeck`'s
    /// `return_types` stack — so it unifies with whatever real value sits
    /// next to it on the other side of an `if`/`match` join.
    Return(Option<ExprRef>),
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
    Match(MatchExpr),
    IsPattern(IsPatternExpr),
    /// `e?` — error propagation (`ERRORS.md` Phase 5). Desugars entirely in
    /// `TypeChecker::lower_try` into a `match` over `e`'s union members, one
    /// `Error`-providing arm returning early per member, so codegen gains
    /// no new control flow of its own.
    Try(ExprRef),
    /// `e!` — panic-on-error. Same shape as `Try`, but an `Error`-providing
    /// arm calls the builtin `panic(msg: Str): Never` instead of returning
    /// (`TypeChecker::lower_unwrap`) — an ordinary call, not a dedicated
    /// node, so user code can call `panic` directly too.
    Unwrap(ExprRef),
    /// `value catch handler` — `handler` is either a plain fallback
    /// expression, or a single-parameter lambda (`[e] -> body`) whose body
    /// is inlined per `Error`-providing member with that parameter bound to
    /// it (`TypeChecker::lower_catch`).
    Catch { value: ExprRef, handler: ExprRef },
    /// `mut name` in call-argument position (`bump(mut a)`) — marks `name`
    /// as the target of a `mut` parameter at this call, per
    /// `MUTABILITY.md`. Grammar produces this only where `mut` is
    /// immediately followed by a bare identifier and *not* `:`/`=` (which
    /// would make it a declaration instead — see `Grammar::mut_prefix`);
    /// anywhere else `TypeChecker` rejects it, since it's only meaningful
    /// as one of `lower_call`'s own arguments.
    MutArg(ExprRef),
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

    pub fn assign(target: Spanned<Expression>, typ: Option<Spanned<TypeExpr>>, value: Spanned<Expression>, decl: Option<Mutability>) -> Expression {
        Expression::Assign(AssignExpr { target: Box::new(target), typ, value: Box::new(value), decl })
    }

    pub fn mut_arg(name: Spanned<Expression>) -> Expression {
        Expression::MutArg(Box::new(name))
    }

    pub fn function(params: Vec<Parameter>, body: Spanned<Expression>) -> Expression {
        Expression::Function(FunctionExpr { params, body: Box::new(body), return_type: None })
    }

    pub fn function_with_return(params: Vec<Parameter>, body: Spanned<Expression>, return_type: Option<Spanned<TypeExpr>>) -> Expression {
        Expression::Function(FunctionExpr { params, body: Box::new(body), return_type })
    }

    pub fn call(func: Spanned<Expression>, args: Vec<Spanned<Expression>>) -> Expression {
        Expression::Call(CallExpr { callable: Box::new(func), args })
    }

    pub fn annotated(expr: Spanned<Expression>, ty: Spanned<TypeExpr>) -> Expression {
        Expression::Annotated(AnnotatedExpr { expr: Box::new(expr), ty })
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

    pub fn is_pattern(subject: Spanned<Expression>, pattern: Pattern) -> Expression {
        Expression::IsPattern(IsPatternExpr { subject: Box::new(subject), pattern })
    }

    pub fn return_value(value: Option<Spanned<Expression>>) -> Expression {
        Expression::Return(value.map(Box::new))
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
                write!(f, "{} : {}", inner.expr, inner.ty.item)
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
                    match &p.name {
                        Some(name) => write!(f, "{}: {}", name, p.ty.item)?,
                        None => write!(f, "{}", p.ty.item)?,
                    }
                }
                write!(f, ")")?;
                if !d.variants.is_empty() {
                    write!(f, " is ")?;
                    for (i, v) in d.variants.iter().enumerate() {
                        if i > 0 { write!(f, " | ")?; }
                        write!(f, "{}", v.name)?;
                        if !v.fields.is_empty() {
                            write!(f, "(")?;
                            for (j, p) in v.fields.iter().enumerate() {
                                if j > 0 { write!(f, ", ")?; }
                                match &p.name {
                                    Some(name) => write!(f, "{}: {}", name, p.ty.item)?,
                                    None => write!(f, "{}", p.ty.item)?,
                                }
                            }
                            write!(f, ")")?;
                        }
                    }
                }
                Ok(())
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

            Expression::Match(m) => {
                write!(f, "match {} {{ ", m.subject)?;
                for arm in &m.arms {
                    write!(f, "is {}", fmt_pattern(&arm.pattern))?;
                    if let Some(g) = &arm.guard { write!(f, " and {}", g)?; }
                    write!(f, " then {}  ", arm.body)?;
                }
                if let Some(d) = &m.default { write!(f, "else {}", d)?; }
                write!(f, "}}")
            }

            Expression::IsPattern(ip) => {
                write!(f, "{} is {}", ip.subject, fmt_pattern(&ip.pattern))
            }

            Expression::Return(value) => match value {
                Some(v) => write!(f, "return {}", v),
                None => write!(f, "return"),
            },

            Expression::Try(inner) => write!(f, "{}?", inner),
            Expression::Unwrap(inner) => write!(f, "{}!", inner),
            Expression::Catch { value, handler } => write!(f, "{} catch {}", value, handler),
            Expression::MutArg(name) => write!(f, "mut {}", name),
        }
    }
}

fn fmt_pattern(p: &Pattern) -> String {
    let head = match &p.path {
        Some(path) => format!("{}.{}", path, p.variant),
        None => p.variant.clone(),
    };
    if p.binds.is_empty() {
        head
    } else {
        format!("{}({})", head, p.binds.join(", "))
    }
}

