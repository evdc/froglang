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

    /// `Name(field=value, ...)` struct construction, already reordered into
    /// declared-field order. Result type is `Type::Struct(name)`.
    StructInit {
        name:   String,
        fields: Vec<(String, TypedExprRef)>,
    },

    /// `target.field` — struct field access. Result type is the field's type.
    FieldAccess {
        target: TypedExprRef,
        field:  String,
    },

    /// `base.field = value` — the struct "mutation" rebind-sugar: `base`
    /// (a plain local, never a nested path — see `Grammar::assign`) is
    /// rebound with `field` replaced by `value`. Result type is `Type::None`.
    FieldAssign {
        base:  String,
        field: String,
        value: TypedExprRef,
    },

    /// `Enum.Variant(field=value, ...)` construction — bare or qualified,
    /// nullary or not, all normalize to this. `fields` are pre-ordered
    /// common-then-variant, matching `struct_fields`'s flattened layout
    /// order (see `StructInit`). `tag` is the variant's declaration index.
    /// Result type is `Type::Enum(enum_name)`.
    VariantInit {
        enum_name: String,
        variant:   String,
        tag:       u32,
        fields:    Vec<(String, TypedExprRef)>,
    },

    /// `target is Variant` — a runtime tag test, no bindings (bindings are
    /// only ever reachable via `match`/`if ... is`'s desugaring into
    /// `Conditional` + `VariantField`, never directly on this node — see
    /// `TypeChecker::lower_match`). Result type is `Type::Bool`.
    IsVariant {
        target:  TypedExprRef,
        variant: String,
        tag:     u32,
    },

    /// One of `variant`'s own declared fields (not a common field — those
    /// use ordinary `FieldAccess`). Only ever emitted already guarded by a
    /// preceding `IsVariant` check, so no runtime tag check happens here.
    VariantField {
        target:  TypedExprRef,
        variant: String,
        field:   String,
    },

    /// `return`, or `return value`. This node's own `.ty` (on the enclosing
    /// `TypedExpr`) is always `Type::Never` — see
    /// `TypeChecker::return_types`. Compiles to an unconditional exit: pop
    /// the shadow frame, then `return_`.
    Return(Option<TypedExprRef>),
}
