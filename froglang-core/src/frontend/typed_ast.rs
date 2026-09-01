use crate::frontend::tokens::{Spanned, Token};
use crate::frontend::typeck::Type;

/// Convenience alias, parallel to `ExprRef` in the untyped AST.
pub type TypedExprRef = Box<Spanned<TypedExpr>>;

/// Identifies one occurrence of a `TypedExpr` node, for analyses (liveness)
/// that need to key results to a specific position in the tree rather than
/// to its structural shape. `0` means "unnumbered" — every node starts here
/// at construction time, since lowering clones subtrees (a guarded match
/// arm's `tail`, a `catch` handler inlined per error member) and assigning
/// real ids during construction would let clones share one id. Real ids are
/// assigned afterward, in one pass, by `liveness::number_nodes`; nothing
/// may look one up before that pass has run.
pub type NodeId = u32;

/// Semantic content of a type-checked node.
/// Wrap in `Spanned<TypedExpr>` for a located value.
/// All `TypeVar`s reachable from `ty` are fully resolved by `check_and_lower`.
#[derive(Debug, Clone)]
pub struct TypedExpr {
    /// See `NodeId`'s doc comment. Deliberately excluded from `PartialEq`
    /// below — tests compare lowered trees structurally (`tests/test_lower.rs`,
    /// `tests/test_typeck.rs`), and two structurally identical nodes built
    /// independently (e.g. in a test's expected value) have no reason to
    /// share an id.
    pub id:   NodeId,
    pub ty:   Type,
    pub kind: TypedExprKind,
}

impl PartialEq for TypedExpr {
    fn eq(&self, other: &Self) -> bool {
        self.ty == other.ty && self.kind == other.kind
    }
}

/// The trait-dispatch fallback for `for var in iterable do ...` — carried
/// on `TypedExprKind::ForLoop`/`Comprehension` when `iterable`'s type
/// provides `Iterable<Item>` rather than being List/Range-shaped
/// (`RANGES.md` Stage 2).
///
/// `iterable`'s value is bound once, before the loop, into a fresh internal
/// `mut` place named `iter_var` — the receiver `next_call`'s `Arg::Mut`
/// argument repeatedly mutates via the ordinary `mut`-parameter copy-in/
/// copy-out convention every other call already uses, so no new codegen is
/// needed for the *call* itself, only for the loop that re-runs it.
/// `next_call` is `<iter_var>.next()`, fully typed (`Item?`, i.e. `Item |
/// None`) — `codegen::compile_iterable_for_loop` recompiles it once per
/// iteration (the same node, into the same Cranelift loop-body block,
/// which runs repeatedly via a back edge — nothing here is re-lowered or
/// re-typechecked per iteration), tag-tests its result for the `None`
/// member to decide whether to exit, and narrows it to `Item` (via the
/// same union-unpacking codegen `TypedExprKind::Narrow` itself uses) to
/// bind the loop variable otherwise.
#[derive(Debug, Clone, PartialEq)]
pub struct IterVia {
    pub iter_var:  String,
    pub next_call: TypedExprRef,
}

/// One step of a `PlaceAssign` path — see its doc comment.
#[derive(Debug, Clone, PartialEq)]
pub enum PlaceSeg {
    Field(String),
    Index {
        /// Already type-checked (unified against `Int`) and lowered —
        /// evaluated once, at the point this step is reached.
        index: TypedExprRef,
        /// The indexed list's element type, resolved once by
        /// `TypeChecker::lower_place_assign` — codegen needs it to lay
        /// out the write (`struct_fields(elem_ty)`) but has no other way
        /// to recover it, since a path's later segments (if this isn't
        /// the last one) only know the element's *fields*, never its
        /// whole type.
        elem_ty: Type,
    },
}

/// A *mutable place*: a root binding plus a root-to-leaf path naming one
/// part of it (`MUTABILITY.md` Stage 8). `root` is always a mutable
/// binding — every constructor checks that before building one — and
/// `path` may be empty, which names the root itself.
///
/// Both of the language's in-place mutations address their target this
/// way: `PlaceAssign` writes a value *into* a place, and `Push` appends to
/// a place that is itself a `List`. Sharing one representation is what
/// lets them accept the same paths; they disagreed before Stage 8, so
/// `b.items[0] = v` was accepted while `push(mut b.items, v)` was not.
#[derive(Debug, Clone, PartialEq)]
pub struct Place {
    pub root: String,
    pub path: Vec<PlaceSeg>,
}

impl Place {
    /// The subexpressions a place evaluates, in evaluation order: its
    /// `[index]` steps. `root` is a binding name, not an expression, and a
    /// `.field` step is a static offset — so these are the only ones.
    pub fn index_exprs(&self) -> impl Iterator<Item = &Spanned<TypedExpr>> {
        self.path.iter().filter_map(|s| match s {
            PlaceSeg::Index { index, .. } => Some(&**index),
            PlaceSeg::Field(_) => None,
        })
    }

    pub fn index_exprs_mut(&mut self) -> impl Iterator<Item = &mut Spanned<TypedExpr>> {
        self.path.iter_mut().filter_map(|s| match s {
            PlaceSeg::Index { index, .. } => Some(&mut **index),
            PlaceSeg::Field(_) => None,
        })
    }
}

/// One argument at a call site.
///
/// A `mut` argument is a **place**, not a value: it names storage the callee
/// may write through, so it is resolved by `codegen::emit_place_ref` and
/// written back after the call, never evaluated as an ordinary expression.
/// Everything else is a value.
///
/// This is what makes mutating operations extensible (`MUTABILITY.md`
/// Stage 8). `push` briefly had a `TypedExprKind` variant of its own so that
/// its receiver could be a path — but a node per mutating operation costs a
/// handler in every walker in typeck, liveness, linear and codegen, and
/// would repeat for `pop`, `insert`, `remove` and every `Map` operation.
/// Carrying the place in the *argument* instead means a new mutating builtin
/// needs only its own codegen arm, and — the half the node could never give
/// — a user's own `func f(mut xs: List<Int>)` accepts `f(mut b.items)` too.
#[derive(Debug, Clone, PartialEq)]
pub enum Arg {
    Value(Spanned<TypedExpr>),
    Mut(Place),
}

impl Arg {
    /// The argument's value expression, or `None` for a `mut` place.
    pub fn value(&self) -> Option<&Spanned<TypedExpr>> {
        match self { Arg::Value(e) => Some(e), Arg::Mut(_) => None }
    }

    pub fn is_mut(&self) -> bool { matches!(self, Arg::Mut(_)) }

    /// Every subexpression this argument evaluates, in evaluation order.
    pub fn subexprs(&self) -> Box<dyn Iterator<Item = &Spanned<TypedExpr>> + '_> {
        match self {
            Arg::Value(e) => Box::new(std::iter::once(e)),
            Arg::Mut(p) => Box::new(p.index_exprs()),
        }
    }

    pub fn subexprs_mut(&mut self) -> Box<dyn Iterator<Item = &mut Spanned<TypedExpr>> + '_> {
        match self {
            Arg::Value(e) => Box::new(std::iter::once(e)),
            Arg::Mut(p) => Box::new(p.index_exprs_mut()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypedExprKind {
    IntLit(i64),
    FloatLit(f64),
    BoolLit(bool),
    StrLit(String),
    /// The `none` literal — the singleton `Type::None` value. Standing on
    /// its own it compiles to `gc::IMMEDIATE_NONE`, a word in the reserved
    /// `111` class that the collector never follows. As a *member* of a
    /// union it is not that at all: an inline union represents it as its
    /// member tag, a boxed one as `gc::immediate_variant` of its tag — see
    /// `TypedExprKind::Widen`.
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
        /// `(name, type, mutable)` — `mutable` is `true` only for a `mut`
        /// parameter (`MUTABILITY.md`) on a `func` declaration; codegen
        /// (`build_func_body`/`make_sig`) uses it to append the
        /// parameter's final value to the function's own return values,
        /// which the caller (`compile_expr_multi`'s `Call` arm) rebinds
        /// into the `mut`-marked argument's leaf `Variable`(s) — copy-in,
        /// copy-out, never a reference, since exclusivity
        /// (`TypeChecker::lower_call`) guarantees no aliasing to protect.
        params:      Vec<(String, Type, bool)>,
        return_type: Type,
        body:        TypedExprRef,
    },

    /// `mut_args[i]` is `true` iff `args[i]` was written `mut name` at this
    /// call site (`bump(mut a)`) — `TypeChecker::lower_call` has already
    /// validated it matches the callee's own declared parameter
    /// mutability exactly, so codegen (`compile_call`) can trust it
    /// without re-deriving anything: a `true` entry means the callee's
    /// final value for that parameter is one of its *extra* return values
    /// (see `Function`'s doc comment), to be copied back into `args[i]`'s
    /// own (already-validated-mutable) binding.
    Call { callable: TypedExprRef, args: Vec<Arg> },

    /// `target[index]` — list element access.
    Index { target: TypedExprRef, index: TypedExprRef },

    /// `target[start..end]` — list slice; either bound may be omitted
    /// (`target[..end]`, `target[start..]`, `target[..]`).
    Slice { target: TypedExprRef, start: Option<TypedExprRef>, end: Option<TypedExprRef> },

    /// `start..end` — standalone range, eagerly materialized as `List<Int>`.
    Range { start: TypedExprRef, end: TypedExprRef },

    /// Homogeneous list (parsed as `[a, b, c]`, inferred as `List<T>`).
    List(Vec<Spanned<TypedExpr>>),

    Block(Vec<Spanned<TypedExpr>>),

    /// `for var in iterable (if cond)? body`, run for effect. Result type
    /// is always `Type::None`.
    ForLoop {
        var:      String,
        iterable: TypedExprRef,
        cond:     Option<TypedExprRef>,
        body:     TypedExprRef,
        /// `Some` iff `iterable`'s type is neither List- nor Range-shaped
        /// but provides `Iterable<Item>` — the trait-dispatch fallback
        /// (`RANGES.md` Stage 2). Carries what `codegen::compile_iterable_for_loop`
        /// needs to run the loop by repeatedly calling the resolved `next`
        /// member rather than indexing: see `IterVia`'s own doc comment.
        /// `None` for every List/Range iterable — the ordinary, unchanged
        /// fast path — so this is never even inspected for those.
        iter_via: Option<IterVia>,
    },

    /// `[for var in iterable (if cond)? body]` — same shape as `ForLoop`,
    /// but each `body` evaluation is collected into a `List` instead of
    /// discarded. Result type is `List(body's type)`.
    Comprehension {
        var:      String,
        iterable: TypedExprRef,
        cond:     Option<TypedExprRef>,
        body:     TypedExprRef,
        iter_via: Option<IterVia>,
    },

    /// `Name(field=value, ...)` struct construction, already reordered into
    /// declared-field order. Result type is `Type::strukt(name)`.
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

    /// `root(.field | [index])* = value` — a *place* assignment
    /// (`MUTABILITY.md`): `root` must be a mutable binding
    /// (`TypeChecker::lower_assign` checks this before constructing the
    /// node), and `path` is always non-empty — a bare `name = value`
    /// reaches `TypedExprKind::Assign` instead, never this. Result type is
    /// `Type::None`.
    ///
    /// A path with no `Index` segment is the struct "mutation" rebind
    /// sugar generalized to any depth (`o.i.v = 5`): codegen rebinds the
    /// touched leaf `Variable`(s) directly, since a struct is a flat set
    /// of named bindings. A path containing an `Index` writes through a
    /// heap-allocated `FrogList` instead — see `codegen`'s
    /// `emit_place_ref`, which walks any number of them
    /// (`rows[y][x] = v`, `grid[y].cells[x] = v`).
    PlaceAssign {
        place: Place,
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
    /// `TypeChecker::return_types`. Compiles to an unconditional exit:
    /// `return_`, carrying each `mut` parameter's current value.
    Return(Option<TypedExprRef>),

    /// Coerce `value` (a strict, narrower member type) up into this node's
    /// own `.ty`, always an *anonymous* `Type::Union` (one with no
    /// matching `UnionDef` — see `TypeChecker::resolve_union`; a nominal
    /// union's own construction already produces its widened type
    /// directly, via `VariantInit`, so never needs this node). `tag` is
    /// `value`'s type's index in the union's own sorted member list
    /// (`Type::normalize`'s canonical order). For an inline union
    /// (`codegen::union_is_inline`) this allocates nothing: `value`'s own
    /// leaves are written into the union's columns and the tag rides in
    /// slot 0. For a boxed one it boxes `value` the same way `VariantInit`
    /// boxes a non-nullary member's fields — see
    /// `codegen::box_into_variant` — except for a `Type::None` value, which
    /// is an immediate and needs no allocation either.
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

    /// Coerce a non-`Bool` `Trait::Truthy` value (`Int`, `Float`, `Str`,
    /// `List`, or `None`) into `Bool` for condition position (`if`, a
    /// match guard, a `for`-loop guard, `and`/`or`/`not`) — see
    /// `TypeChecker::coerce_truthy`. Never wraps an already-`Bool` value;
    /// `0`/`0.0`/empty-`Str`/empty-`List`/`None` are falsey, everything
    /// else truthy. Result type is `Type::Bool`.
    Truthy(TypedExprRef),

    /// A non-lossy numeric promotion of the inner value — today only
    /// `Int -> Float` (`widens_to` is the authority on which pairs
    /// qualify). The target is this node's own `.ty`, so codegen just
    /// needs the inner value's type and this one's; see `coerce_value` in
    /// `codegen/mod.rs`, which the binary-operator path already uses for
    /// exactly the same job.
    ///
    /// Inserted by `TypeChecker::lower_widen` wherever a value flows into
    /// a wider declared slot: a struct/variant field, a list element, an
    /// annotation, a declared return type. Without it, inference accepted
    /// `data P(w: Float)` + `P(w=1)` (via `widens_to`) while lowering left
    /// the argument typed `Int`, so codegen wrote an `i64` into an `f64`
    /// slot and the Cranelift verifier rejected the function. Function
    /// *call arguments* were the one path that already worked, because
    /// codegen coerces those against the callee's signature — this makes
    /// every other slot behave the same way.
    Coerce(TypedExprRef),
}
