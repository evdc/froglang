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
    /// The `none` literal — the singleton `Type::None` value. Compiles to
    /// the immediate `1` (same encoding as a nullary union member's tag
    /// `0` — see `gc::immediate_variant` — chosen for consistency, not
    /// because `None` is "variant 0" of anything).
    NoneLit,
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

    /// `target.field` — struct field access, or a nominal union's *common*
    /// field access (`enum_name: Some(name)` — the union member is boxed,
    /// so unlike a struct's `Variable`-backed leaf this reads out of heap
    /// memory; `None` for an ordinary struct). Result type is the field's
    /// type.
    FieldAccess {
        target:    TypedExprRef,
        field:     String,
        enum_name: Option<String>,
    },

    /// `base.field = value` — the struct "mutation" rebind-sugar: `base`
    /// (a plain local, never a nested path — see `Grammar::assign`) is
    /// rebound with `field` replaced by `value`. Result type is `Type::None`.
    FieldAssign {
        base:  String,
        field: String,
        value: TypedExprRef,
    },

    /// `Union.Variant(field=value, ...)` construction — bare or qualified,
    /// nullary or not, all normalize to this. `fields` are pre-ordered
    /// common-then-variant, matching `struct_fields`'s flattened layout
    /// order (see `StructInit`). `tag` is the variant's declaration index.
    /// Result type is the nominal union's resolved `Type::Union`.
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
        target:    TypedExprRef,
        enum_name: String,
        variant:   String,
        tag:       u32,
    },

    /// One of `variant`'s own declared fields (not a common field — those
    /// use ordinary `FieldAccess`). Only ever emitted already guarded by a
    /// preceding `IsVariant` check, so no runtime tag check happens here.
    VariantField {
        target:    TypedExprRef,
        enum_name: String,
        variant:   String,
        field:     String,
    },

    /// `return`, or `return value`. This node's own `.ty` (on the enclosing
    /// `TypedExpr`) is always `Type::Never` — see
    /// `TypeChecker::return_types`. Compiles to an unconditional exit: pop
    /// the shadow frame, then `return_`.
    Return(Option<TypedExprRef>),

    /// Coerce `value` (a strict, narrower member type) up into this node's
    /// own `.ty`, always an *anonymous* `Type::Union` (one with no
    /// matching `UnionDef` — see `TypeChecker::resolve_union`; a nominal
    /// union's own construction already produces its widened type
    /// directly, via `VariantInit`, so never needs this node). `tag` is
    /// `value`'s type's index in the union's own sorted member list
    /// (`Type::normalize`'s canonical order). Boxes `value` the same way
    /// `VariantInit` boxes a non-nullary member's fields — see
    /// `codegen::box_into_variant` — except when `value.item.ty` is
    /// `Type::None`, which is the immediate `1` already and needs no
    /// allocation at all.
    Widen {
        value: TypedExprRef,
        tag:   u32,
    },

    /// The inverse of `Widen`: `value` (an anonymous-`Type::Union`-typed
    /// expression, already known — from a preceding `TypeTag` test — to
    /// currently hold this node's own `.ty`) extracted back out as a plain
    /// value of that type. Unboxes the payload `Widen` wrote; a target
    /// type of `Type::None` reads nothing (there is no payload to read —
    /// `None` carries no information beyond its own tag, already proven
    /// true by the preceding `TypeTag`).
    Narrow {
        value: TypedExprRef,
        tag:   u32,
    },

    /// `!`'s panicking arm (see `TypeChecker::lower_unwrap`). Always typed
    /// `Type::Never`, exactly like `Return`: it never yields a value, so
    /// codegen prints `message` then terminates the current block with a
    /// trap and opens a fresh (dead) one for whatever follows, the same
    /// shape `Return`'s own codegen uses. `message` is always `Str`-typed.
    Panic {
        message: TypedExprRef,
    },

    /// `target is <TypeName>` where `target`'s static type is an
    /// *anonymous* `Type::Union` — a structural type test, the anonymous
    /// counterpart of `IsVariant`'s nominal one. `tag` is the tested
    /// type's index in the union's own sorted member list, consistent
    /// with `Widen`/`Narrow`'s numbering for the same union. Result type
    /// is `Type::Bool`.
    TypeTag {
        target: TypedExprRef,
        tag:    u32,
    },
}
