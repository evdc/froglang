use std::{collections::{HashMap, HashSet}, fmt::Display, vec};

use crate::frontend::{
    expression::{
        AnnotatedExpr, AnnotationUse, AssignExpr, BinaryExpr, CallExpr,
        ConditionalExpr, Expression, FieldAccessExpr, FieldDecl, ForLoopExpr,
        FunctionExpr, IndexExpr, IsPatternExpr, LiteralExpr, MatchArm, Mutability, Parameter,
        Pattern, RangeExpr, SliceExpr, TraitMemberDecl, TypeParam, UnaryExpr,
    },
    tokens::{Span, Spanned, Token},
};
use crate::frontend::type_expr::TypeExpr;
use crate::frontend::typed_ast::{Arg, IterVia, Place, PlaceSeg, TypedExpr, TypedExprKind, TypedExprRef};

/// Reserved name for the builtin `panic` alias that `!` desugars to.
/// Contains `!`, which the lexer never produces inside an identifier, so
/// no user-written name can ever collide with or shadow it. See
/// `TypeChecker::default_context` and `build_unwrap_arms`.
const UNWRAP_PANIC_NAME: &str = "panic!builtin";

/// The type-variable name `Self` resolves to inside a `trait` declaration
/// (`TRAITS.md` Stage 5). A trait member's signature is a template over one
/// binder, and this is that binder — `resolve_type_name` finds it through
/// `type_param_scope`, the same mechanism a generic `data` declaration's
/// `<A, B>` binders already use, so `Self` needs no special case anywhere in
/// the type grammar or in unification.
const SELF_BINDER: &str = "Self";

/// Prefix of the placeholder callee symbol a member call through a
/// type-parameter bound carries between lowering and monomorphization —
/// see `pending_member_symbol`.
const PENDING_MEMBER_PREFIX: &str = "#member$";

#[derive(Debug)]
pub struct TypeError {
    pub msg: String
}

type TypeResult = Result<Type, Spanned<TypeError>>;

/// A struct/variant constructor's arguments, lowered and paired with the
/// field each fills — in declared field order, whether the call site wrote
/// them positionally or by name.
type RecordArgs = Vec<(String, TypedExprRef)>;

/// Traits constrain type variables. A type must implement a trait to be bound
/// to a TypeVar that carries that bound.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Trait {
    Num,   // Int, Float — arithmetic operators
    Eq,    // Int, Float, Bool, Str — == and !=
    Ord,   // Int, Float, Str — <, >, <=, >=
    /// Structural, derived alongside `Eq` with the same recursion rule
    /// (`plans/DATA.md` stage 1/5: "`Show`/`Eq` must be derived together
    /// with the same recursion rule" — a type printable but not comparable
    /// has an untestable round-trip law). Every primitive, `List<T>` (iff
    /// `T` is), and every struct (iff every field is, recursively) is
    /// `Show`; a `Type::Function` is not, matching `validate_codegen_
    /// constraints`'s existing rejection of function values reaching
    /// codegen at all. Consumed by `repr`'s `check_reprable`, the way
    /// `print` is predicted by `check_printable` without a trait.
    Show,
    /// Marker trait for fallible-function error types (`ERRORS.md`). Unlike
    /// the other three traits, no type implements this structurally — it's
    /// granted per-struct-name by a `provides Error` clause on a `data`
    /// declaration (or implied by the `error X(...)` shorthand), recorded in
    /// `TypeChecker.provides` and consulted only through `type_implements`.
    Error,
    /// Coercible to `Bool` in condition position only (`if`, a match guard,
    /// a `for`-loop guard, `and`/`or`/`not`) — never implicitly elsewhere.
    /// `Int`/`Float`/`Str`/`List` and `None` implement it structurally;
    /// `Bool` itself needs no coercion. A bare struct does **not** — so a
    /// union with any non-`Truthy` member (an error type included) doesn't
    /// either, via the existing "union satisfies a trait iff every member
    /// does" rule: `if user_result` (a `User | DbError`) fails to compile
    /// with no special-casing of `Error` needed here at all.
    Truthy,
    /// Marker trait for resource identity (`TRAITS.md` Part 7) — structurally
    /// identical to `Error` above: no members, never structural, granted only
    /// per-struct-name by a `provides Linear` clause and consulted only
    /// through `type_implements`. A built-in variant for now rather than a
    /// prelude declaration (TRAITS.md Stage 5 doesn't exist yet), added ahead
    /// of `DATA.md` Stage 4's `Sink`, which needs it. See `frontend::linear`
    /// for the enforcement this trait exists to drive — the "no accidental
    /// aliasing" checks are the whole point of granting it; the variant here
    /// is just what lets a `data` declaration state the fact.
    Linear,
    /// A trait declared in source by `trait Name { ... }` (`TRAITS.md`
    /// Stage 5). Carries its own name because there is no fixed set of
    /// them — this is what "`Trait` becomes an open interned name rather
    /// than a closed enum" (Part 5) buys, without disturbing the five
    /// variants above, which stay distinct so the operator/coercion paths
    /// that consult them (`join_operand_types`, `check_condition`,
    /// `linear::check`) keep matching on a constructor rather than a
    /// string.
    ///
    /// Every trait — built-in or user-declared — has an entry in
    /// `TypeChecker.traits`; the built-in five carry `TraitDef.builtin =
    /// Some(..)` and map back to their own variant here, so there is one
    /// registry and one `provides` path rather than two parallel systems.
    /// Like `Error`/`Linear`, a user trait is never structural: it is
    /// granted only by `provides`, recorded in `TypeChecker.provides`, and
    /// consulted through `type_implements`.
    User(String),
}

impl Display for Trait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Trait::Num    => write!(f, "Num"),
            Trait::Eq     => write!(f, "Eq"),
            Trait::Ord    => write!(f, "Ord"),
            Trait::Show   => write!(f, "Show"),
            Trait::Error  => write!(f, "Error"),
            Trait::Truthy => write!(f, "Truthy"),
            Trait::Linear => write!(f, "Linear"),
            Trait::User(name) => write!(f, "{}", name),
        }
    }
}

/// One trait declaration, built-in or user-written (`TRAITS.md` Stage 5).
///
/// `TypeChecker.traits` holds one of these per trait name, and it is the
/// single answer to "is this a trait?" — `provides` clauses, bound
/// annotations, and the `Trait.member(x)` prefix form all resolve through
/// it. `builtin` is what makes the five hardcoded traits *entries in this
/// registry* rather than a second, parallel mechanism: a built-in trait's
/// name maps back to its own `Trait` variant, so the operator and coercion
/// paths keep matching on `Trait::Num`/`Trait::Eq`/... exactly as before,
/// while `provides Num` and `provides MyTrait` travel one code path.
///
/// This is TRAITS.md Part 5's "the builtins become ordinary declarations",
/// implemented as seeded data (`initial_traits`) rather than as prelude
/// *source*: `FrogState::new()` has no builder prelude to inject into, and
/// most of the test suite uses it, so seeding the registry directly is what
/// makes the builtins unconditionally present at no parse cost.
#[derive(Debug, Clone, PartialEq)]
pub struct TraitDef {
    pub name: String,
    /// `Some(t)` for one of the five built-ins, whose membership is decided
    /// structurally or by intrinsic (`type_implements_rec`'s primitive
    /// fallback); `None` for a user trait, which is granted only by
    /// `provides`.
    pub builtin: Option<Trait>,
    /// The trait's members, in declaration order. Empty for a marker trait
    /// — which all five built-ins are here (their operator behaviour is an
    /// intrinsic, not a member; see `TRAITS.md` Part 5's note that operator
    /// desugaring is a separate stage).
    pub members: Vec<TraitMemberSig>,
    /// `trait Iterable<Item> { ... }`'s `<Item>` binder list — `RANGES.md`
    /// Stage 2. Empty for every built-in and every ordinary user trait.
    /// Each name here has a matching `Type::TypeVar { name:
    /// "{trait}::{param}", .. }` placeholder baked into every `members`
    /// signature by `hoist_trait_members` — `register_impl` mints one fresh
    /// unification variable per name here for each impl it registers, so
    /// the concrete type is *inferred* from that impl's own member
    /// annotations rather than written at the `provides` site (there is no
    /// `provides Iterable<Int>` syntax — just `provides Iterable { func
    /// next(mut s: Self): Int? = ... }`, and `Int` is where `Item` comes
    /// from).
    pub type_params: Vec<TypeParam>,
}

/// One trait member's resolved signature.
///
/// Parameter and return types are stored *unsubstituted*, still mentioning
/// `Self` as `Type::TypeVar { name: SELF_BINDER, .. }` — this is a template,
/// not a checkable type. An impl or a call site substitutes `Self` for the
/// concrete implementing type before anything unifies against it; these
/// stored types must never be unified directly, or one impl's `Self` would
/// bind the shared `substitutions` entry for every other's. That is exactly
/// the per-use-fresh discipline `instantiate` already imposes on a scheme's
/// binders, applied to the one binder every member has.
#[derive(Debug, Clone, PartialEq)]
pub struct TraitMemberSig {
    pub name: String,
    /// `(name, type, is_mut)`, matching `TypedExprKind::Function`'s own
    /// parameter shape.
    pub params: Vec<(String, Type, bool)>,
    pub return_type: Type,
    /// The member exactly as written, kept alongside the resolved types.
    ///
    /// Two things need the *source* form rather than the resolved one, and
    /// both would otherwise need a `Type` -> `TypeExpr` inverse that doesn't
    /// exist. First, an impl member may omit an annotation
    /// (`func area(c) = ...`), and the honest way to fill it in is to copy
    /// this declaration's own `TypeExpr` with `Self` rewritten to the
    /// implementing type — exact, and no round-trip through `Display`.
    /// Second, a default body is re-checked per implementing type, so it has
    /// to survive past the entry that declared it (a trait declared at one
    /// REPL prompt is implemented at the next).
    pub decl: TraitMemberDecl,
}

impl TraitMemberSig {
    /// Whether the declaration supplied a default body — what impl
    /// registration consults to decide whether an omitted member is an
    /// error.
    pub fn has_default(&self) -> bool { self.decl.default.is_some() }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    /// A named type constructor applied to zero or more arguments —
    /// `List<Int>` is `Named{name: "List", args: [Int]}`, a plain struct
    /// `Point` is `Named{name: "Point", args: []}`. Nominal for a
    /// zero-arg name: only the name is compared (derived `PartialEq`
    /// gives this for free), and field names/types for a struct live in
    /// `TypeChecker.struct_defs`, not here, so cloning stays cheap
    /// regardless of field count. `unify`/`lookup` recurse into `args`
    /// invariantly — see `TRAITS.md` Part 4, "Variance".
    Named { name: String, args: Vec<Type> },
    /// Sum / union type: a value whose type is one of the variants.
    /// Produced by if-expressions whose branches have incompatible types.
    Union(Vec<Type>),
    /// The bottom type: no value of this type is ever produced. `return`'s
    /// own type (see `TypeChecker::return_types`) — it unifies with
    /// anything and vanishes from any union it appears in (`normalize`,
    /// `is_subtype`), so `if c then return 1 else 2` types as plain `Int`,
    /// not `Never | Int`.
    Never,
}

/// The canonical order `Type::normalize` sorts a union's members into — and
/// therefore **the order that assigns every union member its runtime tag**,
/// since a member's index in the normalized list is what `lower_widen`,
/// `codegen::union_dispatch_cases` and `pack_union_member` all agree to call
/// it by.
///
/// Written out by hand, and deliberately not `#[derive]`d, for two reasons:
///
///  * A derived `Ord` orders by *variant declaration order*, so tidying the
///    `enum` above would silently re-tag every union in the language. The
///    explicit `rank` below can only change when someone edits these numbers,
///    which is a visible, deliberate act.
///  * It used to be `Display`'s job — `sort_by_cached_key(|t| t.to_string())`
///    — which coupled the tag assignment to the *diagnostics* rendering, so
///    making an error message read better re-tagged unions as a side effect.
///    That coupling is what `TRAITS.md` Stage 1 warns about for `mangle_type`,
///    and it applied here just as much.
///
/// Any total, deterministic order works; this one keeps unions reading in a
/// sensible order (scalars first, in "how you'd list them" order, then the
/// composites). Note it is also allocation-free, unlike the string sort it
/// replaces.
impl Ord for Type {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        /// Stable per-variant number. Never reuse or reorder these; append.
        fn rank(t: &Type) -> u8 {
            match t {
                Type::Int      => 0,
                Type::Float    => 1,
                Type::Bool     => 2,
                Type::Str      => 3,
                Type::None     => 4,
                Type::Never    => 5,
                Type::Named    { .. } => 6,
                Type::Union    (..)   => 7,
                Type::Function { .. } => 8,
                Type::TypeVar  { .. } => 9,
            }
        }
        rank(self).cmp(&rank(other)).then_with(|| match (self, other) {
            (Type::Named { name: n1, args: a1 }, Type::Named { name: n2, args: a2 }) =>
                n1.cmp(n2).then_with(|| a1.cmp(a2)),
            (Type::Union(v1), Type::Union(v2)) => v1.cmp(v2),
            (Type::Function { params: p1, result: r1 }, Type::Function { params: p2, result: r2 }) =>
                p1.cmp(p2).then_with(|| r1.cmp(r2)),
            (Type::TypeVar { name: n1, bounds: b1 }, Type::TypeVar { name: n2, bounds: b2 }) =>
                n1.cmp(n2).then_with(|| b1.cmp(b2)),
            // Same rank, no fields: the two are the same scalar.
            _ => std::cmp::Ordering::Equal,
        })
    }
}

impl PartialOrd for Type {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(other)) }
}

/// The `List` type constructor's name, as it appears in `Type::Named`.
pub const LIST_NAME: &str = "List";

/// The `Range` type constructor's name, as it appears in `Type::Named`.
///
/// Unlike `List`, `Range` is a real generic struct (`struct_type_params`/
/// `struct_templates` carry a genuine `[("start", T), ("end", T)]` field
/// template for it — see `TypeChecker::initial_struct_type_params`/
/// `initial_struct_templates`) rather than a hand-rolled special case:
/// `Range<T>`'s runtime representation (two flat leaves) is exactly what the
/// generic-struct "flattened leaf" machinery already produces for any
/// 2-field struct, so it rides that machinery instead of duplicating it.
/// `List` can't do the same because its GC-boxed representation has no
/// field template at all — that asymmetry is deliberate, not an oversight.
pub const RANGE_NAME: &str = "Range";

impl Type {
    /// `Type::Named { name: LIST_NAME, args: vec![elem] }`. Prefer this
    /// over constructing `Type::Named` directly for a list.
    pub fn list(elem: Type) -> Type {
        Type::Named { name: LIST_NAME.to_string(), args: vec![elem] }
    }

    /// `Type::Named { name: RANGE_NAME, args: vec![elem] }`. Prefer this
    /// over constructing `Type::Named` directly for a range.
    pub fn range(elem: Type) -> Type {
        Type::Named { name: RANGE_NAME.to_string(), args: vec![elem] }
    }

    /// `Type::Named { name, args: vec![] }` — a plain struct/nullary type.
    pub fn strukt(name: impl Into<String>) -> Type {
        Type::Named { name: name.into(), args: vec![] }
    }

    /// `Some(elem)` iff this is `List<elem>` — i.e. a `Named` whose name is
    /// `LIST_NAME` and which therefore has exactly one argument.
    pub fn as_list_elem(&self) -> Option<&Type> {
        match self {
            Type::Named { name, args } if name == LIST_NAME => args.first(),
            _ => None,
        }
    }

    /// `Some(elem)` iff this is `Range<elem>`.
    pub fn as_range_elem(&self) -> Option<&Type> {
        match self {
            Type::Named { name, args } if name == RANGE_NAME => args.first(),
            _ => None,
        }
    }

    /// `Some(name)` iff this is a zero-argument `Named` type other than
    /// `List` — i.e. a plain struct. (`List` is excluded so a caller that
    /// wants "the struct name" never mistakes a bare `List` for one; no
    /// zero-arg `List` value exists anyway since it's always applied to
    /// exactly one element type.)
    pub fn as_struct_name(&self) -> Option<&str> {
        match self {
            Type::Named { name, .. } if name != LIST_NAME => Some(name.as_str()),
            _ => None,
        }
    }

    /// Structural substitution of named `TypeVar`s — shared by
    /// `TypeChecker::instantiate` (fresh-renaming a generalized scheme's
    /// binders) and Stage 3a's generic-struct instantiation
    /// (`materialize_struct`/`instantiate_struct`, substituting concrete
    /// `args` into a stored field template). Any `TypeVar` not present in
    /// `bindings` is left untouched — e.g. a scheme's own binder is never
    /// itself a key here.
    pub fn substitute(&self, bindings: &HashMap<String, Type>) -> Type {
        match self {
            Type::TypeVar { name, .. } => bindings.get(name).cloned().unwrap_or_else(|| self.clone()),
            Type::Function { params, result } => Type::Function {
                params: params.iter().map(|p| p.substitute(bindings)).collect(),
                result: Box::new(result.substitute(bindings)),
            },
            Type::Named { name, args } => Type::Named {
                name: name.clone(),
                args: args.iter().map(|a| a.substitute(bindings)).collect(),
            },
            Type::Union(variants) => Type::renormalize(variants, variants.iter().map(|v| v.substitute(bindings)).collect()),
            _ => self.clone(),
        }
    }

    /// Rebuild a `Type::Union` from members that a resolution or
    /// substitution pass has just rewritten, re-running `normalize` iff
    /// anything actually changed.
    ///
    /// A union's canonical form is load-bearing, not cosmetic: `normalize`
    /// flattens, deduplicates and sorts the member list, and a member's
    /// *position* in that list is the runtime tag every representation
    /// agrees on (`TypeChecker::lower_widen`'s `position`, and
    /// `codegen`'s `union_dispatch_cases`/`pack_union_member`). Mapping over
    /// the members without re-normalizing can break that: `Str | ~t0` is
    /// sorted, but resolving `~t0` to `Int` leaves `[Str, Int]`, which
    /// mis-tags against — and compares unequal to — the canonical
    /// `Int | Str` the same union would have had if written out.
    ///
    /// Skipping the work when nothing was rewritten matters: `lookup` runs
    /// constantly during inference, and `normalize` renders every member
    /// through `Display` to build its sort key. A member list no pass
    /// touched is already canonical by construction, so there is nothing to
    /// redo.
    fn renormalize(original: &[Type], rewritten: Vec<Type>) -> Type {
        if rewritten == original {
            Type::Union(rewritten)
        } else {
            Type::Union(rewritten).normalize()
        }
    }

    pub fn is_list(&self) -> bool {
        matches!(self, Type::Named { name, .. } if name == LIST_NAME)
    }

    pub fn is_range(&self) -> bool {
        matches!(self, Type::Named { name, .. } if name == RANGE_NAME)
    }

    pub fn is_struct(&self) -> bool {
        self.as_struct_name().is_some()
    }

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
                // 2b. `Never` never widens a union — it exists to vanish as
                // soon as it stands next to a real value (that's the whole
                // point of a `return` unifying with its surroundings).
                // Kept only when it is the union's sole remaining member, so
                // `Never | Never` still normalizes to `Never` rather than to
                // `None`.
                if seen.len() > 1 {
                    seen.retain(|t| *t != Type::Never);
                }
                // 3. Sort into the canonical order — which is what assigns
                // each member its runtime tag, so it is `Ord for Type`'s
                // explicitly-numbered order and no longer `Display`'s. See
                // that impl for why the two must not be the same thing.
                seen.sort();
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

/// Types as **source syntax** — every one of the ~120 diagnostics that names
/// a type renders it through here, so what it prints has to be something the
/// reader could type back into an annotation.
///
/// That was not true before `TRAITS.md` Stage 3c moved type arguments from
/// `Name(A, B)` to `Name<A, B>`: errors went on quoting `Pair(Int, Str)` and
/// `[Int]`, neither of which the parser accepts any more, and a function type
/// rendered as `[Int] -> Int` — indistinguishable from a list of `Int`
/// applied to an arrow. `typeck`'s own round-trip test is what keeps the two
/// sides from drifting again.
///
/// The single deliberate exception is `TypeVar`: there is no source syntax
/// for an inference variable, and `~t0` at least reads as "not something you
/// wrote". Closed types — everything the round-trip test covers — are exact.
impl Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Type::Int    => write!(f, "Int"),
            Type::Float  => write!(f, "Float"),
            Type::Bool   => write!(f, "Bool"),
            Type::Str    => write!(f, "Str"),
            Type::None   => write!(f, "None"),
            // Parenthesised, matching `Grammar::type_atom`'s only spelling of
            // a function type. The parens are load-bearing rather than
            // decorative: without them `Int | Str -> Bool` reads as a union
            // of `Int` and `Str -> Bool` on the way back in.
            Type::Function { params, result } => {
                let ps: Vec<String> = params.iter().map(|p| p.to_string()).collect();
                write!(f, "({} -> {})", ps.join(", "), result)
            },
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
            Type::Named { name, args } if args.is_empty() => write!(f, "{}", name),
            // `List<Int>` falls out of this arm like any other applied
            // constructor — it needs no special case now that the rendering
            // and the grammar agree.
            Type::Named { name, args } => {
                let strs: Vec<String> = args.iter().map(|t| t.to_string()).collect();
                write!(f, "{}<{}>", name, strs.join(", "))
            }
            Type::Union(variants) => {
                let strs: Vec<String> = variants.iter().map(|t| format!("{}", t)).collect();
                write!(f, "{}", strs.join(" | "))
            }
            Type::Never => write!(f, "Never"),
        }
    }
}

/// Returns true iff `from` can be implicitly widened to `to`.
/// Directed and acyclic — NOT symmetric. Only safe (non-lossy) promotions.
/// To add Int32/Int64/UInt8 etc, add cases here only.
pub fn widens_to(from: &Type, to: &Type) -> bool {
    matches!(
        (from, to),
        (Type::Int, Type::Float)
        // Future: (Type::Int32, Type::Int64)
        //         (Type::Int32, Type::Float)
        //         (Type::Int64, Type::Float)
    )
}

/// Least upper bound of t1 and t2 in the widening lattice.
/// Returns Some(join) if compatible, None if incompatible (type error at call site).
pub fn numeric_join(t1: &Type, t2: &Type) -> Option<Type> {
    if t1 == t2           { return Some(t1.clone()); }
    if widens_to(t1, t2)  { return Some(t2.clone()); }
    if widens_to(t2, t1)  { return Some(t1.clone()); }
    None
}

/// Ordered field list for one declared struct: `(field_name, field_type)`
/// pairs in declaration order — order matters for construction-argument
/// reordering and for flattened codegen layout (see `struct_fields` in
/// `codegen/mod.rs`).
///
/// A positional ("tuple struct") field (`Grammar::field_list`'s `FieldDecl
/// { name: None, .. }`) has no source-level name, but every consumer here
/// keys fields by `String` — `field_name_or_positional` gives it its
/// declaration index instead (`"0"`, `"1"`, ...). This is always
/// unambiguous with a real named field: a lexed identifier can never be
/// all-digits (`is_positional_fields` relies on that same fact).
/// Every struct field list as *declared* — the template, keyed by the bare
/// declared name. For a non-generic struct that is already the concrete
/// layout; for a `data Pair<A, B>(...)` it still carries the binder
/// placeholder `TypeVar`s and is only useful once substituted
/// (`TypeChecker::materialize_struct`/`instantiate_struct`).
///
/// Separate from `StructDefs` because the two answer different questions and
/// used to share one map keyed by strings, where "is `Pair` a declared struct
/// name?" and "what is `Pair<Int, Str>`'s layout?" were distinguished only by
/// whether the key happened to be a bare name or a rendered type.
pub type StructTemplates = HashMap<String, Vec<(String, Type)>>;

/// Concrete field layout per struct *type*, in declaration order — what
/// codegen flattens a value with. Keyed by the `Type` itself, so a generic
/// struct's instantiations (`Pair<Int, Str>` vs `Pair<Str, Int>`) are
/// distinct entries with no rendering step in between: this used to be keyed
/// by `Type::struct_key()`, i.e. by `Display`, which both coupled the layout
/// table to the diagnostics rendering and made every lookup allocate a
/// `String` on codegen's hottest path (`struct_fields` is recursive and runs
/// per leaf).
pub type StructDefs = HashMap<Type, Vec<(String, Type)>>;

/// The stored field-name key for field `i`: its declared name if named,
/// or its index (stringified) if positional — see `StructDefs`.
fn field_name_or_positional(name: &Option<String>, i: usize) -> String {
    name.clone().unwrap_or_else(|| i.to_string())
}

/// True iff every field in `field_defs` was declared positionally — i.e.
/// its stored name is exactly its own index, the marker
/// `field_name_or_positional` leaves behind (never possible for a real
/// named field, since a lexed identifier can't be all-digits). Vacuously
/// true for an empty list (a nullary struct/variant takes no arguments
/// either way, so which "style" it is doesn't matter).
pub fn is_positional_fields(field_defs: &[(String, Type)]) -> bool {
    field_defs.iter().enumerate().all(|(i, (name, _))| *name == i.to_string())
}

/// One registered `data Name(common...) is A(...) | B(...) | ...` nominal
/// union (the language's only sum type — see `ERRORS.md`): its common
/// fields (readable on any member without matching) and its members in
/// declaration order (declaration order fixes each member's runtime tag —
/// see `Codegen`/`runtime::gc::FrogVariant`). `ty` is the resolved
/// `Type::Union` this declaration's name stands for — every member is a
/// nominal marker `Type::strukt("Name.Member")`, sorted the same way
/// `Type::normalize()` would sort them, so it can be looked up in
/// `TypeChecker.union_names` in the other direction.
#[derive(Debug, Clone)]
pub struct UnionDef {
    pub common:   Vec<(String, Type)>,
    pub variants: Vec<(String, Vec<(String, Type)>)>,
    pub ty:       Type,
}

impl UnionDef {
    pub fn variant_index(&self, variant: &str) -> Option<usize> {
        self.variants.iter().position(|(n, _)| n == variant)
    }

    /// All-nullary check used by codegen/typeck to decide whether a union
    /// can be represented as a bare tag instead of a boxed pointer.
    pub fn is_unit_enum(&self) -> bool {
        self.common.is_empty() && self.variants.iter().all(|(_, fs)| fs.is_empty())
    }
}

pub type UnionDefs = HashMap<String, UnionDef>;

/// One union member `?`/`!`/`catch` are building a match arm for (see
/// `TypeChecker::union_entries`). `whole_value`, together with
/// `bind_names`, reconstructs "the entire matched member's value" as an
/// expression — needed because a nominal union's own pattern binds are
/// positional *per field* (`is Circle(r)`), not "the whole value", unlike
/// an anonymous union's `Narrow`-backed single bind. For a nominal member
/// with fields, `whole_value` is a fresh `Union.Variant(field=bind, ...)`
/// construction call referencing `bind_names`; for a nullary one, a fresh
/// no-arg construction (semantically identical to any other instance —
/// nullary values carry no data); for an anonymous member, `bind_names` is
/// the single name `check_type_pattern`/`Narrow` already bind the whole
/// value to, and `whole_value` just reads it back.
/// One step of an assignment target's path, before type-checking — see
/// `TypeChecker::flatten_place`.
enum RawPlaceSeg {
    Field(String),
    Index(Spanned<Expression>),
}

struct ErrorArmEntry {
    pattern_variant: String,
    /// This member's index into the subject's `Type::Union` member list,
    /// for an *anonymous* union only (`None` for a nominal union's variant,
    /// which is resolved by name — `check_pattern`'s `def.variant_index`
    /// looks it up in the actual declaration, not by round-tripping a
    /// stringified type). Threaded through to `Pattern::resolved_member`
    /// so a synthesized arm skips `resolve_type_name` entirely — see that
    /// field's doc comment.
    member_idx: Option<usize>,
    ty: Type,
    is_error: bool,
    bind_names: Vec<String>,
    whole_value: Expression,
}

/// Where a scope began, as handed out by `ScopeStack::open` and handed back
/// to `ScopeStack::close`. Opaque on purpose: it's an index into the undo
/// log, and nothing outside `ScopeStack` should treat it as a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScopeMark(usize);

/// The variable environment: names to types, with lexical scoping.
///
/// This used to be a bare `HashMap` that every scoped construct saved and
/// restored by *deep-cloning the whole map* — thirteen hand-rolled
/// `let prev = self.ctx.clone(); ...; self.ctx = prev;` pairs, each O(number
/// of bindings in scope), and — back when inference and lowering were two
/// separate passes — each run twice per body. That made type-checking quadratic
/// in program width: 50/100/200/400/800 top-level declarations took
/// 5.3/10.3/36.4/118/454 ms, doubling the program roughly quadrupling the
/// time.
///
/// Instead of copying the environment, this records only what actually
/// changes. `bindings` is a flat name-to-type map, so lookup stays a single
/// hash probe no matter how deeply nested the code is. Every write also
/// pushes the *previous* value onto `log`; closing a scope replays that log
/// backwards to the mark the scope opened at. Opening a scope is free,
/// closing one costs only the number of bindings that scope introduced, and
/// neither depends on how much is already in scope.
///
/// Replaying in reverse is what makes repeated writes to one name inside a
/// single scope come out right: `x` written twice logs `[(x, None),
/// (x, Some(first))]`, and unwinding restores `first` and then removes `x`.
///
/// Writes are only logged while at least one scope is open (`depth > 0`).
/// At the top level there is nothing to unwind to, so logging there would
/// just grow forever in a long-lived REPL session.
/// A bound name's type plus whether it accepts reassignment — see
/// `MUTABILITY.md`. `mutable` is what `TypeChecker::lower_assign`'s
/// assignment path (as opposed to its declaration path) checks.
#[derive(Debug, Clone, PartialEq)]
struct Binding {
    ty:      Type,
    mutable: bool,
    /// A `func` or let-bound-lambda's generalized type parameters
    /// (`TRAITS.md` Stage 2's type schemes) — empty for every other
    /// binding, which is the ordinary monomorphic case. Non-empty only for
    /// an immutable binding; see `TypeChecker::generalize`/`instantiate`.
    binders: Vec<(String, Vec<Trait>)>,
    /// The name the *code generator* knows this binding by, when that
    /// differs from the source name it's bound under. `None` — the
    /// ordinary case — means "the same name".
    ///
    /// Two things set it, both function declarations:
    ///  * a generalized declaration (`TRAITS.md` Stage 3b) gets a unique
    ///    template symbol (`name#42`), which is the `generic_templates`
    ///    key. Monomorphization then never has to decide anything by bare
    ///    name — which scope-shadowing a generic (`func g(id: Int)` over a
    ///    generic `id`) and redeclaring one (`let f = ...` twice) both got
    ///    wrong;
    ///  * an alias declaration (`let f = g`) gets whatever symbol `g`
    ///    already resolves to — see `lower_assign`, where a function has
    ///    no runtime value to copy, so binding a second name to one is a
    ///    compile-time rebinding and nothing more.
    ///
    /// Either way, every `Var` reference that resolves *here* is emitted
    /// under this symbol instead of the source name, so which declaration
    /// a reference belongs to is decided in scope, at lowering time, and
    /// stays decided.
    symbol: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ScopeStack {
    bindings: HashMap<String, Binding>,
    log:      Vec<(String, Option<Binding>)>,
    depth:    usize,
}

impl ScopeStack {
    /// Seed the stack with immutable bindings (builtins, `add_ctx`
    /// callers) — nothing outside `lower_assign`'s declaration path should
    /// ever introduce a mutable one.
    fn new(bindings: HashMap<String, Type>) -> Self {
        let bindings = bindings.into_iter()
            .map(|(k, ty)| (k, Binding { ty, mutable: false, binders: Vec::new(), symbol: None }))
            .collect();
        ScopeStack { bindings, log: Vec::new(), depth: 0 }
    }

    fn get(&self, name: &str) -> Option<&Type> {
        self.bindings.get(name).map(|b| &b.ty)
    }

    /// This binding's generalized type parameters (`TRAITS.md` Stage 2),
    /// or `None` if `name` isn't bound — distinct from `Some(&[])`, an
    /// ordinary monomorphic binding.
    fn binders(&self, name: &str) -> Option<&[(String, Vec<Trait>)]> {
        self.bindings.get(name).map(|b| b.binders.as_slice())
    }

    /// The codegen symbol `name` currently resolves to
    /// (`Binding::symbol`), or `None` if it isn't bound or is bound under
    /// its own name — the shadowing-correct answer, since this reads the
    /// innermost binding like any other lookup.
    fn symbol(&self, name: &str) -> Option<&str> {
        self.bindings.get(name).and_then(|b| b.symbol.as_deref())
    }

    /// `None` if `name` isn't bound at all — distinct from `Some(false)`,
    /// which is a real immutable binding. `lower_assign`'s assignment path
    /// needs to tell "not declared" from "declared, not mutable" apart to
    /// give the right error.
    fn is_mutable(&self, name: &str) -> Option<bool> {
        self.bindings.get(name).map(|b| b.mutable)
    }

    fn contains_key(&self, name: &str) -> bool {
        self.bindings.contains_key(name)
    }

    /// Bind `name` immutably in the innermost open scope, shadowing (and,
    /// once that scope closes, restoring) whatever it held before. Used
    /// for every binding that isn't a user-facing `let`/`mut` declaration
    /// — function parameters, loop/comprehension variables, match-arm and
    /// `catch`-handler binds, struct-field synthetic names — all of which
    /// are immutable by default (a `mut` function parameter, the one
    /// exception, goes through `insert_mut` once implemented).
    fn insert(&mut self, name: String, ty: Type) {
        self.insert_mut(name, ty, false);
    }

    /// As `insert`, but the caller states the binding's mutability
    /// explicitly — the declaration path in `lower_assign`.
    fn insert_mut(&mut self, name: String, ty: Type, mutable: bool) {
        self.insert_raw(name, Binding { ty, mutable, binders: Vec::new(), symbol: None });
    }

    /// A generalized (`TRAITS.md` Stage 2) immutable binding — a `func` or
    /// let-bound-lambda whose type scheme quantifies over `binders`. The
    /// value restriction: only ever called for a syntactic function value,
    /// never for a `mut` binding.
    /// `symbol` is `Some` for a generalized declaration (its template
    /// symbol) or an alias (its target's symbol), `None` otherwise — a
    /// trivially-generalized (zero-binder) `func` is monomorphic and needs
    /// no separate identity.
    fn insert_generalized(&mut self, name: String, ty: Type, binders: Vec<(String, Vec<Trait>)>, symbol: Option<String>) {
        self.insert_raw(name, Binding { ty, mutable: false, binders, symbol });
    }

    fn insert_raw(&mut self, name: String, binding: Binding) {
        let previous = self.bindings.insert(name.clone(), binding);
        if self.depth > 0 {
            self.log.push((name, previous));
        }
    }

    /// Open a scope. The returned mark must be passed to `close` — see
    /// `TypeChecker::in_scope`, which pairs them for you and is what
    /// essentially every caller should use instead.
    fn open(&mut self) -> ScopeMark {
        self.depth += 1;
        ScopeMark(self.log.len())
    }

    /// Close the scope that `mark` opened, undoing every binding made since.
    fn close(&mut self, mark: ScopeMark) {
        while self.log.len() > mark.0 {
            let (name, previous) = self.log.pop().expect("log is longer than the mark");
            match previous {
                Some(b) => { self.bindings.insert(name, b); },
                None    => { self.bindings.remove(&name); },
            }
        }
        self.depth -= 1;
    }

    fn into_bindings(self) -> HashMap<String, Type> {
        self.bindings.into_iter().map(|(k, b)| (k, b.ty)).collect()
    }

    /// Every currently-bound type, flat across all open scopes — used by
    /// `TypeChecker::generalize`'s "what's free elsewhere in the
    /// environment" scan.
    fn bound_types(&self) -> impl Iterator<Item = &Type> {
        self.bindings.values().map(|b| &b.ty)
    }

    /// `bound_types`, minus the binding named `skip` — for `generalize`,
    /// which must not see the declaration it is generalizing. Ordinarily
    /// that binding simply isn't in scope yet; the exception is a function
    /// pre-bound to a placeholder so its own body can call it
    /// (`lower_assign`'s recursion pre-bind), where every var of the
    /// placeholder would otherwise read as "already free in the
    /// environment" and block generalization completely.
    fn bound_types_except<'a>(&'a self, skip: &'a str) -> impl Iterator<Item = &'a Type> {
        self.bindings.iter().filter(move |(n, _)| n.as_str() != skip).map(|(_, b)| &b.ty)
    }
}

/// A compile-time-constant scalar — `DATA.md` Stage 6's "annotations are
/// typed values, not raw strings" restricted to literals: an annotation
/// field's value, an annotation field's own `= default`, and an ordinary
/// struct field's `= default` are all one of these, never an arbitrary
/// expression. Never itself a union or struct — `eval_const_expr` only
/// ever produces one of these five shapes, and matching it against a wider
/// declared type (`T?`, an anonymous union) is `lower_widen`'s job once
/// it's lifted into a `TypedExpr` via `TypeChecker::const_value_to_typed`.
#[derive(Debug, Clone, PartialEq)]
enum ConstValue {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    None,
}

/// Evaluate `e` as a `ConstValue`, or explain why it isn't one. Deliberately
/// narrow — a literal, or unary `-` over a numeric literal — not a general
/// const-evaluator: `DATA.md` Stage 6 states this restriction explicitly
/// ("Fields must be literals; const-eval is restricted to literals
/// initially"), and it's what lets an annotation/default value be validated
/// with no dependency on scope, evaluation order, or codegen at all.
fn eval_const_expr(e: &Expression) -> Result<ConstValue, String> {
    const NOT_CONST: &str = "must be a literal constant (a number, string, true, false, or none)";
    match e {
        Expression::Literal(LiteralExpr { token }) => match token {
            Token::Int(n) => Ok(ConstValue::Int(*n)),
            Token::Float(f) => Ok(ConstValue::Float(*f)),
            Token::True => Ok(ConstValue::Bool(true)),
            Token::False => Ok(ConstValue::Bool(false)),
            Token::String(s) => Ok(ConstValue::Str(s.clone())),
            Token::None => Ok(ConstValue::None),
            _ => Err(NOT_CONST.to_string()),
        },
        Expression::Unary(UnaryExpr { op: Token::Minus, expr }) => match &expr.item {
            Expression::Literal(LiteralExpr { token: Token::Int(n) }) => Ok(ConstValue::Int(-n)),
            Expression::Literal(LiteralExpr { token: Token::Float(f) }) => Ok(ConstValue::Float(-f)),
            _ => Err(NOT_CONST.to_string()),
        },
        _ => Err(NOT_CONST.to_string()),
    }
}

pub struct TypeChecker {
    /// Variable (value level) name -> Type, lexically scoped.
    ctx: ScopeStack,
    // TypeVar name -> Type
    substitutions: HashMap<String, Type>,
    next_id: u32,
    /// Registered `data Name(...)` declarations — see `hoist_data_decls`.
    /// Never scoped/popped: once a struct name is registered it stays
    /// visible for the rest of the program, including from later
    /// independent blocks. A known simplification, not a hard limit.
    struct_defs: StructDefs,
    /// Declared field templates, keyed by bare name — see `StructTemplates`.
    /// Registered by `hoist_data_decls` and never scoped/popped, same as
    /// `struct_defs`. Also the authoritative answer to "is this name a
    /// declared struct?", which `struct_defs` can no longer give now that it
    /// is keyed by type.
    struct_templates: StructTemplates,
    /// `TRAITS.md` Stage 3a: declared `<A, B>` binder names for a generic
    /// `data` declaration, keyed by its plain name — populated in
    /// `hoist_data_decls`, holding each binder's *fresh placeholder
    /// `TypeVar`'s internal name* (not the source-level binder text), in
    /// declaration order. This is exactly what `resolve_type_expr`'s
    /// `TypeExpr::Apply` arm needs for arity checking, and what
    /// `materialize_struct`/`instantiate_struct` need to zip against
    /// concrete `args` for substitution — the source-level names
    /// themselves are only needed transiently, while resolving one
    /// decl's own field types (see `type_param_scope`). Only ever
    /// populated for a struct (no variants) — a generic *union* is
    /// rejected in `hoist_data_decls` as not yet supported.
    struct_type_params: HashMap<String, Vec<String>>,
    /// Transient scope, live only while `hoist_data_decls`'s second pass
    /// is resolving one generic decl's own field `TypeExpr`s: source-level
    /// binder name (`"A"`) -> its fresh placeholder `TypeVar`. Consulted
    /// by `resolve_type_name` before the ordinary struct/union/builtin
    /// lookup, so a field's declared type `A` resolves to the placeholder
    /// instead of "unknown type". Cleared between declarations — never
    /// meant to be visible during `check_and_lower`'s ordinary two-pass
    /// walk (`hoist_data_decls` finishes before that starts), so it's
    /// deliberately left out of `TypeCheckerCheckpoint`, same as
    /// `host_names`.
    type_param_scope: HashMap<String, Type>,
    /// Registered `data Name(...) is ...` nominal-union declarations — see
    /// `hoist_data_decls`. Same never-scoped lifetime as `struct_defs`.
    union_defs: UnionDefs,
    /// Reverse of `union_defs`: a union's normalized, sorted member-type
    /// vector (the same shape `Type::normalize()` produces) -> its declared
    /// name. Lets any `Type::Union` value that happens to be a registered
    /// nominal union be resolved back to its `UnionDef` — see
    /// `TypeChecker::resolve_union`.
    union_names: HashMap<Vec<Type>, String>,
    /// variant name -> names of every union declaring it. Used to resolve a
    /// bare (unqualified) variant constructor/pattern: unique -> that
    /// union, ambiguous -> require `Union.Variant` qualification.
    variant_owners: HashMap<String, Vec<String>>,
    /// Stack of enclosing functions' return types, innermost last.
    /// `check_and_lower`'s `Expression::Return` arm checks its value against
    /// `.last()` and errors if the stack is empty — "return outside a
    /// function". Pushed and popped exactly once around each function body,
    /// by whichever of the two entry points is handling it:
    /// `check_and_lower`'s `Function` arm, or `lower_expected`'s
    /// lambda-against-`Type::Function` case.
    return_types: Vec<Type>,
    /// Trait names granted to a struct or union-member by name — populated
    /// from a `data`/`error` declaration's trailing `provides Clause`
    /// (`hoist_data_decls`). Keyed by the same qualified name `struct_defs`
    /// uses for a union member (`"Shape.Circle"`) or the bare name for a
    /// plain struct. Consulted only by `type_implements`.
    provides: HashMap<String, Vec<Trait>>,
    /// Every trait that exists, by name — the five built-ins seeded by
    /// `initial_traits`, plus every user `trait Name { ... }` declaration
    /// (`TRAITS.md` Stage 5). This is the *declaration* table; `provides`
    /// above is the *grant* table, and the two are deliberately separate:
    /// a trait can be declared and implemented by nothing.
    ///
    /// Membership here is what `resolve_trait_name` consults, so it is the
    /// only thing standing between a `provides` clause and an unknown-trait
    /// error — replacing the hardcoded `"Error"`/`"Linear"` match this
    /// field superseded.
    traits: HashMap<String, TraitDef>,
    /// Every registered impl, keyed by `(trait name, type key)` — the pair
    /// TRAITS.md's "Coherence" section says must be unique program-wide.
    /// Froglang sees every declaration through one `FrogState`, so this map
    /// *is* the coherence check: a second insert under the same key is the
    /// error, and there are no orphan rules to write.
    ///
    /// The value is `member name -> compiled symbol` (`Circle$area`) — the
    /// symbol `expand_impls` renamed the member's declaration to, and the
    /// one a resolved call site names in the typed AST.
    impls: HashMap<(String, String), HashMap<String, String>>,
    /// Resolution step 2's index: `(type key, member name)` -> `(trait,
    /// symbol)`. A hash lookup, never a search — which is only possible
    /// because the *other* ambiguity (two traits declaring the same member
    /// name for one type) is rejected at registration, where the error can
    /// name the two impls, rather than at the call site, where it could only
    /// name the call.
    member_index: HashMap<(String, String), (String, String)>,
    /// Every trait that declares a member of this name. Used by the
    /// `Trait.member(x)` prefix form and by zero-`Self` resolution, whose
    /// search is narrow by construction: only over traits declaring *that*
    /// name, typically one.
    member_traits: HashMap<String, Vec<String>>,
    /// `DATA.md` Stage 6: registered `annotation Name(field: Type = default, ...)`
    /// declarations — name -> its field list, in declared order. Consulted
    /// by `validate_annotation_use` to typecheck a `#name(...)` use.
    annotation_defs: HashMap<String, Vec<(String, Type)>>,
    /// Literal default values for `annotation_defs`' fields, keyed the same
    /// way, field name -> its evaluated `ConstValue`. Only fields that
    /// declared `= default` appear; a field absent here is required at
    /// every use site.
    annotation_defaults: HashMap<String, HashMap<String, ConstValue>>,
    /// Literal default values for ordinary `data`/`error` struct (and
    /// variant) fields — `DATA.md` Stage 6's prerequisite. Keyed the same
    /// way `provides`/`struct_templates` key a union member
    /// (`"Shape.Circle"`), field name -> its evaluated `ConstValue`.
    /// Consulted by `lower_record_args` when a construction call omits a
    /// field that has one.
    struct_field_defaults: HashMap<String, HashMap<String, ConstValue>>,
    /// Named function -> which of its parameters are `mut`
    /// (`MUTABILITY.md`), in declaration order. `Type::Function` itself
    /// carries no mutability — "a `mut` parameter does not escape" is the
    /// whole point — so this is a side table, consulted only by
    /// `lower_call` to validate a call site's `mut` markers against the
    /// callee's actual declaration and to know how many extra copy-out
    /// values the call produces. Populated twice per function: once from
    /// the raw `Parameter.mutable` flags at pre-bind time (so a function
    /// can call itself with `mut` arguments before its own body finishes
    /// checking, mirroring how `func_ty` is pre-bound for recursion), and
    /// again, authoritatively, once `lower_function` returns.
    func_mut_params: HashMap<String, Vec<bool>>,
    /// Names installed by `add_ctx` — i.e. registered host functions
    /// (`FrogStateBuilder::func`). Codegen's host dispatch (`Ctx::host_fns`,
    /// `compile_call`) routes a call by name alone, with no regard for
    /// lexical scope, so a user declaration that rebinds one of these names
    /// — anywhere, not just at top level — would silently hijack every call
    /// to it. Checked wherever a `let`/`mut`/`func` declaration binds a
    /// name, and rejected there instead of surfacing later as a confusing
    /// codegen/verifier error. Set once, before any checking starts
    /// (`FrogStateBuilder::build`), so it's left out of
    /// `TypeCheckerCheckpoint`/`restore` deliberately.
    host_names: std::collections::HashSet<String>,
    /// SUPERSEDED by real monomorphization (`TRAITS.md` Stage 3b): used to
    /// hold the single concrete type each generalized name had been
    /// observed instantiated at, and `check_generic_monomorphism` rejected
    /// a second, different one. Now a *set* of every distinct concrete
    /// whole-function type observed for that name across the whole
    /// session — purely a historical/audit record (nothing downstream
    /// consults it to decide whether to compile something; that decision
    /// is `emitted_instantiations`' job now). Still checkpointed like
    /// `substitutions`: a failed entry's would-be instantiation must not
    /// stick, but a successful one persists for the rest of the session.
    generic_instantiations: HashMap<String, std::collections::HashSet<Type>>,
    /// `TRAITS.md` Stage 3b: one generalized declaration, retained so a
    /// *later* call — in this entry or a future one — can clone and
    /// specialize it. Populated in `lower_assign`'s generalized branch,
    /// alongside `ctx.insert_generalized`.
    ///
    /// Keyed by that declaration's unique template symbol
    /// (`Binding::generic_symbol`, `name#42`), never by the source name:
    /// a name is not an identity. Two `let f = ...` generic declarations
    /// in a row are two templates, a `func g(id: Int)` parameter shadowing
    /// a generic `id` is neither, and the symbol on each `Var` node — put
    /// there in scope, during lowering — says which is which. Membership
    /// in this map is therefore also the "is this node a generic
    /// reference?" test that `collect_generic_var_types` and
    /// `rewrite_call_sites` use.
    ///
    /// Never removed once inserted (mirrors `struct_defs`'s "registered
    /// names stay visible forever" precedent) — checkpointed so a failed
    /// entry's would-be declaration doesn't stick.
    generic_templates: HashMap<String, GenericTemplate>,
    /// Mangled symbol names (`monomorphize_generics`' `mangle_type`
    /// output, full `name$Arg1$Arg2` form) already compiled somewhere in
    /// this session — checked before cloning+compiling an instantiation
    /// again, so a generic called at the same type from two different
    /// entries reuses the earlier entry's `FuncId` (`codegen::func_ids`
    /// persists across entries) instead of silently recompiling it.
    /// Checkpointed like everything else above.
    emitted_instantiations: std::collections::HashSet<String>,
    /// Tier 1 function values: every top-level function declaration that
    /// takes at least one function-typed parameter, retained so a call
    /// site — in this entry or a later one — can clone and specialize it
    /// on the *identity* of the function it was passed. The exact
    /// counterpart of `generic_templates`, one level down: that one keys
    /// on a type, this one on a name.
    ///
    /// Keyed by the symbol the declaration compiles under, since that is
    /// what a `Var` in callable position carries.
    fn_templates: HashMap<String, FnTemplate>,
    /// The capture parameters a lifted function gained, in the order they
    /// were appended to its parameter list — what every call site must
    /// pass after the declared arguments.
    ///
    /// Persisted (and checkpointed) because a REPL entry that *calls* a
    /// function declared in an earlier entry has no other way to learn
    /// its real arity: the lifting happened in a tree that entry never
    /// sees. Empty for the overwhelming majority of functions, which
    /// capture nothing.
    lifted_captures: HashMap<String, Vec<(String, Type)>>,
    /// Specialized symbols (`apply$$inc`) already compiled in this
    /// session — the `emitted_instantiations` of tier 1, and checked for
    /// the same reason: `codegen::func_ids` persists across entries, so a
    /// second entry specializing the same callee on the same function
    /// argument must reuse the first entry's body rather than redefine it.
    emitted_specializations: std::collections::HashSet<String>,
    /// Every name ever declared `mut`, recorded as `lower_assign` binds it.
    ///
    /// `lower_function_values` needs to know whether a captured name is a
    /// mutable binding, and by the time it runs the answer is gone: the
    /// typed AST spells a `let` declaration and a later reassignment as
    /// the same `Assign` node, and `ScopeStack`'s `mutable` flag survives
    /// only for bindings still in scope at the end of the entry — never
    /// for a function-local one. Deriving it from the tree instead
    /// ("assigned twice") cannot tell a re-`let` in an inner scope from a
    /// reassignment, and rejected a legal shadow.
    ///
    /// Name-keyed and never scoped, so on its own it over-approximates: a
    /// `mut n` anywhere in the session would make every `n` uncapturable.
    /// `lower_function_values` narrows that back down by checking `ctx`
    /// first — a name still resolvable there as definitely immutable (a
    /// live `let`, however it shadows) wins over this set; only a name
    /// whose scope has already closed (a genuine function-local `mut`)
    /// falls back to this over-approximation, which costs a spurious error
    /// but never a miscompile.
    mut_names: std::collections::HashSet<String>,
}

/// One generalized (`TRAITS.md` Stage 2) `func`/let-bound-lambda
/// declaration's template, retained by `generic_templates` so
/// `monomorphize_generics` can clone and specialize it on demand — see
/// that field's doc comment. `declared_ty` is the scheme's own
/// (unsubstituted, binder-`TypeVar`-carrying) `Type::Function`, exactly
/// what `zip_binder_types` needs as its `declared` argument; `params` /
/// `return_type` / `body` are the declaration's own `TypedExprKind::Function`
/// fields, needed to rebuild a specialized `Function` node without
/// re-deriving them from `declared_ty`.
#[derive(Debug, Clone)]
struct GenericTemplate {
    binders:      Vec<(String, Vec<Trait>)>,
    declared_ty:  Type,
    params:       Vec<(String, Type, bool)>,
    return_type:  Type,
    body:         Spanned<TypedExpr>,
}

/// What `TypeChecker::as_fn_decl` reads off a top-level function
/// declaration statement: its name, its parameters, and its body.
type FnDecl<'a> = (&'a str, &'a [(String, Type, bool)], &'a Spanned<TypedExpr>);

/// One higher-order function declaration, retained by `fn_templates` so
/// `specialize_function_values` can clone it once per distinct tuple of
/// function arguments it is called with. Deliberately *not* a
/// `GenericTemplate` with empty binders: there is no type substitution
/// here at all, only name substitution, and sharing the struct would
/// invite the two passes to be confused for one another.
#[derive(Debug, Clone)]
struct FnTemplate {
    params:      Vec<(String, Type, bool)>,
    return_type: Type,
    body:        Spanned<TypedExpr>,
    span:        Span,
}

/// Which notation a placeholder call is asking for.
///
/// All three are built during lowering as a `Call` on a synthetic `Var`
/// (`Notation::callee`) and expanded by `TypeChecker::desugar_notation`
/// after monomorphization, when every type in the tree is finally
/// substituted — see that function's doc comment for why the expansion
/// cannot happen earlier.
///
/// `Interp` is `Repr` except at `Str`, where it inserts the string raw
/// rather than quoting it (`"hi ${name}"` is `hi Bob`). That one-type
/// difference is the entire reason it exists as a third notation instead of
/// being decided in `lower_interp`: inside a generic function the piece's
/// type is still a `TypeVar` at lowering time, so a decision made there
/// would quote `show("hi")`'s argument and not `show(1)`'s — the choice has
/// to be made per instantiation, which is exactly what this pass does.
#[derive(Clone, Copy, PartialEq)]
enum Notation { Repr, Json, Interp }

impl Notation {
    /// The synthetic callee name that marks a placeholder of this notation.
    /// Unspellable for the two internal ones; `repr` is a real builtin.
    fn callee(self) -> &'static str {
        match self {
            Notation::Repr   => "repr",
            Notation::Json   => "__json_to_str",
            Notation::Interp => "__interp",
        }
    }

    /// How to describe a missing `Show` for this notation.
    fn needs_show(self, ty: &Type) -> String {
        match self {
            Notation::Interp => format!("{} has no notation — interpolation needs Show", ty),
            _                => format!("{} has no notation — 'repr' needs Show", ty),
        }
    }
}

pub struct TypeCheckerCheckpoint {
    ctx: ScopeStack,
    substitutions: HashMap<String, Type>,
    next_id: u32,
    struct_defs: StructDefs,
    struct_templates: StructTemplates,
    struct_type_params: HashMap<String, Vec<String>>,
    union_defs: UnionDefs,
    union_names: HashMap<Vec<Type>, String>,
    variant_owners: HashMap<String, Vec<String>>,
    return_types: Vec<Type>,
    provides: HashMap<String, Vec<Trait>>,
    traits: HashMap<String, TraitDef>,
    impls: HashMap<(String, String), HashMap<String, String>>,
    member_index: HashMap<(String, String), (String, String)>,
    member_traits: HashMap<String, Vec<String>>,
    func_mut_params: HashMap<String, Vec<bool>>,
    generic_instantiations: HashMap<String, std::collections::HashSet<Type>>,
    generic_templates: HashMap<String, GenericTemplate>,
    emitted_instantiations: std::collections::HashSet<String>,
    fn_templates: HashMap<String, FnTemplate>,
    lifted_captures: HashMap<String, Vec<(String, Type)>>,
    emitted_specializations: std::collections::HashSet<String>,
    mut_names: std::collections::HashSet<String>,
    annotation_defs: HashMap<String, Vec<(String, Type)>>,
    annotation_defaults: HashMap<String, HashMap<String, ConstValue>>,
    struct_field_defaults: HashMap<String, HashMap<String, ConstValue>>,
}

impl Default for TypeChecker {
    fn default() -> Self {
        Self::new()
    }
}

impl TypeChecker {
    /// `struct_type_params`'s initial contents: `List`, registered at arity
    /// 1 so `resolve_type_expr`'s `TypeExpr::Apply` arm can arity-check
    /// `List<...>` through the same lookup it uses for a user `data
    /// Name<A>` declaration (`TRAITS.md` Stage 3c), rather than a second
    /// hardcoded branch. The placeholder name is never substituted into
    /// anything — `List` has no field template (`struct_defs` has no entry
    /// for it, deliberately: it stays builtin-represented, not a struct) —
    /// it exists purely so `.len()` reads 1.
    ///
    /// `Range` is registered here too, at arity 1, but — unlike `List` — its
    /// binder name (`"T@Range"`) *is* real: it's the exact key
    /// `initial_struct_templates`'s two `TypeVar` fields use, so
    /// `materialize_struct` actually substitutes a concrete element type
    /// into them (see `RANGE_NAME`'s doc comment for why `Range` gets a real
    /// template while `List` doesn't). The name is a fixed literal, not
    /// minted via `fresh_var` — it never needs to be fresh, since it's the
    /// same two occurrences being zipped against every time, not a
    /// per-call-site instantiation — just distinct from `fresh_var`'s own
    /// `"t{id}"` scheme so an unrelated inference variable can never
    /// collide with it and get substituted in by accident.
    fn initial_struct_type_params() -> HashMap<String, Vec<String>> {
        let mut m = HashMap::new();
        m.insert(LIST_NAME.to_string(), vec!["T".to_string()]);
        m.insert(RANGE_NAME.to_string(), vec!["T@Range".to_string()]);
        m
    }

    /// `struct_templates`'s initial contents: just `Range`'s
    /// `[("start", T@Range), ("end", T@Range)]` field template — see
    /// `RANGE_NAME`'s doc comment. `List` is deliberately absent (no entry
    /// at all, not even an empty one) since it has no field template.
    fn initial_struct_templates() -> StructTemplates {
        let mut m = HashMap::new();
        let t = Type::TypeVar { name: "T@Range".to_string(), bounds: Vec::new() };
        m.insert(RANGE_NAME.to_string(), vec![("start".to_string(), t.clone()), ("end".to_string(), t)]);
        m
    }

    /// `traits`' initial contents: the five built-in traits, as ordinary
    /// registry entries (`TRAITS.md` Part 5, "Prelude traits"). Seeded from
    /// both `new()` and `empty()`, exactly like `initial_struct_type_params`
    /// seeds `List`, so *every* `TypeChecker` has them — including the ones
    /// behind `FrogState::new()`, which has no builder prelude to inject
    /// into and which most of the test suite uses.
    ///
    /// `Truthy` is deliberately absent, and that absence is load-bearing:
    /// Part 5's "`Truthy` exception" keeps it a compiler-internal coercion
    /// relation consulted by `check_condition`, not a callable interface, so
    /// making it implementable would let any impl redefine what `if x`
    /// means. `resolve_trait_name` reports that specifically rather than
    /// letting it fall through to a bare "unknown trait".
    fn initial_traits() -> HashMap<String, TraitDef> {
        let mut m = HashMap::new();
        for t in [Trait::Num, Trait::Eq, Trait::Ord, Trait::Show, Trait::Error, Trait::Linear] {
            let name = t.to_string();
            m.insert(name.clone(), TraitDef { name, builtin: Some(t), members: Vec::new(), type_params: Vec::new() });
        }
        m
    }

    /// A trait name as written in source (`provides Error`, `<T: Ord>`,
    /// `trait Shape`) resolved to the `Trait` the checker reasons with.
    ///
    /// The single string -> `Trait` conversion in the crate; it replaced a
    /// hardcoded `match name { "Error" => .., "Linear" => .., _ => err }`,
    /// which was why only two of the five traits were ever spellable.
    /// `Err` carries the message to report, so the `Truthy` case can explain
    /// itself instead of claiming the trait doesn't exist.
    pub(crate) fn resolve_trait_name(&self, name: &str) -> Result<Trait, String> {
        if let Some(def) = self.traits.get(name) {
            return Ok(def.builtin.clone().unwrap_or_else(|| Trait::User(name.to_string())));
        }
        if name == "Truthy" {
            return Err(
                "'Truthy' is a compiler-internal coercion relation (what `if x` means for a \
                 non-Bool), not an implementable trait".to_string()
            );
        }
        Err(format!("Unknown trait '{}'", name))
    }

    pub fn empty() -> Self {
        let mut tc = TypeChecker { ctx: ScopeStack::new(HashMap::new()), substitutions: HashMap::new(), next_id: 0, struct_defs: HashMap::new(), struct_templates: TypeChecker::initial_struct_templates(), struct_type_params: TypeChecker::initial_struct_type_params(), type_param_scope: HashMap::new(), union_defs: HashMap::new(), union_names: HashMap::new(), variant_owners: HashMap::new(), return_types: Vec::new(), provides: HashMap::new(), traits: TypeChecker::initial_traits(), impls: HashMap::new(), member_index: HashMap::new(), member_traits: HashMap::new(), func_mut_params: HashMap::new(), host_names: std::collections::HashSet::new(), generic_instantiations: HashMap::new(), generic_templates: HashMap::new(), emitted_instantiations: std::collections::HashSet::new(), fn_templates: HashMap::new(), lifted_captures: HashMap::new(), emitted_specializations: std::collections::HashSet::new(), mut_names: std::collections::HashSet::new(), annotation_defs: HashMap::new(), annotation_defaults: HashMap::new(), struct_field_defaults: HashMap::new() };
        tc.seed_iterable_container_traits();
        tc.seed_base_prelude();
        tc
    }

    pub fn new() -> Self {
        let mut tc = TypeChecker { ctx: ScopeStack::new(TypeChecker::default_context()), substitutions: HashMap::new(), next_id: 0, struct_defs: HashMap::new(), struct_templates: TypeChecker::initial_struct_templates(), struct_type_params: TypeChecker::initial_struct_type_params(), type_param_scope: HashMap::new(), union_defs: HashMap::new(), union_names: HashMap::new(), variant_owners: HashMap::new(), return_types: Vec::new(), provides: HashMap::new(), traits: TypeChecker::initial_traits(), impls: HashMap::new(), member_index: HashMap::new(), member_traits: HashMap::new(), func_mut_params: HashMap::new(), host_names: std::collections::HashSet::new(), generic_instantiations: HashMap::new(), generic_templates: HashMap::new(), emitted_instantiations: std::collections::HashSet::new(), fn_templates: HashMap::new(), lifted_captures: HashMap::new(), emitted_specializations: std::collections::HashSet::new(), mut_names: std::collections::HashSet::new(), annotation_defs: HashMap::new(), annotation_defaults: HashMap::new(), struct_field_defaults: HashMap::new() };
        tc.seed_iterable_container_traits();
        tc.seed_base_prelude();
        tc
    }

    /// `ReadError` (`plans/DATA.md` stage 5: `read`'s error type) and
    /// `JsonError` (stage 8: `json.parse`'s) — seeded
    /// directly rather than parsed from a prelude source string the way
    /// `stdlib::install`'s `ErrMsg`/`IndexError` are (`stdlib/mod.rs`'s
    /// `.prelude(...)` calls, which run through `FrogState::eval` and so
    /// only exist for `with_stdlib()`). Two reasons this one is different:
    ///
    /// - `read` is a core builtin like `print`, not a `stdlib`-gated one,
    ///   so its error type has to be unconditionally present in every
    ///   `TypeChecker`, `FrogState::new()` included.
    /// - Running it through `FrogState::eval` would consume an
    ///   `entry_id`/`entry_sources` slot ahead of the embedder's own first
    ///   entry — `test_source_map.rs` pins the first *user* eval at entry
    ///   0, and `codegen::compile_and_run` has no prelude mechanism to
    ///   begin with.
    ///
    /// So it is written out by hand, the way `Range`'s template is
    /// (`initial_struct_templates`) rather than hoisted from source the way
    /// `Iterable`/`Container` are (`seed_iterable_container_traits`) —
    /// `ReadError` has no generics and no trait members, so there is
    /// nothing a parse-and-hoist would buy over stating the two fields
    /// directly, and `hoist_data_decls`' full two-pass machinery (binder
    /// scoping, variant handling, cycle checks) is aimed at exactly the
    /// generality this type doesn't need.
    /// `JsonError` is here, and not behind an import, because the `json.`
    /// namespace is (`lower_call`'s `json_builtin_target`) — a
    /// return-type-directed `json.parse(s): Person | JsonError` has to be
    /// able to *name* its error type in the annotation that drives it, and
    /// making that name cost a second import would be ceremony with
    /// nothing behind it.
    fn seed_base_prelude(&mut self) {
        // Same two fields, same meaning, deliberately not the same type:
        // `T | ReadError` and `T | JsonError` are different failures and a
        // `catch` should be able to tell them apart.
        for name in ["ReadError", "JsonError"] {
            let fields = vec![("msg".to_string(), Type::Str), ("offset".to_string(), Type::Int)];
            self.struct_templates.insert(name.to_string(), fields.clone());
            self.struct_defs.insert(Type::strukt(name), fields);
            self.provides.insert(name.to_string(), vec![Trait::Error]);
        }
    }

    /// `Iterable<Item>`/`Container<Item>` (`RANGES.md` Stage 2) — seeded by
    /// parsing and hoisting a fixed source string rather than hand-building
    /// `TraitDef`s, so their member signatures go through exactly the
    /// parse-and-hoist path a user's own `trait` declaration would (no risk
    /// of a hand-built `Type`/`TypeExpr` tree silently drifting from what
    /// the parser actually produces). `for x in y`/`x in y` fall back to
    /// these — via an ordinary `member_index` lookup, same as any other
    /// trait — when `y`'s type is neither List/Range/Str-shaped; see
    /// `lower_for_loop`/`lower_in`.
    ///
    /// The parse-and-hoist itself runs once per process (`cached_...`,
    /// below), not once per `TypeChecker`: `new()`/`empty()` run once per
    /// compile (`benches/pipeline.rs`'s `compile/*` group measures exactly
    /// that), so re-parsing this fixed string on every construction was a
    /// flat tax on every compile, whether or not the program ever mentions
    /// `Iterable`/`Container` — cloning the two already-hoisted `TraitDef`s
    /// is the same result for a fraction of the cost.
    fn seed_iterable_container_traits(&mut self) {
        let (iterable, container) = Self::cached_iterable_container_traits();
        self.traits.insert("Iterable".to_string(), iterable.clone());
        self.traits.insert("Container".to_string(), container.clone());
    }

    /// The `Iterable`/`Container` `TraitDef`s, parsed and hoisted exactly
    /// once per process and cached for every `TypeChecker` built after the
    /// first. Built against a bare bootstrap checker (not `Self::empty()`,
    /// which would recurse back into `seed_iterable_container_traits`) —
    /// only `traits`/`struct_templates`/`union_defs` matter to hoisting, so
    /// the rest of its fields are never touched.
    fn cached_iterable_container_traits() -> &'static (TraitDef, TraitDef) {
        static CACHE: std::sync::OnceLock<(TraitDef, TraitDef)> = std::sync::OnceLock::new();
        CACHE.get_or_init(|| {
            let src = "trait Iterable<Item> { func next(mut s: Self): Item? }\n\
                        trait Container<Item> { func has(s: Self, x: Item): Bool\nfunc len(s: Self): Int }\n";
            let ast = crate::frontend::parser::Parser::parse(src)
                .expect("builtin Iterable/Container trait source must parse");
            let stmts = match ast.item {
                Expression::Block(stmts) => stmts,
                other => vec![Spanned::from(other, ast.span)],
            };
            let mut boot = TypeChecker { ctx: ScopeStack::new(HashMap::new()), substitutions: HashMap::new(), next_id: 0, struct_defs: HashMap::new(), struct_templates: TypeChecker::initial_struct_templates(), struct_type_params: TypeChecker::initial_struct_type_params(), type_param_scope: HashMap::new(), union_defs: HashMap::new(), union_names: HashMap::new(), variant_owners: HashMap::new(), return_types: Vec::new(), provides: HashMap::new(), traits: TypeChecker::initial_traits(), impls: HashMap::new(), member_index: HashMap::new(), member_traits: HashMap::new(), func_mut_params: HashMap::new(), host_names: std::collections::HashSet::new(), generic_instantiations: HashMap::new(), generic_templates: HashMap::new(), emitted_instantiations: std::collections::HashSet::new(), fn_templates: HashMap::new(), lifted_captures: HashMap::new(), emitted_specializations: std::collections::HashSet::new(), mut_names: std::collections::HashSet::new(), annotation_defs: HashMap::new(), annotation_defaults: HashMap::new(), struct_field_defaults: HashMap::new() };
            boot.hoist_trait_names(&stmts).expect("builtin Iterable/Container trait names must hoist");
            boot.hoist_trait_members(&stmts).expect("builtin Iterable/Container trait members must hoist");
            let iterable = boot.traits.remove("Iterable").expect("hoisted above");
            let container = boot.traits.remove("Container").expect("hoisted above");
            (iterable, container)
        })
    }

    /// Check whether a concrete type implements the given trait. Only makes
    /// sense for non-TypeVar types; TypeVar-TypeVar unification is handled
    /// separately so this is never called on a TypeVar.
    fn type_implements(&self, ty: &Type, tr: &Trait) -> bool {
        self.type_implements_rec(ty, tr, &mut Vec::new())
    }

    /// `type_implements(ty, Trait::Linear)`, exposed for `frontend::linear`
    /// — the one external consumer of trait membership this module has
    /// today, kept as a narrow named accessor rather than widening
    /// `type_implements` itself to `pub(crate)`.
    pub(crate) fn implements_linear(&self, ty: &Type) -> bool {
        self.type_implements(ty, &Trait::Linear)
    }

    /// `type_implements`'s body, carrying the set of struct types already
    /// being examined further up this recursion.
    ///
    /// A structural derivation must recurse into field types
    /// (`TRAITS.md` Part 3, `plans/DATA.md` stage 1): `data W(xs: List<Int>)`
    /// used to satisfy `Eq` because *any* struct did, and
    /// `build_struct_eq`'s synthesized per-field `Binary` nodes are never
    /// re-checked, so `W(xs=[1]) == W(xs=[1])` compiled into a raw pointer
    /// comparison and answered `false` — a silent wrong answer, exactly
    /// what the trait check exists to prevent. Recursing here reports at the
    /// struct instead, before any comparison is synthesized.
    ///
    /// `seen` makes this total over a type that reaches itself (a nominal
    /// union's variant whose field is that union again — the shape
    /// `hoist_data_decls` permits because the field is boxed). Re-entering a
    /// type is treated as satisfied: the derivation for it is exactly the
    /// one still being decided, so the recursion is well-founded on the
    /// fields that are *not* cyclic.
    fn type_implements_rec(&self, ty: &Type, tr: &Trait, seen: &mut Vec<Type>) -> bool {
        match ty {
            // A union satisfies a trait iff every variant does. For `Eq`
            // that is decided at runtime by `codegen::eq_union`: compare the
            // two tags, then that member's payload. A *recursive* union is
            // the one case that rule can't reach — the dispatch would need
            // unbounded branch trees — and it is rejected at the operator by
            // `check_comparable`, with a span, rather than here: "not `Eq`"
            // would be the wrong reason.
            Type::Union(variants) => variants.iter().all(|v| self.type_implements_rec(v, tr, seen)),
            // `Error`, `Linear`, and every user-declared trait are *granted*,
            // never structural — a deliberate design choice, not a gap
            // (`ERRORS.md`, "Why a trait and not an open union"). One arm for
            // all three, covering primitives as well as named types, because
            // `provides Shape for Int` is a legitimate standalone impl.
            _ if matches!(tr, Trait::Error | Trait::Linear | Trait::User(_)) => self.granted(ty, tr),
            // `List<T>` is `Eq` iff `T` is. The comparison is structural —
            // lengths, then elements pairwise — not identity, which is what
            // anyone coming from Python expects `[1, 2] == [1, 2]` to mean.
            // It cannot be desugared into a fixed conjunction the way a
            // struct's is (the length is a runtime value), so codegen emits
            // the element loop instead: `eq_list`. `seen` guards the same
            // cycle a struct field can form (`data W(xs: List<W>)`).
            Type::Named { name, args } if name == LIST_NAME && matches!(tr, Trait::Eq | Trait::Show) => {
                if seen.contains(ty) { return true; }
                seen.push(ty.clone());
                // An element type still a variable is undetermined, not
                // failing — an empty list literal never fixes one, and
                // `[] == []` should hold.
                let ok = args.iter()
                    .all(|a| matches!(a, Type::TypeVar { .. }) || self.type_implements_rec(a, tr, seen));
                seen.pop();
                ok
            },
            // Structs get structural `==`/`!=`, desugared into a per-field
            // conjunction at lowering time — see `TypeChecker::desugar_struct_eq`
            // in `check_and_lower`'s `Binary` arm — so a struct satisfies
            // `Eq` iff every one of its fields does, and iff every one of
            // its generic `args` does (`TRAITS.md` Stage 3a; vacuously true
            // for a non-generic struct's empty `args`). The `args` check is
            // not redundant with the field check: a binder that appears in
            // no field still has to be `Eq` for the instantiation to be.
            Type::Named { name, args } if name != LIST_NAME && matches!(tr, Trait::Eq | Trait::Show) => {
                if seen.contains(ty) { return true; }
                if !args.iter().all(|a| self.type_implements_rec(a, tr, seen)) { return false; }
                seen.push(ty.clone());
                let ok = self.struct_field_types(name, args).iter()
                    // A field still typed as a variable is undetermined, not
                    // failing — this runs during inference, and generic code
                    // is checked again per instantiation.
                    .all(|fty| matches!(fty, Type::TypeVar { .. }) || self.type_implements_rec(fty, tr, seen));
                seen.pop();
                ok
            },
            _ => match tr {
                Trait::Num    => matches!(ty, Type::Int | Type::Float),
                // `None` is `Eq` so an optional is: `Int | None` satisfies
                // the union rule above only if every member does, and
                // `none == none` is true by construction — the type has
                // exactly one value.
                Trait::Eq     => matches!(ty, Type::Int | Type::Float | Type::Bool | Type::Str | Type::None),
                Trait::Show   => matches!(ty, Type::Int | Type::Float | Type::Bool | Type::Str | Type::None),
                Trait::Ord    => matches!(ty, Type::Int | Type::Float | Type::Str),
                Trait::Truthy => matches!(ty, Type::Int | Type::Float | Type::Bool | Type::Str | Type::None) || ty.is_list(),
                // Handled by the granted arm above, before any of this.
                Trait::Error | Trait::Linear | Trait::User(_) => false,
            }
        }
    }

    /// Is `tr` granted to `ty` by a `provides` clause?
    ///
    /// Keyed by `grant_key`, so a generic declaration's grant covers every
    /// instantiation: `provides` states a property of the *declaration*, and
    /// `Box<Int>` is not separately grantable from `Box<Str>`.
    fn granted(&self, ty: &Type, tr: &Trait) -> bool {
        match Self::grant_key(ty) {
            Some(key) => self.provides.get(&key).map(|ts| ts.contains(tr)).unwrap_or(false),
            None => false,
        }
    }

    /// The `provides`/impl table key for a type: its declared name.
    ///
    /// Type arguments are deliberately dropped (`Box<Int>` and `Box<Str>`
    /// share `Box`'s key), and primitives get their own name so a standalone
    /// `provides Shape for Int` has somewhere to land. `None` for a type that
    /// cannot carry a grant at all — an anonymous union (whose membership is
    /// decided by the all-variants rule instead), a function type, an
    /// unresolved variable, `Never`.
    fn grant_key(ty: &Type) -> Option<String> {
        match ty {
            Type::Named { name, .. } => Some(name.clone()),
            Type::Int   => Some("Int".to_string()),
            Type::Float => Some("Float".to_string()),
            Type::Bool  => Some("Bool".to_string()),
            Type::Str   => Some("Str".to_string()),
            Type::None  => Some("None".to_string()),
            _ => None,
        }
    }

    /// `materialize_struct`'s read-only counterpart: this instantiation's
    /// field types, with the declared binders substituted by `args`. Kept
    /// separate because `type_implements` runs behind `&self` and must not
    /// register a layout as a side effect of *asking a question*.
    fn struct_field_types(&self, name: &str, args: &[Type]) -> Vec<Type> {
        let template = match self.struct_templates.get(name) {
            Some(fields) => fields,
            None => return Vec::new(),
        };
        if args.is_empty() {
            return template.iter().map(|(_, fty)| fty.clone()).collect();
        }
        let binders = self.struct_type_params.get(name).cloned().unwrap_or_default();
        let mapping: HashMap<String, Type> = binders.into_iter().zip(args.iter().cloned()).collect();
        template.iter().map(|(_, fty)| fty.substitute(&mapping)).collect()
    }

    /// Validate a condition-position type: `Bool` unifies
    /// directly (as before — this also still pins down an unconstrained
    /// `TypeVar`, e.g. an unannotated lambda param used as a condition);
    /// anything else must satisfy `Trait::Truthy`. Pure type-checking, no
    /// lowering — `coerce_truthy` is `check_and_lower`'s counterpart that
    /// actually inserts the runtime coercion.
    fn check_condition(&mut self, cond_ty: &Type, span: Span) -> Result<(), Spanned<TypeError>> {
        if self.unify(cond_ty, &Type::Bool) { return Ok(()); }
        let resolved = self.lookup(cond_ty);
        if self.type_implements(&resolved, &Trait::Truthy) { return Ok(()); }
        Err(Spanned::from(TypeError {
            msg: format!("condition must be Bool or a Truthy type (Int/Float/Str/List/None), got {}", resolved)
        }, span))
    }

    /// `check_condition`'s lowering counterpart: wraps an already-lowered,
    /// non-`Bool` `Trait::Truthy` value in a `Truthy` node so it actually
    /// becomes a runtime `Bool` (see that node's doc comment). A no-op for
    /// an already-`Bool` value. Assumes the caller already ran
    /// `check_condition` on `e`'s type — this only builds the coercion, it
    /// doesn't reject anything.
    fn coerce_truthy(&mut self, e: Spanned<TypedExpr>, span: Span) -> Spanned<TypedExpr> {
        if e.item.ty == Type::Bool { return e; }
        // A union satisfies `Trait::Truthy` iff every member does
        // (`type_implements_rec`'s union rule) — `check_condition` already
        // proved that, but `codegen::compile_truthy` only knows how to read
        // a bare scalar/`List`/`None`'s own bits, not dispatch on a runtime
        // tag first. Desugar into the dispatch here instead of teaching
        // codegen a new node, the same way `?`/`!`/`catch` desugar into an
        // ordinary `match`: tag-test each member (right-to-left, so the
        // last one needs no test — the tags are exhaustive by
        // construction), narrow to it, and recurse `coerce_truthy` on the
        // narrowed value to get *that* member's own Truthy rule.
        if let Type::Union(members) = self.lookup(&e.item.ty) {
            return self.coerce_truthy_union(e, members, span);
        }
        Spanned::from(TypedExpr { id: 0, ty: Type::Bool, kind: TypedExprKind::Truthy(Box::new(e)) }, span)
    }

    /// `coerce_truthy`'s union case, split out: bind `e` to a temporary
    /// (its members are each read at least once below, and `e` may be
    /// side-effecting) then fold a right-to-left `TypeTag`/`Narrow`
    /// dispatch, exactly like `fold_match_arm`'s tag chain but with no
    /// pattern binds and a boolean result instead of an arm body.
    fn coerce_truthy_union(&mut self, e: Spanned<TypedExpr>, members: Vec<Type>, span: Span) -> Spanned<TypedExpr> {
        let subject_ty = Type::Union(members.clone());
        let subject_name = format!("__truthy_subject_{}", self.next_id); self.next_id += 1;
        let subject_assign = Spanned::from(
            TypedExpr { id: 0, ty: subject_ty.clone(), kind: TypedExprKind::Assign { name: subject_name.clone(), value: Box::new(e) } },
            span,
        );

        let mut chain: Option<Spanned<TypedExpr>> = None;
        for (i, member_ty) in members.iter().enumerate().rev() {
            let subject_var = Spanned::from(
                TypedExpr { id: 0, ty: subject_ty.clone(), kind: TypedExprKind::Var(subject_name.clone()) },
                span,
            );
            let narrowed = Spanned::from(
                TypedExpr { id: 0, ty: member_ty.clone(), kind: TypedExprKind::Narrow { value: Box::new(subject_var.clone()), tag: i as u32 } },
                span,
            );
            let member_truthy = self.coerce_truthy(narrowed, span);
            chain = Some(match chain {
                None => member_truthy,
                Some(rest) => {
                    let tag_test = Spanned::from(
                        TypedExpr { id: 0, ty: Type::Bool, kind: TypedExprKind::TypeTag { target: Box::new(subject_var), tag: i as u32 } },
                        span,
                    );
                    Spanned::from(
                        TypedExpr { id: 0, ty: Type::Bool, kind: TypedExprKind::Conditional {
                            cond: Box::new(tag_test), true_branch: Box::new(member_truthy), false_branch: Some(Box::new(rest)),
                        } },
                        span,
                    )
                }
            });
        }
        let chain = chain.expect("a union type always has at least one member");
        Spanned::from(TypedExpr { id: 0, ty: Type::Bool, kind: TypedExprKind::Block(vec![subject_assign, chain]) }, span)
    }

    /// A statement-position value may not silently discard a possible
    /// `Error` — the must-handle rule (`ERRORS.md`). Applies to every
    /// non-tail `Block` statement and every `for`-loop body; a tail
    /// position is exempt because its value propagates to the caller,
    /// where the same value is (recursively) subject to this same rule.
    /// `let x = ...`/`obj.f = ...` are exempt too: binding a name isn't
    /// discarding — `x` still carries the union type onward to wherever it
    /// (recursively) needs to be handled, exactly like a tail value does.
    fn check_must_handle(&self, stmt: &Spanned<TypedExpr>) -> Result<(), Spanned<TypeError>> {
        if matches!(stmt.item.kind, TypedExprKind::Assign { .. } | TypedExprKind::PlaceAssign { .. }) {
            return Ok(());
        }
        let ty = &stmt.item.ty;
        let span = stmt.span;
        let offender = match ty {
            Type::Union(members) => members.iter().find(|m| self.type_implements(m, &Trait::Error)),
            other if self.type_implements(other, &Trait::Error) => Some(other),
            _ => None,
        };
        if let Some(m) = offender {
            return Err(Spanned::from(TypeError {
                msg: format!(
                    "unhandled error: this statement's value has type '{}', which may be '{}' \
                     (declared to provide Error) — bind it, match it, or otherwise handle it \
                     instead of discarding it",
                    ty, m
                )
            }, span));
        }
        Ok(())
    }

    /// Field layout for every registered struct, in declaration order.
    /// Threaded into `Codegen::compile_entry` so codegen can flatten
    /// struct-typed values into their leaf fields — see `struct_fields`
    /// in `codegen/mod.rs`.
    pub fn struct_defs(&self) -> &StructDefs {
        &self.struct_defs
    }

    /// Layout for every registered nominal union, in declaration order.
    /// Threaded into `Codegen` alongside `struct_defs`.
    pub fn union_defs(&self) -> &UnionDefs {
        &self.union_defs
    }

    /// Concrete, positionally-substituted field list for an already-typed
    /// struct value's type (`resolved.as_struct_name().is_some()` is
    /// assumed already checked by the caller) — `TRAITS.md` Stage 3a's
    /// "read" path (field access, place-assignment, UFCS field lookup).
    /// For a non-generic struct this is just the stored template
    /// unchanged. For a generic instantiation (`Type::Named{name, args}`
    /// with non-empty `args`) it substitutes `args` into the generic
    /// template positionally, keyed by `struct_type_params[name]`'s
    /// placeholder `TypeVar` names, and — the load-bearing part —
    /// registers the result into `struct_defs` under `resolved` itself
    /// (memoized: a repeat lookup of the same instantiation just returns
    /// the cached entry). That registration is what lets
    /// `codegen::struct_fields` (and `field_slice_range`/`print_value`,
    /// which look the same key up directly) find a concrete layout for an
    /// instantiation it never itself computes — by the time codegen runs,
    /// every instantiation appearing anywhere in the typed program has
    /// already passed through here or through `instantiate_struct`
    /// (the construction-site counterpart, which registers the same way).
    fn materialize_struct(&mut self, resolved: &Type) -> Vec<(String, Type)> {
        let Type::Named { name, args } = resolved else { return Vec::new() };
        if let Some(existing) = self.struct_defs.get(resolved) {
            return existing.clone();
        }
        let template = self.struct_templates.get(name).cloned().unwrap_or_default();
        // A non-generic struct's template *is* its layout, and
        // `hoist_data_decls` already registered it under this same key.
        if args.is_empty() { return template; }
        let binder_names = self.struct_type_params.get(name).cloned().unwrap_or_default();
        let mapping: HashMap<String, Type> = binder_names.into_iter().zip(args.iter().cloned()).collect();
        let concrete: Vec<(String, Type)> = template.into_iter()
            .map(|(fname, fty)| (fname, fty.substitute(&mapping)))
            .collect();
        self.struct_defs.insert(resolved.clone(), concrete.clone());
        concrete
    }

    /// Fresh-instantiate a generic struct's declared field template for a
    /// construction call (`Name(...)`) — mints one brand-new placeholder
    /// `TypeVar` per declared binder (mirrors `instantiate`'s per-call
    /// fresh-renaming for a generalized function scheme, applied here to a
    /// struct's own `<A, B>` binders instead), so two separate `Pair(...)`
    /// construction calls in the same program don't fight over one shared
    /// global substitution the way reusing the stored template's own
    /// placeholders directly would. Returns the field list to check
    /// constructor arguments against (via `lower_record_args`, which
    /// `unify`/`lower_widen`s each argument into its declared field type,
    /// binding these fresh vars as a side effect) plus the fresh `TypeVar`
    /// for each binder in declared order — `self.lookup`ing those after
    /// `lower_record_args` returns gives the concrete instantiation's
    /// `args`. A non-generic struct (or an unregistered name) just returns
    /// its ordinary field list and no binder vars.
    fn instantiate_struct(&mut self, name: &str) -> (Vec<(String, Type)>, Vec<Type>) {
        let field_defs = self.struct_templates.get(name).cloned().unwrap_or_default();
        let binder_names = match self.struct_type_params.get(name) {
            Some(b) if !b.is_empty() => b.clone(),
            _ => return (field_defs, Vec::new()),
        };
        let mapping: HashMap<String, Type> = binder_names.iter()
            .map(|n| (n.clone(), self.fresh_var()))
            .collect();
        let fresh_fields = field_defs.into_iter()
            .map(|(fname, fty)| (fname, fty.substitute(&mapping)))
            .collect();
        let arg_vars = binder_names.iter().map(|n| mapping[n].clone()).collect();
        (fresh_fields, arg_vars)
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

    /// Instantiate a type scheme (`TRAITS.md` Stage 2): fresh-rename every
    /// occurrence of each name in `binders` within `ty`, substituting a
    /// fresh bounded `TypeVar` per binder — one fresh set per call, so
    /// `let f = x -> x + x; f(1); f(1.5)` gets an independent instantiation
    /// at each call rather than sharing one substitution the way an
    /// ordinary (non-generalized) `TypeVar` would. Bounds travel from the
    /// binder onto the fresh var, since `unify`/`type_implements` only ever
    /// consult a `TypeVar`'s own `bounds` field, never a separate table —
    /// this is what keeps `func max<T: Ord>(x: T, y: T): T` constrained at
    /// every instantiation, not just the first.
    ///
    /// A structural walk over `ty` itself, not through `lookup`/
    /// `substitutions` — a scheme's binders are bound within the stored
    /// type, distinct from the mutable global unification variables an
    /// ordinary `TypeVar` participates in. Any free `TypeVar` in `ty` that
    /// is *not* one of `binders` (possible if `ty` closes over an outer,
    /// still-live variable) is left untouched, so it stays linked to
    /// whatever it already resolves to rather than being fresh-renamed.
    fn instantiate(&mut self, ty: &Type, binders: &[(String, Vec<Trait>)]) -> Type {
        if binders.is_empty() { return ty.clone(); }
        let mapping: HashMap<String, Type> = binders.iter()
            .map(|(name, bounds)| (name.clone(), self.fresh_bounded_var(bounds.clone())))
            .collect();
        ty.substitute(&mapping)
    }

    /// Collect every free `TypeVar` (after substitution) reachable from
    /// `ty`, name and bounds, into `out` — deduplicated by name. Shared by
    /// `generalize`'s two scans: `ty`'s own free vars, and every free var
    /// still live somewhere in the environment.
    fn free_vars(&self, ty: &Type, out: &mut Vec<(String, Vec<Trait>)>) {
        match self.lookup(ty) {
            Type::TypeVar { name, bounds } => {
                if !out.iter().any(|(n, _)| *n == name) {
                    out.push((name, bounds));
                }
            },
            Type::Function { params, result } => {
                for p in &params { self.free_vars(p, out); }
                self.free_vars(&result, out);
            },
            Type::Named { args, .. } => {
                for a in &args { self.free_vars(a, out); }
            },
            Type::Union(variants) => {
                for v in &variants { self.free_vars(v, out); }
            },
            Type::None | Type::Int | Type::Float | Type::Bool | Type::Str | Type::Never => {},
        }
    }

    /// Which of `ty`'s free vars should become this binding's quantified
    /// binders (`TRAITS.md` Stage 2's generalization step, "env-scanning
    /// generalization"): free in `ty` but not free anywhere else in the
    /// current environment. A var `ty` shares with something already in
    /// scope — captured from an enclosing binding, most concretely a
    /// still-open outer lambda parameter — is deliberately left
    /// un-generalized, since fresh-renaming it per call would silently
    /// disconnect it from what it's meant to stay linked to. Called
    /// *before* the binding being generalized is itself inserted, so the
    /// environment scan doesn't see it.
    ///
    /// `self_name` names the binding being generalized when it *is* already
    /// in scope — a function pre-bound to a placeholder type so its own body
    /// could call it (`lower_assign`'s recursion pre-bind). Its placeholder
    /// resolves to the very type being generalized, so leaving it in the
    /// scan would make every one of `ty`'s vars look environment-captured
    /// and generalize nothing at all: `func id(x) = x` would stop being
    /// generic the moment it became capable of recursion.
    fn generalize(&self, ty: &Type, self_name: Option<&str>) -> Vec<(String, Vec<Trait>)> {
        let mut ty_vars = Vec::new();
        self.free_vars(ty, &mut ty_vars);
        if ty_vars.is_empty() { return Vec::new(); }
        let mut env_vars = Vec::new();
        match self_name {
            Some(skip) => for bound in self.ctx.bound_types_except(skip) {
                self.free_vars(bound, &mut env_vars);
            },
            None => for bound in self.ctx.bound_types() {
                self.free_vars(bound, &mut env_vars);
            },
        }
        ty_vars.into_iter().filter(|(n, _)| !env_vars.iter().any(|(en, _)| en == n)).collect()
    }

    pub fn checkpoint(&self) -> TypeCheckerCheckpoint {
        TypeCheckerCheckpoint {
            ctx: self.ctx.clone(),
            substitutions: self.substitutions.clone(),
            next_id: self.next_id,
            struct_defs: self.struct_defs.clone(),
            struct_templates: self.struct_templates.clone(),
            struct_type_params: self.struct_type_params.clone(),
            union_defs: self.union_defs.clone(),
            union_names: self.union_names.clone(),
            variant_owners: self.variant_owners.clone(),
            return_types: self.return_types.clone(),
            provides: self.provides.clone(),
            traits: self.traits.clone(),
            impls: self.impls.clone(),
            member_index: self.member_index.clone(),
            member_traits: self.member_traits.clone(),
            func_mut_params: self.func_mut_params.clone(),
            generic_instantiations: self.generic_instantiations.clone(),
            generic_templates: self.generic_templates.clone(),
            emitted_instantiations: self.emitted_instantiations.clone(),
            fn_templates: self.fn_templates.clone(),
            lifted_captures: self.lifted_captures.clone(),
            emitted_specializations: self.emitted_specializations.clone(),
            mut_names: self.mut_names.clone(),
            annotation_defs: self.annotation_defs.clone(),
            annotation_defaults: self.annotation_defaults.clone(),
            struct_field_defaults: self.struct_field_defaults.clone(),
        }
    }

    pub fn restore(&mut self, cp: TypeCheckerCheckpoint) {
        self.ctx = cp.ctx;
        self.substitutions = cp.substitutions;
        self.next_id = cp.next_id;
        self.struct_defs = cp.struct_defs;
        self.struct_templates = cp.struct_templates;
        self.struct_type_params = cp.struct_type_params;
        self.union_defs = cp.union_defs;
        self.union_names = cp.union_names;
        self.variant_owners = cp.variant_owners;
        self.return_types = cp.return_types;
        self.provides = cp.provides;
        self.traits = cp.traits;
        self.impls = cp.impls;
        self.member_index = cp.member_index;
        self.member_traits = cp.member_traits;
        self.generic_instantiations = cp.generic_instantiations;
        self.generic_templates = cp.generic_templates;
        self.emitted_instantiations = cp.emitted_instantiations;
        self.fn_templates = cp.fn_templates;
        self.lifted_captures = cp.lifted_captures;
        self.emitted_specializations = cp.emitted_specializations;
        self.mut_names = cp.mut_names;
        self.func_mut_params = cp.func_mut_params;
        self.annotation_defs = cp.annotation_defs;
        self.annotation_defaults = cp.annotation_defaults;
        self.struct_field_defaults = cp.struct_field_defaults;
    }

    /// Install additional global bindings (host functions,
    /// `FrogStateBuilder::func` — see `plans/EMBEDDING.md`) into scope
    /// alongside the builtins `default_context` seeds. `&mut self` rather
    /// than the original by-value builder shape: `FrogState` owns its `tc`
    /// by value, so a builder method here would force an awkward
    /// take-then-put-back at every call site.
    pub fn add_ctx(&mut self, ctx: impl Iterator<Item=(String, Type)>) {
        for (k, v) in ctx {
            self.host_names.insert(k.clone());
            self.ctx.insert(k, v);
        }
    }

    pub fn context(self) -> HashMap<String, Type> {
        self.ctx.into_bindings()
    }

    fn default_context() -> HashMap<String, Type> {
        let mut ctx = HashMap::new();
        ctx.insert("print".to_string(), Type::Function {
            params: vec![Type::Str],
            result: Box::new(Type::None),
        });
        ctx.insert("gc_dump".to_string(), Type::Function {
            params: vec![],
            result: Box::new(Type::None),
        });
        // Prints `msg` then aborts — a deliberate placeholder for real
        // unwinding (`CONCURRENCY.md` stage 1). `result: Never` means a
        // call to `panic` unifies with anything and vanishes from any
        // union it stands in (same as `return`) — see codegen's generic
        // `Call` arm for the corresponding trap-after-call codegen. `!`
        // desugars to a call to `UNWRAP_PANIC_NAME` (below), a reserved
        // alias for this same builtin — rather than `panic` being sugar
        // for `!` — `panic` itself stays callable directly.
        ctx.insert("panic".to_string(), Type::Function {
            params: vec![Type::Str],
            result: Box::new(Type::Never),
        });
        // Reserved alias for the same builtin, resolved by
        // `build_unwrap_arms` instead of `"panic"`. `Token::Identifier`
        // values built here bypass the lexer (which never produces `!` in
        // an identifier), so this name can't collide with, or be shadowed
        // by, any user binding or `func panic(...)` redefinition — `!`
        // always traps through the real builtin regardless of what's in
        // scope. See `codegen`'s matching `declare_rt` alias.
        ctx.insert(UNWRAP_PANIC_NAME.to_string(), Type::Function {
            params: vec![Type::Str],
            result: Box::new(Type::Never),
        });
        ctx
    }

    /// Type-only entry point, kept for callers that want a type and nothing
    /// else (the type-checker's own unit tests, and `frog check`).
    ///
    /// There is no separate inference pass any more: `check_and_lower` walks
    /// the tree exactly once and every node's type falls out of its
    /// already-lowered children, so "infer" is just "lower and throw the
    /// tree away". The clone is the price of the `&`-taking signature; it is
    /// paid only by these few callers, never on the compiler's own path.
    pub fn infer(&mut self, expr: &Spanned<Expression>) -> TypeResult {
        self.check_and_lower(expr.clone()).map(|typed| typed.item.ty)
    }

    /// `infer`'s checking counterpart: validate `expr` against an expected
    /// type and return the type it checks *at* — which is `expected_ty`
    /// itself whenever the two differ only by subtyping or a numeric
    /// promotion, since that's the type the value actually has once
    /// `lower_expected` has widened it into the slot.
    pub fn check(&mut self, expr: &Spanned<Expression>, expected_ty: &Type) -> TypeResult {
        self.lower_expected(expr.clone(), expected_ty).map(|typed| typed.item.ty)
    }

    /// If `ty` is a `Type::Union` matching a registered nominal union
    /// (`data X is A | B`), return its declared name and definition. `None`
    /// for any other type, including an anonymous structural union with no
    /// matching declaration.
    fn resolve_union(&self, ty: &Type) -> Option<(&str, &UnionDef)> {
        if let Type::Union(members) = ty {
            if let Some(name) = self.union_names.get(members) {
                return self.union_defs.get(name).map(|d| (name.as_str(), d));
            }
        }
        None
    }

    /// Resolve a bare type *name* — a builtin, a registered struct, or a
    /// registered nominal union — to a `Type`. Shared by `resolve_type_expr`
    /// (a full `TypeExpr::Name`) and by pattern resolution (`is <Name>` on
    /// an anonymous union — see `check_and_lower`'s `IsPattern` arm and
    /// `lower_match`), which
    /// only ever has a bare `String` to work with, not a parsed `TypeExpr`.
    fn resolve_type_name(&self, name: &str) -> Option<Type> {
        if let Some(ty) = self.type_param_scope.get(name) {
            return Some(ty.clone());
        }
        match name {
            "Int"   => Some(Type::Int),
            "Float" => Some(Type::Float),
            "Bool"  => Some(Type::Bool),
            "Str"   => Some(Type::Str),
            "None"  => Some(Type::None),
            _ if self.struct_templates.contains_key(name) => Some(Type::strukt(name)),
            _ if self.union_defs.contains_key(name)  => Some(self.union_defs[name].ty.clone()),
            _ => None,
        }
    }

    /// If `lowered`'s type doesn't already match `target`, and `target` is
    /// an *anonymous* `Type::Union` (see `resolve_union`) that `lowered`'s
    /// type is a flat member of, wrap `lowered` in an explicit `Widen` node
    /// so codegen actually boxes it into the union's runtime
    /// representation. Subtyping is otherwise invisible at runtime —
    /// `check`/`unify` accept a narrower value for a wider expected type,
    /// but nothing coerces it — so every lowering site that places a
    /// `check`-validated value into a `Type::Union`-typed slot (an
    /// annotated `let`, a function's declared return type, a call
    /// argument, a struct/variant field) must route the lowered value
    /// through here. A no-op for any other case (already the right type,
    /// or `target` isn't a union — those are handled elsewhere, e.g. the
    /// ambient `Int -> Float` widening).
    ///
    /// Widening a value that's *already* union-typed (nominal or anonymous)
    /// into a *different* union isn't supported yet — rejected with a clear
    /// error rather than silently generating an incorrect box, since
    /// `Type::normalize`'s flattening means the outer union's sorted member
    /// list can assign the same member a different local tag than its
    /// original union did, which would silently misread an existing boxed
    /// value's tag under the new numbering. Every other member type is
    /// supported: a union that `codegen::union_is_inline` accepts carries
    /// every member's fields in its own flattened columns and allocates
    /// nothing at all, and one it rejects (more than
    /// `codegen::MAX_INLINE_UNION_MEMBERS` members, or self-referential) is
    /// boxed into a `FrogVariant` exactly like a nominal union's non-nullary
    /// member already is (`box_into_variant`).
    fn lower_widen(&self, mut lowered: Spanned<TypedExpr>, target: &Type) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        // Resolved, not the raw `TypedExpr::ty` — an empty-list literal (or
        // anything else whose element type was a bare `TypeVar` at lowering
        // time) can have just been unified against `target` by the caller
        // (`lower_call_arg`, a struct field, ...), but `unify` only updates
        // the substitution table, never the node's `ty` in place. Comparing
        // the raw types here used to fall through every branch below and
        // return the node unchanged — so codegen, which reads `ty` directly,
        // saw e.g. `List<t0>` land in a `List<Str>` slot and panicked on "no
        // widening". Resolving both sides first, and writing the resolved
        // type back onto every path out of this function, is what keeps a
        // `TypedExpr::ty` trustworthy once lowering has moved past it.
        let from = self.lookup(&lowered.item.ty);
        let target = self.lookup(target);
        // `Never` (a `return`/`panic`-typed expression, which produces no
        // value on the path that reaches here) must keep reading `Never`
        // regardless of `target` — codegen and block-typing downstream both
        // key off that to know the node never falls through, and rewriting
        // it to `target` would claim a value is produced where none is.
        if from == Type::Never {
            return Ok(lowered);
        }
        if from == target {
            lowered.item.ty = target;
            return Ok(lowered);
        }
        // Every remaining path either returns `lowered` as it stands or
        // wraps it in a `Coerce`/`Widen`, so resolve its type once, here,
        // rather than on each of those paths — the wrapped node is just as
        // much a node codegen reads `ty` off as the returned one is, and a
        // `Widen` whose payload still claims `List<~t0>` is the same stale
        // type the resolution above exists to remove.
        lowered.item.ty = from.clone();
        // A non-lossy numeric promotion into a wider declared slot — see
        // `TypedExprKind::Coerce`. This has to happen here rather than at
        // each call site because `lower_widen` is already the single funnel
        // every such value passes through (struct/variant fields, list
        // elements, annotations, declared return types).
        if widens_to(&from, &target) {
            let span = lowered.span;
            return Ok(Spanned::from(
                TypedExpr { id: 0, ty: target.clone(), kind: TypedExprKind::Coerce(Box::new(lowered)) },
                span,
            ));
        }
        let Type::Union(members) = &target else { return Ok(lowered) };
        let span = lowered.span;
        // `target`'s own members are always flat (`Type::normalize` never
        // leaves a union nested inside another), so `from` being a
        // `Type::Union` itself can never equal one of them — the `position`
        // lookup below would always miss and silently fall through to
        // `Ok(lowered)` unchanged, leaving a value in its *narrower* union's
        // own representation (a different member count and tag numbering)
        // embedded somewhere that expects `target`'s wider one. That used
        // to be a silent miscompile (a two-member `ParseError`'s tag word
        // misread as an index into a three-member `Int | ParseError`, e.g.
        // a `BadChar` reading UnexpectedEof's payload) rather than a
        // rejection, since nothing upstream re-checks that a widened node's
        // representation actually matches its claimed type. Reject it
        // explicitly instead — this whole-union-to-union widening just
        // isn't implemented yet (unlike widening a single member in).
        if matches!(from, Type::Union(_)) {
            return Err(Spanned::from(TypeError {
                msg: format!("widening a union-typed value ({}) into a different union ({}) is not yet supported", from, target)
            }, span));
        }
        let Some(tag) = members.iter().position(|m| *m == from) else { return Ok(lowered) };
        Ok(Spanned::from(
            TypedExpr { id: 0, ty: target.clone(), kind: TypedExprKind::Widen { value: Box::new(lowered), tag: tag as u32 } },
            span,
        ))
    }

    /// Type-check *and* lower `expr` against a known expected type — the
    /// checking half of the bidirectional pair whose other half is
    /// `check_and_lower` (synthesis). Every site that has an expected type
    /// to impose goes through here: an annotated `let`, a declared return
    /// type, a `return` statement, a struct field assignment.
    ///
    /// Three cases, in order:
    ///
    /// 1. **A lambda literal against a `Function` type** — the parameters
    ///    take their types from the expectation rather than becoming fresh
    ///    variables, so `let f: (Int -> Int) = [x] -> x + 1` types
    ///    `x` without an annotation on the lambda itself.
    /// 2. **A list literal against a `List` type** — the expected *element*
    ///    type is pushed into each element (recursively, so
    ///    `List<List<Int>>` works too). Bottom-up can't type either of the
    ///    two cases an annotation exists to resolve:
    ///    - `let xs: List<Str> = []` — bare `[]` synthesizes `List(~t0)`,
    ///      and `is_subtype` can't see through the `List` to bind it.
    ///    - `let xs: List<Int | Str> = [1, "a"]` — the elements only agree
    ///      once each is widened to the annotated union, which the
    ///      literal's own same-type unification rejects first.
    /// 3. **Anything else** — synthesize normally, then accept the result
    ///    if it's a subtype of `expected`, a non-lossy numeric promotion
    ///    into it, or an open type variable that unifies with it; and
    ///    finally widen the lowered value into the expected slot
    ///    (`lower_widen` inserts the `Widen`/`Coerce` that makes the
    ///    accepted subtyping actually hold at runtime).
    fn lower_expected(&mut self, expr: Spanned<Expression>, expected: &Type) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let span = expr.span;
        let expected = self.lookup(expected);

        if let Expression::Function(func) = expr.item {
            let Type::Function { params, result } = &expected else {
                return Err(Spanned::from(TypeError { msg: "Expected function type".to_string() }, span));
            };
            if func.params.len() != params.len() {
                return Err(Spanned::from(TypeError {
                    msg: format!("Wrong number of arguments, expected {}, got {}", params.len(), func.params.len())
                }, span));
            }
            let bindings: Vec<(String, Type, bool)> = func.params.iter().zip(params.iter())
                .map(|(p, pty)| (p.name.clone(), pty.clone(), p.mutable))
                .collect();
            let return_type = (**result).clone();
            self.return_types.push(return_type.clone());
            // Pop before propagating: an error inside the body must not
            // leave a stale frame on `return_types`, or a later entry on
            // this same checker would accept a top-level `return`.
            // A lambda parameter is always immutable in practice (the
            // grammar has no `mut` marker for one), so a plain
            // `with_context` — which always binds immutably — matches
            // `bindings`' own `mutable` field here regardless.
            let body = self.with_context(
                bindings.iter().map(|(n, t, _)| (n.clone(), t.clone())),
                |t| t.lower_expected(*func.body, &return_type),
            );
            self.return_types.pop();
            let body = body?;
            return Ok(Spanned::from(
                TypedExpr { id: 0,
                    ty: expected.clone(),
                    kind: TypedExprKind::Function { params: bindings, return_type, body: Box::new(body) },
                },
                span,
            ));
        }

        match (expr.item, &expected) {
            // A zero-`Self` member — `zero()`, or `Zero.zero()` — names no
            // value to dispatch on, so the expected type is what picks the
            // impl (`TRAITS.md` Part 1, "The prefix form"). This is the whole
            // reason members are plain functions rather than methods with a
            // receiver: a receiver model cannot express `zero(): Self` at
            // all.
            (Expression::Call(c), _) if self.zero_self_target(&c).is_some() => {
                let (want_trait, member) = self.zero_self_target(&c).expect("just checked");
                let lowered = self.lower_zero_self_call(c, &want_trait, &member, &expected, span)?;
                self.lower_widen(lowered, &expected)
            },
            // `read(s)` — `zero_self_target`'s sibling: the expected type
            // is what says what to read, not a value at the call site.
            (Expression::Call(c), _) if Self::is_read_call(&c) => self.lower_read(c, &expected, span),
            // `json.parse(s)` — the same story one tier up (`plans/DATA.md`
            // stage 8): the annotation says what shape to fill in, so it
            // has to be intercepted here rather than after the expected
            // type is gone.
            (Expression::Call(c), _) if self.is_json_parse_call(&c) => self.lower_json_parse(c, &expected, span),
            (Expression::Tuple(elems), _) if expected.as_list_elem().is_some() => {
                let elem_ty = expected.as_list_elem().expect("checked above").clone();
                let mut items = Vec::with_capacity(elems.len());
                for e in elems {
                    items.push(self.lower_expected(e, &elem_ty)?);
                }
                Ok(Spanned::from(
                    TypedExpr { id: 0, ty: Type::list(elem_ty), kind: TypedExprKind::List(items) },
                    span,
                ))
            },
            (other, _) => {
                // A variant constructor call (`ParseError.UnexpectedEof(pos=3)`)
                // is retyped down to that exact variant's own qualified
                // struct type before the subtype check below, rather than
                // left at `lower_call`'s `VariantInit` default (the whole
                // *union*'s type — so an unannotated `let` still gets the
                // familiar `Shape.Circle | Shape.Rectangle`, not a silently
                // narrower type dependent on which arm happened to run).
                // Left at that default, the check below would be backwards
                // whenever `expected` names the one variant just
                // constructed, or an anonymous union containing it: the
                // union is not a subtype of one of its own members, even
                // though the value being checked plainly *is* exactly that
                // member already — this is `T ≤ (T|U)`'s narrowing
                // counterpart (README, "Union types"), applied at the one
                // site the value's exact shape is knowable before it's even
                // lowered. Ordinary subtyping/widening below then handles
                // every shape `expected` can take from here: the bare
                // qualified type itself, an anonymous union member, or
                // (through `is_subtype`) the variant's own declaring union.
                let exact_variant_ty = if let Expression::Call(c) = &other {
                    self.resolve_variant_callee(&c.callable.item).ok().flatten()
                        .map(|(enum_name, variant)| Type::strukt(format!("{}.{}", enum_name, variant)))
                } else {
                    None
                };
                let lowered = self.check_and_lower(Spanned::from(other, span))?;
                let lowered = match exact_variant_ty {
                    Some(ty) => Spanned::from(TypedExpr { id: lowered.item.id, ty, kind: lowered.item.kind }, lowered.span),
                    None => lowered,
                };
                let resolved = self.lookup(&lowered.item.ty);
                let accepted = self.is_subtype(&resolved, &expected)
                    || widens_to(&resolved, &expected)
                    || (matches!(resolved, Type::TypeVar { .. }) && self.unify(&resolved, &expected))
                    || self.unify_with_one_union_member(&resolved, &expected);
                if !accepted {
                    return Err(Spanned::from(TypeError {
                        msg: format!("Expected {} got {}", expected, resolved)
                    }, span));
                }
                self.lower_widen(lowered, &expected)
            },
        }
    }

    /// If this call names a trait member with nothing to dispatch on, say
    /// which trait (if the prefix form named one) and which member.
    ///
    /// Two spellings, both requiring that the name is *not* an ordinary
    /// binding: a bare `zero()` where `zero` is some trait's member and no
    /// function of that name is in scope, and the trait-qualified
    /// `Zero.zero()`. Anything else is `None` and lowers normally.
    fn zero_self_target(&self, c: &CallExpr) -> Option<(Option<String>, String)> {
        match &c.callable.item {
            Expression::Literal(_) => {
                let name = c.callable.item.get_identifier()?;
                if self.ctx.contains_key(name) { return None; }
                // Only a member with nothing to dispatch on may claim the bare
                // name; a member with a `Self` parameter dispatches on that
                // argument, and stealing the name here would shadow builtins
                // and struct construction that happen to share it.
                let owners = self.member_traits.get(name)?;
                if !owners.iter().any(|t| self.is_zero_self_member(t, name)) { return None; }
                Some((None, name.to_string()))
            }
            Expression::FieldAccess(fa) => {
                let trait_name = fa.target.item.get_identifier()?;
                // Only the no-Self-argument case comes here; the ordinary
                // prefix form dispatches on its first argument and is handled
                // by `lower_trait_prefix_call`.
                if !self.is_zero_self_member(trait_name, &fa.field) { return None; }
                Some((Some(trait_name.to_string()), fa.field.clone()))
            }
            _ => None,
        }
    }

    /// `json.<member>(...)` — the builtin `json` namespace (`plans/DATA.md`
    /// stage 8), returning the member name.
    ///
    /// `json` is a *namespace*, not a value, so this has to match on the
    /// callee's syntax before anything tries to lower `json` itself as an
    /// expression — exactly the reason `Ord.compare(a, b)`'s trait-prefix
    /// check sits where it does in `lower_call`, ahead of `lower_ufcs_call`.
    /// And exactly like that check, it is *gated*: an actual binding named
    /// `json` in scope wins, so `let json = ...` shadows the namespace
    /// rather than being shadowed by it. Stage 8 chose a builtin namespace
    /// over an `import "std/json"` module precisely so that `json.parse(s):
    /// Person | JsonError` costs no ceremony; this gate is what keeps that
    /// from also costing the name.
    fn json_builtin_target(&self, c: &CallExpr) -> Option<String> {
        let Expression::FieldAccess(fa) = &c.callable.item else { return None };
        if fa.target.item.get_identifier() != Some("json") { return None }
        if self.ctx.get("json").is_some() { return None }
        Some(fa.field.clone())
    }

    /// `json.parse(s)` specifically — `is_read_call`'s twin, and for the
    /// same reason: `lower_expected` needs to intercept it before the
    /// expected type is lost.
    fn is_json_parse_call(&self, c: &CallExpr) -> bool {
        self.json_builtin_target(c).as_deref() == Some("parse")
    }

    /// The `json` namespace's members, once `lower_call` has established
    /// that this really is one. `json.parse` reaching here at all means
    /// there was no expected type to parse *into* — the same "return-type
    /// directed, so say what's missing" shape `read` has just above.
    fn lower_json_call(&mut self, member: &str, c: CallExpr, callee_span: Span, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        match member {
            "to_str" => {
                if c.args.len() != 1 {
                    return Err(Spanned::from(TypeError {
                        msg: format!("Wrong number of arguments, expected 1, got {}", c.args.len())
                    }, callee_span));
                }
                let arg = self.check_and_lower(c.args.into_iter().next().expect("arity checked just above"))?;
                let arg_ty = self.lookup(&arg.item.ty);
                // Deferred past a bare generic `TypeVar` for exactly the
                // reason `is_repr`'s own checks are — see its comment; the
                // re-check happens in `desugar_notation` once each
                // monomorphized instantiation has a concrete type.
                if !matches!(arg_ty, Type::TypeVar { .. }) {
                    self.check_json_serializable(&arg_ty, arg.span)?;
                }
                // A placeholder, expanded by `desugar_notation` into
                // `build_json`'s per-type fragments — `repr`'s exact
                // arrangement, and for the same post-monomorphization
                // reasons (see `desugar_notation`'s own doc comment).
                let callable = Spanned::from(TypedExpr {
                    id: 0,
                    ty: Type::Function { params: vec![arg_ty], result: Box::new(Type::Str) },
                    kind: TypedExprKind::Var("__json_to_str".to_string()),
                }, callee_span);
                Ok(Spanned::from(TypedExpr {
                    id: 0, ty: Type::Str,
                    kind: TypedExprKind::Call { callable: Box::new(callable), args: vec![Arg::Value(arg)] },
                }, span))
            }
            "parse" => Err(Spanned::from(TypeError {
                msg: "cannot infer what to parse; annotate the expected type, e.g. 'let x: T | JsonError = json.parse(s)'".to_string()
            }, span)),
            other => Err(Spanned::from(TypeError {
                msg: format!("unknown json builtin 'json.{}' — the json namespace has 'to_str' and 'parse'", other)
            }, callee_span)),
        }
    }

    /// `json.to_str`'s gate. No new `Trait` variant: `Show` is already
    /// structural over every `data` and already excludes `Type::Function`,
    /// which is exactly the line JSON needs to draw too, and `DATA.md`
    /// stage 8's own reasoning against a `Json`/`Serialize` trait is that a
    /// granted trait nobody can opt out of (there is no `without` yet) is
    /// worse than no trait at all.
    fn check_json_serializable(&mut self, ty: &Type, span: Span) -> Result<(), Spanned<TypeError>> {
        if !self.type_implements(ty, &Trait::Show) {
            return Err(Spanned::from(TypeError {
                msg: format!("{} has no JSON form — json.to_str needs Show", ty)
            }, span));
        }
        self.check_no_recursive_union(ty, span, "json.to_str", "formats it field-by-field")
    }

    /// `read(s)` — a bare call to the reserved name, exactly the shape
    /// `is_repr`'s own `matches!` checks (`lower_call`), pulled out to a
    /// named predicate since `lower_expected`'s dispatch arm needs it too.
    fn is_read_call(c: &CallExpr) -> bool {
        matches!(
            &c.callable.item,
            Expression::Literal(LiteralExpr { token: Token::Identifier(name) }) if name == "read"
        )
    }

    /// `read(s): T | ReadError` — return-type directed, `zero_self_target`'s
    /// sibling arm in `lower_expected`. `expected` must already be
    /// `self.lookup`-resolved (`lower_expected`'s own contract).
    fn lower_read(&mut self, c: CallExpr, expected: &Type, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        if c.args.len() != 1 {
            return Err(Spanned::from(TypeError {
                msg: format!("Wrong number of arguments, expected 1, got {}", c.args.len())
            }, span));
        }
        let arg = self.check_and_lower(c.args.into_iter().next().expect("arity checked just above"))?;
        let arg_span = arg.span;
        let arg_ty = self.lookup(&arg.item.ty);
        if arg_ty != Type::Str {
            return Err(Spanned::from(TypeError {
                msg: format!("read's argument must be Str, got {}", arg_ty)
            }, arg_span));
        }

        let read_error_ty = Type::strukt("ReadError");
        let members = match expected {
            Type::Union(ms) if ms.contains(&read_error_ty) => ms.clone(),
            _ => return Err(Spanned::from(TypeError {
                msg: "read returns 'T | ReadError'; annotate e.g. 'let x: Person | ReadError = read(s)'".to_string()
            }, span)),
        };
        let t_members: Vec<Type> = members.into_iter().filter(|m| *m != read_error_ty).collect();
        if t_members.is_empty() {
            return Err(Spanned::from(TypeError {
                msg: "read needs a type to read besides ReadError itself".to_string()
            }, span));
        }

        let node_name = format!("__read_root{}", self.next_id); self.next_id += 1;
        let result_name = format!("__read_result{}", self.next_id); self.next_id += 1;

        let open_call = Self::read_leaf_call("frog_read_open", Type::Str, Type::Int, arg, span);
        let open_assign = Spanned::from(TypedExpr { id: 0, ty: Type::Int, kind: TypedExprKind::Assign { name: node_name.clone(), value: Box::new(open_call) } }, span);

        // `t_members.len() == 1` — an ordinary single-type `read`
        // (`Int | ReadError`, `Person | ReadError`, ...): build against
        // that one type and widen the single member into `expected`,
        // which `lower_widen` supports directly.
        //
        // `t_members.len() > 1` — `T` is *itself* a union (`Shape |
        // ReadError`, where `Shape` is `Circle | Rect`): build directly
        // against `expected` instead, via `build_read_union_as` — see its
        // own doc comment for why `lower_widen` can't take the widening
        // step from here (union-into-union widening isn't supported, and
        // building against the wider type from the start sidesteps rather
        // than needs that support).
        let happy_widened = if t_members.len() == 1 {
            let t = t_members.into_iter().next().expect("len checked above");
            self.check_readable(&t, span)?;
            let happy = self.build_read(&t, Self::node_var(&node_name, span), span)?;
            self.lower_widen(happy, expected)?
        } else {
            let t = Type::Union(t_members.clone()).normalize();
            self.check_readable(&t, span)?;
            let Type::Union(normalized_members) = t else { unreachable!("normalize of >1 members is always a Union") };
            self.build_read_union_as(&normalized_members, Self::node_var(&node_name, span), expected, span)?
        };

        let msg_call = Self::read_call0("frog_read_msg", Type::Str, span);
        let offset_call = Self::read_call0("frog_read_offset", Type::Int, span);
        let error_value = Spanned::from(TypedExpr {
            id: 0, ty: read_error_ty.clone(),
            kind: TypedExprKind::StructInit { name: "ReadError".to_string(), fields: vec![
                ("msg".to_string(), Box::new(msg_call)), ("offset".to_string(), Box::new(offset_call)),
            ] },
        }, span);
        let error_widened = self.lower_widen(error_value, expected)?;

        // `happy_widened` must run — and so discover any sticky failure —
        // *before* `frog_read_failed()` is checked, or the check always
        // sees "not failed yet" (nothing has read anything). Binding it to
        // a temporary first, unconditionally, is what makes the check
        // downstream of the read it's supposed to be checking, rather
        // than racing ahead of it.
        let happy_name = format!("__read_happy{}", self.next_id); self.next_id += 1;
        let happy_assign = Spanned::from(TypedExpr { id: 0, ty: expected.clone(), kind: TypedExprKind::Assign { name: happy_name.clone(), value: Box::new(happy_widened) } }, span);
        let happy_var = Spanned::from(TypedExpr { id: 0, ty: expected.clone(), kind: TypedExprKind::Var(happy_name) }, span);

        let failed_test = Self::read_call0("frog_read_failed", Type::Bool, span);
        let dispatch = Spanned::from(TypedExpr {
            id: 0, ty: expected.clone(),
            kind: TypedExprKind::Conditional { cond: Box::new(failed_test), true_branch: Box::new(error_widened), false_branch: Some(Box::new(happy_var)) },
        }, span);
        let result_assign = Spanned::from(TypedExpr { id: 0, ty: expected.clone(), kind: TypedExprKind::Assign { name: result_name.clone(), value: Box::new(dispatch) } }, span);
        let close_call = Self::read_call0("frog_read_close", Type::None, span);
        let result_var = Spanned::from(TypedExpr { id: 0, ty: expected.clone(), kind: TypedExprKind::Var(result_name) }, span);

        Ok(Spanned::from(TypedExpr {
            id: 0, ty: expected.clone(),
            kind: TypedExprKind::Block(vec![open_assign, happy_assign, result_assign, close_call, result_var]),
        }, span))
    }

    fn read_call0(rt_name: &str, ret: Type, span: Span) -> Spanned<TypedExpr> {
        let callable = Spanned::from(TypedExpr {
            id: 0,
            ty: Type::Function { params: vec![], result: Box::new(ret.clone()) },
            kind: TypedExprKind::Var(rt_name.to_string()),
        }, span);
        Spanned::from(TypedExpr {
            id: 0, ty: ret,
            kind: TypedExprKind::Call { callable: Box::new(callable), args: vec![] },
        }, span)
    }

    /// Does `trait_name` declare `member` with no `Self` in its first
    /// parameter — the shape that has nothing to dispatch on?
    fn is_zero_self_member(&self, trait_name: &str, member: &str) -> bool {
        let Some(def) = self.traits.get(trait_name) else { return false };
        let Some(sig) = def.members.iter().find(|m| m.name == member) else { return false };
        !sig.params.first().map(|(_, ty, _)| Self::mentions_self(ty)).unwrap_or(false)
    }

    /// Resolve a zero-`Self` member call against the expected type and lower
    /// it as an ordinary call to that impl's symbol.
    fn lower_zero_self_call(
        &mut self,
        c: CallExpr,
        want_trait: &Option<String>,
        member: &str,
        expected: &Type,
        span: Span,
    ) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let named = match want_trait {
            Some(t) => format!("'{}.{}'", t, member),
            None => format!("'{}'", member),
        };
        // No usable expectation — an unresolved variable, a function type, a
        // union. TRAITS.md commits to exactly this error rather than to
        // inference heroics: a good message beats an ambiguity report listing
        // every candidate.
        let Some(key) = Self::grant_key(expected) else {
            return Err(Spanned::from(TypeError {
                msg: format!("cannot infer which {} is meant; annotate the expected type", named)
            }, span));
        };
        let Some((owner, symbol)) = self.member_index.get(&(key, member.to_string())).cloned() else {
            return Err(Spanned::from(TypeError {
                msg: format!("{} doesn't implement {}", expected, named)
            }, span));
        };
        if let Some(want) = want_trait {
            if &owner != want {
                return Err(Spanned::from(TypeError {
                    msg: format!("{} implements '{}' from trait '{}', not '{}'", expected, member, owner, want)
                }, span));
            }
        }
        let callee = Spanned::from(
            Expression::literal(Token::Identifier(symbol)),
            c.callable.span,
        );
        self.check_and_lower(Spanned::from(Expression::call(callee, c.args), span))
    }

    /// Partitions an already-known union subject type into one
    /// `ErrorArmEntry` per member (nominal or anonymous — see
    /// `resolve_union`). Errors if the subject isn't a union at all.
    ///
    /// `subject_ty` is passed in rather than inferred here: `?`/`!`/`catch`
    /// all lower their subject exactly once and hand its type over, so the
    /// subject subtree is never walked twice.
    fn union_entries(&self, subject_ty: &Type, subject_span: Span, span: Span) -> Result<Vec<ErrorArmEntry>, Spanned<TypeError>> {
        let resolved = self.lookup(subject_ty);
        let mut out = Vec::new();
        if let Some((enum_name, def)) = self.resolve_union(&resolved) {
            let enum_name = enum_name.to_string();
            for (vn, fields) in &def.variants {
                let ty = Type::strukt(format!("{}.{}", enum_name, vn));
                let is_error = self.type_implements(&ty, &Trait::Error);
                let callee = Spanned::from(
                    Expression::field_access(
                        Spanned::from(Expression::literal(Token::Identifier(enum_name.clone())), span),
                        vn.clone(),
                    ),
                    span,
                );
                let mut bind_names = Vec::with_capacity(fields.len());
                let mut args = Vec::with_capacity(fields.len());
                for (i, (fname, _)) in fields.iter().enumerate() {
                    let bind = format!("__f{}", i);
                    args.push(Spanned::from(
                        Expression::assign(
                            Spanned::from(Expression::literal(Token::Identifier(fname.clone())), span),
                            None,
                            Spanned::from(Expression::literal(Token::Identifier(bind.clone())), span),
                            None,
                        ),
                        span,
                    ));
                    bind_names.push(bind);
                }
                let whole_value = Expression::call(callee, args);
                out.push(ErrorArmEntry { pattern_variant: vn.clone(), member_idx: None, ty, is_error, bind_names, whole_value });
            }
        } else if let Type::Union(members) = &resolved {
            for (i, m) in members.iter().enumerate() {
                let is_error = self.type_implements(m, &Trait::Error);
                let bind = "__whole".to_string();
                let whole_value = Expression::literal(Token::Identifier(bind.clone()));
                out.push(ErrorArmEntry { pattern_variant: m.to_string(), member_idx: Some(i), ty: m.clone(), is_error, bind_names: vec![bind], whole_value });
            }
        } else {
            return Err(Spanned::from(TypeError {
                msg: format!("expected a union value here, got {}", resolved)
            }, subject_span));
        }
        Ok(out)
    }

    /// `union_entries`, plus the precondition `?`, `!` and `catch` share:
    /// the subject union must have at least one `Error`-providing member,
    /// or the operator has nothing to do.
    fn error_arm_entries(&self, op: &str, subject_ty: &Type, subject_span: Span, span: Span) -> Result<Vec<ErrorArmEntry>, Spanned<TypeError>> {
        let entries = self.union_entries(subject_ty, subject_span, span)?;
        if !entries.iter().any(|e| e.is_error) {
            return Err(Spanned::from(TypeError {
                msg: format!("'{}' requires a union with at least one Error-providing member", op)
            }, subject_span));
        }
        Ok(entries)
    }

    /// One arm per union member: an `Error`-providing member's body is
    /// whatever `error_body` makes of its whole (reconstructed) value —
    /// return it, panic on it, hand it to a handler — and every other
    /// member passes that value through unchanged.
    fn error_arms(
        entries: Vec<ErrorArmEntry>,
        span: Span,
        mut error_body: impl FnMut(Expression) -> Spanned<Expression>,
    ) -> Vec<MatchArm> {
        entries.into_iter().map(|e| {
            let body = if e.is_error { error_body(e.whole_value) } else { Spanned::from(e.whole_value, span) };
            MatchArm {
                pattern: Pattern { path: None, variant: e.pattern_variant, binds: e.bind_names, resolved_member: e.member_idx },
                guard: None,
                body: Box::new(body),
            }
        }).collect()
    }

    /// `e?`'s match arms: an `Error`-providing member returns its (whole,
    /// reconstructed) value early; every other member passes its value
    /// through unchanged. The join across arms (`lower_match`'s own
    /// machinery) is exactly the doc's "set subtraction" type rule for
    /// free — a `return` arm is `Never`-typed and vanishes from the join,
    /// leaving only the non-`Error` members' union (or a single type, or
    /// `Never` if every member is an `Error`).
    fn build_try_arms(&mut self, subject_ty: &Type, subject_span: Span, span: Span) -> Result<Vec<MatchArm>, Spanned<TypeError>> {
        let entries = self.error_arm_entries("?", subject_ty, subject_span, span)?;
        let return_ty = self.return_types.last().cloned().ok_or_else(|| Spanned::from(
            TypeError { msg: "'?' used outside of a function".to_string() }, span
        ))?;
        let resolved_return = self.lookup(&return_ty);
        if matches!(resolved_return, Type::TypeVar { .. }) {
            return Err(Spanned::from(TypeError {
                msg: "'?' requires the enclosing function to have an explicit return type that includes the propagated error type(s) — inferred error sets are not supported".to_string()
            }, span));
        }
        for e in &entries {
            if e.is_error && !self.is_subtype(&e.ty, &resolved_return) {
                return Err(Spanned::from(TypeError {
                    msg: format!("'?' may propagate {}, but the enclosing function's return type ({}) does not include it", e.ty, resolved_return)
                }, span));
            }
        }
        Ok(Self::error_arms(entries, span, |whole| Spanned::from(
            Expression::return_value(Some(Spanned::from(whole, span))), span,
        )))
    }

    fn lower_try(&mut self, inner: Spanned<Expression>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let (subject, narrow_target, subject_span) = self.lower_match_subject(inner)?;
        let arms = self.build_try_arms(&subject.item.ty, subject_span, span)?;
        self.lower_match_lowered(subject, narrow_target, subject_span, arms, None, span)
    }

    /// `e!`'s match arms: an `Error`-providing member calls the builtin
    /// `panic(msg: Str): Never` (see `default_context`), ignoring the
    /// reconstructed value; every other member passes its value through
    /// unchanged — same join as `?`, except the panicking arm is also
    /// `Never`-typed (an ordinary call whose callee's declared result is
    /// `Never`, resolved the same way any other call's result type is), so
    /// it vanishes from the join exactly like a `return` arm does. `panic`
    /// being an ordinary function (not a dedicated node) means user code
    /// can call it directly too, not just reach it through `!`.
    fn build_unwrap_arms(&mut self, subject_ty: &Type, subject_span: Span, span: Span) -> Result<Vec<MatchArm>, Spanned<TypeError>> {
        let entries = self.error_arm_entries("!", subject_ty, subject_span, span)?;
        Ok(Self::error_arms(entries, span, |_whole| {
            let callee = Spanned::from(Expression::literal(Token::Identifier(UNWRAP_PANIC_NAME.to_string())), span);
            let msg = Spanned::from(Expression::literal(Token::String("unwrapped an error value with '!'".to_string())), span);
            Spanned::from(Expression::call(callee, vec![msg]), span)
        }))
    }

    fn lower_unwrap(&mut self, inner: Spanned<Expression>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let (subject, narrow_target, subject_span) = self.lower_match_subject(inner)?;
        let arms = self.build_unwrap_arms(&subject.item.ty, subject_span, span)?;
        self.lower_match_lowered(subject, narrow_target, subject_span, arms, None, span)
    }

    /// `value catch handler`'s match arms. `handler` is inlined per
    /// `Error`-providing member rather than called: a single-parameter
    /// lambda (`[e] -> body`, i.e. `Expression::Function` with exactly one
    /// param) contributes its own param name, assigned the member's whole
    /// (reconstructed) value in a one-statement prelude, then its body —
    /// no actual closure/call codegen is needed. Any other handler
    /// expression is used as a plain fallback value, cloned into every
    /// `Error` arm, ignoring the matched value entirely.
    fn build_catch_arms(&mut self, subject_ty: &Type, subject_span: Span, handler: &Spanned<Expression>, span: Span) -> Result<Vec<MatchArm>, Spanned<TypeError>> {
        let entries = self.error_arm_entries("catch", subject_ty, subject_span, span)?;
        let (handler_bind, handler_body): (Option<String>, Spanned<Expression>) = match &handler.item {
            Expression::Function(f) if f.params.len() == 1 => (Some(f.params[0].name.clone()), (*f.body).clone()),
            _ => (None, handler.clone()),
        };
        Ok(Self::error_arms(entries, span, |whole| match &handler_bind {
            None => handler_body.clone(),
            Some(bind) => {
                let assign = Spanned::from(
                    Expression::assign(
                        Spanned::from(Expression::literal(Token::Identifier(bind.clone())), span),
                        None,
                        Spanned::from(whole, span),
                        Some(Mutability::Immutable),
                    ),
                    span,
                );
                Spanned::from(Expression::Block(vec![assign, handler_body.clone()]), span)
            }
        }))
    }

    fn lower_catch(&mut self, value: Spanned<Expression>, handler: Spanned<Expression>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let (subject, narrow_target, subject_span) = self.lower_match_subject(value)?;
        let arms = self.build_catch_arms(&subject.item.ty, subject_span, &handler, span)?;
        self.lower_match_lowered(subject, narrow_target, subject_span, arms, None, span)
    }

    /// The `Self` type for a trait's member signatures: one bounded type
    /// variable, bound by the trait itself. Bounding it is not decoration —
    /// it is what makes a default body checkable, since the body may call
    /// the trait's *other* members on `Self` and the bound is what says it
    /// may.
    fn self_type(trait_name: &str) -> Type {
        Type::TypeVar { name: SELF_BINDER.to_string(), bounds: vec![Trait::User(trait_name.to_string())] }
    }

    /// The stable placeholder name a type-parameterized trait's own binder
    /// (e.g. `Iterable`'s `Item`) is stored under in every member signature
    /// — scoped by trait name so two traits' same-named binder (`Item`)
    /// never collide in `Type::substitute`'s flat `HashMap<String, Type>`.
    /// Parallel to `SELF_BINDER`, except per-trait rather than global,
    /// since (unlike `Self`) more than one such name can exist at once.
    fn trait_type_param_binder(trait_name: &str, param_name: &str) -> String {
        format!("{}::{}", trait_name, param_name)
    }

    /// First of the two trait-hoisting passes: register every `trait Name`
    /// in this statement list under its name, with no members yet.
    ///
    /// Split from `hoist_trait_members` because the three declaration forms
    /// are mutually referential and only this order terminates:
    /// a `data X provides Shape` (resolved by `hoist_data_decls`) needs
    /// `Shape` to *exist*, while `Shape`'s own member signatures may mention
    /// `X`. Registering names first, then data, then signatures, breaks the
    /// cycle without a fixed point — the same two-pass shape
    /// `hoist_data_decls` already uses internally for mutually recursive
    /// `data` declarations.
    fn hoist_trait_names(&mut self, stmts: &[Spanned<Expression>]) -> Result<(), Spanned<TypeError>> {
        for s in stmts {
            if let Expression::TraitDecl(t) = &s.item {
                if self.traits.contains_key(&t.name) {
                    // Names the built-in case explicitly: redeclaring `Eq`
                    // is a different mistake from redeclaring your own
                    // trait, and the generic message would send the reader
                    // looking for a declaration that isn't in their source.
                    let msg = if self.traits[&t.name].builtin.is_some() {
                        format!("'{}' is a built-in trait and can't be redeclared", t.name)
                    } else {
                        format!("trait '{}' is already declared", t.name)
                    };
                    return Err(Spanned::from(TypeError { msg }, s.span));
                }
                if self.struct_templates.contains_key(&t.name) || self.union_defs.contains_key(&t.name) {
                    return Err(Spanned::from(TypeError {
                        msg: format!("'{}' is already declared as a type", t.name)
                    }, s.span));
                }
                self.traits.insert(t.name.clone(), TraitDef {
                    name: t.name.clone(), builtin: None, members: Vec::new(),
                    type_params: t.type_params.clone(),
                });
            }
        }
        Ok(())
    }

    /// Second trait-hoisting pass: resolve each member's signature, now that
    /// every trait name *and* every `data` name in this list is registered.
    ///
    /// Signatures are stored as templates over `Self` (see `TraitMemberSig`);
    /// `type_param_scope` is what makes the bare name `Self` resolve, and it
    /// is restored immediately afterward so it can't leak into an ordinary
    /// annotation — same discipline as a generic `data` declaration's
    /// binders in `hoist_data_decls`.
    ///
    /// *Restored*, not cleared: a declaration nested in a generic function's
    /// body is hoisted with that function's `<T>` binders already installed
    /// (`lower_assign`), and dropping them would make `T` unknown for the
    /// rest of the body.
    fn hoist_trait_members(&mut self, stmts: &[Spanned<Expression>]) -> Result<(), Spanned<TypeError>> {
        let outer = self.type_param_scope.clone();
        for s in stmts {
            let Expression::TraitDecl(t) = &s.item else { continue };
            let self_ty = Self::self_type(&t.name);
            let mut members: Vec<TraitMemberSig> = Vec::with_capacity(t.members.len());
            for m in &t.members {
                if members.iter().any(|prev| prev.name == m.name) {
                    return Err(Spanned::from(TypeError {
                        msg: format!("Member '{}' is declared twice in trait {}", m.name, t.name)
                    }, s.span));
                }
                // A type-parameterized trait's default body would need
                // `register_impl` to fill in `Item` (etc.) via
                // `substitute_self_expr`-style *source-text* substitution,
                // which only handles `Self` (a single spellable name) — a
                // fresh unification variable, which is how `Item` is
                // resolved (see `TraitDef::type_params`'s doc comment), has
                // no source spelling to substitute in. Not attempted; every
                // member of a type-parameterized trait must be supplied.
                if !t.type_params.is_empty() && m.default.is_some() {
                    return Err(Spanned::from(TypeError {
                        msg: format!(
                            "trait member '{}.{}' can't have a default body — '{}' is type-parameterized, and a default's type-parameter-mentioning positions can't be filled in automatically",
                            t.name, m.name, t.name
                        )
                    }, s.span));
                }
                let mut scope: HashMap<String, Type> = HashMap::with_capacity(t.type_params.len());
                for tp in &t.type_params {
                    // Same discipline as a generic `data` decl's binders
                    // (`hoist_data_decls`): the written bounds ride on the
                    // placeholder `TypeVar` itself, since `unify` and
                    // `type_implements` only ever consult a `TypeVar`'s own
                    // `bounds` field.
                    let bounds = match self.resolve_bounds(&tp.bounds, s.span) {
                        Ok(b) => b,
                        Err(e) => { self.type_param_scope = outer.clone(); return Err(e); }
                    };
                    scope.insert(tp.name.clone(), Type::TypeVar { name: Self::trait_type_param_binder(&t.name, &tp.name), bounds });
                }
                scope.insert(SELF_BINDER.to_string(), self_ty.clone());
                self.type_param_scope = scope;
                let mut params = Vec::with_capacity(m.params.len());
                let mut resolve_err = None;
                for p in &m.params {
                    let Some(ann) = &p.ty else {
                        resolve_err = Some(TypeError {
                            msg: format!(
                                "Parameter '{}' of trait member '{}.{}' needs a type annotation — a trait member is a signature, so nothing can infer it",
                                p.name, t.name, m.name
                            )
                        });
                        break;
                    };
                    match self.resolve_type_expr(ann) {
                        Ok(ty) => params.push((p.name.clone(), ty, p.mutable)),
                        Err(e) => { self.type_param_scope = outer.clone(); return Err(e); }
                    }
                }
                if let Some(msg) = resolve_err {
                    self.type_param_scope = outer.clone();
                    return Err(Spanned::from(msg, s.span));
                }
                let return_type = match &m.return_type {
                    Some(rt) => match self.resolve_type_expr(rt) {
                        Ok(ty) => ty,
                        Err(e) => { self.type_param_scope = outer.clone(); return Err(e); }
                    },
                    None => Type::None,
                };
                self.type_param_scope = outer.clone();

                // A member that mentions `Self` nowhere is a free function
                // that happens to be written inside a trait: nothing can
                // dispatch it, and no impl could vary it. Rejecting it here
                // is the difference between a confusing "can't infer which
                // impl" at some later call site and an error at the
                // declaration that caused it.
                let mentions_self = params.iter().any(|(_, ty, _)| Self::mentions_self(ty))
                    || Self::mentions_self(&return_type);
                if !mentions_self {
                    return Err(Spanned::from(TypeError {
                        msg: format!(
                            "Trait member '{}.{}' mentions Self nowhere in its signature — nothing could dispatch it; declare it as an ordinary func instead",
                            t.name, m.name
                        )
                    }, s.span));
                }

                members.push(TraitMemberSig {
                    name: m.name.clone(), params, return_type, decl: m.clone(),
                });
            }
            self.traits.get_mut(&t.name)
                .expect("registered by hoist_trait_names")
                .members = members;
        }
        Ok(())
    }

    /// The compiled symbol an impl member is declared under: `Circle$area`.
    ///
    /// `$` is the same separator `mangle_type` uses for generic
    /// instantiations and, like it, is unproducible by the lexer — so a
    /// member symbol can never collide with a user-written name. This is
    /// TRAITS.md's "Where the seam is" in one line: member resolution ends
    /// with the type checker writing this string into the typed AST, and
    /// codegen keeps doing the string lookup it already did.
    fn member_symbol(type_key: &str, member: &str) -> String {
        format!("{}${}", type_key, member)
    }

    /// The placeholder callee a member call through a type-parameter bound
    /// carries until monomorphization can name a real impl —
    /// `#member$Shape$area`. Built from the same unproducible `$` separator
    /// `member_symbol` uses, behind a leading `#` that no source name and no
    /// member symbol can start with, so a leftover is recognizable rather
    /// than mistakable for a function someone declared.
    fn pending_member_symbol(trait_name: &str, member: &str) -> String {
        format!("{}{}${}", PENDING_MEMBER_PREFIX, trait_name, member)
    }

    /// `pending_member_symbol`'s inverse: `(trait, member)`, or `None` if
    /// this is an ordinary name.
    fn parse_pending_member(name: &str) -> Option<(&str, &str)> {
        name.strip_prefix(PENDING_MEMBER_PREFIX)?.split_once('$')
    }

    /// Rewrite every occurrence of the bare name `Self` in a type annotation
    /// to `type_name`.
    ///
    /// Used to specialize a trait member's declared `TypeExpr` for one impl
    /// — both when filling in an annotation the impl omitted and when
    /// letting an impl member write `Self` itself. Legal for every type an
    /// impl can target, since `grant_key`'s answer is always a spellable
    /// type name (a declared `data` name, or a primitive's).
    fn substitute_self_expr(ty: &mut Spanned<TypeExpr>, type_name: &str) {
        for name in ty.item.names_mut() {
            if name == SELF_BINDER { *name = type_name.to_string(); }
        }
    }

    /// Expand and register every impl in this statement list — `TRAITS.md`
    /// Stage 5's registration pass, run after both hoists so it can see
    /// every trait *and* every type this entry declares.
    ///
    /// Each impl is replaced, in place, by its member functions renamed to
    /// their mangled symbols. In place matters: froglang requires
    /// declaration before use everywhere else (`func a() = b()` with `b`
    /// declared below is an error today), and an impl behaving differently
    /// would be a special case with nothing to recommend it. So an impl's
    /// members become callable exactly where the impl block is written.
    ///
    /// The members themselves need no new compilation path — they are
    /// ordinary `Assign(name, Function)` nodes under a different name, and
    /// the existing `lower_assign` handles them, including generalization.
    fn expand_impls(&mut self, stmts: &mut Vec<Spanned<Expression>>) -> Result<(), Spanned<TypeError>> {
        if !stmts.iter().any(|s| matches!(&s.item,
            Expression::ImplDecl(_) | Expression::DataDecl(_))) {
            return Ok(());
        }
        let mut out: Vec<Spanned<Expression>> = Vec::with_capacity(stmts.len());
        for s in std::mem::take(stmts) {
            let span = s.span;
            match s.item {
                Expression::ImplDecl(i) => {
                    let self_ty = self.resolve_type_expr(&i.self_ty)?;
                    let members = self.register_impl(&i.traits, &self_ty, i.members, span)?;
                    out.extend(members);
                }
                Expression::DataDecl(mut d) => {
                    let members = std::mem::take(&mut d.members);
                    // The *resolved* type, not `Type::strukt(&d.name)`: for a
                    // nominal union those differ, and a member declared
                    // `(s: Sh)` resolves to the union, so checking it against
                    // a struct type by that name could never succeed.
                    let self_ty = if d.variants.is_empty() {
                        Type::strukt(&d.name)
                    } else {
                        self.union_defs.get(&d.name).map(|u| u.ty.clone())
                            .unwrap_or_else(|| Type::strukt(&d.name))
                    };
                    let provides = d.provides.clone();
                    out.push(Spanned::from(Expression::DataDecl(d), span));
                    if !provides.is_empty() {
                        let lowered = self.register_impl(&provides, &self_ty, members, span)?;
                        out.extend(lowered);
                    }
                }
                other => out.push(Spanned::from(other, span)),
            }
        }
        *stmts = out;
        Ok(())
    }

    /// The `member_index`/`impls`/`provides` keys an impl for `self_ty` must
    /// be registered under: a nominal union's variants, or the type's own key
    /// for everything else.
    fn impl_index_keys(&self, self_ty: &Type, type_key: &str) -> Vec<String> {
        match self_ty {
            Type::Union(members) => {
                let keys: Vec<String> = members.iter().filter_map(Self::grant_key).collect();
                if keys.len() == members.len() { keys } else { vec![type_key.to_string()] }
            }
            _ => vec![type_key.to_string()],
        }
    }

    /// Register one impl of `traits` for `self_ty`, returning its member
    /// declarations renamed to their compiled symbols.
    ///
    /// Every rule TRAITS.md states about impls is enforced here rather than
    /// at any call site, which is what keeps resolution step 2 a lookup:
    /// coherence (one impl per `(trait, type)`), member partitioning across
    /// the listed traits, member-name collision between two traits on one
    /// type, signature conformance, and missing members.
    fn register_impl(
        &mut self,
        traits: &[String],
        self_ty: &Type,
        members: Vec<Spanned<Expression>>,
        span: Span,
    ) -> Result<Vec<Spanned<Expression>>, Spanned<TypeError>> {
        let err = |msg: String| Spanned::from(TypeError { msg }, span);

        // A nominal union resolves to `Type::Union`, which `grant_key` has no
        // answer for — its declared name is the one `Self` can be spelled as,
        // and the one its members' symbols are named for.
        let key = match self_ty {
            Type::Union(members) => self.union_names.get(members).cloned(),
            _ => Self::grant_key(self_ty),
        };
        let Some(type_key) = key else {
            return Err(err(format!(
                "'{}' can't implement a trait — only a declared type or a primitive can", self_ty
            )));
        };
        if !members.is_empty()
            && (matches!(self_ty, Type::Named { args, .. } if !args.is_empty())
                || self.struct_type_params.get(&type_key).map(|p| !p.is_empty()).unwrap_or(false))
        {
            // A member of a generic type's impl would have to be generic over
            // that type's binders, which are not in scope in an impl body and
            // have no syntax there. Rejected explicitly rather than
            // mistyped — the same call `hoist_data_decls` makes about generic
            // unions (`TRAITS.md` Stage 3a). A bare marker grant supplies no
            // members, so it has nothing to be generic over and stays legal.
            return Err(err(format!(
                "'{}' is generic; implementing a trait for a generic type isn't supported yet", type_key
            )));
        }

        // Resolve the listed traits. A built-in among them is not itself a
        // problem — it can be granted alongside a trait that does have a
        // body — so the body check waits until the members are partitioned,
        // where it can see whether one was actually meant for the built-in.
        let mut defs: Vec<TraitDef> = Vec::with_capacity(traits.len());
        for name in traits {
            // Resolved for its rejections — an unknown name, or `Truthy`,
            // which explains itself. The `Trait` value is recovered per-def
            // below, when the grant is recorded.
            self.resolve_trait_name(name).map_err(&err)?;
            defs.push(self.traits.get(name).expect("resolve_trait_name found it").clone());
        }

        // Partition the supplied members across the listed traits, by which
        // one declares each name. TRAITS.md Part 1 spends its whole design on
        // avoiding overload resolution; this is the one place two traits can
        // still collide, and it is answered here, at the declaration.
        let mut owner_of: HashMap<String, String> = HashMap::new();
        for def in &defs {
            for m in &def.members {
                if let Some(prev) = owner_of.insert(m.name.clone(), def.name.clone()) {
                    return Err(err(format!(
                        "'{}' declares member '{}' and so does '{}' — one type can't implement both for the same member name",
                        prev, m.name, def.name
                    )));
                }
            }
        }

        let mut supplied: HashMap<String, Spanned<Expression>> = HashMap::new();
        for m in members {
            let Some(name) = Self::impl_member_name(&m) else {
                return Err(Spanned::from(TypeError {
                    msg: "an impl body may contain only `func` member declarations".to_string()
                }, m.span));
            };
            if !owner_of.contains_key(&name) {
                // A built-in declares no members, so nothing supplied can
                // belong to one — but if the only trait it could have been
                // meant for is built-in, say why rather than reporting a
                // member name that no listing could ever accept.
                if let Some(b) = defs.iter().find(|d| d.builtin.is_some()) {
                    if defs.iter().all(|d| d.builtin.is_some()) {
                        return Err(Spanned::from(TypeError {
                            msg: format!("'{}' is a built-in trait: its behaviour is a compiler intrinsic, so it can be granted but not implemented with a body", b.name)
                        }, m.span));
                    }
                }
                let listed: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
                return Err(Spanned::from(TypeError {
                    msg: format!("'{}' is not a member of {}", name, listed.join(" or "))
                }, m.span));
            }
            if supplied.insert(name.clone(), m).is_some() {
                return Err(err(format!("member '{}' is supplied twice in this impl", name)));
            }
        }

        // The keys this impl is *reachable* under. For everything but a
        // nominal union that is the type's own key. A union has no
        // `Type::Struct` of its own — a value of it is always one of its
        // variants, and both `union_member_arms` and `granted` ask under the
        // variant's key — so registering under the alias name alone would
        // index the impl somewhere no call site ever looks.
        let index_keys = self.impl_index_keys(self_ty, &type_key);

        // Coherence and the step-2 index. Both checked before anything is
        // written, so a rejected impl leaves no half-registration behind.
        for def in &defs {
            for key in &index_keys {
                if self.impls.contains_key(&(def.name.clone(), key.clone())) {
                    return Err(err(format!(
                        "'{}' already implements '{}' — one impl per trait per type", key, def.name
                    )));
                }
                for m in &def.members {
                    if let Some((other_trait, _)) = self.member_index.get(&(key.clone(), m.name.clone())) {
                        return Err(err(format!(
                            "'{}' already has a member '{}' from trait '{}'; '{}' can't also provide it",
                            key, m.name, other_trait, def.name
                        )));
                    }
                }
            }
        }

        let mut out = Vec::new();
        for def in &defs {
            // One fresh unification variable per binder of a
            // type-parameterized trait (empty for the overwhelming
            // majority, which have none) — `check_impl_member` binds each
            // from this impl's own explicit member annotations rather than
            // from anything written at the `provides` site. See
            // `TraitDef::type_params`'s doc comment.
            //
            // Bounded, not bare: `def.type_params[i].bounds` is the
            // binder's own declared bound list (`trait Boxy<Item: Num>`),
            // and this fresh var is what `check_impl_member`'s
            // `declared.substitute(&subst)` replaces the binder with — so a
            // bare `fresh_var` here would silently drop the bound before
            // `bind_item_template`/`unify` ever get a chance to check it.
            let mut item_subst: HashMap<String, Type> = HashMap::with_capacity(def.type_params.len());
            for tp in &def.type_params {
                let bounds = self.resolve_bounds(&tp.bounds, span)?;
                let var = self.fresh_bounded_var(bounds);
                item_subst.insert(Self::trait_type_param_binder(&def.name, &tp.name), var);
            }
            let mut symbols: HashMap<String, String> = HashMap::new();
            for m in &def.members {
                // One symbol per member, named for the declared type even
                // when it is indexed under several variant keys: the member
                // takes the union itself, so there is one body to call.
                let symbol = Self::member_symbol(&type_key, &m.name);
                match supplied.remove(&m.name) {
                    Some(decl) => {
                        let renamed = self.check_impl_member(decl, m, self_ty, &type_key, &symbol, &item_subst)?;
                        out.push(renamed);
                    }
                    None if m.has_default() => {
                        out.push(self.specialize_default(m, &type_key, &symbol, span)?);
                    }
                    None => {
                        return Err(err(format!(
                            "'{}' provides '{}' but doesn't implement member '{}'",
                            type_key, def.name, m.name
                        )));
                    }
                }
                symbols.insert(m.name.clone(), symbol.clone());
                for key in &index_keys {
                    self.member_index.insert(
                        (key.clone(), m.name.clone()),
                        (def.name.clone(), symbol.clone()),
                    );
                }
                let owners = self.member_traits.entry(m.name.clone()).or_default();
                if !owners.contains(&def.name) { owners.push(def.name.clone()); }
            }
            // The grant itself. A marker trait's `provides` already went
            // through `hoist_data_decls`; a standalone impl's has not, and
            // re-granting is harmless either way since `type_implements` only
            // asks for membership.
            let tr = self.resolve_trait_name(&def.name).map_err(&err)?;
            for key in &index_keys {
                self.impls.insert((def.name.clone(), key.clone()), symbols.clone());
                let granted = self.provides.entry(key.clone()).or_default();
                if !granted.contains(&tr) { granted.push(tr.clone()); }
            }
        }
        Ok(out)
    }

    /// Build the member declaration for a default body this impl didn't
    /// override: the trait's own body, with every `Self` in its signature
    /// rewritten to the implementing type, under this impl's symbol.
    ///
    /// This is monomorphization by construction rather than by machinery.
    /// `Self` is a binder, so a default body *is* a generic declaration over
    /// one parameter, and one specialized copy per implementing type is
    /// exactly what monomorphizing it would emit — but every implementing
    /// type is already known here, at registration, so there is nothing to
    /// discover later and no template to keep. Per-instantiation checking
    /// falls out too: the body is re-checked against each concrete `Self`,
    /// which is the trade `CONCURRENCY.md` and `TRAITS.md` both already take
    /// for generics.
    ///
    /// Known limit, shared with every other declaration in the language: a
    /// default body that calls a member declared *after* it in the same
    /// trait won't resolve, because froglang requires declaration before use
    /// and these are spliced in declaration order. Order the trait's members
    /// so callees come first.
    fn specialize_default(
        &mut self,
        sig: &TraitMemberSig,
        type_key: &str,
        symbol: &str,
        span: Span,
    ) -> Result<Spanned<Expression>, Spanned<TypeError>> {
        let default = sig.decl.default.clone().expect("caller checked has_default");
        let mut params = Vec::with_capacity(sig.decl.params.len());
        for p in &sig.decl.params {
            let mut ty = p.ty.clone().ok_or_else(|| Spanned::from(TypeError {
                msg: format!("Parameter '{}' of trait member '{}' needs a type annotation", p.name, sig.name)
            }, span))?;
            Self::substitute_self_expr(&mut ty, type_key);
            params.push(Parameter { name: p.name.clone(), ty: Some(ty), mutable: p.mutable });
        }
        let return_type = sig.decl.return_type.clone().map(|mut rt| {
            Self::substitute_self_expr(&mut rt, type_key);
            rt
        });
        let func = Spanned::from(
            Expression::function_with_return(params, *default, return_type),
            span,
        );
        Ok(Spanned::from(
            Expression::assign(
                Spanned::from(Expression::literal(Token::Identifier(symbol.to_string())), span),
                None,
                func,
                Some(Mutability::Immutable),
            ),
            span,
        ))
    }

    /// The declared name of an impl-body member, or `None` if the statement
    /// isn't a `func` declaration at all.
    fn impl_member_name(stmt: &Spanned<Expression>) -> Option<String> {
        match &stmt.item {
            Expression::Assign(a) if matches!(&a.value.item, Expression::Function(_)) =>
                a.target.item.get_identifier().map(|s| s.to_string()),
            _ => None,
        }
    }

    /// Check one supplied member against its trait signature and rename it to
    /// its compiled symbol.
    ///
    /// Omitted annotations are filled in from the trait's own declaration
    /// with `Self` rewritten to the implementing type — so
    /// `func area(c) = ...` is legal and means exactly what the signature
    /// says, and `func area(c: Self) = ...` is legal too. Supplied
    /// annotations are resolved and checked for equality against the
    /// signature; anything else would let an impl silently narrow or widen
    /// the interface every call site was type-checked against.
    fn check_impl_member(
        &mut self,
        decl: Spanned<Expression>,
        sig: &TraitMemberSig,
        self_ty: &Type,
        type_key: &str,
        symbol: &str,
        item_subst: &HashMap<String, Type>,
    ) -> Result<Spanned<Expression>, Spanned<TypeError>> {
        let span = decl.span;
        let err = |msg: String| Spanned::from(TypeError { msg }, span);
        let Expression::Assign(mut a) = decl.item else { unreachable!("checked by impl_member_name") };
        let Expression::Function(f) = &mut a.value.item else { unreachable!("checked by impl_member_name") };

        if f.params.len() != sig.params.len() {
            return Err(err(format!(
                "member '{}' takes {} parameter(s), but trait declares {}",
                sig.name, f.params.len(), sig.params.len()
            )));
        }

        let mut subst = item_subst.clone();
        subst.insert(SELF_BINDER.to_string(), self_ty.clone());
        for (i, p) in f.params.iter_mut().enumerate() {
            let (_, declared, declared_mut) = &sig.params[i];
            let expected = declared.substitute(&subst);
            match &p.ty {
                Some(_) => {
                    let mut ann = p.ty.clone().expect("just matched Some");
                    Self::substitute_self_expr(&mut ann, type_key);
                    let got = self.resolve_type_expr(&ann)?;
                    // `unify`, not `==`: `expected` may still carry an
                    // unbound fresh variable for one of the trait's own
                    // type parameters (`item_subst`) — this is the site
                    // that *infers* it, from this member's own explicit
                    // annotation. For every ordinary (non-parameterized)
                    // trait `expected` is already fully concrete, so
                    // `unify` behaves exactly like `==` did (its first
                    // check is a plain equality short-circuit).
                    if !self.bind_item_template(&expected, &got) {
                        return Err(err(format!(
                            "parameter '{}' of member '{}' is declared {}, but the trait says {}",
                            p.name, sig.name, got, self.lookup(&expected)
                        )));
                    }
                    p.ty = Some(ann);
                }
                None => {
                    // A position that mentions one of the trait's own type
                    // parameters has nothing to fill in *from* — its
                    // concrete type is exactly what's being inferred here,
                    // there is no source spelling for it to copy the way
                    // `Self` (a single spellable name) can be. Only `Self`
                    // itself may still be omitted and auto-filled.
                    if item_subst.keys().any(|k| Self::mentions_typevar_named(declared, k)) {
                        return Err(err(format!(
                            "parameter '{}' of member '{}' needs a type annotation — its type isn't just Self, so nothing can infer it",
                            p.name, sig.name
                        )));
                    }
                    // Fill it in from the trait's own annotation, specialized
                    // to this impl — exact, and it keeps the lowering path
                    // that follows entirely ordinary.
                    let Some(mut ann) = sig.decl.params[i].ty.clone() else {
                        return Err(err(format!(
                            "parameter '{}' of member '{}' needs a type annotation", p.name, sig.name
                        )));
                    };
                    Self::substitute_self_expr(&mut ann, type_key);
                    p.ty = Some(ann);
                }
            }
            if p.mutable != *declared_mut {
                return Err(err(format!(
                    "parameter '{}' of member '{}' is declared {}mut, but the trait says {}mut",
                    p.name, sig.name,
                    if p.mutable { "" } else { "not " },
                    if *declared_mut { "" } else { "not " },
                )));
            }
        }

        let expected_ret = sig.return_type.substitute(&subst);
        match &f.return_type {
            Some(_) => {
                let mut ann = f.return_type.clone().expect("just matched Some");
                Self::substitute_self_expr(&mut ann, type_key);
                let got = self.resolve_type_expr(&ann)?;
                if !self.bind_item_template(&expected_ret, &got) {
                    return Err(err(format!(
                        "member '{}' returns {}, but the trait says {}", sig.name, got, self.lookup(&expected_ret)
                    )));
                }
                f.return_type = Some(ann);
            }
            None => {
                if item_subst.keys().any(|k| Self::mentions_typevar_named(&sig.return_type, k)) {
                    return Err(err(format!(
                        "member '{}' needs a return type annotation — its type isn't just Self, so nothing can infer it",
                        sig.name
                    )));
                }
                if let Some(mut ann) = sig.decl.return_type.clone() {
                    Self::substitute_self_expr(&mut ann, type_key);
                    f.return_type = Some(ann);
                }
            }
        }

        // The rename. From here it is an ordinary function declaration under
        // a name no source text can spell.
        a.target = Box::new(Spanned::from(
            Expression::literal(Token::Identifier(symbol.to_string())),
            a.target.span,
        ));
        Ok(Spanned::from(Expression::Assign(a), span))
    }

    /// Resolve a written bound list (`T: Ord + Eq`) to `Trait`s.
    fn resolve_bounds(&self, names: &[String], span: Span) -> Result<Vec<Trait>, Spanned<TypeError>> {
        let mut out = Vec::with_capacity(names.len());
        for n in names {
            let tr = self.resolve_trait_name(n)
                .map_err(|msg| Spanned::from(TypeError { msg }, span))?;
            if !out.contains(&tr) { out.push(tr); }
        }
        Ok(out)
    }

    /// Does this type mention the `Self` binder anywhere, however deeply
    /// (`List<Self>`, `Self | None`, `(Self) -> Int`)?
    fn mentions_self(ty: &Type) -> bool {
        match ty {
            Type::TypeVar { name, .. } => name == SELF_BINDER,
            Type::Named { args, .. } => args.iter().any(Self::mentions_self),
            Type::Union(members) => members.iter().any(Self::mentions_self),
            Type::Function { params, result } =>
                params.iter().any(Self::mentions_self) || Self::mentions_self(result),
            _ => false,
        }
    }

    /// Bind a type-parameterized trait member's declared type (`template`,
    /// already `Self`-substituted but still possibly mentioning one of the
    /// trait's own type-parameter placeholders as a free `TypeVar`) against
    /// an impl's concrete, fully-resolved annotation (`concrete`) —
    /// `check_impl_member`'s comparison step for a member that mentions
    /// `Item` (or another trait-level binder).
    ///
    /// Not plain `unify`: `unify`'s `Union` handling is built for "does a
    /// bare member widen into a union", not "match two structurally
    /// parallel unions member-for-member" — and the two sides genuinely can
    /// have different member *orders*, since `Type::normalize`'s sort key
    /// ranks an unbound placeholder `TypeVar` differently than whatever
    /// concrete type it ends up bound to (`Item | None` and `Int | None`
    /// are both canonical, but not the same list order). So a `Union` match
    /// here pairs each side's *fully concrete* members up by equality
    /// first (`None` with `None`), and only pairs the — necessarily
    /// placeholder-mentioning — leftovers positionally, which for every
    /// shape this trait system's members actually produce (an optional
    /// return, `T | None`) is exactly the one leftover pair that resolves
    /// the placeholder. Every other `Type` shape just recurses structurally
    /// down to a bare `TypeVar` (bound via ordinary `unify`, the standard
    /// "bind a free variable to a concrete type" case) or a scalar
    /// (compared by equality).
    fn bind_item_template(&mut self, template: &Type, concrete: &Type) -> bool {
        match template {
            Type::TypeVar { name, bounds } => self.unify(&Type::TypeVar { name: name.clone(), bounds: bounds.clone() }, concrete),
            Type::Named { name: n1, args: a1 } => match concrete {
                Type::Named { name: n2, args: a2 } if n1 == n2 && a1.len() == a2.len() =>
                    a1.iter().zip(a2.iter()).all(|(x, y)| self.bind_item_template(x, y)),
                _ => false,
            },
            Type::Function { params: p1, result: r1 } => match concrete {
                Type::Function { params: p2, result: r2 } if p1.len() == p2.len() =>
                    p1.iter().zip(p2.iter()).all(|(x, y)| self.bind_item_template(x, y))
                        && self.bind_item_template(r1, r2),
                _ => false,
            },
            Type::Union(tmembers) => {
                let Type::Union(cmembers) = concrete else { return false };
                if tmembers.len() != cmembers.len() { return false; }
                let mut cleft: Vec<Type> = cmembers.clone();
                let mut pending: Vec<&Type> = Vec::new();
                for tm in tmembers {
                    if let Some(pos) = cleft.iter().position(|c| c == tm) {
                        cleft.remove(pos);
                    } else {
                        pending.push(tm);
                    }
                }
                if pending.len() != cleft.len() { return false; }
                pending.iter().zip(cleft.iter()).all(|(t, c)| self.bind_item_template(t, c))
            },
            _ => template == concrete,
        }
    }

    /// `mentions_self`, generalized to any named placeholder — used for a
    /// type-parameterized trait's own binders (`RANGES.md` Stage 2), which
    /// unlike `Self` are not one fixed global name.
    fn mentions_typevar_named(ty: &Type, name: &str) -> bool {
        match ty {
            Type::TypeVar { name: n, .. } => n == name,
            Type::Named { args, .. } => args.iter().any(|a| Self::mentions_typevar_named(a, name)),
            Type::Union(members) => members.iter().any(|m| Self::mentions_typevar_named(m, name)),
            Type::Function { params, result } =>
                params.iter().any(|p| Self::mentions_typevar_named(p, name)) || Self::mentions_typevar_named(result, name),
            _ => false,
        }
    }

    /// Register every `data Name(field: Type, ...)` declaration found
    /// directly in `stmts` into `self.struct_defs`, in three phases so
    /// declarations can reference each other regardless of source order:
    /// (1) register every name, so forward references resolve; (2) resolve
    /// every field list to concrete `Type`s; (3) check the resulting
    /// field-type graph for direct/transitive self-reference, which would
    /// make an unboxed struct infinite size.
    fn hoist_data_decls(&mut self, stmts: &[Spanned<Expression>]) -> Result<(), Spanned<TypeError>> {
        let outer = self.type_param_scope.clone();
        for s in stmts {
            if let Expression::DataDecl(d) = &s.item {
                if self.struct_templates.contains_key(&d.name) || self.union_defs.contains_key(&d.name) {
                    return Err(Spanned::from(TypeError {
                        msg: format!("'{}' is already declared", d.name)
                    }, s.span));
                }
                if !d.type_params.is_empty() && !d.variants.is_empty() {
                    // Struct "monomorphization" is purely a layout
                    // substitution (see `materialize_struct`) — a nominal
                    // union's variants are boxed/tagged instead
                    // (`FrogVariant`), which this substitution machinery
                    // was never built for. Out of scope for `TRAITS.md`
                    // Stage 3a; revisit alongside Stage 3b/3c if generic
                    // unions are ever needed.
                    return Err(Spanned::from(TypeError {
                        msg: format!(
                            "generic unions aren't supported yet — '{}' declares both <{}> and 'is' variants",
                            d.name,
                            d.type_params.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
                        )
                    }, s.span));
                }
                if d.variants.is_empty() {
                    self.struct_templates.insert(d.name.clone(), Vec::new());
                    self.struct_defs.insert(Type::strukt(&d.name), Vec::new());
                    if !d.type_params.is_empty() {
                        let tvar_names: Vec<String> = d.type_params.iter()
                            .map(|_| match self.fresh_var() {
                                Type::TypeVar { name, .. } => name,
                                _ => unreachable!("fresh_var always returns a TypeVar"),
                            })
                            .collect();
                        self.struct_type_params.insert(d.name.clone(), tvar_names);
                    }
                } else {
                    for v in &d.variants {
                        self.variant_owners.entry(v.name.clone()).or_default().push(d.name.clone());
                    }
                    // Only the member *names* are needed to build the
                    // nominal marker types below — field types aren't
                    // resolved until the second pass, which is what lets a
                    // union reference itself recursively (`data Tree is
                    // Leaf | Node(l: Tree, r: Tree)`): by the time a
                    // member's fields are resolved, `Tree`'s own `Type`
                    // (and its `union_names` reverse entry) already exist.
                    let mut member_types: Vec<Type> = d.variants.iter()
                        .map(|v| Type::strukt(format!("{}.{}", d.name, v.name)))
                        .collect();
                    // The same canonical order `normalize` uses — these
                    // marker types bypass it (they are built already
                    // flattened and deduplicated) but must still agree with
                    // it about member position, which is the tag.
                    member_types.sort();
                    let ty = Type::Union(member_types.clone());
                    self.union_names.insert(member_types, d.name.clone());
                    self.union_defs.insert(d.name.clone(), UnionDef { common: Vec::new(), variants: Vec::new(), ty });
                }
            }
        }
        for s in stmts {
            if let Expression::DataDecl(d) = &s.item {
                // Scope each generic decl's own binder names to a
                // placeholder `TypeVar` (minted above, in the first pass)
                // while its own field `TypeExpr`s are resolved just below —
                // `resolve_type_name` consults `type_param_scope` before
                // its ordinary lookup. Cleared right after, so it never
                // leaks into another decl's fields (or, later, into
                // ordinary `check_and_lower` type annotations) — and any
                // scope this pass was entered with is put back once the whole
                // list is hoisted, below.
                self.type_param_scope = HashMap::new();
                if !d.type_params.is_empty() {
                    let tvar_names = self.struct_type_params.get(&d.name).cloned().unwrap_or_default();
                    let mut scope = HashMap::new();
                    for (tp, tvar) in d.type_params.iter().zip(tvar_names) {
                        // Declared bounds ride on the placeholder variable
                        // itself (`TRAITS.md` Part 4) — `unify` and
                        // `type_implements` only ever consult a `TypeVar`'s
                        // own `bounds`, so there is nowhere else to put them.
                        let bounds = self.resolve_bounds(&tp.bounds, s.span)?;
                        scope.insert(tp.name.clone(), Type::TypeVar { name: tvar, bounds });
                    }
                    self.type_param_scope = scope;
                }
                let mut fields: Vec<(String, Type)> = Vec::with_capacity(d.fields.len());
                for (i, p) in d.fields.iter().enumerate() {
                    let ty = self.resolve_type_expr(&p.ty)?;
                    let fname = field_name_or_positional(&p.name, i);
                    // A field list is a map from name to layout slot
                    // everywhere downstream (`struct_defs`, field access,
                    // `lower_record_args`' named-argument matching), and
                    // all of those take the first match — so a repeated
                    // name leaves the second field unreachable, unwritable
                    // by name, and silently occupying a slot. Positional
                    // fields get their index as a synthetic name, which is
                    // unique by construction, so only named ones can trip
                    // this.
                    if fields.iter().any(|(n, _)| *n == fname) {
                        return Err(Spanned::from(TypeError {
                            msg: format!("Field '{}' is declared twice in {}", fname, d.name)
                        }, p.ty.span));
                    }
                    fields.push((fname, ty));
                }
                self.type_param_scope = HashMap::new();
                if d.variants.is_empty() {
                    // The template always; the concrete layout too when the
                    // declaration is non-generic, where the two are the same
                    // list. A generic declaration's layouts are registered per
                    // instantiation by `materialize_struct` instead.
                    self.struct_templates.insert(d.name.clone(), fields.clone());
                    if d.type_params.is_empty() {
                        self.struct_defs.insert(Type::strukt(&d.name), fields.clone());
                    }
                    self.register_field_extras(&d.name, &d.fields, &fields)?;
                } else {
                    let mut seen_variants: HashMap<String, Span> = HashMap::new();
                    let mut variants = Vec::with_capacity(d.variants.len());
                    for v in &d.variants {
                        if seen_variants.contains_key(&v.name) {
                            return Err(Spanned::from(TypeError {
                                msg: format!("Variant '{}' is declared twice in {}", v.name, d.name)
                            }, s.span));
                        }
                        seen_variants.insert(v.name.clone(), s.span);
                        let mut vfields: Vec<(String, Type)> = Vec::with_capacity(v.fields.len());
                        for (i, p) in v.fields.iter().enumerate() {
                            let ty = self.resolve_type_expr(&p.ty)?;
                            let fname = field_name_or_positional(&p.name, i);
                            // Same rule as the common fields above, plus:
                            // a variant's marker struct is the common
                            // fields followed by its own (see the `flat`
                            // list below), so a name that collides with a
                            // common one is just as much a duplicate.
                            if vfields.iter().any(|(n, _)| *n == fname) {
                                return Err(Spanned::from(TypeError {
                                    msg: format!("Field '{}' is declared twice in {}.{}", fname, d.name, v.name)
                                }, p.ty.span));
                            }
                            if fields.iter().any(|(n, _)| *n == fname) {
                                // Positional fields on both sides are the
                                // same collision wearing different clothes:
                                // each side numbers its own slots from 0,
                                // so `flat` would hold two fields named
                                // "0". Say that, rather than report a
                                // duplicate of a name the source never
                                // wrote.
                                let msg = if p.name.is_none() {
                                    format!("{} declares positional common fields, so its variants' fields must be named (`field: Type`) — {}.{}'s positional slots would collide with them", d.name, d.name, v.name)
                                } else {
                                    format!("Field '{}' of {}.{} is already declared as a common field of {}", fname, d.name, v.name, d.name)
                                };
                                return Err(Spanned::from(TypeError { msg }, p.ty.span));
                            }
                            vfields.push((fname, ty));
                        }
                        variants.push((v.name.clone(), vfields));
                    }
                    // Also register each qualified member name as an
                    // ordinary struct — common fields, then the variant's
                    // own — so `Type::strukt("X.Variant")` is a legitimate
                    // standalone (unboxed) type, not just a tag inside the
                    // union's own boxed representation. Nothing routed a
                    // value through this path before flow narrowing
                    // (ERRORS.md Phase 6, `lower_match`'s narrowing
                    // prelude) — `VariantInit`/`VariantField`/`IsVariant`
                    // all go through `union_defs` directly and are
                    // unaffected by this being additionally present here.
                    for (v, (vn, vfields)) in d.variants.iter().zip(variants.iter()) {
                        let mut flat = fields.clone();
                        flat.extend(vfields.clone());
                        // A nominal union's variant marker struct is never
                        // generic, so template and layout coincide.
                        let vkey = format!("{}.{}", d.name, vn);
                        self.struct_templates.insert(vkey.clone(), flat.clone());
                        self.struct_defs.insert(Type::strukt(&vkey), flat.clone());
                        // Common fields, then the variant's own — same order
                        // `flat` was built in above, so the two zip.
                        let raw_flat: Vec<FieldDecl> = d.fields.iter().cloned().chain(v.fields.iter().cloned()).collect();
                        self.register_field_extras(&vkey, &raw_flat, &flat)?;
                    }
                    let def = self.union_defs.get_mut(&d.name).expect("registered in the first pass, above");
                    def.common = fields;
                    def.variants = variants;
                }

                if !d.provides.is_empty() {
                    let mut traits = Vec::with_capacity(d.provides.len());
                    for name in &d.provides {
                        match self.resolve_trait_name(name) {
                            Ok(t) => traits.push(t),
                            Err(msg) => return Err(Spanned::from(TypeError { msg }, s.span)),
                        }
                    }
                    // A union's `provides` grants every variant the trait
                    // (the doc's "a union satisfies a trait iff every
                    // member does" rule then makes the alias itself
                    // satisfy it too, for free, via `type_implements`) —
                    // there's no `Type::Struct` for the alias name itself
                    // to key `provides` off of.
                    if d.variants.is_empty() {
                        self.provides.entry(d.name.clone()).or_default().extend(traits);
                    } else {
                        for v in &d.variants {
                            self.provides.entry(format!("{}.{}", d.name, v.name)).or_default().extend(traits.clone());
                        }
                    }
                }
            }
        }
        // Whatever binders were in scope around these declarations — a
        // generic function's, when a `data` is declared inside its body.
        self.type_param_scope = outer;
        for s in stmts {
            if let Expression::DataDecl(d) = &s.item {
                self.check_struct_acyclic(&d.name, &mut Vec::new(), s.span)?;
            }
        }
        Ok(())
    }

    /// Validate and register one `kind_name`'s field-level `#ann` uses and
    /// literal `= default`s — shared by `hoist_data_decls`'s two call
    /// sites (a plain struct's own fields, and a nominal union variant's
    /// flattened common+own fields). `raw_fields` and `resolved` must be
    /// the same length and in the same order — `hoist_data_decls` builds
    /// both from the same source list, so they always are.
    fn register_field_extras(&mut self, kind_name: &str, raw_fields: &[FieldDecl], resolved: &[(String, Type)]) -> Result<(), Spanned<TypeError>> {
        for (p, (fname, fty)) in raw_fields.iter().zip(resolved.iter()) {
            for ann in &p.annotations {
                self.validate_annotation_use(ann, p.ty.span)?;
            }
            if let Some(default_expr) = &p.default {
                // A field whose declared type is (or contains) one of the
                // declaration's own type parameters has no single type to
                // check a literal default against: `fty` here is the
                // *template*, whose binder `TypeVar`s every instantiation
                // replaces with fresh ones. Checking against the template
                // would both succeed unconditionally (`unify` binds the
                // binder var to the literal's type, permanently, for every
                // later use of that var) and leave the construction site's
                // fresh var unbound, which reaches codegen as a bare
                // `TypeVar` and panics there. Reject it up front instead.
                let mut vars = Vec::new();
                self.free_vars(fty, &mut vars);
                if !vars.is_empty() {
                    return Err(Spanned::from(TypeError {
                        msg: format!("field '{}' of {} has a generic type, so it cannot have a default value", fname, kind_name)
                    }, default_expr.span));
                }
                let cv = eval_const_expr(&default_expr.item).map_err(|msg| Spanned::from(TypeError {
                    msg: format!("default value for field '{}' of {}: {}", fname, kind_name, msg)
                }, default_expr.span))?;
                self.widen_const_checked(&cv, fty, default_expr.span).map_err(|e| Spanned::from(TypeError {
                    msg: format!("default value for field '{}' of {} doesn't match its declared type: {}", fname, kind_name, e.item.msg)
                }, default_expr.span))?;
                self.struct_field_defaults.entry(kind_name.to_string()).or_default().insert(fname.clone(), cv);
            }
        }
        Ok(())
    }

    /// First pass, `annotation` declarations: register every `annotation
    /// name(field: Type = default, ...)` in `stmts` into `annotation_defs`/
    /// `annotation_defaults`, so a `#name(...)` use anywhere in the same
    /// block — including one that appears *before* its declaration,
    /// exactly like `data` — resolves. Must run before
    /// `strip_and_validate_annotations`, which is what actually validates
    /// uses against what's registered here.
    fn hoist_annotation_decls(&mut self, stmts: &[Spanned<Expression>]) -> Result<(), Spanned<TypeError>> {
        for s in stmts {
            // Look through any `#ann` wrapper: this pass runs *before*
            // `strip_and_validate_annotations` (it has to — that pass
            // validates uses against what this one registers), so an
            // `annotation` declaration that itself carries an annotation
            // is still wrapped in `Decorated` here. Missing it would
            // silently drop the declaration entirely: the lowering loops
            // skip `AnnotationDecl` unconditionally.
            let mut item = &s.item;
            while let Expression::Decorated(d) = item { item = &d.target.item; }
            if let Expression::AnnotationDecl(a) = item {
                if self.annotation_defs.contains_key(&a.name) {
                    return Err(Spanned::from(TypeError {
                        msg: format!("annotation '{}' is already declared", a.name)
                    }, s.span));
                }
                let mut fields: Vec<(String, Type)> = Vec::with_capacity(a.fields.len());
                let mut defaults = HashMap::new();
                for f in &a.fields {
                    let Some(fname) = f.name.clone() else {
                        return Err(Spanned::from(TypeError {
                            msg: format!("annotation '{}' fields must be named (`field: Type`), not positional", a.name)
                        }, f.ty.span));
                    };
                    // `validate_annotation_use` matches a provided value
                    // against the *first* declaration of a name but then
                    // type-checks positionally, so a duplicate would report
                    // a bogus type mismatch on the second one rather than
                    // the real problem.
                    if fields.iter().any(|(n, _)| *n == fname) {
                        return Err(Spanned::from(TypeError {
                            msg: format!("annotation '{}' declares field '{}' twice", a.name, fname)
                        }, f.ty.span));
                    }
                    let ty = self.resolve_type_expr(&f.ty)?;
                    if let Some(default_expr) = &f.default {
                        let cv = eval_const_expr(&default_expr.item).map_err(|msg| Spanned::from(TypeError {
                            msg: format!("default value for annotation field '{}': {}", fname, msg)
                        }, default_expr.span))?;
                        self.widen_const_checked(&cv, &ty, default_expr.span).map_err(|e| Spanned::from(TypeError {
                            msg: format!("default value for annotation field '{}' doesn't match its declared type: {}", fname, e.item.msg)
                        }, default_expr.span))?;
                        defaults.insert(fname.clone(), cv);
                    }
                    fields.push((fname, ty));
                }
                self.annotation_defs.insert(a.name.clone(), fields);
                self.annotation_defaults.insert(a.name.clone(), defaults);
            }
        }
        Ok(())
    }

    /// Second pass: unwrap every `Expression::Decorated(annotations,
    /// target)` in `stmts` back into its bare `target`, validating each
    /// annotation against `annotation_defs` on the way. Runs once,
    /// directly on the block's own top-level statement list (never
    /// recurses into nested blocks — `check_and_lower_entry`/`lower_block`
    /// call this and `hoist_annotation_decls` at their own level, same as
    /// `hoist_data_decls`), so every hoist/lower pass downstream sees only
    /// the plain `DataDecl`/`Assign`/etc nodes it already knows how to
    /// handle — no call site anywhere else needs to know `Decorated`
    /// exists.
    fn strip_and_validate_annotations(&mut self, stmts: &mut [Spanned<Expression>]) -> Result<(), Spanned<TypeError>> {
        for s in stmts.iter_mut() {
            // `while`, not `if`: annotations can nest (an annotated
            // `annotation` declaration, most concretely), and every layer
            // has to be validated and unwrapped for the passes downstream
            // to see the plain node.
            while matches!(s.item, Expression::Decorated(_)) {
                let placeholder = Expression::Literal(LiteralExpr { token: Token::None });
                let taken = std::mem::replace(&mut s.item, placeholder);
                let Expression::Decorated(d) = taken else { unreachable!("just checked above") };
                for ann in &d.annotations {
                    self.validate_annotation_use(ann, s.span)?;
                }
                s.item = d.target.item;
            }
        }
        Ok(())
    }

    /// Typecheck one `#name(...)` use against its `annotation_defs` entry:
    /// the annotation exists, every provided field is declared and
    /// well-typed, every field without a provided value falls back to its
    /// default (or is a "missing required field" error). Also recognizes
    /// two sugars alongside plain `field=value`: a bare identifier naming a
    /// declared `Bool` field (`#json(skip)` ≡ `skip=true`), and — only
    /// when the annotation has exactly one field — a single positional
    /// value (`#rename("x")` ≡ `name="x"`).
    fn validate_annotation_use(&mut self, ann: &AnnotationUse, span: Span) -> Result<(), Spanned<TypeError>> {
        let field_defs = self.annotation_defs.get(&ann.name).cloned().ok_or_else(|| Spanned::from(TypeError {
            msg: format!("unknown annotation '#{}' — no `annotation {}(...)` is declared", ann.name, ann.name)
        }, span))?;
        let defaults = self.annotation_defaults.get(&ann.name).cloned().unwrap_or_default();

        let mut provided: Vec<(String, ConstValue)> = Vec::with_capacity(ann.args.len());
        for arg in &ann.args {
            match &arg.item {
                Expression::Assign(a) => {
                    let Some(fname) = a.target.item.get_identifier() else {
                        return Err(Spanned::from(TypeError {
                            msg: "annotation field name must be a plain identifier".to_string()
                        }, arg.span));
                    };
                    let cv = eval_const_expr(&a.value.item).map_err(|msg| Spanned::from(TypeError {
                        msg: format!("annotation field '{}': {}", fname, msg)
                    }, a.value.span))?;
                    provided.push((fname.to_string(), cv));
                }
                Expression::Literal(LiteralExpr { token: Token::Identifier(fname) })
                    if field_defs.iter().any(|(n, t)| n == fname && matches!(self.lookup(t), Type::Bool)) =>
                {
                    // `#json(skip)` sugar for `skip=true`.
                    provided.push((fname.clone(), ConstValue::Bool(true)));
                }
                _ if ann.args.len() == 1 && field_defs.len() == 1 => {
                    // Single positional value sugar: `#rename("x")`.
                    let cv = eval_const_expr(&arg.item).map_err(|msg| Spanned::from(TypeError {
                        msg: format!("annotation '{}': {}", ann.name, msg)
                    }, arg.span))?;
                    provided.push((field_defs[0].0.clone(), cv));
                }
                _ => return Err(Spanned::from(TypeError {
                    msg: format!("annotation '#{}' requires named fields, e.g. #{}(field=value)", ann.name, ann.name)
                }, arg.span)),
            }
        }

        let mut seen = std::collections::HashSet::new();
        for (fname, _) in &provided {
            if !field_defs.iter().any(|(n, _)| n == fname) {
                return Err(Spanned::from(TypeError {
                    msg: format!("annotation '{}' has no field '{}'", ann.name, fname)
                }, span));
            }
            if !seen.insert(fname.clone()) {
                return Err(Spanned::from(TypeError {
                    msg: format!("duplicate field '{}' in '#{}(...)'", fname, ann.name)
                }, span));
            }
        }

        for (fname, fty) in &field_defs {
            let value = provided.iter().find(|(n, _)| n == fname).map(|(_, v)| v.clone())
                .or_else(|| defaults.get(fname).cloned());
            let Some(value) = value else {
                return Err(Spanned::from(TypeError {
                    msg: format!("annotation '#{}' is missing required field '{}'", ann.name, fname)
                }, span));
            };
            self.widen_const_checked(&value, fty, span).map_err(|e| Spanned::from(TypeError {
                msg: format!("annotation '#{}' field '{}': {}", ann.name, fname, e.item.msg)
            }, span))?;
        }
        Ok(())
    }

    /// DFS over the struct field-type graph, following only direct
    /// `Type::Struct` fields (a `List<SomeStruct>` field is fine — a list is
    /// a heap pointer, not inline storage, so it can't create an
    /// infinite-size cycle the way a direct field can). A `Type::Union`
    /// field is always fine too — a nominal union's member is a single
    /// boxed pointer (see `runtime::gc::FrogVariant`), so it can't create
    /// an infinite-size cycle either; this is what makes a recursive union
    /// (e.g. a binary tree, `data Tree is Leaf | Node(l: Tree, r: Tree)`)
    /// legal even though a recursive struct is not.
    fn check_struct_acyclic(&self, name: &str, path: &mut Vec<String>, span: Span) -> Result<(), Spanned<TypeError>> {
        if path.iter().any(|n| n == name) {
            path.push(name.to_string());
            return Err(Spanned::from(TypeError {
                msg: format!("Struct type contains itself: {}", path.join(" -> "))
            }, span));
        }
        path.push(name.to_string());
        if let Some(fields) = self.struct_templates.get(name).cloned() {
            for (_, fty) in &fields {
                if let Some(inner) = fty.as_struct_name() {
                    self.check_struct_acyclic(inner, path, span)?;
                }
            }
        }
        path.pop();
        Ok(())
    }

    /// Type-check a `Kind(field=value, ...)` (named fields) or `Kind(value,
    /// ...)` (positional/"tuple struct" fields — see `is_positional_fields`)
    /// construction call's arguments against `field_defs` — shared by
    /// struct construction and enum variant construction (`kind_name` is
    /// only used for error text, e.g. `"Person"` or `"Shape.Circle"`).
    /// Which shape is required is fixed entirely by how `Kind` was
    /// declared (`Grammar::field_list` already rejected a mixed
    /// declaration), never chosen by the call site.
    /// Check *and* lower a construction call's arguments against
    /// `field_defs`, returning them in declared-field order (which is the
    /// order codegen's flattened leaf layout — `struct_fields` in
    /// codegen/mod.rs — expects, regardless of the order the source wrote
    /// them in). Shared by struct construction and enum variant
    /// construction; `kind_name` is only used for error text, e.g.
    /// `"Person"` or `"Shape.Circle"`.
    ///
    /// Each argument is lowered once and its own resulting type is what's
    /// checked against the declared field type, then widened into it (a
    /// union member gets boxed, an `Int` in a `Float` slot gets a `Coerce`
    /// — see `lower_widen`).
    fn lower_record_args(&mut self, kind_name: &str, field_defs: &[(String, Type)], args: Vec<Spanned<Expression>>, span: Span) -> Result<RecordArgs, Spanned<TypeError>> {
        if is_positional_fields(field_defs) && !field_defs.is_empty() {
            return self.lower_positional_record_args(kind_name, field_defs, args, span);
        }
        // Argument order is the *source's*, and each is checked against the
        // field it names, so lowering happens here (in source order, for
        // predictable diagnostics) and reordering afterwards.
        let mut fields: Vec<(String, Box<Spanned<TypedExpr>>)> = Vec::with_capacity(args.len());
        for arg in args {
            let arg_span = arg.span;
            let (fname, value_expr) = match arg.item {
                Expression::Assign(a) => {
                    let fname = a.target.item.get_identifier()
                        .ok_or_else(|| Spanned::from(TypeError {
                            msg: "Field name must be a plain identifier".to_string()
                        }, arg_span))?
                        .to_string();
                    (fname, *a.value)
                },
                _ => return Err(Spanned::from(TypeError {
                    msg: format!("Construction requires named fields, e.g. {}(field=value)", kind_name)
                }, arg_span)),
            };
            if fields.iter().any(|(n, _)| *n == fname) {
                return Err(Spanned::from(TypeError {
                    msg: format!("Duplicate field '{}' in construction of {}", fname, kind_name)
                }, arg_span));
            }
            let field_ty = field_defs.iter().find(|(n, _)| n == &fname)
                .map(|(_, t)| t.clone())
                .ok_or_else(|| Spanned::from(TypeError {
                    msg: format!("{} has no field '{}'", kind_name, fname)
                }, arg_span))?;
            let value_span = value_expr.span;
            let value = self.check_and_lower(value_expr)?;
            let resolved_value_ty = self.lookup(&value.item.ty);
            let resolved_field_ty = self.lookup(&field_ty);
            if !(widens_to(&resolved_value_ty, &resolved_field_ty)
                || self.unify(&value.item.ty, &field_ty)
                || self.unify_with_one_union_member(&value.item.ty, &field_ty)) {
                return Err(Spanned::from(TypeError {
                    msg: format!("Field '{}' of {} expects {}, got {}", fname, kind_name, resolved_field_ty, resolved_value_ty)
                }, value_span));
            }
            fields.push((fname, Box::new(value)));
        }
        let mut ordered = Vec::with_capacity(field_defs.len());
        for (fname, fty) in field_defs {
            let idx = fields.iter().position(|(n, _)| n == fname);
            let value = match idx {
                Some(idx) => {
                    let (_, value) = fields.remove(idx);
                    Box::new(self.lower_widen(*value, fty)?)
                }
                // `plans/DATA.md` Stage 6's struct-field-default
                // prerequisite: a field the call omitted falls back to its
                // declared `= default` (a literal constant, already
                // type-checked against `fty` when it was registered — see
                // `register_field_extras`) rather than erroring.
                None => match self.struct_field_defaults.get(kind_name).and_then(|m| m.get(fname)).cloned() {
                    Some(default) => Box::new(self.lower_widen(Self::const_value_to_typed(&default, span), fty)?),
                    None => return Err(Spanned::from(TypeError {
                        msg: format!("Missing field '{}' in construction of {}", fname, kind_name)
                    }, span)),
                },
            };
            ordered.push((fname.clone(), value));
        }
        Ok(ordered)
    }

    /// `lower_record_args`'s counterpart for a positionally-declared
    /// `Kind` (`is_positional_fields(field_defs)`): `args` must be plain
    /// expressions (no `field=value` — there's no field name to assign
    /// to), one per declared field, in declared order. Which shape is
    /// required is fixed entirely by how `Kind` was declared
    /// (`Grammar::field_list` already rejected a mixed declaration), never
    /// chosen by the call site.
    fn lower_positional_record_args(&mut self, kind_name: &str, field_defs: &[(String, Type)], args: Vec<Spanned<Expression>>, span: Span) -> Result<RecordArgs, Spanned<TypeError>> {
        if args.len() != field_defs.len() {
            return Err(Spanned::from(TypeError {
                msg: format!("{} takes {} positional argument(s), got {}", kind_name, field_defs.len(), args.len())
            }, span));
        }
        let mut ordered = Vec::with_capacity(field_defs.len());
        for (arg, (fname, field_ty)) in args.into_iter().zip(field_defs.iter()) {
            let arg_span = arg.span;
            if matches!(&arg.item, Expression::Assign(_)) {
                return Err(Spanned::from(TypeError {
                    msg: format!("{} takes positional arguments, not named ones (it was declared with positional fields)", kind_name)
                }, arg_span));
            }
            let value = self.check_and_lower(arg)?;
            let resolved_value_ty = self.lookup(&value.item.ty);
            let resolved_field_ty = self.lookup(field_ty);
            if !(widens_to(&resolved_value_ty, &resolved_field_ty) || self.unify(&value.item.ty, field_ty)) {
                return Err(Spanned::from(TypeError {
                    msg: format!("Argument to {} expects {}, got {}", kind_name, resolved_field_ty, resolved_value_ty)
                }, arg_span));
            }
            ordered.push((fname.clone(), Box::new(self.lower_widen(value, field_ty)?)));
        }
        Ok(ordered)
    }

    /// Resolve a call's callee to `(enum_name, variant_name)` if it names
    /// an enum variant constructor — either bare (`Circle(...)`, valid
    /// only if `Circle` is declared by exactly one enum) or qualified
    /// (`Shape.Circle(...)`). `Ok(None)` means "not a variant constructor
    /// at all" (an ordinary call, or struct construction — handled by the
    /// caller before/after this).
    fn resolve_variant_callee(&self, callable: &Expression) -> Result<Option<(String, String)>, String> {
        match callable {
            Expression::Literal(LiteralExpr { token: Token::Identifier(name) }) => {
                match self.variant_owners.get(name) {
                    None => Ok(None),
                    Some(owners) if owners.len() == 1 => Ok(Some((owners[0].clone(), name.clone()))),
                    Some(owners) => Err(format!(
                        "Variant '{}' is ambiguous between {} — qualify it, e.g. {}.{}(...)",
                        name, owners.join(", "), owners[0], name
                    )),
                }
            }
            Expression::FieldAccess(fa) => {
                if let Some(enum_name) = fa.target.item.get_identifier() {
                    if let Some(def) = self.union_defs.get(enum_name) {
                        return if def.variant_index(&fa.field).is_some() {
                            Ok(Some((enum_name.to_string(), fa.field.clone())))
                        } else {
                            Err(format!("{} has no variant '{}'", enum_name, fa.field))
                        };
                    }
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// Infer a bare identifier that isn't a bound variable as a nullary
    /// (parenthesis-free) enum variant construction, e.g. `Red` for
    /// `data Color is Red | Green | Blue`. Only legal when the variant (and
    /// its enum's common fields) declare no fields at all — anything with
    /// fields must be constructed with call syntax, even if the variant
    /// itself is empty (to supply the common fields).
    fn infer_bare_variant(&self, name: &str, span: Span) -> TypeResult {
        match self.variant_owners.get(name) {
            None => Err(Spanned::from(TypeError { msg: format!("Unbound variable {}", name) }, span)),
            Some(owners) if owners.len() > 1 => Err(Spanned::from(TypeError {
                msg: format!("Variant '{}' is ambiguous between {} — qualify it, e.g. {}.{}(...)", name, owners.join(", "), owners[0], name)
            }, span)),
            Some(owners) => {
                let enum_name = &owners[0];
                let def = self.union_defs.get(enum_name).expect("registered");
                let variant_fields = def.variants.iter().find(|(n, _)| n == name)
                    .map(|(_, fs)| fs).expect("registered");
                if !def.common.is_empty() || !variant_fields.is_empty() {
                    return Err(Spanned::from(TypeError {
                        msg: format!("Variant '{}' has fields and must be constructed with {}(...)", name, name)
                    }, span));
                }
                Ok(def.ty.clone())
            }
        }
    }

    /// `is Error` (ERRORS.md Phase 6): a trait-bound arm on a nominal
    /// union, expanded here into one concrete arm per `Error`-providing
    /// variant not otherwise literally named `Error` — same `guard`/`body`
    /// cloned into each, so the *existing* per-arm machinery (bind
    /// resolution, flow narrowing, `Conditional` chaining) handles each
    /// expansion exactly like a user-written arm, and exhaustiveness falls
    /// out for free once every expansion's own `idx` is marked covered.
    /// Binds are rejected — a single positional bind can't mean the same
    /// thing across variants with different field lists; flow narrowing
    /// (no binder, on a bare-identifier subject) is the way to read a
    /// specific member's own fields after the trait test.
    fn expand_trait_arms(&self, arms: Vec<MatchArm>, enum_name: &str, def: &UnionDef) -> Result<Vec<MatchArm>, Spanned<TypeError>> {
        let mut out = Vec::with_capacity(arms.len());
        for arm in arms {
            let is_trait_arm = arm.pattern.path.is_none() && arm.pattern.variant == "Error" && def.variant_index("Error").is_none();
            if !is_trait_arm {
                out.push(arm);
                continue;
            }
            if !arm.pattern.binds.is_empty() {
                return Err(Spanned::from(TypeError {
                    msg: "trait pattern 'Error' cannot bind fields — match on a concrete variant, or use flow narrowing (no binder) to read a specific member's fields".to_string()
                }, arm.body.span));
            }
            for (vn, _) in &def.variants {
                let ty = Type::strukt(format!("{}.{}", enum_name, vn));
                if self.type_implements(&ty, &Trait::Error) {
                    out.push(MatchArm {
                        pattern: Pattern { path: None, variant: vn.clone(), binds: Vec::new(), resolved_member: None },
                        guard: arm.guard.clone(),
                        body: arm.body.clone(),
                    });
                }
            }
        }
        Ok(out)
    }

    /// `expand_trait_arms`'s counterpart for an *anonymous* union subject.
    fn expand_trait_arms_anon(&self, arms: Vec<MatchArm>, members: &[Type]) -> Result<Vec<MatchArm>, Spanned<TypeError>> {
        let mut out = Vec::with_capacity(arms.len());
        for arm in arms {
            let is_trait_arm = arm.pattern.path.is_none() && arm.pattern.variant == "Error" && self.resolve_type_name("Error").is_none();
            if !is_trait_arm {
                out.push(arm);
                continue;
            }
            if !arm.pattern.binds.is_empty() {
                return Err(Spanned::from(TypeError {
                    msg: "trait pattern 'Error' cannot bind fields — match on a concrete variant, or use flow narrowing (no binder) to read a specific member's fields".to_string()
                }, arm.body.span));
            }
            for (i, m) in members.iter().enumerate() {
                if self.type_implements(m, &Trait::Error) {
                    out.push(MatchArm {
                        pattern: Pattern { path: None, variant: m.to_string(), binds: Vec::new(), resolved_member: Some(i) },
                        guard: arm.guard.clone(),
                        body: arm.body.clone(),
                    });
                }
            }
        }
        Ok(out)
    }

    /// Validate a match/`is` pattern against `enum_name`'s definition:
    /// the optional qualifier (if present) must name the same enum, the
    /// variant must exist, and — if any binds are given at all — their
    /// count must exactly match the variant's field arity. Returns the
    /// variant's declaration index (its runtime tag).
    fn check_pattern(&self, pattern: &Pattern, enum_name: &str, def: &UnionDef, span: Span) -> Result<usize, Spanned<TypeError>> {
        if let Some(path) = &pattern.path {
            if path != enum_name {
                return Err(Spanned::from(TypeError {
                    msg: format!("Pattern '{}.{}' does not match subject type {}", path, pattern.variant, enum_name)
                }, span));
            }
        }
        let idx = def.variant_index(&pattern.variant).ok_or_else(|| Spanned::from(TypeError {
            msg: format!("{} has no variant '{}'", enum_name, pattern.variant)
        }, span))?;
        let arity = def.variants[idx].1.len();
        if !pattern.binds.is_empty() && pattern.binds.len() != arity {
            return Err(Spanned::from(TypeError {
                msg: format!("Pattern for variant '{}' expects {} binding(s), got {}", pattern.variant, arity, pattern.binds.len())
            }, span));
        }
        Ok(idx)
    }

    /// Validate a match/`is` pattern naming a bare type (`is Int`) against
    /// an *anonymous* union's flat, sorted member list — the counterpart
    /// of `check_pattern` for a nominal union's declared variant name.
    /// Returns the member's index in `members` (its runtime tag, used
    /// consistently by `Widen`/`Narrow`/`TypeTag` for the same union) and
    /// its resolved `Type`.
    fn check_type_pattern(&self, pattern: &Pattern, members: &[Type], span: Span) -> Result<(usize, Type), Spanned<TypeError>> {
        // A pattern the compiler synthesized itself (`union_entries`'s
        // anonymous-union branch) already knows its member's index — use it
        // directly rather than re-deriving it from `variant`, which for a
        // synthesized pattern is only `Type::to_string()`'s *display* form
        // and may not be a resolvable (or even parseable) type name at all,
        // e.g. `List<Int>`. See `Pattern::resolved_member`'s doc comment.
        let (idx, member_ty) = if let Some(idx) = pattern.resolved_member {
            let member_ty = members.get(idx).cloned().ok_or_else(|| Spanned::from(TypeError {
                msg: format!("internal error: resolved_member index {} out of range for {}", idx, Type::Union(members.to_vec()))
            }, span))?;
            (idx, member_ty)
        } else {
            // A qualified pattern (`is ParseError.UnexpectedEof(...)`) names
            // one *flat* member of this anonymous union directly — `data X
            // is A | B`'s desugaring registers `X.A`/`X.B` as their own
            // struct types, and normalizing a union that contains the
            // nominal alias `X` flattens it to those same qualified member
            // types (`Type::normalize`), so there is no separate nested tag
            // to unbox here: `"path.variant"` is simply this member's own
            // registered name, resolved exactly like a bare one.
            let lookup_name = match &pattern.path {
                Some(path) => format!("{}.{}", path, pattern.variant),
                None => pattern.variant.clone(),
            };
            let member_ty = self.resolve_type_name(&lookup_name).ok_or_else(|| Spanned::from(TypeError {
                msg: format!("Unknown type '{}'", lookup_name)
            }, span))?;
            let idx = members.iter().position(|m| *m == member_ty).ok_or_else(|| Spanned::from(TypeError {
                msg: format!("{} is not a member of {}", member_ty, Type::Union(members.to_vec()))
            }, span))?;
            (idx, member_ty)
        };
        if pattern.binds.len() > 1 {
            return Err(Spanned::from(TypeError {
                msg: format!("Pattern for '{}' expects at most 1 binding, got {}", member_ty, pattern.binds.len())
            }, span));
        }
        Ok((idx, member_ty))
    }

    /// `lower_expected`'s tail, split out for callers that already have the
    /// type in hand and no expression to lower: given an `actual` type,
    /// accept it if
    /// it's a subtype of `expected`, or unify if `actual` is still an open
    /// type variable. Used by a bare `return` (no expression to hand
    /// `check`) checking its implicit `None` against the enclosing
    /// function's return type.
    fn check_ty(&mut self, actual: Type, expected: &Type, span: Span) -> TypeResult {
        let resolved = self.lookup(&actual);
        if self.is_subtype(&resolved, expected) {
            Ok(resolved)
        } else if (matches!(resolved, Type::TypeVar { .. }) && self.unify(&resolved, expected))
            || self.unify_with_one_union_member(&resolved, expected)
        {
            Ok(self.lookup(&resolved))
        } else {
            Err(Spanned::from(TypeError {
                msg: format!("Expected {} got {}", expected, resolved)
            }, span))
        }
    }

    /// Subtype relation:
    ///   T ≤ T
    ///   Never ≤ T  (the bottom type is a subtype of everything)
    ///   T ≤ T | U  (T is a member of any union it belongs to)
    ///   T | U ≤ V  iff T ≤ V and U ≤ V
    pub fn is_subtype(&self, sub: &Type, sup: &Type) -> bool {
        let sub = self.lookup(sub);
        let sup = self.lookup(sup);
        if sub == sup { return true; }
        if sub == Type::Never { return true; }
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

    /// Shared logic for polymorphic operators bounded by a single trait:
    /// arithmetic (`Num`) and ordering (`Ord`) both (a) require each
    /// operand to satisfy `tr` unless it's still an unbound TypeVar, then
    /// (b) either fold the concrete operands through the widening lattice,
    /// or — if any operand is an unresolved TypeVar or a union — unify
    /// every operand against one fresh `tr`-bounded type variable instead.
    /// Returns the joined/unified operand type; `Ord` callers always want
    /// `Bool` instead, so they call this for its type-checking/unification
    /// side effects and discard the result.
    ///
    /// Operands arrive as already-computed `(type, span)` pairs — they were
    /// lowered by the caller, which is the whole point of the single pass:
    /// no operand subtree is ever walked a second time to find out its
    /// type. The trait check still happens operand-by-operand in source
    /// order, so diagnostics are unchanged.
    fn join_operand_types(&mut self, op: &str, tr: Trait, args: &[(Type, Span)], span: Span) -> TypeResult {
        let mut arg_types: Vec<Type> = Vec::with_capacity(args.len());
        for (argt, arg_span) in args {
            let resolved = self.lookup(argt);
            if !matches!(&resolved, Type::TypeVar { .. }) && !self.type_implements(&resolved, &tr) {
                return Err(Spanned::from(TypeError {
                    msg: format!("Operator '{}' requires {}, got {}", op, tr, resolved)
                }, *arg_span));
            }
            arg_types.push(resolved);
        }
        let has_union = arg_types.iter().any(|t| matches!(t, Type::Union(..)));
        let concrete: Vec<&Type> = arg_types.iter().filter(|t| !matches!(t, Type::TypeVar { .. })).collect();

        if concrete.is_empty() || has_union {
            // All TypeVars, or a union operand present — unify via one fresh
            // tr-bounded var. Unions satisfy `tr` iff every variant does
            // (handled by unify).
            let t = self.fresh_bounded_var(vec![tr.clone()]);
            for ((_, arg_span), argt) in args.iter().zip(&arg_types) {
                if !self.unify(argt, &t) {
                    return Err(Spanned::from(TypeError {
                        msg: format!("Operator '{}' requires {}, got {}", op, tr, argt)
                    }, *arg_span));
                }
            }
            return Ok(self.lookup(&t));
        }

        // All scalar operands — fold through the widening lattice.
        let mut join = concrete[0].clone();
        for ty in &concrete[1..] {
            join = numeric_join(&join, ty).ok_or_else(|| Spanned::from(TypeError {
                msg: format!("Operator '{}' got incompatible types: {} and {}", op, join, ty)
            }, span))?;
        }
        // Bind any TypeVars to the join type so lambda params get concrete types.
        for argt in &arg_types {
            if matches!(argt, Type::TypeVar { .. }) { self.unify(argt, &join); }
        }
        Ok(join)
    }

    /// Result type of a built-in operator applied to operands whose types
    /// are already known (they were lowered first — see `check_and_lower`'s
    /// `Unary`/`Binary` arms).
    ///
    /// Arithmetic (+, -, *, /, unary-): require `Num` — works for Int and Float.
    /// Equality (==, !=): require `Eq`  — works for Int, Float, Bool, Str.
    /// Ordering (<, >, <=, >=): require `Ord` — works for Int, Float, Str.
    /// Logical (and, or, not): any `Truthy` type, coerced to Bool.
    fn builtin_op_type(&mut self, op: &str, args: &[(Type, Span)], span: Span) -> TypeResult {
        match op {
            "not" | "and" | "or" => {
                for (argt, arg_span) in args {
                    self.check_condition(argt, *arg_span)?;
                }
                Ok(Type::Bool)
            },

            "+" | "-" | "*" | "/" | "<unaryminus>" => {
                // String concatenation: `Str + Str -> Str`, decided from the
                // left operand's type and checked against the right.
                if op == "+" && args.len() == 2 {
                    let resolved_left = self.lookup(&args[0].0);
                    if resolved_left == Type::Str {
                        let resolved_right = self.lookup(&args[1].0);
                        if resolved_right == Type::Str {
                            return Ok(Type::Str);
                        }
                        return Err(Spanned::from(TypeError {
                            msg: format!("Operator '+' on Str requires Str on both sides, got {}", resolved_right)
                        }, args[1].1));
                    }
                }
                self.join_operand_types(op, Trait::Num, args, span)
            },

            "==" | "!=" => {
                let t = self.fresh_bounded_var(vec![Trait::Eq]);
                // Union-typed operands first: `unify` accepts a bare member
                // against a union it belongs to but not the reverse, so
                // binding `t` to the narrower side would reject `none == x`
                // while accepting the identical `x == none`.
                let mut ordered: Vec<&(Type, Span)> = args.iter().collect();
                ordered.sort_by_key(|(argt, _)| !matches!(self.lookup(argt), Type::Union(_)));
                for (argt, arg_span) in ordered {
                    if !self.unify(argt, &t) {
                        let resolved_t    = self.lookup(&t);
                        let resolved_argt = self.lookup(argt);
                        let msg = if matches!(resolved_t, Type::TypeVar { .. }) {
                            format!("Operator '{}' requires Eq, got {}", op, resolved_argt)
                        } else {
                            format!("Operator '{}' got incompatible types: expected {}, got {}", op, resolved_t, resolved_argt)
                        };
                        return Err(Spanned::from(TypeError { msg }, *arg_span));
                    }
                }
                Ok(Type::Bool)
            },

            "<" | ">" | "<=" | ">=" => {
                // Comparisons always yield Bool regardless of the operand
                // type; `join_operand_types` is called purely for its
                // type-checking/unification side effects here.
                self.join_operand_types(op, Trait::Ord, args, span)?;
                Ok(Type::Bool)
            },

            // `in` is handled entirely by `lower_in`, called directly from
            // `lower_binary` before this generic dispatch is ever reached
            // — see its own doc comment for why.
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
                result: Box::new(self.lookup(result)),
            },
            Type::Named { name, args } => Type::Named {
                name: name.clone(),
                args: args.iter().map(|a| self.lookup(a)).collect(),
            },
            Type::Union(variants)  => Type::renormalize(variants, variants.iter().map(|t| self.lookup(t)).collect()),
            _ => ty.clone(),
        }
    }

    /// Run `closure` inside a fresh scope, then close it — so any binding
    /// made while it runs, whether by the closure itself or by anything it
    /// recurses into, is gone again on return.
    ///
    /// This is the *only* thing that should open a scope. Note that `R` is
    /// deliberately unconstrained: when the closure returns a `Result`, the
    /// scope closes before that result reaches the caller's `?`, so an early
    /// error can't leak a half-built scope. Every call site is therefore
    /// `self.in_scope(|t| ...)?` rather than `self.in_scope(|t| ...?)`.
    fn in_scope<F, R>(&mut self, closure: F) -> R where F: FnOnce(&mut Self) -> R {
        let mark = self.ctx.open();
        // `func_mut_params` is a flat name->mutability-list map with no
        // scope structure of its own, so it is saved and restored around
        // every scope alongside `ctx`. Without this, a nested `func f`
        // kept dictating argument mutability for calls to an *outer* `f`
        // after the inner one went out of scope, and a local binding that
        // shadowed a `func` left the shadowed function's list in place.
        // Restoring wholesale (rather than tracking marks) is cheap: the
        // map has one entry per named `func`, not one per binding.
        let saved_mut_params = self.func_mut_params.clone();
        let result = closure(self);
        self.func_mut_params = saved_mut_params;
        self.ctx.close(mark);
        result
    }

    /// `in_scope`, with `update_ctx` bound in the new scope before `closure`
    /// runs — the common case of "these names are visible for exactly this
    /// region".
    fn with_context<F, R>(
        &mut self,
        update_ctx: impl Iterator<Item = (String, Type)>,
        closure: F,
    ) -> R where F: FnOnce(&mut Self) -> R,
    {
        self.in_scope(|t| {
            for (name, ty) in update_ctx { t.ctx.insert(name, ty); }
            closure(t)
        })
    }

    /// As `with_context`, but each binding states its own mutability —
    /// needed where a rebind may shadow an existing `mut` name (flow
    /// narrowing rebinding a match subject under its own name) alongside
    /// ordinary always-immutable pattern binds in the same call.
    fn with_context_mut<F, R>(
        &mut self,
        update_ctx: impl Iterator<Item = (String, Type, bool)>,
        closure: F,
    ) -> R where F: FnOnce(&mut Self) -> R,
    {
        self.in_scope(|t| {
            for (name, ty, mutable) in update_ctx { t.ctx.insert_mut(name, ty, mutable); }
            closure(t)
        })
    }

    /// True iff the type variable named `name` appears free anywhere inside
    /// `ty` (after substitution) — i.e. binding `name := ty` in
    /// `substitutions` would create an infinite type, like `~t = List(~t)`.
    /// `unify`'s TypeVar-binding arms all check this before writing to
    /// `substitutions`; without it, `List(~t)` unifying with `~t` would
    /// silently loop the next time anything called `lookup` on `~t`.
    /// Harmless before `TRAITS.md` Stage 2 (no generalization means no
    /// scheme can introduce a genuinely cyclic constraint on its own), but
    /// generalization makes this reachable, so it's added now rather than
    /// discovered as a hang once schemes exist.
    fn occurs_in(&self, name: &str, ty: &Type) -> bool {
        match self.lookup(ty) {
            Type::TypeVar { name: n, .. } => n == name,
            Type::Function { params, result } =>
                params.iter().any(|p| self.occurs_in(name, p)) || self.occurs_in(name, &result),
            Type::Named { args, .. } => args.iter().any(|a| self.occurs_in(name, a)),
            Type::Union(variants) => variants.iter().any(|v| self.occurs_in(name, v)),
            Type::None | Type::Int | Type::Float | Type::Bool | Type::Str | Type::Never => false,
        }
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
                // `t1 == t2` above already caught `n1 == n2` with identical
                // bounds; a same-named pair with different bounds shouldn't
                // arise (a TypeVar's bounds are fixed at creation), but
                // binding a variable to itself is a no-op guarded against
                // here rather than relied upon not to happen.
                if n1 == n2 { return true; }
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
                if self.occurs_in(name, &t1) { return false; }
                if bounds.iter().all(|b| variants.iter().all(|v| self.type_implements(v, b))) {
                    self.substitutions.insert(name.clone(), t1.clone());
                    true
                } else {
                    false
                }
            },
            (Type::TypeVar { name, bounds }, Type::Union(variants)) => {
                if self.occurs_in(name, &t2) { return false; }
                if bounds.iter().all(|b| variants.iter().all(|v| self.type_implements(v, b))) {
                    self.substitutions.insert(name.clone(), t2.clone());
                    true
                } else {
                    false
                }
            },
            // Bounded TypeVar on left, concrete type on right.
            (Type::TypeVar { name, bounds }, _) => {
                if self.occurs_in(name, &t2) { return false; }
                if !bounds.iter().all(|b| self.type_implements(&t2, b)) {
                    return false;
                }
                self.substitutions.insert(name.clone(), t2.clone());
                true
            },
            // Concrete type on left, bounded TypeVar on right.
            (_, Type::TypeVar { name, bounds }) => {
                if self.occurs_in(name, &t1) { return false; }
                if !bounds.iter().all(|b| self.type_implements(&t1, b)) {
                    return false;
                }
                self.substitutions.insert(name.clone(), t1.clone());
                true
            },
            // Concrete type on left, Union on right:
            // accept (without binding) if the concrete is a member of the union.
            // This handles e.g. the second operand of `(Int|Float) + 2` after the first
            // operand already bound the operator TypeVar to the union.
            (_, Type::Union(variants)) => variants.contains(&t1),
            // Structural unification for functions.
            (Type::Function { params: p1, result: r1 }, Type::Function { params: p2, result: r2 }) => {
                if p1.len() != p2.len() {
                    return false;
                }
                p1.iter().zip(p2.iter()).all(|(l, r)| self.unify(l, r)) &&
                self.unify(r1, r2)
            },
            // Structural unification for named type constructors — invariant
            // in every argument position (`TRAITS.md` Part 4, "Variance"):
            // `List<Int>` does not unify with `List<Int | Str>` in either
            // direction, since a list's runtime layout (stride, ptr_mask)
            // depends on its element type and differs between them. Same
            // constructor name and arity is required; a zero-arg `Named`
            // (an ordinary struct) still falls out of this as a vacuous
            // conjunction over zero arguments, so this arm also replaces
            // the old nominal struct comparison — nothing else needs to be
            // said for that case since `t1 == t2`'s fast path above already
            // catches the common one, and this arm covers the (rare)
            // remaining case where one side still carries a TypeVar in a
            // field of a not-yet-fully-resolved structural type.
            (Type::Named { name: n1, args: a1 }, Type::Named { name: n2, args: a2 }) => {
                n1 == n2 && a1.len() == a2.len()
                    && a1.iter().zip(a2.iter()).all(|(l, r)| self.unify(l, r))
            },
            _ => false,
        }
    }

    /// The last resort when a value is being checked against an *expected*
    /// union type and plain `unify` has already said no: unify it with the
    /// one member it fits.
    ///
    /// `unify`'s own `(_, Type::Union(variants))` arm accepts a concrete
    /// type only by *equality* with some member, and deliberately binds
    /// nothing (see `join_types`, which depends on that). Equality is too
    /// weak for two cases that are otherwise unreachable:
    ///
    ///   `data Box<A>(v: A | Str)` / `Box(v=1)` — the member is the binder
    ///       `~t1`, so nothing is equal to `Int` and the instantiation that
    ///       would make it fit is exactly what has to be discovered here;
    ///   `let x: List<Str> | Int = []` — `[]` synthesizes `List(~t0)`, which
    ///       is equal to no member either, though it unifies with one.
    ///
    /// Directional on purpose: this is only ever right where one side is an
    /// expectation imposed on the other (an annotation, a declared field, a
    /// return slot). It must not be reachable from `join_types`, where the
    /// two types are peers and binding one side's variable to the other's
    /// member would pin a variable the program never constrained.
    ///
    /// Each member is tried against a snapshot of `substitutions` and rolled
    /// back, so a failed attempt leaves nothing behind. Exactly one member
    /// must fit: `let x: List<Int> | List<Str> = []` fits two, and guessing
    /// between them would silently pick a runtime tag, so it is left to fail
    /// as "not accepted" and be annotated properly.
    fn unify_with_one_union_member(&mut self, actual: &Type, expected: &Type) -> bool {
        let Type::Union(members) = self.lookup(expected) else { return false };
        let snapshot = self.substitutions.clone();
        let mut winner: Option<HashMap<String, Type>> = None;
        for member in &members {
            if self.unify(actual, member) {
                // Take the bindings this member produced and put the clean
                // snapshot back, so the next member starts from the same
                // state this one did.
                let produced = std::mem::replace(&mut self.substitutions, snapshot.clone());
                if winner.is_some() {
                    return false;      // ambiguous; `substitutions` is already the snapshot
                }
                winner = Some(produced);
            } else {
                self.substitutions = snapshot.clone();
            }
        }
        match winner {
            Some(produced) => { self.substitutions = produced; true },
            None => false,
        }
    }

    /// Join two arm/branch types (an `if`/`match`'s arms, or a
    /// `Conditional`'s two branches) into their combined result type: if
    /// they're really the same type — including through any `TypeVar`
    /// binding `unify` performs as a side effect, e.g. binding an
    /// unconstrained lambda parameter to a concrete arm type — that type;
    /// otherwise their union (`normalize`, which drops `Never` and
    /// flattens/dedupes nested unions).
    ///
    /// `unify`'s own boolean return isn't trustworthy for this: its
    /// `(_, Type::Union(variants)) => variants.iter().any(|v| t1 == *v)` arm
    /// — needed so e.g. `(Int|Float) + 2`'s second operand is accepted
    /// against an already-established union — returns `true` for "this
    /// concrete type is merely compatible with that union" without binding
    /// anything. That's correct for an operand check, but wrong here: it
    /// would leave a match/if's overall type as the narrower concrete type
    /// even though one arm is genuinely union-typed, producing a
    /// type-inconsistent typed AST (the node's own `.ty` disagreeing with
    /// that arm's actual multi-value shape) that codegen can't safely act
    /// on — it trusts `.ty` to decide single- vs. multi-value handling, so
    /// it would read only the union's tag word as if it were the whole
    /// scalar result. Checking resolved equality *after* calling `unify`
    /// (rather than trusting its return value) tells the two apart: real
    /// unification always leaves both sides resolving to the same type;
    /// the union-membership shortcut leaves them different.
    fn join_types(&mut self, a: &Type, b: &Type) -> Type {
        let unified = self.unify(a, b);
        let ra = self.lookup(a);
        let rb = self.lookup(b);
        if unified && ra == rb {
            ra
        } else {
            Type::Union(vec![a.clone(), b.clone()]).normalize()
        }
    }

    /// Type-check and lower a top-level program or REPL entry.
    ///
    /// The parser always wraps its output in a `Block` (see `Parser::block`),
    /// but unlike a *nested* `{ ... }` block expression, the outermost list
    /// of statements is not its own lexical scope: a top-level `let` must
    /// remain visible to later statements in the same entry **and** to later
    /// `eval` calls on the same `FrogState` (which reuses this `TypeChecker`
    /// across entries). Plain `check_and_lower` scopes every `Block` it
    /// sees, so it can only be used here on the unwrapped statement list.
    pub fn check_and_lower_entry(
        &mut self,
        expr: Spanned<Expression>,
    ) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let span = expr.span;
        match expr.item {
            Expression::Block(mut stmts) => {
                self.hoist_annotation_decls(&stmts)?;
                self.strip_and_validate_annotations(&mut stmts)?;
                self.hoist_trait_names(&stmts)?;
                self.hoist_data_decls(&stmts)?;
                self.hoist_trait_members(&stmts)?;
                self.expand_impls(&mut stmts)?;
                let mut lowered = Vec::with_capacity(stmts.len());
                let mut ty = Type::None;
                for s in stmts {
                    if matches!(s.item, Expression::DataDecl(_) | Expression::TraitDecl(_) | Expression::AnnotationDecl(_)) { continue; }
                    let t = self.check_and_lower(s)?;
                    ty = t.item.ty.clone();
                    lowered.push(t);
                }
                if let Some((_, rest)) = lowered.split_last() {
                    for t in rest {
                        self.check_must_handle(t)?;
                    }
                }
                Ok(Spanned::from(TypedExpr { id: 0, ty, kind: TypedExprKind::Block(lowered) }, span))
            },
            other => self.check_and_lower(Spanned::from(other, span)),
        }
    }

    /// The source-level name behind a lowered `Var`'s name, for a
    /// diagnostic: a reference to a generalized declaration carries that
    /// declaration's template symbol (`id#42`, see `Binding::symbol`), and
    /// no user ever wrote that.
    fn source_name(lowered: &str) -> &str {
        lowered.split('#').next().unwrap_or(lowered)
    }

    /// Post-lowering validation: reject constructs the type checker accepts
    /// but codegen can't yet compile, as a spanned `TypeError` rather than a
    /// codegen-time `panic!` recovered by `catch_unwind` (see
    /// `print_union`'s recursion guard, whose condition this mirrors
    /// exactly). It is
    /// decidable from the fully-resolved typed AST alone, so this walks it
    /// once after `check_and_lower_entry`/`check_and_lower` produce it —
    /// deliberately *not* folded into the single-pass lowering walk itself,
    /// since these checks need every type fully resolved (no stray
    /// `TypeVar`s), which is only guaranteed once lowering has finished.
    pub fn validate_codegen_constraints(&self, expr: &Spanned<TypedExpr>) -> Result<(), Spanned<TypeError>> {
        match &expr.item.kind {
            TypedExprKind::IntLit(_) | TypedExprKind::FloatLit(_) | TypedExprKind::BoolLit(_)
            | TypedExprKind::StrLit(_) | TypedExprKind::NoneLit => Ok(()),

            // A function in value position used to be rejected here.
            // `lower_function_values` compiles those away now — hoisting
            // the lambda, capturing by value, specializing the callee —
            // and reports whatever it could not resolve
            // (`reject_function_values`), which is a judgement this early
            // pass has no way to make: it runs before monomorphization,
            // so it cannot yet see which calls resolve to what.
            TypedExprKind::Var(_) => Ok(()),

            TypedExprKind::Unary { expr: inner, .. } => self.validate_codegen_constraints(inner),

            TypedExprKind::Binary { left, right, .. } => {
                self.validate_codegen_constraints(left)?;
                self.validate_codegen_constraints(right)
            },

            TypedExprKind::Conditional { cond, true_branch, false_branch } => {
                self.validate_codegen_constraints(cond)?;
                self.validate_codegen_constraints(true_branch)?;
                if let Some(fb) = false_branch { self.validate_codegen_constraints(fb)?; }
                Ok(())
            },

            TypedExprKind::Assign { value, .. } => match &value.item.kind {
                TypedExprKind::Function { body, .. } => self.validate_codegen_constraints(body),
                _ => self.validate_codegen_constraints(value),
            },

            // A function literal away from a declaration used to be
            // rejected here; `lower_function_values` hoists it to a
            // top-level declaration of its own instead. See the `Var` arm.
            TypedExprKind::Function { body, .. } => self.validate_codegen_constraints(body),

            TypedExprKind::Call { callable, args, .. } => {
                // Not `validate_codegen_constraints(callable)`: a bare name
                // in callable position is exactly the supported case, and
                // the `Var` arm rejects that node everywhere else.
                if !matches!(callable.item.kind, TypedExprKind::Var(_)) {
                    self.validate_codegen_constraints(callable)?;
                }
                for a in args.iter().flat_map(Arg::subexprs) { self.validate_codegen_constraints(a)?; }
                // `print`'s argument is dispatched at runtime by
                // `codegen::print_union`, which recurses through struct
                // fields and nested unions to render whichever member
                // actually matched — reject anything that would recurse
                // into itself before codegen has to discover that the hard
                // way (see `print_union`'s own guard, which this mirrors).
                if let (TypedExprKind::Var(name), Some(Some(arg))) = (&callable.item.kind, args.first().map(Arg::value)) {
                    if name == "print" {
                        self.check_printable(&arg.item.ty, arg.span)?;
                    }
                }
                Ok(())
            },

            TypedExprKind::Index { target, index } => {
                self.validate_codegen_constraints(target)?;
                self.validate_codegen_constraints(index)
            },

            TypedExprKind::Slice { target, start, end } => {
                self.validate_codegen_constraints(target)?;
                if let Some(s) = start { self.validate_codegen_constraints(s)?; }
                if let Some(e) = end { self.validate_codegen_constraints(e)?; }
                Ok(())
            },

            TypedExprKind::Range { start, end } => {
                self.validate_codegen_constraints(start)?;
                self.validate_codegen_constraints(end)
            },

            TypedExprKind::List(elems) => {
                for e in elems { self.validate_codegen_constraints(e)?; }
                Ok(())
            },

            TypedExprKind::Block(stmts) => {
                for s in stmts { self.validate_codegen_constraints(s)?; }
                Ok(())
            },

            TypedExprKind::ForLoop { iterable, cond, body, .. } => {
                self.validate_codegen_constraints(iterable)?;
                if let Some(c) = cond { self.validate_codegen_constraints(c)?; }
                self.validate_codegen_constraints(body)
            },

            TypedExprKind::Comprehension { iterable, cond, body, .. } => {
                self.validate_codegen_constraints(iterable)?;
                if let Some(c) = cond { self.validate_codegen_constraints(c)?; }
                self.validate_codegen_constraints(body)
            },

            TypedExprKind::StructInit { fields, .. } => {
                for (_, v) in fields { self.validate_codegen_constraints(v)?; }
                Ok(())
            },

            TypedExprKind::FieldAccess { target, .. } => self.validate_codegen_constraints(target),
            TypedExprKind::PlaceAssign { place, value } => {
                for seg in &place.path {
                    if let PlaceSeg::Index { index, .. } = seg {
                        self.validate_codegen_constraints(index)?;
                    }
                }
                self.validate_codegen_constraints(value)
            },

            TypedExprKind::VariantInit { fields, .. } => {
                for (_, v) in fields { self.validate_codegen_constraints(v)?; }
                Ok(())
            },

            TypedExprKind::IsVariant { target, .. } => self.validate_codegen_constraints(target),
            TypedExprKind::VariantField { target, .. } => self.validate_codegen_constraints(target),

            TypedExprKind::Return(value) => {
                if let Some(v) = value { self.validate_codegen_constraints(v)?; }
                Ok(())
            },

            TypedExprKind::Widen { value, .. } => self.validate_codegen_constraints(value),

            TypedExprKind::Narrow { value, .. } => self.validate_codegen_constraints(value),
            TypedExprKind::TypeTag { target, .. } => self.validate_codegen_constraints(target),
            TypedExprKind::Truthy(value) => self.validate_codegen_constraints(value),
            TypedExprKind::Coerce(value) => self.validate_codegen_constraints(value),
        }
    }

    /// `TRAITS.md` Stage 3b: real monomorphization, replacing Stage 2's
    /// single-instantiation codegen gate (`check_generic_monomorphism`,
    /// which rejected a second distinct instantiation) and its
    /// `resolve_single_instantiations` companion. A generalized name may
    /// now be called at any number of distinct concrete types — in this
    /// entry, or split across several REPL entries — and each distinct
    /// instantiation gets its own compiled body under a mangled symbol
    /// name, so no rejection path is needed at all any more.
    ///
    /// Mutates `typed` in three steps (always leaving it a
    /// `TypedExprKind::Block`, even if it started as a single non-`Block`
    /// top-level expression, so newly-emitted instantiations have
    /// somewhere to live):
    ///  1. collect every `(generalized name, concrete whole-function
    ///     type)` pair actually referenced anywhere in this entry
    ///     (`collect_generic_var_types`);
    ///  2. for each pair not already compiled in some earlier entry
    ///     (`emitted_instantiations`), clone the name's retained
    ///     declaration (`generic_templates`), substitute its binder
    ///     `TypeVar`s to the concrete types recovered by
    ///     `zip_binder_types`, and emit it as a new top-level `Assign`
    ///     under a mangled name (`mangle_type`) — inserted before the
    ///     entry's own tail statement, so the entry's result value/type
    ///     is unaffected. Step 2 runs to a fixed point: substituting a
    ///     clone's binders can make a generic call *inside* that body
    ///     concrete for the first time, so every emitted body is
    ///     re-collected and anything new it needs is queued;
    ///  3. rewrite every `Var(name)` reference — in the entry's original
    ///     statements *and* inside every newly-cloned body, which is what
    ///     makes a recursive generic call resolve correctly — to its
    ///     mangled name.
    ///
    /// The un-substituted generic declaration itself (if made in this
    /// entry) is stripped from the statement list before this returns —
    /// it must never reach codegen with un-substituted binder `TypeVar`s,
    /// which `codegen::cl_type` would silently default to `I64`. Its
    /// template still lives on in `generic_templates` for a future call
    /// (possibly in a later entry) to instantiate from.
    ///
    /// A generic declared but never called anywhere (`seen` doesn't
    /// mention it) is simply dropped from this entry's compiled output —
    /// correct, since there is nothing to compile yet, and its template
    /// remains available for whenever it first is called.
    pub fn monomorphize_generics(&mut self, typed: &mut Spanned<TypedExpr>) -> Result<(), Spanned<TypeError>> {
        if self.generic_templates.is_empty() { return Ok(()); }

        let mut stmts: Vec<Spanned<TypedExpr>> = match std::mem::replace(&mut typed.item.kind, TypedExprKind::IntLit(0)) {
            TypedExprKind::Block(s) => s,
            other => vec![Spanned::from(TypedExpr { id: 0, ty: typed.item.ty.clone(), kind: other }, typed.span)],
        };

        // Pull the tail statement (the entry's own result value) aside
        // before anything else, so newly-emitted instantiations are
        // inserted *before* it, never after — inserting after would
        // silently change which statement determines the entry's result,
        // and so would dropping it in the `retain` below.
        let mut tail = stmts.pop();

        // Never let an un-substituted generic declaration reach codegen —
        // only its instantiations (built below) may. Keyed by the
        // declaration's own template symbol, so a *different*, monomorphic
        // declaration that merely reuses the source name survives.
        let is_generic_decl = |s: &Spanned<TypedExpr>| matches!(&s.item.kind,
            TypedExprKind::Assign { name, .. } if self.generic_templates.contains_key(name));
        stmts.retain(|s| !is_generic_decl(s));
        // An entry whose *last* statement is a generic declaration has no
        // representable result value — the declaration is stripped like
        // any other, and `None` stands in for it, rather than the entry
        // silently reporting the value of the statement before it.
        if tail.as_ref().is_some_and(&is_generic_decl) {
            let span = tail.as_ref().expect("just checked").span;
            tail = Some(Spanned::from(TypedExpr { id: 0, ty: Type::None, kind: TypedExprKind::NoneLit }, span));
        }

        // Resolve every remaining node's stored type through `lookup`
        // (`substitute_types_deep` with an empty mapping is exactly
        // Stage 2's old `resolve_types_deep`) — a call site that passed
        // an argument to a generic function stamped its `Coerce`/`Widen`
        // nodes with that *call's own fresh instantiation `TypeVar`*
        // (`lower_call_arg`/`lower_widen`), which only becomes concrete
        // once `unify` pins it in `self.substitutions`; nothing else in
        // the pipeline goes back and resolves it in place. This has to
        // run over the whole entry, not just a generic's own body, since
        // any ordinary call site anywhere in the program can carry one of
        // these not-yet-resolved nodes.
        for s in stmts.iter_mut() { self.substitute_types_deep(s, &HashMap::new()); }
        if let Some(t) = tail.as_mut() { self.substitute_types_deep(t, &HashMap::new()); }

        // Discovering which instantiations an entry needs is a fixed
        // point, not a single pass over the original tree. Substituting a
        // generic's binders into its cloned body is what makes a call
        // *inside* that body concrete: in `func idpair(a) = id(a)`, the
        // `id` reference carries `idpair`'s own un-resolved binder
        // `TypeVar` until `idpair$Int` is built, and only that clone knows
        // it needs `id$Int`. So each body emitted below is re-scanned and
        // whatever it newly asks for goes back on the worklist.
        // Seeded from the statements that will actually be *compiled* —
        // after the strip above, never from the whole pre-strip tree. A
        // reference inside a generic declaration's own body is a reference
        // from code that is about to be thrown away: its type is still that
        // declaration's abstract binder, so seeding from it emitted a body
        // with un-substituted binder `TypeVar`s under a name like `id$Vt4`
        // — dead code that `codegen::cl_type` silently laid out as `I64`,
        // and a junk symbol permanently occupying `emitted_instantiations`.
        // The only correct source for such a reference is the *clone* the
        // fixed-point loop makes, where the binder is concrete.
        let mut seen: HashMap<String, Vec<(Type, Span)>> = HashMap::new();
        for s in stmts.iter() { self.collect_generic_var_types(s, &mut seen); }
        if let Some(t) = tail.as_ref() { self.collect_generic_var_types(t, &mut seen); }

        let mut mangled_for: HashMap<(String, Type), String> = HashMap::new();
        let mut work: Vec<(String, Type, Span)> = seen.iter()
            .flat_map(|(name, occurrences)| {
                occurrences.iter().map(|(ty, span)| (name.clone(), ty.clone(), *span))
            })
            .collect();
        // `seen` is a `HashMap`, so its iteration order varies run to run and
        // the emitted instantiations came out in a different order each time —
        // same program, different module layout, and nothing reproducible to
        // diff when something goes wrong. Sorting by (name, type) makes the
        // output a function of the program alone. `work` is drained from the
        // back, so sort descending to emit in ascending order.
        work.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));

        while let Some((name, concrete, span)) = work.pop() {
            let Some(template) = self.generic_templates.get(&name).cloned() else { continue };
            // Already handled for this entry — including the recursive
            // case, where the clone's own body refers back to itself.
            if mangled_for.contains_key(&(name.clone(), concrete.clone())) { continue; }

            // Historical/audit record only — see `generic_instantiations`'s
            // doc comment. Nothing here consults it to decide anything.
            self.generic_instantiations.entry(name.clone()).or_default().insert(concrete.clone());

            let mut mapping = HashMap::new();
            Self::zip_binder_types(&template.declared_ty, &concrete, &template.binders, &mut mapping);
            let mangled = format!(
                "{}${}",
                name,
                template.binders.iter()
                    .map(|(bn, _)| Self::mangle_type(&mapping.get(bn).cloned().unwrap_or(Type::None)))
                    .collect::<Vec<_>>()
                    .join("$"),
            );
            // Recorded even when the body itself was compiled by an
            // earlier entry — call sites here still need to be rewritten
            // to that existing symbol.
            mangled_for.insert((name.clone(), concrete.clone()), mangled.clone());
            // Claiming the name *before* walking the new body is the
            // second half of the recursion guard above.
            if !self.emitted_instantiations.insert(mangled.clone()) { continue; }

            let mut new_params = template.params.clone();
            for (_, ty, _) in new_params.iter_mut() { *ty = self.lookup(ty).substitute(&mapping); }
            let new_return = self.lookup(&template.return_type).substitute(&mapping);
            let mut new_body = template.body.clone();
            self.substitute_types_deep(&mut new_body, &mapping);

            // The clone's binders are concrete now, so any generic call it
            // contains finally names a real instantiation. Queue those.
            let mut nested: HashMap<String, Vec<(Type, Span)>> = HashMap::new();
            self.collect_generic_var_types(&new_body, &mut nested);
            // Sorted for the same reason the initial seeding is — see there.
            let mut queued: Vec<(String, Type, Span)> = nested.into_iter()
                .flat_map(|(n, occurrences)| {
                    occurrences.into_iter().map(move |(ty, sp)| (n.clone(), ty, sp))
                })
                .collect();
            queued.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
            work.extend(queued);

            let fn_ty = Type::Function {
                params: new_params.iter().map(|(_, t, _)| t.clone()).collect(),
                result: Box::new(new_return.clone()),
            };
            let assign_node = Spanned::from(TypedExpr {
                id: 0,
                ty: fn_ty.clone(),
                kind: TypedExprKind::Assign {
                    name: mangled.clone(),
                    value: Box::new(Spanned::from(TypedExpr {
                        id: 0,
                        ty: fn_ty,
                        kind: TypedExprKind::Function { params: new_params, return_type: new_return, body: Box::new(new_body) },
                    }, template.body.span)),
                },
            }, span);
            stmts.push(assign_node);
        }

        if let Some(t) = tail { stmts.push(t); }

        // Every binder is substituted by now, in the entry's own statements
        // and in every body emitted above, so a member call made through a
        // bound finally knows which impl it meant.
        for s in stmts.iter_mut() {
            let span = s.span;
            self.resolve_bound_members(s, span)?;
        }

        for s in stmts.iter_mut() { self.rewrite_call_sites(s, &mangled_for); }

        typed.item.ty = stmts.last().map(|s| s.item.ty.clone()).unwrap_or(Type::None);
        typed.item.kind = TypedExprKind::Block(stmts);
        Ok(())
    }

    /// Collect, for every `Var` reference to a generalized name reachable
    /// from `expr`, the distinct concrete whole-function types it's
    /// resolved to (deduplicated by equality) — the enumeration
    /// `monomorphize_generics` needs to know which instantiations this
    /// entry actually requires. Unlike Stage 2's version of this walk,
    /// finding a name used at a second distinct type is no longer an
    /// error — that's exactly the case Stage 3b now handles.
    fn collect_generic_var_types(&self, expr: &Spanned<TypedExpr>, seen: &mut HashMap<String, Vec<(Type, Span)>>) {
        if let TypedExprKind::Var(name) = &expr.item.kind {
            if self.generic_templates.contains_key(name) {
                // `expr.item.ty` is the type as it stood at the moment
                // this `Var` node was built — a freshly-instantiated,
                // still-unbound `TypeVar` at that point, since unification
                // against this call's actual arguments happens afterward.
                // `lookup` resolves it to what it was actually pinned to,
                // which is the comparison that matters here; two
                // instantiations that both happened to resolve to `Int`
                // must not be double-counted just because their fresh
                // `TypeVar` names differ.
                let resolved = self.lookup(&expr.item.ty);
                let list = seen.entry(name.clone()).or_default();
                if !list.iter().any(|(t, _)| *t == resolved) {
                    list.push((resolved, expr.span));
                }
            }
        }
        match &expr.item.kind {
            TypedExprKind::IntLit(_) | TypedExprKind::FloatLit(_) | TypedExprKind::BoolLit(_)
            | TypedExprKind::StrLit(_) | TypedExprKind::NoneLit | TypedExprKind::Var(_) => {},

            TypedExprKind::Unary { expr: inner, .. } => self.collect_generic_var_types(inner, seen),

            TypedExprKind::Binary { left, right, .. } => {
                self.collect_generic_var_types(left, seen);
                self.collect_generic_var_types(right, seen);
            },

            TypedExprKind::Conditional { cond, true_branch, false_branch } => {
                self.collect_generic_var_types(cond, seen);
                self.collect_generic_var_types(true_branch, seen);
                if let Some(fb) = false_branch { self.collect_generic_var_types(fb, seen); }
            },

            TypedExprKind::Assign { value, .. } => self.collect_generic_var_types(value, seen),

            TypedExprKind::Function { body, .. } => self.collect_generic_var_types(body, seen),

            TypedExprKind::Call { callable, args, .. } => {
                self.collect_generic_var_types(callable, seen);
                for a in args.iter().flat_map(Arg::subexprs) { self.collect_generic_var_types(a, seen); }
            },

            TypedExprKind::Index { target, index } => {
                self.collect_generic_var_types(target, seen);
                self.collect_generic_var_types(index, seen);
            },

            TypedExprKind::Slice { target, start, end } => {
                self.collect_generic_var_types(target, seen);
                if let Some(s) = start { self.collect_generic_var_types(s, seen); }
                if let Some(e) = end { self.collect_generic_var_types(e, seen); }
            },

            TypedExprKind::Range { start, end } => {
                self.collect_generic_var_types(start, seen);
                self.collect_generic_var_types(end, seen);
            },

            TypedExprKind::List(elems) => {
                for e in elems { self.collect_generic_var_types(e, seen); }
            },

            TypedExprKind::Block(stmts) => {
                for s in stmts { self.collect_generic_var_types(s, seen); }
            },

            TypedExprKind::ForLoop { iterable, cond, body, .. } => {
                self.collect_generic_var_types(iterable, seen);
                if let Some(c) = cond { self.collect_generic_var_types(c, seen); }
                self.collect_generic_var_types(body, seen);
            },

            TypedExprKind::Comprehension { iterable, cond, body, .. } => {
                self.collect_generic_var_types(iterable, seen);
                if let Some(c) = cond { self.collect_generic_var_types(c, seen); }
                self.collect_generic_var_types(body, seen);
            },

            TypedExprKind::StructInit { fields, .. } => {
                for (_, v) in fields { self.collect_generic_var_types(v, seen); }
            },

            TypedExprKind::FieldAccess { target, .. } => self.collect_generic_var_types(target, seen),
            TypedExprKind::PlaceAssign { place, value } => {
                for seg in &place.path {
                    if let PlaceSeg::Index { index, .. } = seg {
                        self.collect_generic_var_types(index, seen);
                    }
                }
                self.collect_generic_var_types(value, seen);
            },

            TypedExprKind::VariantInit { fields, .. } => {
                for (_, v) in fields { self.collect_generic_var_types(v, seen); }
            },

            TypedExprKind::IsVariant { target, .. } => self.collect_generic_var_types(target, seen),
            TypedExprKind::VariantField { target, .. } => self.collect_generic_var_types(target, seen),

            TypedExprKind::Return(value) => {
                if let Some(v) = value { self.collect_generic_var_types(v, seen); }
            },

            TypedExprKind::Widen { value, .. } => self.collect_generic_var_types(value, seen),

            TypedExprKind::Narrow { value, .. } => self.collect_generic_var_types(value, seen),
            TypedExprKind::TypeTag { target, .. } => self.collect_generic_var_types(target, seen),
            TypedExprKind::Truthy(value) => self.collect_generic_var_types(value, seen),
            TypedExprKind::Coerce(value) => self.collect_generic_var_types(value, seen),
        }
    }

    /// A stable, distinct-per-type string usable as a Cranelift symbol
    /// fragment — deliberately *not* `Type`'s `Display` (`TRAITS.md`
    /// Stage 1's hazard note: `Display`'s output is load-bearing for
    /// union-tag sort keys, so mangling must not be coupled to it). Only
    /// needs to be unique per distinct `Type` and safe as an identifier
    /// fragment (alphanumeric plus `_`); never parsed back.
    fn mangle_type(ty: &Type) -> String {
        match ty {
            Type::None => "None".to_string(),
            Type::Int => "Int".to_string(),
            Type::Float => "Float".to_string(),
            Type::Bool => "Bool".to_string(),
            Type::Str => "Str".to_string(),
            Type::Never => "Never".to_string(),
            Type::TypeVar { name, .. } => {
                let safe: String = name.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
                format!("V{}", safe)
            },
            Type::Function { params, result } => {
                let ps: String = params.iter().map(|p| format!("_{}", Self::mangle_type(p))).collect();
                format!("Fn{}_{}", ps, Self::mangle_type(result))
            },
            Type::Named { name, args } => {
                if args.is_empty() { name.clone() }
                else { format!("{}_{}", name, args.iter().map(Self::mangle_type).collect::<Vec<_>>().join("_")) }
            },
            Type::Union(variants) => format!("U_{}", variants.iter().map(Self::mangle_type).collect::<Vec<_>>().join("_")),
        }
    }

    /// Apply `f` to every `Var` node in `expr`, giving it that node's name
    /// (mutably) and its stored type. The single place the typed AST's
    /// shape is enumerated for a `Var`-renaming pass — `rewrite_call_sites`
    /// and `rename_var` are both wrappers, and adding a `TypedExprKind`
    /// variant should break exactly this match rather than silently skip a
    /// subtree in one pass but not the other.
    ///
    /// Note `Assign`/`Function`/`ForLoop` etc. expose only their
    /// sub-expressions: a *binding* occurrence of a name is not a `Var` and
    /// is deliberately out of reach here. Every caller rewrites references,
    /// never declarations.
    fn walk_vars_mut(expr: &mut Spanned<TypedExpr>, f: &mut impl FnMut(&mut String, &Type)) {
        // Destructured so the name and the type are two disjoint borrows of
        // `expr.item` rather than two overlapping ones.
        let TypedExpr { ty, kind, .. } = &mut expr.item;
        if let TypedExprKind::Var(name) = kind {
            f(name, ty);
        }
        match kind {
            TypedExprKind::IntLit(_) | TypedExprKind::FloatLit(_) | TypedExprKind::BoolLit(_)
            | TypedExprKind::StrLit(_) | TypedExprKind::NoneLit | TypedExprKind::Var(_) => {},

            TypedExprKind::Unary { expr: inner, .. } => Self::walk_vars_mut(inner, f),

            TypedExprKind::Binary { left, right, .. } => {
                Self::walk_vars_mut(left, f);
                Self::walk_vars_mut(right, f);
            },

            TypedExprKind::Conditional { cond, true_branch, false_branch } => {
                Self::walk_vars_mut(cond, f);
                Self::walk_vars_mut(true_branch, f);
                if let Some(fb) = false_branch { Self::walk_vars_mut(fb, f); }
            },

            TypedExprKind::Assign { value, .. } => Self::walk_vars_mut(value, f),

            TypedExprKind::Function { body, .. } => Self::walk_vars_mut(body, f),

            TypedExprKind::Call { callable, args, .. } => {
                Self::walk_vars_mut(callable, f);
                for a in args.iter_mut().flat_map(Arg::subexprs_mut) { Self::walk_vars_mut(a, f); }
            },

            TypedExprKind::Index { target, index } => {
                Self::walk_vars_mut(target, f);
                Self::walk_vars_mut(index, f);
            },

            TypedExprKind::Slice { target, start, end } => {
                Self::walk_vars_mut(target, f);
                if let Some(s) = start { Self::walk_vars_mut(s, f); }
                if let Some(e) = end { Self::walk_vars_mut(e, f); }
            },

            TypedExprKind::Range { start, end } => {
                Self::walk_vars_mut(start, f);
                Self::walk_vars_mut(end, f);
            },

            TypedExprKind::List(elems) => {
                for e in elems.iter_mut() { Self::walk_vars_mut(e, f); }
            },

            TypedExprKind::Block(stmts) => {
                for s in stmts.iter_mut() { Self::walk_vars_mut(s, f); }
            },

            TypedExprKind::ForLoop { iterable, cond, body, .. } => {
                Self::walk_vars_mut(iterable, f);
                if let Some(c) = cond { Self::walk_vars_mut(c, f); }
                Self::walk_vars_mut(body, f);
            },

            TypedExprKind::Comprehension { iterable, cond, body, .. } => {
                Self::walk_vars_mut(iterable, f);
                if let Some(c) = cond { Self::walk_vars_mut(c, f); }
                Self::walk_vars_mut(body, f);
            },

            TypedExprKind::StructInit { fields, .. } => {
                for (_, v) in fields.iter_mut() { Self::walk_vars_mut(v, f); }
            },

            TypedExprKind::FieldAccess { target, .. } => Self::walk_vars_mut(target, f),
            TypedExprKind::PlaceAssign { place, value } => {
                for seg in place.path.iter_mut() {
                    if let PlaceSeg::Index { index, .. } = seg {
                        Self::walk_vars_mut(index, f);
                    }
                }
                Self::walk_vars_mut(value, f);
            },

            TypedExprKind::VariantInit { fields, .. } => {
                for (_, v) in fields.iter_mut() { Self::walk_vars_mut(v, f); }
            },

            TypedExprKind::IsVariant { target, .. } => Self::walk_vars_mut(target, f),
            TypedExprKind::VariantField { target, .. } => Self::walk_vars_mut(target, f),

            TypedExprKind::Return(value) => {
                if let Some(v) = value { Self::walk_vars_mut(v, f); }
            },

            TypedExprKind::Widen { value, .. } => Self::walk_vars_mut(value, f),
            TypedExprKind::Narrow { value, .. } => Self::walk_vars_mut(value, f),
            TypedExprKind::TypeTag { target, .. } => Self::walk_vars_mut(target, f),
            TypedExprKind::Truthy(value) => Self::walk_vars_mut(value, f),
            TypedExprKind::Coerce(value) => Self::walk_vars_mut(value, f),
        }
    }

    /// Rewrite every `Var(name)` reference to a generalized name into its
    /// mangled instantiation name, per `table` (built by
    /// `monomorphize_generics` from the exact `(name, concrete
    /// whole-function type)` pairs it just compiled, or found already
    /// compiled in an earlier entry). Run over every top-level statement
    /// *including* the newly-cloned instantiation bodies themselves, so a
    /// recursive call inside a generic's own body — which resolves to the
    /// same concrete type as the instantiation it lives in — is rewritten
    /// too.
    fn rewrite_call_sites(&self, expr: &mut Spanned<TypedExpr>, table: &HashMap<(String, Type), String>) {
        Self::walk_vars_mut(expr, &mut |name, ty| {
            if !self.generic_templates.contains_key(name.as_str()) { return; }
            let resolved = self.lookup(ty);
            if let Some(mangled) = table.get(&(name.clone(), resolved)) {
                *name = mangled.clone();
            }
        });
    }

    /// Replace every pending member callee (`lower_bound_member_call`) in
    /// `expr` with the compiled symbol of the impl it now names.
    ///
    /// Run by `monomorphize_generics` once every instantiation has been
    /// emitted and every binder substituted, so each pending callee's own
    /// function type carries a *concrete* `Self` in its first parameter —
    /// which is the whole reason nothing had to be recorded on the side.
    /// From there it is the same `(type key, member)` lookup an ordinary
    /// member call does at lowering time, just later.
    ///
    /// A lookup that fails here is a bug rather than a user error — the
    /// declared bound is checked at every call site, so a type reaching this
    /// point implements the trait — but it is reported rather than
    /// `expect`ed, since the alternative is a pending symbol reaching
    /// codegen as an unknown function.
    fn resolve_bound_members(&self, expr: &mut Spanned<TypedExpr>, span: Span) -> Result<(), Spanned<TypeError>> {
        let mut failure: Option<String> = None;
        Self::walk_vars_mut(expr, &mut |name, ty| {
            if failure.is_some() { return; }
            let Some((trait_name, member)) = Self::parse_pending_member(name) else { return };
            let (trait_name, member) = (trait_name.to_string(), member.to_string());
            let self_ty = match self.lookup(ty) {
                Type::Function { params, .. } if !params.is_empty() => params[0].clone(),
                _ => {
                    failure = Some(format!("'{}.{}' lost its receiver type before monomorphization", trait_name, member));
                    return;
                }
            };
            let Some(key) = Self::grant_key(&self_ty) else {
                failure = Some(format!(
                    "'{}' stayed abstract as {}, so '{}.{}' has no impl to call — give the call site a concrete type",
                    trait_name, self_ty, trait_name, member,
                ));
                return;
            };
            match self.member_index.get(&(key, member.clone())) {
                Some((owner, symbol)) if owner == &trait_name => *name = symbol.clone(),
                Some((owner, _)) => failure = Some(format!(
                    "{} implements '{}' from trait '{}', not '{}'", self_ty, member, owner, trait_name)),
                None => failure = Some(format!(
                    "{} doesn't implement '{}.{}'", self_ty, trait_name, member)),
            }
        });
        match failure {
            Some(msg) => Err(Spanned::from(TypeError { msg }, span)),
            None => Ok(()),
        }
    }

    /// Rename every `Var(from)` reference in `expr` to `to`. Used for
    /// exactly one thing: undoing `lower_assign`'s recursion pre-bind when
    /// the declaration turns out monomorphic and keeps its source name —
    /// see the call site for why no shadowing analysis is needed.
    fn rename_var(expr: &mut Spanned<TypedExpr>, from: &str, to: &str) {
        Self::walk_vars_mut(expr, &mut |name, _| {
            if name == from { *name = to.to_string(); }
        });
    }

    // ── Tier 1 function values ────────────────────────────────────────────
    //
    // froglang has no closure object, no function pointer and no indirect
    // call, and this pass is what lets it have first-class functions
    // anyway: every function value whose callee is statically known is
    // compiled away, leaving only top-level declarations and direct calls
    // — precisely the shape `codegen::compile_entry`'s two passes already
    // handle (Pass 1 declares top-level `Assign { value: Function }`
    // statements; Pass 2 compiles each body with `vars` seeded only from
    // its parameters). `liveness.rs` and `linear.rs` already *assume* that
    // invariant, so this pass strengthens what they rely on.
    //
    // Three transforms, in order:
    //
    //  1. **Lambda lifting** (`hoist_expr`) — every `Function` node that
    //     isn't already a top-level declaration's value is hoisted to top
    //     level under a fresh name; a nested `func`/`let f = ...` leaves a
    //     rename behind for the rest of its block, a lambda in expression
    //     position is replaced by a `Var` naming the hoisted declaration.
    //  2. **Capture propagation** (`resolve_captures`) — each declaration's
    //     free value names become trailing parameters, and every call site
    //     passes them. Run to a fixed point: if `f` calls `g` and `g` gained
    //     captures `f` doesn't bind, `f` gains them too.
    //  3. **Specialization** (`specialize_calls`) — a call to a function
    //     with function-typed parameters is rewritten to a clone of that
    //     function with each one substituted by the concrete callee, so
    //     `f(x)` inside the body becomes a direct call.
    //
    // Whatever function value survives all three is one whose callee is
    // *not* statically known — it escaped — and `reject_function_values`
    // reports it as the tier-2 case it is.
    //
    // The whole thing mirrors `monomorphize_generics` deliberately, one
    // level down: that pass clones a template per distinct *type*, this one
    // per distinct *name*. Running after it is what makes that split work —
    // two different lambdas of the same type both land in `map$Int$Int`,
    // and this pass then separates them. The reverse order cannot work, and
    // no iteration back is needed: substituting a name never creates a new
    // type instantiation.
    pub fn lower_function_values(&mut self, typed: &mut Spanned<TypedExpr>) -> Result<(), Spanned<TypeError>> {
        let mut stmts: Vec<Spanned<TypedExpr>> = match std::mem::replace(&mut typed.item.kind, TypedExprKind::IntLit(0)) {
            TypedExprKind::Block(s) => s,
            other => vec![Spanned::from(TypedExpr { id: 0, ty: typed.item.ty.clone(), kind: other }, typed.span)],
        };

        // Held aside so hoisted declarations land *before* it and the
        // entry's result value is unaffected — `monomorphize_generics`
        // does the same, for the same reason.
        let mut tail = stmts.pop();

        // A lambda in argument position takes its parameter types from the
        // expected type, so its `Function` node is built carrying the
        // fresh `TypeVar`s unification later pins — resolved in
        // `self.substitutions` and nowhere else. Hoisting it to a top-level
        // declaration makes those types a *signature*, which codegen reads
        // directly, so they have to be real first.
        // `monomorphize_generics` resolves the same way for the same
        // reason, but it returns early when nothing is generic, so this
        // cannot rely on it having run.
        for s in stmts.iter_mut().chain(tail.iter_mut()) {
            self.substitute_types_deep(s, &HashMap::new());
        }

        // Which names a capture may not name: anything declared `mut`
        // (`mut_names`, recorded during lowering — see its doc comment for
        // why the tree can't answer this), plus anything written through
        // as a place or handed to a `mut` parameter.
        //
        // `mut_names` is name-keyed and never forgets, so a `mut` in one
        // function's body (or an earlier REPL entry) would otherwise poison
        // every same-named binding anywhere else, including one that is
        // provably, currently a `let` — `self.ctx` still resolves it. So a
        // name still live in `ctx` as definitely immutable is trusted over
        // the historical record; only a name `ctx` can't answer for (its
        // scope already closed — a genuine function-local `mut`) falls back
        // to the over-approximation.
        let mut mutated: HashSet<String> = self.mut_names.iter()
            .filter(|n| self.ctx.is_mutable(n) != Some(false))
            .cloned()
            .collect();
        for s in stmts.iter().chain(tail.iter()) { Self::collect_mutated_names(s, &mut mutated); }

        // ── 1. Hoist ─────────────────────────────────────────────────────
        let mut hoisted: Vec<Spanned<TypedExpr>> = Vec::new();
        let mut failed: Option<Spanned<TypeError>> = None;
        {
            let mut scopes: Vec<HashMap<String, Option<String>>> = vec![HashMap::new()];
            // Rebuilt, like a nested block's: a top-level declaration can
            // expand into its capture snapshots plus itself.
            let mut rebuilt: Vec<Spanned<TypedExpr>> = Vec::with_capacity(stmts.len());
            for s in stmts.iter_mut().chain(tail.iter_mut()) {
                let is_decl = matches!(&s.item.kind, TypedExprKind::Assign { value, .. }
                    if matches!(value.item.kind, TypedExprKind::Function { .. }));
                if is_decl {
                    // Already top level: it keeps its name and its place,
                    // so only its body is walked — but it still needs its
                    // captures snapshotted, since a call to it can sit in
                    // a scope that shadows one of them.
                    let span = s.span;
                    let TypedExprKind::Assign { value, .. } = &mut s.item.kind else { unreachable!() };
                    let mut inner: Vec<HashMap<String, Option<String>>> = {
                        let TypedExprKind::Function { params, .. } = &value.item.kind else { unreachable!() };
                        vec![params.iter().map(|(n, _, _)| (n.clone(), None)).collect()]
                    };
                    let TypedExprKind::Function { body, .. } = &mut value.item.kind else { unreachable!() };
                    self.hoist_expr(body, &mut inner, &mut hoisted, &mutated, &mut failed);
                    let TypedExprKind::Assign { value, .. } = &mut s.item.kind else { unreachable!() };
                    if let Err(e) = self.snapshot_captures(value, &mutated, span, &mut rebuilt) {
                        failed = Some(e);
                    }
                } else {
                    self.hoist_expr(s, &mut scopes, &mut hoisted, &mutated, &mut failed);
                }
                rebuilt.push(std::mem::replace(s, Spanned::from(
                    TypedExpr { id: 0, ty: Type::None, kind: TypedExprKind::NoneLit }, s.span)));
            }
            // The tail was walked through the same loop, so it is the last
            // entry in `rebuilt`; take it back out.
            if tail.is_some() { tail = rebuilt.pop(); }
            stmts = rebuilt;
        }
        if let Some(e) = failed { return Err(e); }
        stmts.extend(hoisted);

        // ── 2. Specialization ────────────────────────────────────────────
        // Before captures, not after: a specialized clone that calls the
        // lambda it was specialized on is an ordinary caller of a
        // capturing function, so step 3's fixed point threads that
        // lambda's captures through it with no forwarding logic of its
        // own. The other order would need one.
        self.specialize_calls(&mut stmts, &mut tail);

        // ── 3. Captures ──────────────────────────────────────────────────
        self.resolve_captures(&mut stmts, &mut tail, &mutated)?;

        if let Some(t) = tail { stmts.push(t); }

        // ── 4. Whatever is left is tier 2 ────────────────────────────────
        for s in stmts.iter() { self.reject_function_values(s)?; }

        typed.item.ty = stmts.last().map(|s| s.item.ty.clone()).unwrap_or(Type::None);
        typed.item.kind = TypedExprKind::Block(stmts);
        Ok(())
    }

    /// Names written *through* somewhere in `expr`: a `PlaceAssign` root
    /// or a `mut` argument's root. Complements `mut_names`, which covers
    /// the declaration side; between them a captured name that anything
    /// can write is rejected. Deliberately scope-blind — see `mut_names`.
    fn collect_mutated_names(expr: &Spanned<TypedExpr>, out: &mut HashSet<String>) {
        match &expr.item.kind {
            TypedExprKind::PlaceAssign { place, .. } => { out.insert(place.root.clone()); },
            TypedExprKind::Call { args, .. } => {
                for a in args {
                    if let Arg::Mut(p) = a { out.insert(p.root.clone()); }
                }
            },
            _ => {},
        }
        for child in Self::children(expr) { Self::collect_mutated_names(child, out); }
    }

    /// Hoist every `Function` node reachable from `expr` to the top level,
    /// pushing each one onto `out` as an `Assign { name, value: Function }`
    /// statement and leaving a reference behind in its place.
    ///
    /// `scopes` is a lexical stack of *rewrites*: `Some(hoisted)` means a
    /// reference to this name now denotes the hoisted declaration,
    /// `None` means an inner binder has shadowed whatever the outer scope
    /// said. Tracking the shadowing is the whole reason this is a walk of
    /// its own rather than a `walk_vars_mut` callback — that one is
    /// scope-blind, which is fine for its own job (template symbols are
    /// unspellable, so nothing can shadow them) and wrong here, where the
    /// names being rewritten are ordinary user identifiers.
    ///
    /// A hoisted declaration's own name is registered *before* its body is
    /// walked, so a recursive call inside it resolves to the hoisted name
    /// too.
    fn hoist_expr(
        &mut self,
        expr: &mut Spanned<TypedExpr>,
        scopes: &mut Vec<HashMap<String, Option<String>>>,
        out: &mut Vec<Spanned<TypedExpr>>,
        mutated: &HashSet<String>,
        failed: &mut Option<Spanned<TypeError>>,
    ) {
        match &mut expr.item.kind {
            TypedExprKind::Var(name) => {
                if let Some(hoisted) = Self::scope_lookup(scopes, name) {
                    *name = hoisted;
                }
            },

            // Statement sequences are the only place a declaration can
            // appear, so they're the only place a rewrite is introduced.
            // Processed in order, since a name binds for the rest of the
            // block and not before it.
            TypedExprKind::Block(stmts) => {
                scopes.push(HashMap::new());
                // Rebuilt rather than mutated in place: a declaration can
                // expand into several statements (its capture snapshots)
                // or into none at all (it is hoisted away).
                let mut replaced: Vec<Spanned<TypedExpr>> = Vec::with_capacity(stmts.len());
                for s in stmts.iter_mut() {
                    let decl = match &s.item.kind {
                        TypedExprKind::Assign { name, value } if matches!(value.item.kind, TypedExprKind::Function { .. }) =>
                            Some(name.clone()),
                        _ => None,
                    };
                    match decl {
                        Some(name) => {
                            let hoisted = self.fresh_lifted_name(&name);
                            scopes.last_mut().expect("just pushed").insert(name, Some(hoisted.clone()));
                            let mut decl_node = std::mem::replace(s, Spanned::from(
                                TypedExpr { id: 0, ty: Type::None, kind: TypedExprKind::NoneLit }, s.span));
                            let TypedExprKind::Assign { name: decl_name, value } = &mut decl_node.item.kind else { unreachable!() };
                            *decl_name = hoisted;
                            self.hoist_function_value(value, scopes, out, mutated, failed);
                            // The snapshots stand where the declaration
                            // did, so they are evaluated at exactly the
                            // point the function value was created.
                            let mut snaps = Vec::new();
                            if let Err(e) = self.snapshot_captures(value, mutated, decl_node.span, &mut snaps) {
                                *failed = Some(e);
                            }
                            replaced.extend(snaps);
                            out.push(decl_node);
                        },
                        None => {
                            self.hoist_expr(s, scopes, out, mutated, failed);
                            // A non-function `let` shadows any hoisted
                            // name it reuses for the rest of the block.
                            if let TypedExprKind::Assign { name, .. } = &s.item.kind {
                                scopes.last_mut().expect("just pushed").insert(name.clone(), None);
                            }
                            replaced.push(std::mem::replace(s, Spanned::from(
                                TypedExpr { id: 0, ty: Type::None, kind: TypedExprKind::NoneLit }, s.span)));
                        },
                    }
                }
                scopes.pop();
                *stmts = replaced;
            },

            // A lambda in expression position — a call argument, most of
            // the time. It becomes a reference to its own hoisted
            // declaration, which is exactly the "statically known callee"
            // shape specialization then consumes.
            TypedExprKind::Function { .. } => {
                let hoisted = self.fresh_lifted_name("lambda");
                let ty = expr.item.ty.clone();
                let span = expr.span;
                let mut value = std::mem::replace(expr, Spanned::from(
                    TypedExpr { id: 0, ty: ty.clone(), kind: TypedExprKind::Var(hoisted.clone()) }, span));
                self.hoist_function_value(&mut value, scopes, out, mutated, failed);
                // No snapshot: a lambda literal is created *at* the
                // expression it stands in, and that is the only place the
                // hoisted name is ever referenced, so reading its captures
                // at the call site already reads them at creation time.
                if let Err(e) = self.check_captures_immutable(&value, mutated) { *failed = Some(e); }
                out.push(Spanned::from(TypedExpr {
                    id: 0,
                    ty,
                    kind: TypedExprKind::Assign { name: hoisted, value: Box::new(value) },
                }, span));
            },

            TypedExprKind::ForLoop { var, iterable, cond, body, iter_via }
            | TypedExprKind::Comprehension { var, iterable, cond, body, iter_via } => {
                // The iterable is evaluated *outside* the loop variable's
                // scope, so it is walked before the frame is pushed.
                self.hoist_expr(iterable, scopes, out, mutated, failed);
                let mut frame = HashMap::new();
                frame.insert(var.clone(), None);
                if let Some(iv) = iter_via.as_ref() { frame.insert(iv.iter_var.clone(), None); }
                scopes.push(frame);
                if let Some(c) = cond { self.hoist_expr(c, scopes, out, mutated, failed); }
                self.hoist_expr(body, scopes, out, mutated, failed);
                if let Some(iv) = iter_via { self.hoist_expr(&mut iv.next_call, scopes, out, mutated, failed); }
                scopes.pop();
            },

            _ => {
                for child in Self::children_mut(expr) {
                    self.hoist_expr(child, scopes, out, mutated, failed);
                }
            },
        }
    }

    /// Walk into a hoisted declaration's `Function` value: its parameters
    /// open a fresh scope, so nothing outside can be rewritten by a name
    /// the parameters shadow.
    fn hoist_function_value(
        &mut self,
        value: &mut Spanned<TypedExpr>,
        scopes: &mut Vec<HashMap<String, Option<String>>>,
        out: &mut Vec<Spanned<TypedExpr>>,
        mutated: &HashSet<String>,
        failed: &mut Option<Spanned<TypeError>>,
    ) {
        let TypedExprKind::Function { params, body, .. } = &mut value.item.kind else {
            unreachable!("hoist_function_value is only ever called on a Function value")
        };
        scopes.push(params.iter().map(|(n, _, _)| (n.clone(), None)).collect());
        self.hoist_expr(body, scopes, out, mutated, failed);
        scopes.pop();
    }

    /// The free value names of a `Function` value — the ones it captures.
    fn captured_names(&self, value: &Spanned<TypedExpr>) -> Vec<(String, Type)> {
        let TypedExprKind::Function { params, body, .. } = &value.item.kind else {
            unreachable!("captured_names is only ever called on a Function value")
        };
        let mut bound: Vec<HashSet<String>> = vec![params.iter().map(|(n, _, _)| n.clone()).collect()];
        let mut free = Vec::new();
        // An empty `known`: this is about a body's *own* free names, and a
        // capture that arrives by calling something else is already an
        // unspellable synthetic name that needs no snapshot.
        self.collect_free_vars(body, &mut bound, &HashMap::new(), &mut free);
        free.sort_by(|a, b| a.0.cmp(&b.0));
        free
    }

    /// Capture is by value, so a captured binding must be immutable —
    /// which is also what makes lifting one into a parameter
    /// semantics-preserving without any escape analysis.
    fn check_captures_immutable(&self, value: &Spanned<TypedExpr>, mutated: &HashSet<String>) -> Result<(), Spanned<TypeError>> {
        for (n, _) in self.captured_names(value) {
            if mutated.contains(&n) {
                return Err(Spanned::from(TypeError {
                    msg: format!(
                        "closures capture by value; '{}' is a mut binding — copy it into a `let` first",
                        n,
                    ),
                }, value.span));
            }
        }
        Ok(())
    }

    /// Bind each of `value`'s captures to a fresh unspellable name and
    /// rewrite the body to read that instead, emitting the bindings onto
    /// `out` to stand where the declaration did.
    ///
    /// Without this the capture would be passed at each *call site* as a
    /// plain `Var(n)`, and a call site is free to shadow `n`:
    ///
    /// ```text
    /// let n = 1
    /// let f = x -> x + n
    /// let g = { let n = 1000; f(0) }   // f must still see 1
    /// ```
    ///
    /// Reading the snapshot at the declaration instead is both the fix for
    /// that and the definition of by-value capture: the lambda sees its
    /// captures as of where it was created, not where it is called.
    fn snapshot_captures(
        &mut self,
        value: &mut Spanned<TypedExpr>,
        mutated: &HashSet<String>,
        span: Span,
        out: &mut Vec<Spanned<TypedExpr>>,
    ) -> Result<(), Spanned<TypeError>> {
        self.check_captures_immutable(value, mutated)?;
        let caps = self.captured_names(value);
        if caps.is_empty() { return Ok(()); }

        let mut renames: HashMap<String, String> = HashMap::new();
        for (n, ty) in &caps {
            let snap = format!("{}$snap{}", n, self.next_id);
            self.next_id += 1;
            out.push(Spanned::from(TypedExpr {
                id: 0,
                ty: ty.clone(),
                kind: TypedExprKind::Assign {
                    name: snap.clone(),
                    value: Box::new(Spanned::from(TypedExpr {
                        id: 0, ty: ty.clone(), kind: TypedExprKind::Var(n.clone()),
                    }, span)),
                },
            }, span));
            renames.insert(n.clone(), snap);
        }

        let TypedExprKind::Function { params, body, .. } = &mut value.item.kind else { unreachable!() };
        let mut bound: Vec<HashSet<String>> = vec![params.iter().map(|(n, _, _)| n.clone()).collect()];
        Self::rename_free_vars(body, &mut bound, &renames);
        Ok(())
    }

    /// The innermost rewrite for `name`, or `None` if it isn't rewritten
    /// (never was, or an inner binder shadowed it).
    fn scope_lookup(scopes: &[HashMap<String, Option<String>>], name: &str) -> Option<String> {
        scopes.iter().rev().find_map(|frame| frame.get(name)).cloned().flatten()
    }

    /// A top-level symbol for a hoisted declaration. `$` keeps it in the
    /// same unspellable namespace `monomorphize_generics`' mangled names
    /// live in, and `next_id` (checkpointed) keeps it unique across REPL
    /// entries as well as within one.
    fn fresh_lifted_name(&mut self, base: &str) -> String {
        let name = format!("{}$lift{}", base, self.next_id);
        self.next_id += 1;
        name
    }

    /// Every direct subexpression of `expr`, in evaluation order — the
    /// read-only counterpart of `walk_vars_mut`'s traversal, factored out
    /// so the several walks this pass needs don't each restate the node
    /// inventory. `Function` bodies and `IterVia::next_call` are included:
    /// a walk that skipped them would miss exactly the nested cases this
    /// pass exists to find.
    fn children(expr: &Spanned<TypedExpr>) -> Vec<&Spanned<TypedExpr>> {
        let mut out: Vec<&Spanned<TypedExpr>> = Vec::new();
        match &expr.item.kind {
            TypedExprKind::IntLit(_) | TypedExprKind::FloatLit(_) | TypedExprKind::BoolLit(_)
            | TypedExprKind::StrLit(_) | TypedExprKind::NoneLit | TypedExprKind::Var(_) => {},
            TypedExprKind::Unary { expr: e, .. } => out.push(e),
            TypedExprKind::Binary { left, right, .. } => { out.push(left); out.push(right); },
            TypedExprKind::Conditional { cond, true_branch, false_branch } => {
                out.push(cond); out.push(true_branch);
                if let Some(fb) = false_branch { out.push(fb); }
            },
            TypedExprKind::Assign { value, .. } => out.push(value),
            TypedExprKind::Function { body, .. } => out.push(body),
            TypedExprKind::Call { callable, args } => {
                out.push(callable);
                for a in args { out.extend(a.subexprs()); }
            },
            TypedExprKind::Index { target, index } => { out.push(target); out.push(index); },
            TypedExprKind::Slice { target, start, end } => {
                out.push(target);
                if let Some(s) = start { out.push(s); }
                if let Some(e) = end { out.push(e); }
            },
            TypedExprKind::Range { start, end } => { out.push(start); out.push(end); },
            TypedExprKind::List(elems) => out.extend(elems.iter()),
            TypedExprKind::Block(stmts) => out.extend(stmts.iter()),
            TypedExprKind::ForLoop { iterable, cond, body, iter_via, .. }
            | TypedExprKind::Comprehension { iterable, cond, body, iter_via, .. } => {
                out.push(iterable);
                if let Some(c) = cond { out.push(c); }
                out.push(body);
                if let Some(iv) = iter_via { out.push(&iv.next_call); }
            },
            TypedExprKind::StructInit { fields, .. } | TypedExprKind::VariantInit { fields, .. } => {
                for (_, v) in fields { out.push(v); }
            },
            TypedExprKind::FieldAccess { target, .. } => out.push(target),
            TypedExprKind::PlaceAssign { place, value } => {
                out.extend(place.index_exprs());
                out.push(value);
            },
            TypedExprKind::IsVariant { target, .. } => out.push(target),
            TypedExprKind::VariantField { target, .. } => out.push(target),
            TypedExprKind::Return(v) => if let Some(v) = v { out.push(v) },
            TypedExprKind::Widen { value, .. } | TypedExprKind::Narrow { value, .. } => out.push(value),
            TypedExprKind::TypeTag { target, .. } => out.push(target),
            TypedExprKind::Truthy(v) | TypedExprKind::Coerce(v) => out.push(v),
        }
        out
    }

    /// Replace every call that passes a statically-known function with a
    /// call to a clone of the callee that has that function substituted
    /// in — so the parameter disappears and `f(x)` inside the body becomes
    /// an ordinary direct call to the function it was passed.
    ///
    /// The same transform `monomorphize_generics` performs, keyed on a
    /// name instead of a type, and it reuses that pass's structure: retain
    /// the declaration as a template, emit one clone per distinct
    /// instantiation under a mangled symbol, strip the un-substituted
    /// declaration (it can never be compiled — a function-typed parameter
    /// has no runtime representation), and run to a fixed point, since a
    /// clone's own body can contain the first specializable call to
    /// something else.
    ///
    /// Infallible: a call this can't specialize is simply left alone, and
    /// whatever function value is still standing afterwards is reported by
    /// `reject_function_values` with the context to say *why*.
    fn specialize_calls(&mut self, stmts: &mut Vec<Spanned<TypedExpr>>, tail: &mut Option<Spanned<TypedExpr>>) {
        // Both branches matter. Registering a higher-order declaration is
        // the point; *un*-registering one that this entry redeclares
        // without function parameters is what keeps a REPL honest — these
        // maps are keyed by name and persist, so a stale template would
        // otherwise strip the new declaration as if it were still the old
        // one. (`monomorphize_generics` sidesteps this by keying on a
        // unique symbol instead; a function's name is not its identity.)
        // `emitted_specializations` is keyed by *source* names too — the
        // callee's and each substituted function's — so a redeclaration of
        // either one has to evict every entry mentioning it, or a stale
        // clone from the old declaration keeps answering calls under the
        // new one (`func inc(n)=n+1` ... `func inc(n)=n+100` must not keep
        // `apply$$inc` pointing at the `+1` clone).
        let redeclared: std::collections::HashSet<&str> = stmts.iter().chain(tail.iter())
            .filter_map(|s| Self::as_fn_decl(s).map(|(n, _, _)| n))
            .collect();
        if !redeclared.is_empty() {
            self.emitted_specializations.retain(|mangled| {
                let (callee, args) = mangled.split_once("$$").unwrap_or((mangled.as_str(), ""));
                !redeclared.contains(callee) && !args.split('$').any(|a| redeclared.contains(a))
            });
        }

        for s in stmts.iter().chain(tail.iter()) {
            let Some((name, params, body)) = Self::as_fn_decl(s) else { continue };
            if !params.iter().any(|(_, t, _)| matches!(self.lookup(t), Type::Function { .. })) {
                self.fn_templates.remove(name);
                continue;
            }
            let TypedExprKind::Assign { value, .. } = &s.item.kind else { unreachable!("as_fn_decl matched") };
            let TypedExprKind::Function { return_type, .. } = &value.item.kind else { unreachable!("as_fn_decl matched") };
            self.fn_templates.insert(name.to_string(), FnTemplate {
                params: params.to_vec(),
                return_type: return_type.clone(),
                body: body.clone(),
                span: s.span,
            });
        }
        if self.fn_templates.is_empty() { return; }

        // Each round scans everything compiled so far — including the
        // clones the previous round emitted, which is what makes a
        // function parameter forwarded to a second higher-order function
        // resolve.
        let mut emitted: Vec<Spanned<TypedExpr>> = Vec::new();
        loop {
            let mut wanted: Vec<(String, Vec<(usize, String)>)> = Vec::new();
            for s in stmts.iter().chain(tail.iter()).chain(emitted.iter()) {
                // A template's own body is about to be stripped; the calls
                // that matter in it are the ones in its clones.
                if Self::as_fn_decl(s).is_some_and(|(n, _, _)| self.fn_templates.contains_key(n)) { continue }
                self.scan_specializations(s, &mut wanted);
            }
            let mut fresh = Vec::new();
            for (callee, subs) in wanted {
                let mangled = Self::specialized_name(&callee, &subs);
                // Already compiled — in an earlier round, or in an earlier
                // entry. Nothing to build, but the call sites here still
                // have to be pointed at it, which is why the rewrite below
                // runs unconditionally rather than only when something new
                // was emitted.
                if !self.emitted_specializations.insert(mangled.clone()) { continue }
                fresh.push(self.build_specialization(&callee, &subs, &mangled));
            }
            let done = fresh.is_empty();
            emitted.extend(fresh);
            for s in stmts.iter_mut().chain(tail.iter_mut()).chain(emitted.iter_mut()) {
                self.rewrite_specialized_calls(s);
            }
            if done { break }
        }
        stmts.extend(emitted);

        // An un-substituted higher-order declaration must never reach
        // codegen: `make_sig` has no Cranelift type for a function-typed
        // parameter. Its template survives in `fn_templates` for a later
        // entry to specialize, exactly as a generic's does.
        stmts.retain(|s| !Self::as_fn_decl(s).is_some_and(|(n, _, _)| self.fn_templates.contains_key(n)));
        if tail.as_ref().is_some_and(|t| Self::as_fn_decl(t).is_some_and(|(n, _, _)| self.fn_templates.contains_key(n))) {
            let span = tail.as_ref().expect("just checked").span;
            *tail = Some(Spanned::from(TypedExpr { id: 0, ty: Type::None, kind: TypedExprKind::NoneLit }, span));
        }
    }

    /// Collect every call in `expr` that can be specialized right now:
    /// the callee is a retained template, and every one of its
    /// function-typed parameters is given a plain name. A call that
    /// doesn't qualify (an argument that is itself an unresolved function
    /// parameter, say) is skipped — a later round, after the enclosing
    /// function is itself specialized, will see it resolved.
    fn scan_specializations(&self, expr: &Spanned<TypedExpr>, out: &mut Vec<(String, Vec<(usize, String)>)>) {
        if let TypedExprKind::Call { callable, args } = &expr.item.kind {
            if let TypedExprKind::Var(callee) = &callable.item.kind {
                if let Some(t) = self.fn_templates.get(callee.as_str()) {
                    let mut subs = Vec::new();
                    let mut complete = true;
                    for (i, (_, pty, _)) in t.params.iter().enumerate() {
                        if !matches!(self.lookup(pty), Type::Function { .. }) { continue }
                        match args.get(i).and_then(Arg::value).map(|a| &a.item.kind) {
                            Some(TypedExprKind::Var(g)) => subs.push((i, g.clone())),
                            _ => complete = false,
                        }
                    }
                    if complete && !subs.is_empty() {
                        let entry = (callee.clone(), subs);
                        if !out.contains(&entry) { out.push(entry); }
                    }
                }
            }
        }
        for child in Self::children(expr) { self.scan_specializations(child, out); }
    }

    /// `apply$$inc`. Deterministic in the program alone, so the emitted
    /// module is diffable run to run — the same property
    /// `monomorphize_generics` sorts its worklist to get.
    fn specialized_name(callee: &str, subs: &[(usize, String)]) -> String {
        let args = subs.iter().map(|(_, g)| g.as_str()).collect::<Vec<_>>().join("$");
        format!("{}$${}", callee, args)
    }

    /// Clone `callee`'s template with each function-typed parameter
    /// replaced by the name it was passed, and that parameter dropped from
    /// the signature.
    fn build_specialization(&mut self, callee: &str, subs: &[(usize, String)], mangled: &str) -> Spanned<TypedExpr> {
        let t = self.fn_templates.get(callee).cloned().expect("scan only proposes retained templates");
        let renames: HashMap<String, String> = subs.iter()
            .map(|(i, g)| (t.params[*i].0.clone(), g.clone()))
            .collect();

        let mut body = t.body.clone();
        let mut bound: Vec<HashSet<String>> = vec![
            t.params.iter().enumerate()
                .filter(|(i, _)| !subs.iter().any(|(si, _)| si == i))
                .map(|(_, (n, _, _))| n.clone())
                .collect(),
        ];
        Self::rename_free_vars(&mut body, &mut bound, &renames);

        let params: Vec<(String, Type, bool)> = t.params.iter().enumerate()
            .filter(|(i, _)| !subs.iter().any(|(si, _)| si == i))
            .map(|(_, p)| p.clone())
            .collect();
        let fn_ty = Type::Function {
            params: params.iter().map(|(_, t, _)| t.clone()).collect(),
            result: Box::new(t.return_type.clone()),
        };
        Spanned::from(TypedExpr {
            id: 0,
            ty: fn_ty.clone(),
            kind: TypedExprKind::Assign {
                name: mangled.to_string(),
                value: Box::new(Spanned::from(TypedExpr {
                    id: 0,
                    ty: fn_ty,
                    kind: TypedExprKind::Function { params, return_type: t.return_type.clone(), body: Box::new(body) },
                }, t.body.span)),
            },
        }, t.span)
    }

    /// Point every specializable call at the clone built for it and drop
    /// the function arguments, which the clone no longer takes. Idempotent:
    /// once rewritten, the callee is a specialization rather than a
    /// template, so a later round leaves it alone.
    fn rewrite_specialized_calls(&self, expr: &mut Spanned<TypedExpr>) {
        let mut subs: Vec<(usize, String)> = Vec::new();
        if let TypedExprKind::Call { callable, args } = &mut expr.item.kind {
            if let TypedExprKind::Var(callee) = &callable.item.kind {
                if let Some(t) = self.fn_templates.get(callee.as_str()) {
                    let mut complete = true;
                    for (i, (_, pty, _)) in t.params.iter().enumerate() {
                        if !matches!(self.lookup(pty), Type::Function { .. }) { continue }
                        match args.get(i).and_then(Arg::value).map(|a| &a.item.kind) {
                            Some(TypedExprKind::Var(g)) => subs.push((i, g.clone())),
                            _ => complete = false,
                        }
                    }
                    // Only point at a clone that exists. A call the
                    // *current* round has not built yet — the forwarded
                    // `inner(g, x)` inside a freshly-emitted `outer`
                    // clone, say — must keep naming its template so the
                    // next round's scan can still see it; rewriting it
                    // early renamed it to a symbol nothing would ever
                    // define. The cross-entry case reads the same way:
                    // `emitted_specializations` persists, so a clone an
                    // earlier entry compiled counts as existing.
                    let mangled = Self::specialized_name(callee, &subs);
                    if complete && !subs.is_empty() && self.emitted_specializations.contains(&mangled) {
                        let drop: HashSet<usize> = subs.iter().map(|(i, _)| *i).collect();
                        let mut i = 0;
                        args.retain(|_| { i += 1; !drop.contains(&(i - 1)) });
                        if let Type::Function { params, .. } = &mut callable.item.ty {
                            let mut i = 0;
                            params.retain(|_| { i += 1; !drop.contains(&(i - 1)) });
                        }
                        if let TypedExprKind::Var(name) = &mut callable.item.kind { *name = mangled; }
                    } else {
                        subs.clear();
                    }
                }
            }
        }
        for child in Self::children_mut(expr) { self.rewrite_specialized_calls(child); }
    }

    /// Report any function value still standing after the three
    /// transforms. Reaching here means its callee is not statically known
    /// — it escaped the scope that created it — which is the tier-2 case
    /// this pass deliberately does not implement.
    ///
    /// This replaces the blanket rejection
    /// `validate_codegen_constraints` used to make, and is run *after* the
    /// pass rather than before it precisely so it describes what is left
    /// rather than what was written.
    fn reject_function_values(&self, expr: &Spanned<TypedExpr>) -> Result<(), Spanned<TypeError>> {
        let escaped = |what: &str, span| Err(Spanned::from(TypeError {
            msg: format!(
                "this function value {} — froglang can only pass a function where the compiler \
                 can see which one is called, so it can't be stored, returned, or reassigned",
                what,
            ),
        }, span));

        match &expr.item.kind {
            // A declaration's own value is the one legal position.
            TypedExprKind::Assign { value, .. } if matches!(value.item.kind, TypedExprKind::Function { .. }) => {
                let TypedExprKind::Function { params, body, return_type } = &value.item.kind else { unreachable!() };
                if let Some((n, _, _)) = params.iter().find(|(_, t, _)| matches!(self.lookup(t), Type::Function { .. })) {
                    return Err(Spanned::from(TypeError {
                        msg: format!(
                            "parameter '{}' is a function, but this function is never called with one the \
                             compiler can name — pass a `func` or a lambda literal directly at the call site",
                            n,
                        ),
                    }, value.span));
                }
                if matches!(self.lookup(return_type), Type::Function { .. }) {
                    return escaped("is returned from a function", value.span);
                }
                self.reject_function_values(body)
            },

            // A bare name in callable position is the supported case, and
            // every other position is covered by the `Var` arm below —
            // `validate_codegen_constraints`' `Call` arm makes exactly the
            // same distinction, for the same reason.
            TypedExprKind::Call { callable, args } => {
                if !matches!(callable.item.kind, TypedExprKind::Var(_)) {
                    self.reject_function_values(callable)?;
                }
                for a in args.iter().flat_map(Arg::subexprs) { self.reject_function_values(a)?; }
                Ok(())
            },

            TypedExprKind::Var(name) if matches!(self.lookup(&expr.item.ty), Type::Function { .. }) => {
                // A hoisted lambda is named after nothing the user wrote
                // (`lambda$lift7`), so the message describes it instead —
                // the span already points at the literal.
                let subject = match name.contains("$lift") {
                    true => "this function literal is used as a value".to_string(),
                    false => format!("'{}' is a function used as a value here", Self::source_name(name)),
                };
                Err(Spanned::from(TypeError {
                    msg: format!(
                        "{} — froglang can only pass a function where the compiler can see which one \
                         is called, so it can't be stored, returned, or reassigned",
                        subject,
                    ),
                }, expr.span))
            },

            TypedExprKind::Function { .. } => escaped("has nowhere to be compiled to", expr.span),

            TypedExprKind::Return(Some(v)) if matches!(self.lookup(&v.item.ty), Type::Function { .. }) =>
                escaped("is returned from a function", v.span),

            _ => {
                for child in Self::children(expr) { self.reject_function_values(child)?; }
                Ok(())
            },
        }
    }

    /// Turn every function's free value names into trailing parameters,
    /// and make every call site pass them.
    ///
    /// Three steps, in this order for a reason:
    ///
    ///  1. compute each function's captures, to a fixed point. A call to a
    ///     function that captures `n` *uses* `n` at the call site, so
    ///     `collect_free_vars` treats it as an occurrence of `n` and the
    ///     ordinary free-variable machinery propagates the capture outward
    ///     — including the shadowing rules, which is why this is not a
    ///     separate graph walk over the call graph;
    ///  2. append `Var(n)` arguments at every call site;
    ///  3. append the parameters and rename the body's free occurrences of
    ///     `n` to the parameter's synthetic name.
    ///
    /// Step 2 before step 3 is what makes the transitive case fall out:
    /// the argument `Var(n)` step 2 inserts into a *capturing* function's
    /// body is itself a free occurrence of `n` there, so step 3 rewrites
    /// it to that function's own capture parameter without any special
    /// case for forwarding.
    fn resolve_captures(
        &mut self,
        stmts: &mut [Spanned<TypedExpr>],
        tail: &mut Option<Spanned<TypedExpr>>,
        mutated: &HashSet<String>,
    ) -> Result<(), Spanned<TypeError>> {
        // Seeded from earlier entries: a call here to a function declared
        // in an earlier REPL entry must pass whatever captures that entry's
        // lifting gave it.
        let mut captures: HashMap<String, Vec<(String, Type)>> = self.lifted_captures.clone();
        // ...but a function this entry *redeclares* starts over: the
        // captures are recomputed from the body below, and keeping the
        // previous declaration's would append phantom arguments at every
        // call site. Same name-is-not-identity hazard `specialize_calls`
        // notes.
        // Cleared from the persistent map too, not just this run's copy:
        // a redeclaration that captures nothing must leave nothing behind
        // for the *next* entry's call sites to pass.
        for s in stmts.iter().chain(tail.iter()) {
            let Some((name, _, _)) = Self::as_fn_decl(s) else { continue };
            captures.remove(name);
            self.lifted_captures.remove(name);
        }

        // ── 1. Captures, to a fixed point ────────────────────────────────
        loop {
            let mut changed = false;
            for s in stmts.iter().chain(tail.iter()) {
                let Some((name, params, body)) = Self::as_fn_decl(s) else { continue };
                let mut bound: Vec<HashSet<String>> = vec![params.iter().map(|(n, _, _)| n.clone()).collect()];
                let mut free: Vec<(String, Type)> = Vec::new();
                self.collect_free_vars(body, &mut bound, &captures, &mut free);
                if free.is_empty() { continue; }
                free.sort_by(|a, b| a.0.cmp(&b.0));
                let entry = captures.entry(name.to_string()).or_default();
                for (n, ty) in free {
                    if entry.iter().any(|(e, _)| *e == n) { continue; }
                    if mutated.contains(&n) {
                        return Err(Spanned::from(TypeError {
                            msg: format!(
                                "closures capture by value; '{}' is a mut binding — copy it into a `let` first",
                                n,
                            ),
                        }, s.span));
                    }
                    entry.push((n, ty));
                    changed = true;
                }
                entry.sort_by(|a, b| a.0.cmp(&b.0));
            }
            if !changed { break; }
        }

        captures.retain(|_, v| !v.is_empty());
        if captures.is_empty() { return Ok(()); }

        // ── 2. Pass the captures at every call site ──────────────────────
        for s in stmts.iter_mut().chain(tail.iter_mut()) {
            Self::append_capture_args(s, &captures);
        }

        // ── 3. Receive them as parameters ────────────────────────────────
        for s in stmts.iter_mut().chain(tail.iter_mut()) {
            let TypedExprKind::Assign { name, value } = &mut s.item.kind else { continue };
            let Some(caps) = captures.get(name.as_str()) else { continue };
            let name = name.clone();
            let TypedExprKind::Function { params, body, .. } = &mut value.item.kind else { continue };
            let mut bound: Vec<HashSet<String>> = vec![params.iter().map(|(n, _, _)| n.clone()).collect()];
            let renames: HashMap<String, String> =
                caps.iter().map(|(n, _)| (n.clone(), Self::capture_param(n))).collect();
            Self::rename_free_vars(body, &mut bound, &renames);
            for (n, ty) in caps {
                params.push((Self::capture_param(n), ty.clone(), false));
            }
            // The declaration's own type has to grow with its parameter
            // list: `codegen::compile_call` reads the callee's parameter
            // types off the *callable node's* type, not off the callee's
            // declaration, so leaving this stale silently mis-coerces
            // every captured argument.
            if let Type::Function { params: pt, .. } = &mut value.item.ty {
                pt.extend(caps.iter().map(|(_, t)| t.clone()));
            }
            s.item.ty = value.item.ty.clone();
            // No `func_mut_params` update: that map is consulted only by
            // `lower_call`, which has long since run — `monomorphize_generics`
            // registers nothing for its own mangled instantiations either,
            // for the same reason. Capture parameters are never `mut`
            // anyway, so a callee's extra return values are unchanged.
            self.lifted_captures.insert(name, caps.clone());
        }
        Ok(())
    }

    /// `(name, params, body)` if this statement is a function declaration.
    fn as_fn_decl(s: &Spanned<TypedExpr>) -> Option<FnDecl<'_>> {
        let TypedExprKind::Assign { name, value } = &s.item.kind else { return None };
        let TypedExprKind::Function { params, body, .. } = &value.item.kind else { return None };
        Some((name, params, body))
    }

    /// The parameter a capture of `name` arrives as. `$` makes it
    /// unspellable, so no user binding at any call site can shadow it —
    /// the same guarantee `Binding::symbol`'s `name#42` relies on.
    fn capture_param(name: &str) -> String { format!("{}$cap", name) }

    /// Collect the free *value* names of `expr` — names it reads that
    /// nothing in `bound` declares.
    ///
    /// Function-typed names are never free: after hoisting every function
    /// is a top-level declaration called by symbol, so a reference to one
    /// is not a value read at all. That single rule is also what keeps
    /// builtins (`print`, `panic`) and host functions out, since they are
    /// bound at `Type::Function` too.
    ///
    /// A call to a function in `known` counts as an occurrence of each of
    /// that function's captures, which is what propagates a capture out of
    /// the function that introduced it and into the ones that call it.
    fn collect_free_vars(
        &self,
        expr: &Spanned<TypedExpr>,
        bound: &mut Vec<HashSet<String>>,
        known: &HashMap<String, Vec<(String, Type)>>,
        out: &mut Vec<(String, Type)>,
    ) {
        let note = |name: &str, ty: Type, bound: &Vec<HashSet<String>>, out: &mut Vec<(String, Type)>| {
            if bound.iter().any(|f| f.contains(name)) { return; }
            if out.iter().any(|(n, _)| n == name) { return; }
            out.push((name.to_string(), ty));
        };

        match &expr.item.kind {
            TypedExprKind::Var(name) => {
                let ty = self.lookup(&expr.item.ty);
                if !matches!(ty, Type::Function { .. }) { note(name, ty, bound, out); }
            },

            TypedExprKind::Block(stmts) => {
                bound.push(HashSet::new());
                for s in stmts {
                    self.collect_free_vars(s, bound, known, out);
                    if let TypedExprKind::Assign { name, .. } = &s.item.kind {
                        bound.last_mut().expect("just pushed").insert(name.clone());
                    }
                }
                bound.pop();
            },

            TypedExprKind::ForLoop { var, iterable, cond, body, iter_via }
            | TypedExprKind::Comprehension { var, iterable, cond, body, iter_via } => {
                self.collect_free_vars(iterable, bound, known, out);
                let mut frame = HashSet::from([var.clone()]);
                if let Some(iv) = iter_via { frame.insert(iv.iter_var.clone()); }
                bound.push(frame);
                if let Some(c) = cond { self.collect_free_vars(c, bound, known, out); }
                self.collect_free_vars(body, bound, known, out);
                if let Some(iv) = iter_via { self.collect_free_vars(&iv.next_call, bound, known, out); }
                bound.pop();
            },

            // Post-hoist there are none of these left in a body, but the
            // fixed point re-runs over trees this pass has already
            // rewritten, so the arm has to be right rather than absent.
            TypedExprKind::Function { params, body, .. } => {
                bound.push(params.iter().map(|(n, _, _)| n.clone()).collect());
                self.collect_free_vars(body, bound, known, out);
                bound.pop();
            },

            // A `mut` argument reads *and writes* its root binding, so the
            // root is a use like any other — and one that
            // `collect_mutated_names` has already marked, so a capture of
            // it is rejected rather than silently passed by value.
            TypedExprKind::PlaceAssign { place, .. } => {
                let ty = self.lookup(&expr.item.ty);
                note(&place.root, ty, bound, out);
                for child in Self::children(expr) { self.collect_free_vars(child, bound, known, out); }
            },

            _ => {
                if let TypedExprKind::Call { callable, args } = &expr.item.kind {
                    if let TypedExprKind::Var(callee) = &callable.item.kind {
                        for (n, ty) in known.get(callee).into_iter().flatten() {
                            note(n, ty.clone(), bound, out);
                        }
                    }
                    for a in args {
                        if let Arg::Mut(p) = a {
                            let ty = self.lookup(&expr.item.ty);
                            note(&p.root, ty, bound, out);
                        }
                    }
                }
                for child in Self::children(expr) { self.collect_free_vars(child, bound, known, out); }
            },
        }
    }

    /// Append `Var(n)` arguments to every call of a capturing function.
    /// Scope-blind on purpose: the names inserted here are then resolved
    /// by `rename_free_vars`, which is not.
    fn append_capture_args(expr: &mut Spanned<TypedExpr>, captures: &HashMap<String, Vec<(String, Type)>>) {
        if let TypedExprKind::Call { callable, args } = &mut expr.item.kind {
            if let TypedExprKind::Var(callee) = &callable.item.kind {
                if let Some(caps) = captures.get(callee.as_str()) {
                    let span = expr.span;
                    for (n, ty) in caps {
                        args.push(Arg::Value(Spanned::from(TypedExpr {
                            id: 0, ty: ty.clone(), kind: TypedExprKind::Var(n.clone()),
                        }, span)));
                    }
                    // The callable's own type gained parameters too — see
                    // the matching comment in `resolve_captures`.
                    if let Type::Function { params, .. } = &mut callable.item.ty {
                        params.extend(caps.iter().map(|(_, t)| t.clone()));
                    }
                }
            }
        }
        for child in Self::children_mut(expr) { Self::append_capture_args(child, captures); }
    }

    /// Rename every *free* occurrence of a name per `map`. The scope
    /// tracking is the point: a `let n = ...` or a loop variable inside the
    /// body shadows the outer name, and those occurrences must be left
    /// alone.
    ///
    /// Two callers, both renaming a name that is free by construction:
    /// `resolve_captures` maps a captured name to its capture parameter,
    /// and `specialize_calls` maps a function-typed parameter to the
    /// function it was passed.
    fn rename_free_vars(
        expr: &mut Spanned<TypedExpr>,
        bound: &mut Vec<HashSet<String>>,
        map: &HashMap<String, String>,
    ) {
        match &mut expr.item.kind {
            TypedExprKind::Var(name) => {
                if bound.iter().any(|f| f.contains(name.as_str())) { return; }
                if let Some(to) = map.get(name.as_str()) { *name = to.clone(); }
            },

            TypedExprKind::Block(stmts) => {
                bound.push(HashSet::new());
                for s in stmts.iter_mut() {
                    Self::rename_free_vars(s, bound, map);
                    if let TypedExprKind::Assign { name, .. } = &s.item.kind {
                        bound.last_mut().expect("just pushed").insert(name.clone());
                    }
                }
                bound.pop();
            },

            TypedExprKind::ForLoop { var, iterable, cond, body, iter_via }
            | TypedExprKind::Comprehension { var, iterable, cond, body, iter_via } => {
                Self::rename_free_vars(iterable, bound, map);
                let mut frame = HashSet::from([var.clone()]);
                if let Some(iv) = iter_via.as_ref() { frame.insert(iv.iter_var.clone()); }
                bound.push(frame);
                if let Some(c) = cond { Self::rename_free_vars(c, bound, map); }
                Self::rename_free_vars(body, bound, map);
                if let Some(iv) = iter_via { Self::rename_free_vars(&mut iv.next_call, bound, map); }
                bound.pop();
            },

            TypedExprKind::Function { params, body, .. } => {
                bound.push(params.iter().map(|(n, _, _)| n.clone()).collect());
                Self::rename_free_vars(body, bound, map);
                bound.pop();
            },

            _ => {
                // A place's root is a name like any other. It can only be
                // a capture in the rejected `mut` case, but renaming it
                // here keeps the walk total rather than subtly partial.
                if let TypedExprKind::PlaceAssign { place, .. } = &mut expr.item.kind {
                    if !bound.iter().any(|f| f.contains(place.root.as_str())) {
                        if let Some(to) = map.get(place.root.as_str()) { place.root = to.clone(); }
                    }
                }
                if let TypedExprKind::Call { args, .. } = &mut expr.item.kind {
                    for a in args.iter_mut() {
                        let Arg::Mut(p) = a else { continue };
                        if bound.iter().any(|f| f.contains(p.root.as_str())) { continue }
                        if let Some(to) = map.get(p.root.as_str()) { p.root = to.clone(); }
                    }
                }
                for child in Self::children_mut(expr) { Self::rename_free_vars(child, bound, map); }
            },
        }
    }

    /// `children`, mutably. Kept as a separate inventory rather than
    /// generic over mutability because the borrow checker will not let one
    /// function return either — and because the two really are the same
    /// list, a divergence between them is a bug either walk would expose.
    fn children_mut(expr: &mut Spanned<TypedExpr>) -> Vec<&mut Spanned<TypedExpr>> {
        let mut out: Vec<&mut Spanned<TypedExpr>> = Vec::new();
        match &mut expr.item.kind {
            TypedExprKind::IntLit(_) | TypedExprKind::FloatLit(_) | TypedExprKind::BoolLit(_)
            | TypedExprKind::StrLit(_) | TypedExprKind::NoneLit | TypedExprKind::Var(_) => {},
            TypedExprKind::Unary { expr: e, .. } => out.push(e),
            TypedExprKind::Binary { left, right, .. } => { out.push(left); out.push(right); },
            TypedExprKind::Conditional { cond, true_branch, false_branch } => {
                out.push(cond); out.push(true_branch);
                if let Some(fb) = false_branch { out.push(fb); }
            },
            TypedExprKind::Assign { value, .. } => out.push(value),
            TypedExprKind::Function { body, .. } => out.push(body),
            TypedExprKind::Call { callable, args } => {
                out.push(callable);
                for a in args { out.extend(a.subexprs_mut()); }
            },
            TypedExprKind::Index { target, index } => { out.push(target); out.push(index); },
            TypedExprKind::Slice { target, start, end } => {
                out.push(target);
                if let Some(s) = start { out.push(s); }
                if let Some(e) = end { out.push(e); }
            },
            TypedExprKind::Range { start, end } => { out.push(start); out.push(end); },
            TypedExprKind::List(elems) => out.extend(elems.iter_mut()),
            TypedExprKind::Block(stmts) => out.extend(stmts.iter_mut()),
            TypedExprKind::ForLoop { iterable, cond, body, iter_via, .. }
            | TypedExprKind::Comprehension { iterable, cond, body, iter_via, .. } => {
                out.push(iterable);
                if let Some(c) = cond { out.push(c); }
                out.push(body);
                if let Some(iv) = iter_via { out.push(&mut iv.next_call); }
            },
            TypedExprKind::StructInit { fields, .. } | TypedExprKind::VariantInit { fields, .. } => {
                for (_, v) in fields { out.push(v); }
            },
            TypedExprKind::FieldAccess { target, .. } => out.push(target),
            TypedExprKind::PlaceAssign { place, value } => {
                out.extend(place.index_exprs_mut());
                out.push(value);
            },
            TypedExprKind::IsVariant { target, .. } => out.push(target),
            TypedExprKind::VariantField { target, .. } => out.push(target),
            TypedExprKind::Return(v) => if let Some(v) = v { out.push(v) },
            TypedExprKind::Widen { value, .. } | TypedExprKind::Narrow { value, .. } => out.push(value),
            TypedExprKind::TypeTag { target, .. } => out.push(target),
            TypedExprKind::Truthy(v) | TypedExprKind::Coerce(v) => out.push(v),
        }
        out
    }

    /// Walk `declared` and `concrete` in lockstep (same shape by
    /// construction — `concrete` is `lookup(instantiate(declared, ..))`
    /// resolved at some call site) and record, for every `TypeVar` in
    /// `declared` whose name is one of `binders`, the type standing in the
    /// same position in `concrete`.
    fn zip_binder_types(declared: &Type, concrete: &Type, binders: &[(String, Vec<Trait>)], out: &mut HashMap<String, Type>) {
        if let Type::TypeVar { name, .. } = declared {
            if binders.iter().any(|(n, _)| n == name) {
                out.entry(name.clone()).or_insert_with(|| concrete.clone());
                return;
            }
        }
        match (declared, concrete) {
            (Type::Function { params: p1, result: r1 }, Type::Function { params: p2, result: r2 }) => {
                for (a, b) in p1.iter().zip(p2.iter()) { Self::zip_binder_types(a, b, binders, out); }
                Self::zip_binder_types(r1, r2, binders, out);
            },
            (Type::Named { args: a1, .. }, Type::Named { args: a2, .. }) => {
                for (a, b) in a1.iter().zip(a2.iter()) { Self::zip_binder_types(a, b, binders, out); }
            },
            (Type::Union(v1), Type::Union(v2)) => {
                for (a, b) in v1.iter().zip(v2.iter()) { Self::zip_binder_types(a, b, binders, out); }
            },
            _ => {},
        }
    }

    /// `TRAITS.md` Stage 3b's substitution-aware sibling of what used to
    /// be `resolve_types_deep` (Stage 2's version, which only ever
    /// resolved through `self.lookup` — no local mapping existed yet
    /// because at most one instantiation was ever live). Rewrite
    /// `expr.item.ty`, recursively, to `lookup(expr.item.ty).substitute(mapping)`
    /// — and, for a `Function` node, its `params`/`return_type` too, since
    /// those live outside any node's own `.ty` field. Mutates in place
    /// rather than rebuilding, since every other field of every node is
    /// already correct; only the `Type`s themselves may be stale or still
    /// carry an unsubstituted binder `TypeVar`. Used only on a freshly
    /// deep-cloned instantiation body (`monomorphize_generics`) — `mapping`
    /// is that one instantiation's `binder name -> concrete type` table,
    /// never the shared session-wide `substitutions` map, since two
    /// different live instantiations need two different mappings applied
    /// to two different clones of the same template.
    fn substitute_types_deep(&self, expr: &mut Spanned<TypedExpr>, mapping: &HashMap<String, Type>) {
        expr.item.ty = self.lookup(&expr.item.ty).substitute(mapping);
        match &mut expr.item.kind {
            TypedExprKind::IntLit(_) | TypedExprKind::FloatLit(_) | TypedExprKind::BoolLit(_)
            | TypedExprKind::StrLit(_) | TypedExprKind::NoneLit | TypedExprKind::Var(_) => {},

            TypedExprKind::Unary { expr: inner, .. } => self.substitute_types_deep(inner, mapping),

            TypedExprKind::Binary { left, right, .. } => {
                self.substitute_types_deep(left, mapping);
                self.substitute_types_deep(right, mapping);
            },

            TypedExprKind::Conditional { cond, true_branch, false_branch } => {
                self.substitute_types_deep(cond, mapping);
                self.substitute_types_deep(true_branch, mapping);
                if let Some(fb) = false_branch { self.substitute_types_deep(fb, mapping); }
            },

            TypedExprKind::Assign { value, .. } => self.substitute_types_deep(value, mapping),

            TypedExprKind::Function { params, return_type, body } => {
                for (_, ty, _) in params.iter_mut() { *ty = self.lookup(ty).substitute(mapping); }
                *return_type = self.lookup(return_type).substitute(mapping);
                self.substitute_types_deep(body, mapping);
            },

            TypedExprKind::Call { callable, args, .. } => {
                self.substitute_types_deep(callable, mapping);
                for a in args.iter_mut().flat_map(Arg::subexprs_mut) { self.substitute_types_deep(a, mapping); }
            },

            TypedExprKind::Index { target, index } => {
                self.substitute_types_deep(target, mapping);
                self.substitute_types_deep(index, mapping);
            },

            TypedExprKind::Slice { target, start, end } => {
                self.substitute_types_deep(target, mapping);
                if let Some(s) = start { self.substitute_types_deep(s, mapping); }
                if let Some(e) = end { self.substitute_types_deep(e, mapping); }
            },

            TypedExprKind::Range { start, end } => {
                self.substitute_types_deep(start, mapping);
                self.substitute_types_deep(end, mapping);
            },

            TypedExprKind::List(elems) => {
                for e in elems.iter_mut() { self.substitute_types_deep(e, mapping); }
            },

            TypedExprKind::Block(stmts) => {
                for s in stmts.iter_mut() { self.substitute_types_deep(s, mapping); }
            },

            TypedExprKind::ForLoop { iterable, cond, body, .. } => {
                self.substitute_types_deep(iterable, mapping);
                if let Some(c) = cond { self.substitute_types_deep(c, mapping); }
                self.substitute_types_deep(body, mapping);
            },

            TypedExprKind::Comprehension { iterable, cond, body, .. } => {
                self.substitute_types_deep(iterable, mapping);
                if let Some(c) = cond { self.substitute_types_deep(c, mapping); }
                self.substitute_types_deep(body, mapping);
            },

            TypedExprKind::StructInit { fields, .. } => {
                for (_, v) in fields.iter_mut() { self.substitute_types_deep(v, mapping); }
            },

            TypedExprKind::FieldAccess { target, .. } => self.substitute_types_deep(target, mapping),

            TypedExprKind::PlaceAssign { place, value } => {
                for seg in place.path.iter_mut() {
                    if let PlaceSeg::Index { index, elem_ty } = seg {
                        self.substitute_types_deep(index, mapping);
                        *elem_ty = self.lookup(elem_ty).substitute(mapping);
                    }
                }
                self.substitute_types_deep(value, mapping);
            },

            TypedExprKind::VariantInit { fields, .. } => {
                for (_, v) in fields.iter_mut() { self.substitute_types_deep(v, mapping); }
            },

            TypedExprKind::IsVariant { target, .. } => self.substitute_types_deep(target, mapping),
            TypedExprKind::VariantField { target, .. } => self.substitute_types_deep(target, mapping),

            TypedExprKind::Return(value) => {
                if let Some(v) = value { self.substitute_types_deep(v, mapping); }
            },

            TypedExprKind::Widen { value, .. } => self.substitute_types_deep(value, mapping),
            TypedExprKind::Narrow { value, .. } => self.substitute_types_deep(value, mapping),
            TypedExprKind::TypeTag { target, .. } => self.substitute_types_deep(target, mapping),
            TypedExprKind::Truthy(value) => self.substitute_types_deep(value, mapping),
            TypedExprKind::Coerce(value) => self.substitute_types_deep(value, mapping),
        }
    }

    /// Statically predict whether `codegen::print_union` would recurse into
    /// itself while printing a value of type `ty` — mirrors its own guard
    /// (`ctx.printing_unions`, keyed by the union's member list) exactly,
    /// but at typeck time so the user gets a spanned type error instead of
    /// a codegen panic. A genuinely recursive union (e.g. `Add(lhs: Node,
    /// rhs: Node)` where `Node` is itself that union) would need unbounded
    /// branch trees at codegen time to print.
    fn check_printable(&self, ty: &Type, span: Span) -> Result<(), Spanned<TypeError>> {
        self.check_no_recursive_union(ty, span, "printing", "formats it field-by-field")
    }

    /// The same prediction for `codegen::eq_union` (guard: `ctx.comparing_
    /// unions`), which dispatches on both operands' tags and then compares
    /// that member's payload — recursing through a struct member's fields
    /// exactly as printing does, and hitting exactly the same wall on a
    /// recursive union.
    ///
    /// This is why `type_implements` can keep saying a recursive union *is*
    /// `Eq`: it is, structurally — what's missing is a way to emit the
    /// comparison, which is a codegen limit and deserves to be reported as
    /// one, at the operator.
    fn check_comparable(&self, ty: &Type, span: Span) -> Result<(), Spanned<TypeError>> {
        self.check_no_recursive_union(ty, span, "comparing", "compares it field-by-field")
    }

    /// The field list codegen will actually walk for `ty`, with a generic
    /// struct's type arguments already substituted in — the read-only
    /// counterpart of `materialize_struct`, for callers that only have
    /// `&self`. The registered concrete layout is preferred when it
    /// exists (`materialize_struct` may already have cached it); otherwise
    /// the bare-name template's binder `TypeVar`s are substituted
    /// positionally from `args`, the same way `materialize_struct` does it.
    /// Returning the *uninstantiated* template instead would hide any cycle
    /// that only closes through a type argument, since the template's field
    /// types are still binders at that point.
    fn substituted_struct_fields(&self, ty: &Type) -> Option<Vec<(String, Type)>> {
        let Type::Named { name, args } = ty else { return None };
        ty.as_struct_name()?;
        if let Some(fields) = self.struct_defs.get(ty) {
            return Some(fields.clone());
        }
        let template = self.struct_templates.get(name)?;
        if args.is_empty() { return Some(template.clone()) }
        let binder_names = self.struct_type_params.get(name).cloned().unwrap_or_default();
        let mapping: HashMap<String, Type> = binder_names.into_iter().zip(args.iter().cloned()).collect();
        Some(template.iter().map(|(f, fty)| (f.clone(), fty.substitute(&mapping))).collect())
    }

    /// Shared body of `check_printable`/`check_comparable`: walk `ty` the way
    /// the corresponding codegen walk does — through a list's static element
    /// type, through a struct's fields, through a union's members — and fail
    /// if it reaches a type it is already inside.
    ///
    /// Both codegen walks are *monomorphizing*: each step emits the code for
    /// one statically known type, so a type that encloses itself would need
    /// an unbounded amount of code. A cycle can only close through something
    /// boxed — a union member, or (since `plans/DATA.md` stage 0 taught the
    /// walks to descend into a list's element type) a `List` field, which is
    /// how `data Tree(v: Int, kids: List<Tree>)` closes one. Both are
    /// rejected here, with a span, rather than by overflowing the compiler's
    /// own stack.
    fn check_no_recursive_union(&self, ty: &Type, span: Span, verb: &str, advice: &str) -> Result<(), Spanned<TypeError>> {
        fn walk(
            ty: &Type, tc: &TypeChecker, seen: &mut Vec<Type>,
            span: Span, verb: &str, advice: &str,
        ) -> Result<(), Spanned<TypeError>> {
            if let Some(elem) = ty.as_list_elem() {
                return walk(elem, tc, seen, span, verb, advice);
            }
            // Only composite types can close a cycle, and only they are
            // worth naming in the error. `seen` is a DFS *stack*, popped on
            // the way out, so two sibling fields of the same struct type are
            // not a cycle — only re-entering a type still being walked is.
            let composite = matches!(ty, Type::Union(_)) || ty.as_struct_name().is_some();
            if composite {
                if seen.contains(ty) {
                    return Err(Spanned::from(TypeError {
                        msg: format!(
                            "unsupported: {} a recursive type ({}) isn't supported yet \
                             — write a recursive function that {} instead.",
                            verb, ty, advice,
                        )
                    }, span));
                }
                seen.push(ty.clone());
            }
            let result = match ty {
                Type::Union(members) => members.iter()
                    .try_for_each(|m| walk(m, tc, seen, span, verb, advice)),
                _ => match tc.substituted_struct_fields(ty) {
                    Some(fields) => fields.iter()
                        .try_for_each(|(_, fty)| walk(fty, tc, seen, span, verb, advice)),
                    None => Ok(()),
                },
            };
            if composite { seen.pop(); }
            result
        }
        walk(ty, self, &mut Vec::new(), span, verb, advice)
    }

    /// Type-check and lower an untyped `Spanned<Expression>` into a
    /// `Spanned<TypedExpr>`, consuming the source node by move. This is the
    /// *synthesis* half of the bidirectional pair; `lower_expected` is the
    /// checking half, for the sites that have an expected type to impose.
    ///
    /// **One pass.** Every arm lowers its children first and then computes
    /// its own type from theirs — it never asks a separate inference pass
    /// what a subtree's type is. That's what keeps the walk linear (a
    /// leading `let ty = self.infer(&expr)?` here used to re-walk the whole
    /// subtree at every level, making the cost O(n · depth)), and it's also
    /// what keeps each construct's type rule stated exactly once. When
    /// there were two passes, every non-trivial construct had a paired
    /// `infer_x`/`lower_x` that could — and did — drift apart, producing a
    /// typed AST whose `.ty` disagreed with the node's real value shape.
    ///
    /// Consequently the checking a node needs lives in its arm, not in a
    /// separate validator: `Index` checks its target is a `List` here,
    /// `lower_match` checks its patterns and exhaustiveness, and so on.
    /// Anything downstream that says "already validated" means validated
    /// earlier *in this same walk*.
    ///
    /// Every node in the output carries a resolved `Type` — resolved as of
    /// the moment it was built, which is why decisions taken later (in an
    /// enclosing arm) read a child's type back through `lookup` rather than
    /// trusting the stored copy: a `TypeVar` in it may have been bound
    /// since, by a sibling.
    ///
    /// A `Block` encountered here is always a *nested* block (an `if`
    /// branch, a function body, an explicit `{ ... }` subexpression) and is
    /// therefore scoped — see `check_and_lower_entry` for the top-level entry
    /// point, which is not.
    pub fn check_and_lower(
        &mut self,
        expr: Spanned<Expression>,
    ) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let span = expr.span;

        match expr.item {
            Expression::Literal(lit)         => self.lower_literal(lit, span),
            Expression::Interp(parts)        => self.lower_interp(parts, span),
            Expression::Unary(u)             => self.lower_unary(u, span),
            Expression::Binary(b)            => self.lower_binary(b, span),
            Expression::Conditional(c)       => self.lower_conditional(c, span),
            Expression::Assign(a)            => self.lower_assign(a, span),
            Expression::Function(f)          => self.lower_function(f, span),
            Expression::Call(c)              => self.lower_call(c, span),
            Expression::Tuple(elems)         => self.lower_tuple(elems, span),
            Expression::Block(stmts)         => self.lower_block(stmts, span),
            Expression::Annotated(a)         => self.lower_annotated(a, span),
            Expression::Index(idx)           => self.lower_index(idx, span),
            Expression::Slice(s)             => self.lower_slice(s, span),
            Expression::Range(r)             => self.lower_range(r, span),
            Expression::ForLoop(fl)          => self.lower_for_loop_expr(fl, span),
            Expression::Comprehension(inner) => self.lower_comprehension(*inner, span),
            // Handled entirely by `hoist_data_decls` — never reaches codegen.
            Expression::DataDecl(_) => Ok(Spanned::from(TypedExpr { id: 0, ty: Type::None, kind: TypedExprKind::IntLit(0) }, span)),
            // Handled entirely by `hoist_annotation_decls` — never reaches codegen.
            Expression::AnnotationDecl(_) => Ok(Spanned::from(TypedExpr { id: 0, ty: Type::None, kind: TypedExprKind::IntLit(0) }, span)),
            // Always stripped by `strip_and_validate_annotations` (run in
            // `check_and_lower_entry`/`lower_block`, before any statement
            // reaches this match) — never actually seen here.
            Expression::Decorated(_) => unreachable!(
                "Expression::Decorated is stripped by TypeChecker::strip_and_validate_annotations before any statement is lowered"
            ),
            // Likewise handled entirely by `hoist_trait_names`/
            // `hoist_trait_members`. A trait declaration is a fact about the
            // type system; it produces no value and emits no code.
            Expression::TraitDecl(_) => Ok(Spanned::from(TypedExpr { id: 0, ty: Type::None, kind: TypedExprKind::IntLit(0) }, span)),
            Expression::ImplDecl(_) => unreachable!(
                "Expression::ImplDecl is replaced by its member declarations in TypeChecker::expand_impls, which runs before any statement is lowered"
            ),
            Expression::FieldAccess(fa)      => self.lower_field_access(fa, span),
            Expression::Match(m)             => self.lower_match(*m.subject, m.arms, m.default, span),
            Expression::IsPattern(ip)        => self.lower_is_pattern(ip, span),
            Expression::Import(_) => unreachable!(
                "Expression::Import must be resolved and stripped by frontend::modules before typeck ever sees it"
            ),
            Expression::Return(value)        => self.lower_return(value, span),
            Expression::Try(inner)           => self.lower_try(*inner, span),
            Expression::Unwrap(inner)        => self.lower_unwrap(*inner, span),
            Expression::Catch { value, handler } => self.lower_catch(*value, *handler, span),
            // `mut name` is only meaningful as one of `lower_call`'s own
            // arguments, which pattern-matches it before this dispatch is
            // ever reached (mirroring how `Person(name="Alice")`'s
            // `Assign`-shaped arguments are consumed by `lower_call`
            // before it falls through to the ordinary call path). Any
            // other position — `let x = mut y`, `1 + mut y`, a bare
            // statement — reaches here and is rejected.
            Expression::MutArg(_) => Err(Spanned::from(TypeError {
                msg: "'mut' may only mark an argument at a call site, e.g. f(mut x)".to_string()
            }, span)),
        }
    }

    /// A checked notation placeholder — shared by an explicit `repr(...)`
    /// call and by string interpolation, which must format a value exactly
    /// as `repr` does (bar `Str`) or the two notations drift.
    ///
    /// A placeholder, not the finished node: `desugar_notation` (run from
    /// `FrogState::eval_with_base`/`codegen::compile_and_run`, after
    /// monomorphization) replaces every one of these with the actual
    /// per-type expansion, once every type in the tree is fully substituted
    /// — see its own doc comment for why that ordering matters. The
    /// `callable`'s `Function` type carries no `func_ids` entry (nothing
    /// ever looks "repr" up there): the node never reaches codegen under
    /// this name.
    fn notation_placeholder(
        &mut self,
        notation: Notation,
        arg: Spanned<TypedExpr>,
        callee_span: Span,
    ) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let span = arg.span;
        let arg_ty = self.lookup(&arg.item.ty);
        // A bare (still-generic) `TypeVar` defers both checks to
        // `desugar_notation` time, once monomorphization has produced
        // a concretely-typed clone of this call site to check instead
        // — the same "don't reject at the unresolved binder, check
        // each instantiation" rule `join_operand_types` already
        // applies to every binary operator's own trait bound
        // (`Trait::Num`/`Trait::Eq`/`Trait::Ord`). Without this,
        // `func show<T>(x: T): Str = repr(x)` could never type-check
        // for *any* concrete `T`, since `Show` isn't yet inferable as
        // a bound the way `<T: Num>`/`<T: Eq>` are.
        if !matches!(arg_ty, Type::TypeVar { .. }) {
            if !self.type_implements(&arg_ty, &Trait::Show) {
                return Err(Spanned::from(TypeError { msg: notation.needs_show(&arg_ty) }, arg.span));
            }
            // Same prediction `check_printable` makes for `print`, at
            // the same span — see `is_repr`'s own comment on why this
            // happens here rather than in `validate_codegen_constraints`.
            self.check_no_recursive_union(&arg_ty, arg.span, "repr", "formats it field-by-field")?;
        }
        let callable = Spanned::from(TypedExpr {
            id: 0,
            ty: Type::Function { params: vec![arg_ty.clone()], result: Box::new(Type::Str) },
            kind: TypedExprKind::Var(notation.callee().to_string()),
        }, callee_span);
        Ok(Spanned::from(TypedExpr {
            id: 0,
            ty: Type::Str,
            kind: TypedExprKind::Call { callable: Box::new(callable), args: vec![Arg::Value(arg)] },
        }, span))
    }

    /// `"a ${x} b"` (`Expression::Interp`) — each piece converted to text,
    /// then concatenated.
    ///
    /// The conversion rule is `print`'s, not `repr`'s: a `Str` piece goes in
    /// **raw**, so `"hi ${name}"` is `hi Bob` rather than `hi "Bob"`, and
    /// every other type is formatted exactly as `repr` formats it. That
    /// reuse is the whole point — a struct interpolates as it prints, `Show`
    /// is required at the same span, and a string *nested* inside a list is
    /// still quoted, because that is `repr`'s business rather than
    /// interpolation's.
    ///
    /// Which of those two a piece gets is decided by `desugar_notation`, not
    /// here: inside a generic function body the piece's type is still an
    /// unresolved `TypeVar` at this point, so deciding here would quote
    /// `show("hi")` and not `show(1)` from the one body. See `Notation`.
    ///
    /// Concatenation is a left fold of `Str + Str` — the same nodes the
    /// surface syntax would have produced — so codegen learns nothing new,
    /// at the cost of one intermediate string per piece. Worth revisiting
    /// with an n-ary runtime concat if interpolation lands in a hot loop.
    fn lower_interp(&mut self, parts: Vec<Spanned<Expression>>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let mut acc: Option<Spanned<TypedExpr>> = None;
        for part in parts {
            let part_span = part.span;
            // The literal runs between the interpolations are already
            // `Str` and already themselves — only a `${...}` needs
            // converting (`Grammar::interp_string` builds both).
            let literal_text = matches!(&part.item, Expression::Literal(LiteralExpr { token: Token::String(_) }));
            let lowered = self.check_and_lower(part)?;
            let piece = match literal_text {
                true  => lowered,
                false => self.notation_placeholder(Notation::Interp, lowered, part_span)?,
            };
            acc = Some(match acc {
                None => piece,
                Some(prev) => Spanned::from(TypedExpr {
                    id: 0,
                    ty: Type::Str,
                    kind: TypedExprKind::Binary { op: Token::Plus, left: Box::new(prev), right: Box::new(piece) },
                }, span),
            });
        }
        // Only reachable for a literal with no pieces at all, which the
        // lexer spells as a plain `Token::String` — kept total anyway.
        Ok(acc.unwrap_or_else(|| Spanned::from(TypedExpr { id: 0, ty: Type::Str, kind: TypedExprKind::StrLit(String::new()) }, span)))
    }

    fn lower_literal(&mut self, lit: LiteralExpr, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let (kind, ty) = match lit.token {
            Token::Int(n)         => (TypedExprKind::IntLit(n),    Type::Int),
            Token::Float(f)       => (TypedExprKind::FloatLit(f),  Type::Float),
            Token::String(s)      => (TypedExprKind::StrLit(s),    Type::Str),
            Token::True           => (TypedExprKind::BoolLit(true),  Type::Bool),
            Token::False          => (TypedExprKind::BoolLit(false), Type::Bool),
            Token::None           => (TypedExprKind::NoneLit,      Type::None),
            Token::Inf            => (TypedExprKind::FloatLit(f64::INFINITY), Type::Float),
            Token::Nan            => (TypedExprKind::FloatLit(f64::NAN),      Type::Float),
            // A bound value takes priority; otherwise this might be a
            // nullary enum variant used without call syntax (`Red` for
            // `data Color is Red | ...`) — see `infer_bare_variant`.
            Token::Identifier(nm) => match self.ctx.get(&nm).cloned() {
                Some(bound) => {
                    // Instantiate the scheme (`TRAITS.md` Stage 2) before
                    // resolving substitutions — a generalized binding's
                    // binders are frozen quantifiers, not live unification
                    // variables, so `lookup` must never see them until
                    // after they've been fresh-renamed for this call. For
                    // an ordinary monomorphic binding `binders` is empty
                    // and `instantiate` is a no-op clone, same as before.
                    let binders = self.ctx.binders(&nm).expect("just found by get").to_vec();
                    let instantiated = self.instantiate(&bound, &binders);
                    let ty = self.lookup(&instantiated);
                    // A reference to a generalized declaration is emitted
                    // under that declaration's own template symbol rather
                    // than the source name (`TRAITS.md` Stage 3b) — the
                    // scope stack has just told us *which* declaration
                    // this is, and nothing downstream can recover that
                    // from a bare name. `monomorphize_generics` rewrites
                    // the symbol again, to the mangled instantiation.
                    let name = self.ctx.symbol(&nm).map(str::to_string).unwrap_or(nm);
                    (TypedExprKind::Var(name), ty)
                },
                None => {
                    let ty = self.infer_bare_variant(&nm, span)?;
                    let enum_name = self.variant_owners.get(&nm).and_then(|owners| owners.first()).cloned()
                        .expect("infer_bare_variant already resolved this name to exactly one owning union");
                    let def = self.union_defs.get(&enum_name).expect("registered");
                    let tag = def.variant_index(&nm).expect("registered") as u32;
                    (TypedExprKind::VariantInit { enum_name, variant: nm, tag, fields: Vec::new() }, ty)
                },
            },
            _ => unreachable!("unexpected literal token"),
        };
        Ok(Spanned::from(TypedExpr { id: 0, ty, kind }, span))
    }

    fn lower_unary(&mut self, u: UnaryExpr, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let op = match u.op {
            Token::Minus => "<unaryminus>",
            Token::Not   => "not",
            _ => unreachable!("weird unary"),
        };
        let inner = self.check_and_lower(*u.expr)?;
        let ty = self.builtin_op_type(op, &[(inner.item.ty.clone(), inner.span)], span)?;
        let inner = if u.op == Token::Not { self.coerce_truthy(inner, span) } else { inner };
        Ok(Spanned::from(TypedExpr { id: 0, ty, kind: TypedExprKind::Unary { op: u.op, expr: Box::new(inner) } }, span))
    }

    fn lower_binary(&mut self, b: BinaryExpr, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let left  = self.check_and_lower(*b.left)?;
        let right = self.check_and_lower(*b.right)?;
        // Pulled out of the generic `builtin_op_type` dispatch below:
        // unlike every other operator, `in`'s trait-dispatch fallback
        // (`Container<Item>`, `RANGES.md` Stage 2) needs to change the
        // node's whole *shape* (a `Call` to `y.has(x)`, not a `Binary`),
        // not just its type — see `lower_in`.
        if b.op == Token::In {
            return self.lower_in(left, right, span);
        }
        let op = format!("{}", b.op);
        let ty = self.builtin_op_type(
            &op,
            &[(left.item.ty.clone(), left.span), (right.item.ty.clone(), right.span)],
            span,
        )?;
        let kind = if matches!(b.op, Token::And | Token::Or) {
            let left  = self.coerce_truthy(left, span);
            let right = self.coerce_truthy(right, span);
            TypedExprKind::Binary { op: b.op, left: Box::new(left), right: Box::new(right) }
        } else if matches!(b.op, Token::EqEq | Token::NotEq) {
            let left_ty  = self.lookup(&left.item.ty);
            let right_ty = self.lookup(&right.item.ty);
            // When either side is union-typed, both are widened into the
            // joined union exactly as `lower_conditional` widens its
            // branches — `codegen::eq_union` dispatches on a tag and reads
            // the union's full layout width from *both* operands, so a bare
            // member (`x == none`, `x == 1`) has to be boxed on the way in
            // or codegen reads slots the operand doesn't have.
            let (left, right, resolved) =
                if matches!(left_ty, Type::Union(_)) || matches!(right_ty, Type::Union(_)) {
                    let joined = self.join_types(&left_ty, &right_ty);
                    let left  = self.lower_widen(left,  &joined)?;
                    let right = self.lower_widen(right, &joined)?;
                    (left, right, joined)
                } else {
                    (left, right, left_ty)
                };
            // `codegen::eq_union`'s tag dispatch can't be emitted for a
            // union that encloses itself — the same wall `print_union` hits.
            // Reported here, at the operator, rather than as "not Eq".
            self.check_comparable(&resolved, span)?;
            if resolved.as_struct_name().is_some() {
                self.desugar_struct_eq(b.op, left, right, &resolved, span)
            } else {
                TypedExprKind::Binary { op: b.op, left: Box::new(left), right: Box::new(right) }
            }
        } else {
            TypedExprKind::Binary { op: b.op, left: Box::new(left), right: Box::new(right) }
        };
        Ok(Spanned::from(TypedExpr { id: 0, ty, kind }, span))
    }

    /// `x in y` — `y` decides the shape: `Str` requires `x: Str` (substring
    /// search), `List<T>`/`Range<T>` require `x: T` (element membership,
    /// O(1) for `Range` — see `codegen`'s `Token::In` handling), and —
    /// the trait-dispatch fallback (`RANGES.md` Stage 2) — any type
    /// providing `Container<Item>` desugars entirely into `y.has(x)`, an
    /// ordinary `Call` to the resolved symbol (`lower_container_has`).
    /// Pulled out of `builtin_op_type`'s generic dispatch (unlike every
    /// other operator) because that last case changes the node's shape,
    /// not just its type.
    fn lower_in(&mut self, left: Spanned<TypedExpr>, right: Spanned<TypedExpr>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let left_span = left.span;
        let right_span = right.span;
        let resolved_right = self.lookup(&right.item.ty);
        let bool_binary = |left: Spanned<TypedExpr>, right: Spanned<TypedExpr>| Spanned::from(
            TypedExpr { id: 0, ty: Type::Bool, kind: TypedExprKind::Binary { op: Token::In, left: Box::new(left), right: Box::new(right) } },
            span,
        );
        if resolved_right == Type::Str {
            let resolved_left = self.lookup(&left.item.ty);
            if resolved_left != Type::Str {
                return Err(Spanned::from(TypeError {
                    msg: format!("Operator 'in' on Str requires Str on the left side, got {}", resolved_left)
                }, left_span));
            }
            return Ok(bool_binary(left, right));
        }
        if let Some(elem) = resolved_right.as_range_elem().cloned() {
            if !self.unify(&left.item.ty, &elem) {
                let resolved_left = self.lookup(&left.item.ty);
                return Err(Spanned::from(TypeError {
                    msg: format!("Operator 'in' got incompatible types: expected {}, got {}", elem, resolved_left)
                }, left_span));
            }
            return Ok(bool_binary(left, right));
        }
        if let Some(elem) = resolved_right.as_list_elem().cloned() {
            if !self.unify(&left.item.ty, &elem) {
                let resolved_left = self.lookup(&left.item.ty);
                return Err(Spanned::from(TypeError {
                    msg: format!("Operator 'in' got incompatible types: expected {}, got {}", elem, resolved_left)
                }, left_span));
            }
            return Ok(bool_binary(left, right));
        }
        if let Some(call) = self.lower_container_has(&resolved_right, left, right, span)? {
            return Ok(call);
        }
        Err(Spanned::from(TypeError {
            msg: format!("Operator 'in' requires Str, List, Range, or a type providing Container on the right side, got {}", resolved_right)
        }, right_span))
    }

    /// `x in y` when `y`'s type provides `Container<Item>` — desugars to
    /// `y.has(x)`, the same way `IterVia`'s `next_call` desugars `for`.
    /// `Ok(None)` iff `y`'s type has no `Container` impl at all; the caller
    /// (`lower_in`) reports "operator 'in' requires..." itself.
    fn lower_container_has(&mut self, resolved_right: &Type, left: Spanned<TypedExpr>, right: Spanned<TypedExpr>, span: Span) -> Result<Option<Spanned<TypedExpr>>, Spanned<TypeError>> {
        let Some(type_key) = Self::grant_key(resolved_right) else { return Ok(None) };
        let Some((trait_name, symbol)) = self.member_index.get(&(type_key, "has".to_string())).cloned() else {
            return Ok(None);
        };
        if trait_name != "Container" {
            // Some other trait happens to declare a `has` member — not
            // this one's business.
            return Ok(None);
        }
        // See the identical situation in `lower_iter_via`: `member_index`
        // already knows about this impl, but its member's type isn't in
        // `ctx` until the (position-preserving) renamed declaration itself
        // gets lowered, which hasn't happened yet for a forward reference.
        let Some(has_ty) = self.ctx.get(&symbol).cloned() else {
            return Err(Spanned::from(TypeError {
                msg: format!(
                    "'{}' provides 'Container', but that impl is declared later in the program — move it before this use",
                    resolved_right
                )
            }, span));
        };
        let Type::Function { params, result } = has_ty else {
            unreachable!("a trait member's resolved type is always Function")
        };
        // `has(s: Self, x: Item)` — `params[1]` is `Item`, already resolved
        // to a concrete type at registration (`check_impl_member`); `left`
        // (`x`) is unified against it exactly like any ordinary call's
        // argument would be.
        let item_ty = params.get(1).cloned().unwrap_or(Type::Never);
        let left_span = left.span;
        if !self.unify(&left.item.ty, &item_ty) {
            let resolved_left = self.lookup(&left.item.ty);
            return Err(Spanned::from(TypeError {
                msg: format!("Operator 'in' got incompatible types: expected {}, got {}", item_ty, resolved_left)
            }, left_span));
        }
        let callable = Spanned::from(TypedExpr {
            id: 0,
            ty: Type::Function { params: params.clone(), result: result.clone() },
            kind: TypedExprKind::Var(symbol),
        }, span);
        let call = TypedExpr {
            id: 0,
            ty: *result,
            kind: TypedExprKind::Call { callable: Box::new(callable), args: vec![Arg::Value(right), Arg::Value(left)] },
        };
        Ok(Some(Spanned::from(call, span)))
    }

    fn lower_conditional(&mut self, c: ConditionalExpr, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        // `if subject is Pattern then A (else B)?` — a binding
        // pattern is only meaningful with a `then`-scope to bind
        // into, so this is intercepted here (before the pattern
        // ever reaches the generic `IsPattern` arm, which rejects
        // binds) and handled as sugar for a single-arm `match`,
        // which `lower_match` already knows how to desugar.
        if matches!(&c.cond.item, Expression::IsPattern(_)) {
            let ip = match c.cond.item {
                Expression::IsPattern(ip) => ip,
                _ => unreachable!(),
            };
            let arms = vec![crate::frontend::expression::MatchArm {
                pattern: ip.pattern,
                guard: None,
                body: c.true_branch,
            }];
            return self.lower_match(*ip.subject, arms, c.false_branch, span);
        }

        let cond = self.check_and_lower(*c.cond)?;
        self.check_condition(&cond.item.ty.clone(), cond.span)?;
        let cond = self.coerce_truthy(cond, span);

        // Each branch is its own scope, whether or not it's written
        // with `{ }` — `if c then let y = 5 else 0` must not leave
        // `y` bound afterward, any more than
        // `if c then { let y = 5 } else 0` does.
        let true_branch = self.in_scope(|t| t.check_and_lower(*c.true_branch))?;
        let false_branch = match c.false_branch {
            Some(fb) => Some(self.in_scope(|t| t.check_and_lower(*fb))?),
            None => None,
        };

        // `join_types` handles both "the branches agree" and
        // "they don't, so produce a union" — plus, critically, the
        // case where one branch is *already* union-typed and the
        // other a plain member of it (e.g. `if c then 999 else
        // <union-typed expr>`), which needs the same union
        // promotion, not just `unify`'s permissive-but-non-binding
        // compatibility check. Normalizing matters beyond tidiness:
        // it's what drops `Never` (a `return`ed branch) out of the
        // result entirely, so `if c then return 1 else 2` types as
        // plain `Int` rather than `Never | Int`.
        let true_ty  = true_branch.item.ty.clone();
        let false_ty = false_branch.as_ref().map(|b| b.item.ty.clone()).unwrap_or(Type::None);
        let result_ty = self.join_types(&true_ty, &false_ty);

        // Each branch is widened into the joined type, so a branch
        // that produced a bare member of a union result actually
        // gets boxed — see `lower_widen`.
        let true_branch = self.lower_widen(true_branch, &result_ty)?;
        let false_branch = match false_branch {
            Some(fb) => Some(Box::new(self.lower_widen(fb, &result_ty)?)),
            None => None,
        };
        Ok(Spanned::from(TypedExpr { id: 0,
            ty: result_ty,
            kind: TypedExprKind::Conditional {
                cond:         Box::new(cond),
                true_branch:  Box::new(true_branch),
                false_branch,
            },
        }, span))
    }

    /// One step of an assignment target's path, before type-checking —
    /// the untyped-`Expression` counterpart of `typed_ast::PlaceSeg`.
    /// `flatten_place` walks a target expression into a root name plus a
    /// list of these, root-to-leaf.
    /// `None` when the path bottoms out in something that isn't a bare
    /// identifier — impossible for an assignment target (`Grammar::assign`
    /// guarantees it) but reachable through UFCS, where the receiver of
    /// `xs.push(v)` is an arbitrary expression.
    fn flatten_place(target: Spanned<Expression>) -> Option<(String, Vec<RawPlaceSeg>)> {
        let mut segs = Vec::new();
        let mut cur = target;
        loop {
            match cur.item {
                Expression::FieldAccess(fa) => {
                    segs.push(RawPlaceSeg::Field(fa.field));
                    cur = *fa.target;
                },
                Expression::Index(idx) => {
                    segs.push(RawPlaceSeg::Index(*idx.index));
                    cur = *idx.target;
                },
                other => {
                    let name = other.get_identifier()?.to_string();
                    segs.reverse();
                    return Some((name, segs));
                },
            }
        }
    }

    /// Resolve an assignment/`push` target into a `Place` plus the type of
    /// the part it names. `target` is a `FieldAccess`, an `Index`, or a
    /// bare identifier; `flatten_place` reduces it to `root` plus a
    /// root-to-leaf path, which this walks segment by segment, tracking
    /// the current type exactly as `lower_field_access`/`lower_index` do
    /// for a *read* of the same path, and enforcing that `root` is mutable
    /// before touching anything.
    ///
    /// Shared by `lower_place_assign` and `finish_push` — the two in-place
    /// mutations — so they accept exactly the same paths. See
    /// `typed_ast::Place`.
    fn lower_place(
        &mut self,
        target: Spanned<Expression>,
        span: Span,
    ) -> Result<(Place, Type), Spanned<TypeError>> {
        let Some((root, raw_path)) = Self::flatten_place(target) else {
            return Err(Spanned::from(TypeError {
                msg: "a mutation target must be a mutable binding or a path into one, not an expression".to_string()
            }, span));
        };
        match self.ctx.is_mutable(&root) {
            None => return Err(Spanned::from(TypeError {
                msg: format!("'{}' is not declared", root)
            }, span)),
            Some(false) => return Err(Spanned::from(TypeError {
                msg: format!("'{}' is not mutable — declare it with 'mut {} = ...' to assign into it", root, root)
            }, span)),
            Some(true) => {},
        }
        let mut cur_ty = self.ctx.get(&root).expect("checked mutable above").clone();
        let mut path = Vec::with_capacity(raw_path.len());
        for seg in raw_path {
            match seg {
                RawPlaceSeg::Field(fname) => {
                    let resolved = self.lookup(&cur_ty);
                    if resolved.as_struct_name().is_none() {
                        return Err(Spanned::from(TypeError {
                            msg: format!("Can't assign field '{}' on {}, expected a struct", fname, resolved)
                        }, span));
                    }
                    let field_defs = self.materialize_struct(&resolved);
                    let field_ty = field_defs.iter().find(|(n, _)| n == &fname)
                        .map(|(_, t)| t.clone())
                        .ok_or_else(|| Spanned::from(TypeError {
                            msg: format!("Struct {} has no field '{}'", resolved, fname)
                        }, span))?;
                    path.push(PlaceSeg::Field(fname));
                    cur_ty = field_ty;
                },
                RawPlaceSeg::Index(idx_expr) => {
                    let resolved = self.lookup(&cur_ty);
                    let elem_ty = match resolved.as_list_elem() {
                        Some(inner) => inner.clone(),
                        None => return Err(Spanned::from(TypeError {
                            msg: format!("Can't index into {}, expected a List", resolved)
                        }, span)),
                    };
                    let idx_span = idx_expr.span;
                    let lowered_idx = self.check_and_lower(idx_expr)?;
                    if !self.unify(&lowered_idx.item.ty, &Type::Int) {
                        return Err(Spanned::from(TypeError {
                            msg: format!("List index must be Int, got {}", self.lookup(&lowered_idx.item.ty))
                        }, idx_span));
                    }
                    path.push(PlaceSeg::Index { index: Box::new(lowered_idx), elem_ty: self.lookup(&elem_ty) });
                    cur_ty = elem_ty;
                },
            }
        }
        Ok((Place { root, path }, self.lookup(&cur_ty)))
    }

    /// `root(.field | [index])* = value` — see `TypedExprKind::PlaceAssign`.
    fn lower_place_assign(
        &mut self,
        target: Spanned<Expression>,
        value: Spanned<Expression>,
        span: Span,
    ) -> Result<(TypedExprKind, Type), Spanned<TypeError>> {
        let (place, leaf_ty) = self.lower_place(target, span)?;
        // The assigned leaf is exactly as much a union-typed slot as a
        // `StructInit` argument is, so it needs the same check-and-widen
        // — without it, `c.v = 9` would overwrite a boxed `Int | Bool`
        // field with the raw immediate `9`, and the next `TypeTag`/
        // `Narrow` would dereference it as a `FrogVariant*`.
        let value = self.lower_expected(value, &leaf_ty)?;
        // A place assignment is a statement: codegen rebinds the touched
        // leaf `Variable`(s) (a pure field path) or writes through the
        // indexed list, and yields one dummy value — `None` is both the
        // documented result type and the only single-slot type that can't
        // disagree with that.
        Ok((TypedExprKind::PlaceAssign { place, value: Box::new(value) }, Type::None))
    }

    fn lower_assign(&mut self, a: AssignExpr, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        // `a.b.c = ...` / `xs[0] = ...` / `o.xs[0].f = ...` — a place
        // assignment. `Grammar::assign` only lets a `FieldAccess`/`Index`
        // target reach here when its own root (chased through any number
        // of further `.field`/`[index]` steps) is a bare identifier, so
        // `lower_place_assign` never has to handle a non-identifier root.
        let (kind, ty) = if matches!(a.target.item, Expression::FieldAccess(_) | Expression::Index(_)) {
            self.lower_place_assign(*a.target, *a.value, span)?
        } else {
            let name = a.target.item.get_identifier()
                .expect("assignment target must be identifier").to_string();
            match a.decl {
                // `let name = ...` / `mut name = ...` — a fresh
                // declaration, shadowing any binding of the same name
                // already in scope. `mutability` decides whether the
                // *new* binding accepts later reassignment; it says
                // nothing about whatever it shadows.
                Some(mutability) => {
                    // A `let`/`mut`/`func` declaration can shadow an
                    // ordinary binding at typeck level just fine, but a
                    // registered host function's dispatch (`Ctx::host_fns`,
                    // `compile_call`) is name-based, not scope-based — a
                    // user declaration of the same name would still route
                    // through the host shim at every call site, at
                    // whatever type the user declared. Reject it here
                    // instead of letting it surface as a confusing
                    // codegen/verifier error later.
                    if self.host_names.contains(&name) {
                        return Err(Spanned::from(TypeError {
                            msg: format!("'{}' is a registered host function and can't be redeclared", name),
                        }, span));
                    }
                    let mutable = mutability == Mutability::Mutable;
                    match &a.typ {
                        Some(ann) => {
                            let annotated_ty = self.resolve_type_expr(ann)?;
                            // Validate *and* lower the value against the
                            // annotation, then bind the name at the
                            // annotation type (not the value's own). For
                            // function types that distinction matters: the
                            // body's type is not the variable's type.
                            let value = self.lower_expected(*a.value, &annotated_ty)?;
                            if mutable { self.mut_names.insert(name.clone()); }
                            self.ctx.insert_mut(name.clone(), annotated_ty.clone(), mutable);
                            (TypedExprKind::Assign { name, value: Box::new(value) }, annotated_ty)
                        },
                        None => {
                            // `let f = g`, where `g` names a function: an
                            // *alias*, not a value copy. A function has no
                            // runtime representation in froglang — there is
                            // no closure object, no function pointer, no
                            // indirect call — so the only coherent reading
                            // of a second name for one is a compile-time
                            // rebinding, which is exactly what this is:
                            // `f` takes on `g`'s type, its binders (so an
                            // alias of a generic stays generic and
                            // instantiates through the same template), its
                            // codegen symbol, and its `mut` parameter list
                            // (`lower_call` looks that up by the *source*
                            // callee name, which is now `f`). The
                            // declaration itself compiles to nothing.
                            //
                            // A `mut` binding is deliberately excluded:
                            // reassigning it would have to change what a
                            // call site resolves to at runtime, which is
                            // the indirect call that doesn't exist. It
                            // falls through and is rejected by
                            // `validate_codegen_constraints` instead.
                            let alias_target = a.value.item.get_identifier()
                                .filter(|_| !mutable)
                                .map(str::to_string)
                                .filter(|t| matches!(self.ctx.get(t), Some(Type::Function { .. })));
                            if let Some(target) = alias_target {
                                let target_ty = self.ctx.get(&target).cloned().expect("just matched");
                                let binders = self.ctx.binders(&target).unwrap_or(&[]).to_vec();
                                let symbol = self.ctx.symbol(&target).unwrap_or(&target).to_string();
                                if let Some(mut_params) = self.func_mut_params.get(&target).cloned() {
                                    self.func_mut_params.insert(name.clone(), mut_params);
                                }
                                self.ctx.insert_generalized(name, target_ty, binders, Some(symbol));
                                // Nothing to emit, and nothing this
                                // declaration could evaluate *to* — same
                                // as a generic declaration, which
                                // `monomorphize_generics` strips for the
                                // same reason.
                                return Ok(Spanned::from(
                                    TypedExpr { id: 0, ty: Type::None, kind: TypedExprKind::NoneLit }, span));
                            }
                            // Pre-bind fully-annotated functions so the body
                            // can reference the function by name (enabling
                            // recursion). `func_mut_params` is pre-bound
                            // alongside `func_ty` for the same reason: a
                            // recursive call with a `mut` argument
                            // (`lower_call`) needs it before this function's
                            // own body finishes checking, not after.
                            // A not-fully-annotated function gets pre-bound
                            // too, to a *placeholder* built from fresh vars
                            // where the annotations run out — one per
                            // parameter plus one for the result. Without it
                            // such a function could not be recursive at all
                            // (`func fact(n) = ... fact(n - 1)` was "Unbound
                            // variable fact"), which also made the recursive
                            // case `monomorphize_generics` is written to
                            // handle unreachable. The placeholder is bound
                            // with *zero* binders, so a call inside the body
                            // uses it at one type: monomorphic recursion,
                            // the decidable half. `unify_placeholder` below
                            // ties it to what the body actually inferred.
                            //
                            // The template symbol has to be minted here,
                            // before the body is checked, so that a
                            // recursive `Var` resolves to *this*
                            // declaration's symbol the same way an external
                            // reference does — decided in scope, at lowering
                            // time, which is the whole point of
                            // `Binding::symbol`. If the function turns out
                            // monomorphic after all, `rename_var` below
                            // puts the source name back.
                            let mut recursion_prebind: Option<(String, Type)> = None;
                            // Declared `<T: Bound>` binders (`TRAITS.md`
                            // Part 4) go in scope here rather than inside
                            // `lower_function`, because the recursion
                            // pre-bind below resolves this declaration's
                            // annotations *before* the function itself is
                            // lowered — so both halves have to see the same
                            // binder variables, or a recursive call would be
                            // typed against a different `T` than the body.
                            // Added onto the enclosing scope (not replacing
                            // it) so a generic nested inside another still
                            // sees the outer binders; restored below.
                            let saved_type_params = self.type_param_scope.clone();
                            let mut declared_binders: Vec<(String, Vec<Trait>, Type)> = Vec::new();
                            if let Expression::Function(func) = &a.value.item {
                                for tp in &func.type_params {
                                    let bounds = self.resolve_bounds(&tp.bounds, span)?;
                                    let var = self.fresh_bounded_var(bounds.clone());
                                    self.type_param_scope.insert(tp.name.clone(), var.clone());
                                    declared_binders.push((tp.name.clone(), bounds, var));
                                }
                            }
                            if let Expression::Function(func) = &a.value.item {
                                // Explicit `<T>` binders mean the writer
                                // *intends* a scheme, so a fully annotated
                                // generic declaration must still generalize —
                                // otherwise `func twice<T: Num>(x: T): T`
                                // would monomorphize to whichever type called
                                // it first, which is exactly what writing the
                                // binder says it doesn't do.
                                let fully_annotated = func.type_params.is_empty()
                                    && func.return_type.is_some()
                                    && func.params.iter().all(|p| p.ty.is_some());
                                let mut param_tys = Vec::with_capacity(func.params.len());
                                for p in &func.params {
                                    param_tys.push(match &p.ty {
                                        Some(t) => self.resolve_type_expr(t)?,
                                        None    => self.fresh_var(),
                                    });
                                }
                                let ret_ty = match &func.return_type {
                                    Some(t) => self.resolve_type_expr(t)?,
                                    None    => self.fresh_var(),
                                };
                                let func_ty = Type::Function { params: param_tys, result: Box::new(ret_ty) };
                                if fully_annotated {
                                    self.ctx.insert_mut(name.clone(), func_ty, mutable);
                                } else if !mutable {
                                    let sym = format!("{}#{}", name, self.next_id);
                                    self.next_id += 1;
                                    self.ctx.insert_generalized(name.clone(), func_ty.clone(), Vec::new(), Some(sym.clone()));
                                    recursion_prebind = Some((sym, func_ty));
                                }
                                // `func_mut_params` is pre-bound alongside
                                // for the same reason: a recursive call with
                                // a `mut` argument (`lower_call`) needs it
                                // before this function's own body finishes
                                // checking, not after.
                                self.func_mut_params.insert(name.clone(), func.params.iter().map(|p| p.mutable).collect());
                            }
                            let lowered = self.check_and_lower(*a.value);
                            self.type_param_scope = saved_type_params;
                            let mut value = lowered?;

                            // The declared bounds must cover the inferred
                            // ones. Inference already derives what a body
                            // needs (`generalize` always has); what writing
                            // the binders adds is the claim that the
                            // *signature* says so, and this is where that
                            // claim is checked — TRAITS.md's "infer, then
                            // require the inferred bounds to be written". A
                            // declared-but-unused bound is fine; an
                            // undeclared-but-required one is not, because
                            // callers read the signature, not the body.
                            for (binder, declared, var) in &declared_binders {
                                let Type::TypeVar { bounds: inferred, .. } = self.lookup(var) else { continue };
                                for b in &inferred {
                                    if !declared.contains(b) {
                                        return Err(Spanned::from(TypeError {
                                            msg: format!(
                                                "type parameter '{}' of '{}' is used as {}, but is declared without that bound — write <{}: {}>",
                                                binder, name, b, binder,
                                                declared.iter().map(|d| d.to_string())
                                                    .chain(std::iter::once(b.to_string()))
                                                    .collect::<Vec<_>>().join(" + ")
                                            )
                                        }, span));
                                    }
                                }
                            }
                            // Tie the placeholder the body called back to
                            // the signature the body actually has. This can
                            // only fail if a recursive call disagreed with
                            // the declaration — polymorphic recursion, which
                            // is undecidable to infer and so is reported
                            // rather than guessed at.
                            if let Some((_, placeholder)) = &recursion_prebind {
                                let placeholder = placeholder.clone();
                                if !self.unify(&placeholder, &value.item.ty) {
                                    return Err(Spanned::from(TypeError {
                                        msg: format!(
                                            "recursive calls to '{}' must all be at the same type — it is used as {} but defined as {}; \
                                             annotate its parameters and return type to say which you mean",
                                            name, self.lookup(&placeholder), self.lookup(&value.item.ty),
                                        )
                                    }, span));
                                }
                                // A recursive call was typed against the
                                // placeholder, so its `Var`/`Call` nodes
                                // carry placeholder vars that only the unify
                                // above pins down — and `unify` writes to
                                // `substitutions`, never into a node's `ty`.
                                // Resolve them here, while the subtree that
                                // has them is still in hand. A generic
                                // declaration would get this again from
                                // `monomorphize_generics`; a monomorphic one
                                // is never walked by that pass at all (it
                                // returns early when nothing is generic),
                                // which is how `fact`'s `~t3` return slot
                                // reached codegen as an unresolved var.
                                self.substitute_types_deep(&mut value, &HashMap::new());
                            }
                            let ty = self.lookup(&value.item.ty);
                            // The value restriction (`TRAITS.md` Stage 2):
                            // generalize exactly a syntactic function value
                            // bound immutably — a `func` declaration or a
                            // let-bound lambda literal. `mut xs = []` and
                            // every other binding stay monomorphic, which
                            // is what keeps generalization sound against
                            // mutable lists. Computed *before* the
                            // authoritative bind below, so `generalize`'s
                            // environment scan doesn't see `name` itself.
                            let is_fn_value = matches!(&value.item.kind, TypedExprKind::Function { .. });
                            // The name the emitted `Assign` node carries.
                            // Diverges from `name` only for a generalized
                            // declaration (below); everything keyed by the
                            // *source* name — `func_mut_params`, the scope
                            // stack — must keep using `name`.
                            let mut decl_name = name.clone();
                            if !mutable && is_fn_value {
                                // `name` is in scope here whenever the
                                // recursion pre-bind ran, so tell
                                // `generalize` to look past it.
                                let binders = self.generalize(&ty, recursion_prebind.as_ref().map(|_| name.as_str()));
                                // Only a *genuinely* polymorphic binding
                                // (non-empty binders) needs the codegen
                                // gate to watch it — a monomorphic func
                                // declaration (e.g. every parameter and
                                // the return type annotated) is generalized
                                // trivially to zero binders and compiles
                                // exactly as before.
                                let mut symbol = None;
                                if !binders.is_empty() {
                                    // Retain the declaration itself
                                    // (`TRAITS.md` Stage 3b) so a later call
                                    // — in this entry or a future one — can
                                    // clone and specialize it; see
                                    // `generic_templates`'s doc comment.
                                    // Keyed by a symbol minted fresh here,
                                    // not by `name`, so redeclaring a
                                    // generic replaces nothing: the two
                                    // declarations are separate templates
                                    // with separate instantiations, and the
                                    // references already lowered against
                                    // the first one keep pointing at it.
                                    if let TypedExprKind::Function { params, return_type, body } = &value.item.kind {
                                        // Reuse the symbol the recursion
                                        // pre-bind already minted, so a
                                        // recursive `Var` inside `body` —
                                        // lowered under that symbol — names
                                        // this very template. Only a
                                        // declaration that never got one
                                        // (`mut`, which is never
                                        // generalized) mints here instead.
                                        let sym = match &recursion_prebind {
                                            Some((sym, _)) => sym.clone(),
                                            None => {
                                                let sym = format!("{}#{}", name, self.next_id);
                                                self.next_id += 1;
                                                sym
                                            },
                                        };
                                        self.generic_templates.insert(sym.clone(), GenericTemplate {
                                            binders: binders.clone(),
                                            declared_ty: ty.clone(),
                                            params: params.clone(),
                                            return_type: return_type.clone(),
                                            body: (**body).clone(),
                                        });
                                        symbol = Some(sym);
                                    }
                                }
                                if symbol.is_none() {
                                    // Generalization found no binders, so
                                    // this declaration is monomorphic after
                                    // all and keeps its source name — but
                                    // the body was lowered against the
                                    // pre-bind's symbol. Put the name back.
                                    // Safe without any shadowing analysis:
                                    // `name#N` is not a spellable
                                    // identifier, so the only `Var` nodes
                                    // carrying it are the ones scope
                                    // resolution created for this
                                    // declaration. A local that shadowed
                                    // `name` inside the body lowered to
                                    // `Var(name)` and is untouched.
                                    if let Some((sym, _)) = &recursion_prebind {
                                        Self::rename_var(&mut value, sym, &name);
                                    }
                                }
                                self.ctx.insert_generalized(name.clone(), ty.clone(), binders, symbol.clone());
                                // The declaration node carries the symbol
                                // too, so `monomorphize_generics` strips
                                // exactly this declaration from codegen
                                // and leaves any same-named monomorphic
                                // one alone.
                                if let Some(sym) = symbol { decl_name = sym; }
                            } else {
                                if mutable { self.mut_names.insert(name.clone()); }
                                self.ctx.insert_mut(name.clone(), ty.clone(), mutable);
                            }
                            // Authoritative overwrite: covers the
                            // not-fully-annotated case the pre-bind above
                            // skips, and stays correct even when it ran.
                            if let TypedExprKind::Function { params, .. } = &value.item.kind {
                                self.func_mut_params.insert(name.clone(), params.iter().map(|(_, _, m)| *m).collect());
                            }
                            (TypedExprKind::Assign { name: decl_name, value: Box::new(value) }, ty)
                        },
                    }
                },
                // `name = ...` with no `let`/`mut` — assignment to a
                // binding declared earlier. Closes two holes: an
                // undeclared name no longer silently declares one
                // (`MUTABILITY.md`'s "implicit declaration"), and an
                // immutable binding can no longer be silently retyped by
                // reassignment. The grammar (`Grammar::assign`) never
                // attaches a type annotation to this form, so the
                // binding's own declared type is the only one in play —
                // `lower_expected` both checks and (as any other slot
                // does) widens the value into it.
                None => {
                    match self.ctx.is_mutable(&name) {
                        None => return Err(Spanned::from(TypeError {
                            msg: format!("'{}' is not declared — did you mean 'let {} = ...' or 'mut {} = ...'?", name, name, name)
                        }, span)),
                        Some(false) => return Err(Spanned::from(TypeError {
                            msg: format!("'{}' is not mutable — declare it with 'mut {} = ...' to allow reassignment", name, name)
                        }, span)),
                        Some(true) => {},
                    }
                    let existing_ty = self.ctx.get(&name).expect("checked mutable above").clone();
                    let value = self.lower_expected(*a.value, &existing_ty)?;
                    (TypedExprKind::Assign { name, value: Box::new(value) }, existing_ty)
                },
            }
        };
        Ok(Spanned::from(TypedExpr { id: 0, ty, kind }, span))
    }

    fn lower_function(&mut self, f: FunctionExpr, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let mut param_bindings: Vec<(String, Type, bool)> = Vec::with_capacity(f.params.len());
        for p in &f.params {
            let param_ty = match &p.ty {
                Some(annotation) => self.resolve_type_expr(annotation)?,
                None => self.fresh_var(),
            };
            param_bindings.push((p.name.clone(), param_ty, p.mutable));
        }

        // `return`'s type rule needs to know what it's returning
        // into, even when the body has no `: RetType` annotation at
        // all — a fresh, unbound type var serves as that slot in the
        // unannotated case, and gets pinned down the same way any
        // other inferred type does: every `return e` unifies against
        // it, and so does the body's own tail value below.
        let declared_ret = match &f.return_type {
            Some(ann) => Some(self.resolve_type_expr(ann)?),
            None => None,
        };
        let return_slot = declared_ret.clone().unwrap_or_else(|| self.fresh_var());

        // Parameters are bound for the body only. With a declared
        // return type the body is *checked* against it, which both
        // widens its tail value into a union-typed slot and pushes
        // the type down into a list literal
        // (`func f(): List<Str> = []`); without one it's
        // synthesized and unified with the `return`s below.
        let body = self.in_scope(|t| {
            // Unlike `with_context`, each parameter binds with its own
            // declared mutability (`mut p: T` binds mutably; an ordinary
            // parameter, like any other non-`let`/`mut` binding, is
            // immutable) rather than uniformly immutable.
            for (name, ty, mutable) in param_bindings.iter().cloned() {
                t.ctx.insert_mut(name, ty, mutable);
            }
            t.return_types.push(return_slot.clone());
            let result = match &declared_ret {
                Some(ret_ty) => t.lower_expected(*f.body, ret_ty),
                None         => t.check_and_lower(*f.body),
            };
            t.return_types.pop();
            result
        })?;

        let body_type = body.item.ty.clone();
        let return_type = if declared_ret.is_some() || body_type == Type::Never {
            // `Never` means the body ends in an unconditional
            // `return`, so it never falls through to a final value —
            // the `return` statements alone determine the result
            // type, and there is nothing to unify with. (`Never`
            // unifies with nothing, so without this every
            // unannotated function ending in `return` would fail.)
            self.lookup(&return_slot)
        } else if self.unify(&return_slot, &body_type) {
            self.lookup(&return_slot)
        } else {
            return Err(Spanned::from(TypeError {
                msg: format!(
                    "Function's return statements disagree with its final value: {} vs {}",
                    self.lookup(&return_slot), body_type
                )
            }, span));
        };

        let params: Vec<(String, Type, bool)> = param_bindings.into_iter()
            .map(|(n, t, mutable)| { let t = self.lookup(&t); (n, t, mutable) })
            .collect();
        let ty = Type::Function {
            params: params.iter().map(|(_, t, _)| t.clone()).collect(),
            result: Box::new(return_type.clone()),
        };
        Ok(Spanned::from(TypedExpr { id: 0, ty, kind: TypedExprKind::Function { params, return_type, body: Box::new(body) } }, span))
    }

    fn lower_call(&mut self, c: CallExpr, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let callee_span = c.callable.span;
        // Captured before `c.callable` is consumed below — only a call
        // through a bare name can have `mut` arguments at all (`mut`
        // "does not escape" a named `func` declaration, `MUTABILITY.md`),
        // so this is what the generic call branch looks
        // `func_mut_params` up by.
        let callee_name = c.callable.item.get_identifier().map(|s| s.to_string());

        // A zero-`Self` member reached here means there was no expected type
        // to resolve it against — `lower_expected` intercepts the cases where
        // there is one. Say so, rather than letting it surface as an unbound
        // variable, which would name the symptom and not the fix.
        if let Some((want_trait, member)) = self.zero_self_target(&c) {
            let named = match &want_trait {
                Some(t) => format!("'{}.{}'", t, member),
                None => format!("'{}'", member),
            };
            return Err(Spanned::from(TypeError {
                msg: format!("cannot infer which {} is meant; annotate the expected type", named)
            }, span));
        }

        // `read(s)` reached here (rather than through `lower_expected`'s
        // own arm, below) means there was no expected type to read *into*
        // — the whole reason it's return-type directed at all (`plans/
        // DATA.md` stage 5, mirroring `zero_self_target`'s `zero(): Self`
        // just above).
        if Self::is_read_call(&c) {
            return Err(Spanned::from(TypeError {
                msg: "cannot infer what to read; annotate the expected type, e.g. 'let x: T | ReadError = read(s)'".to_string()
            }, span));
        }

        // The builtin `json` namespace (`plans/DATA.md` stage 8), matched
        // here — before the `FieldAccess`-callee branch far below would try
        // to lower `json` as a value and die as an unbound variable.
        if let Some(member) = self.json_builtin_target(&c) {
            return self.lower_json_call(&member, c, callee_span, span);
        }

        // Struct construction: `Person(name="Alice", age=42)` looks
        // like an ordinary call syntactically (there's no dedicated
        // construction grammar — see `Grammar::data_decl`'s doc
        // comment), so it's disambiguated here, before the generic
        // function-call path, by checking whether the callee name is
        // a registered struct.
        let struct_name = c.callable.item.get_identifier()
            .filter(|n| self.struct_templates.contains_key(*n))
            .map(|n| n.to_string());
        // Enum variant construction: `Circle(r=4)` (bare, unique
        // owner) or `Shape.Circle(r=4)` (qualified) — same syntactic
        // shape, disambiguated the same way.
        let variant_callee = self.resolve_variant_callee(&c.callable.item);
        // `print` is a builtin conversion: unlike ordinary
        // functions its argument is accepted at any type and is
        // formatted as text by the code generator/runtime.
        let is_print = matches!(
            &c.callable.item,
            Expression::Literal(LiteralExpr { token: Token::Identifier(name) }) if name == "print"
        );
        // `repr` (`plans/DATA.md` stage 5) is `print`'s canonical twin: a
        // builtin conversion, not a `default_context()` entry, since it
        // too needs to accept any `Show` type rather than one monomorphic
        // signature. Unlike `print` it is not total — its argument must
        // implement `Show` — so that check happens right here rather than
        // being deferred to `validate_codegen_constraints` the way
        // `print`'s recursion guard is: there is no codegen arm for `repr`
        // to protect (`desugar_notation` expands it away before codegen
        // ever runs), so the check has nothing to predict *for*, only a
        // typed AST to build correctly.
        let is_repr = matches!(
            &c.callable.item,
            Expression::Literal(LiteralExpr { token: Token::Identifier(name) }) if name == "repr"
        );
        // `push` is a builtin mutating operation on `List<T>`, polymorphic
        // over `T` — like `print`, it can't be a monomorphic
        // `default_context()` entry (there's no generics system; `List<T>`
        // is already a special-cased "builtin hack" per roadmap.md), so it
        // special-cases on the callee's literal name the same way `print`
        // does, rather than being a resolvable binding.
        let is_push = matches!(
            &c.callable.item,
            Expression::Literal(LiteralExpr { token: Token::Identifier(name) }) if name == "push"
        );
        // `len` is a builtin read-only query on `List<T>` or `Str`,
        // polymorphic over `T` the same way `push` is — same reason it
        // can't be a `default_context()` entry (a TypeVar there would get
        // permanently bound by the first call site, not re-instantiated
        // per call; there's no generalization/generics system yet).
        let is_len = matches!(
            &c.callable.item,
            Expression::Literal(LiteralExpr { token: Token::Identifier(name) }) if name == "len"
        );
        // `get` is a builtin bounds-checked read on `List<T>`, polymorphic
        // over `T` for the same reason `len`/`push` are — but unlike them
        // it can't be a plain synthesized `Function` type, since its result
        // isn't just `T`, it's `T | IndexError` (the stdlib's `get`-specific
        // error type, `stdlib::install`'s prelude). Desugared entirely in
        // `finish_get` into ordinary constructs (`if`/index/struct-init)
        // rather than given dedicated codegen, the same way `?`/`!`/`catch`
        // desugar into `match` — see `finish_get`'s own comment.
        let is_get = matches!(
            &c.callable.item,
            Expression::Literal(LiteralExpr { token: Token::Identifier(name) }) if name == "get"
        );
        // `to_list` is the explicit, opt-in `Range<T> -> List<T>` conversion
        // (RANGES.md Stage 1's "breaking-change handling") — since `Range`
        // no longer secretly *is* a `List`, code that genuinely needs List
        // semantics (indexing, push, slicing) on a range has to ask for the
        // materialization explicitly rather than get it for free at every
        // call boundary. Same "can't be a plain `default_context()` entry"
        // reasoning as `len`/`push` (polymorphic over `T`, no generics
        // system for a monomorphic binding to express that).
        let is_to_list = matches!(
            &c.callable.item,
            Expression::Literal(LiteralExpr { token: Token::Identifier(name) }) if name == "to_list"
        );

        let (kind, ty) = if let Some(name) = struct_name {
            // `TRAITS.md` Stage 3a: a generic struct's binders are
            // instantiated fresh per construction call (`instantiate_struct`)
            // — `lower_record_args` below unifies each constructor argument
            // into its (fresh-var) declared field type, and looking those
            // fresh vars back up afterward gives this call's concrete
            // `args`. A non-generic struct just gets its ordinary field
            // list back with no binder vars, unchanged from before.
            let (field_defs, arg_vars) = self.instantiate_struct(&name);
            let ordered = self.lower_record_args(&name, &field_defs, c.args, callee_span)?;
            let ty = if arg_vars.is_empty() {
                Type::strukt(name.clone())
            } else {
                let args: Vec<Type> = arg_vars.iter().map(|v| self.lookup(v)).collect();
                let resolved = Type::Named { name: name.clone(), args };
                // Registers this concrete instantiation's layout into
                // `struct_defs` (keyed by the `Type` itself) even if the
                // program never reads a field back — codegen needs the
                // full layout regardless (e.g. for GC slot masks).
                self.materialize_struct(&resolved);
                resolved
            };
            (TypedExprKind::StructInit { name, fields: ordered }, ty)
        } else if let Some((enum_name, variant)) = variant_callee.map_err(|msg| Spanned::from(TypeError { msg }, callee_span))? {
            let def = self.union_defs.get(&enum_name).cloned()
                .expect("enum_name resolved via variant_owners/union_defs, must be registered");
            let variant_fields = def.variants.iter().find(|(n, _)| n == &variant)
                .map(|(_, fs)| fs.clone())
                .expect("variant resolved via variant_owners/union_defs, must be registered");
            let mut field_defs = def.common.clone();
            field_defs.extend(variant_fields);

            let kind_name = format!("{}.{}", enum_name, variant);
            let ordered = self.lower_record_args(&kind_name, &field_defs, c.args, callee_span)?;
            let tag = def.variant_index(&variant).expect("registered") as u32;
            (TypedExprKind::VariantInit { enum_name, variant, tag, fields: ordered }, def.ty.clone())
        } else if is_print {
            if c.args.len() != 1 {
                return Err(Spanned::from(TypeError {
                    msg: format!("Wrong number of arguments, expected 1, got {}", c.args.len())
                }, callee_span));
            }
            let callable = self.check_and_lower(*c.callable)?;
            let arg = self.check_and_lower(c.args.into_iter().next().expect("arity checked just above"))?;
            // If the argument never actually produces a value (e.g.
            // `print(panic("x"))`), `print` itself never returns
            // either — propagate `Never` so this call is treated
            // uniformly with any other `Never`-typed expression
            // (join with other branches, dead-code trapping, etc.)
            // instead of falsely claiming `None`.
            let ty = if arg.item.ty == Type::Never { Type::Never } else { Type::None };
            (TypedExprKind::Call { callable: Box::new(callable), args: vec![Arg::Value(arg)] }, ty)
        } else if is_repr {
            if c.args.len() != 1 {
                return Err(Spanned::from(TypeError {
                    msg: format!("Wrong number of arguments, expected 1, got {}", c.args.len())
                }, callee_span));
            }
            let arg = self.check_and_lower(c.args.into_iter().next().expect("arity checked just above"))?;
            (self.notation_placeholder(Notation::Repr, arg, callee_span)?.item.kind, Type::Str)
        } else if is_push {
            if c.args.len() != 2 {
                return Err(Spanned::from(TypeError {
                    msg: format!("Wrong number of arguments, expected 2, got {}", c.args.len())
                }, callee_span));
            }
            let mut arg_iter = c.args.into_iter();
            let xs_arg = arg_iter.next().expect("arity checked just above");
            let v_arg  = arg_iter.next().expect("arity checked just above");
            let xs_span = xs_arg.span;

            let xs_inner = match xs_arg.item {
                Expression::MutArg(inner) => *inner,
                _ => return Err(Spanned::from(TypeError {
                    msg: "push's first argument must be marked 'mut'".to_string()
                }, xs_span)),
            };
            // `push`'s receiver is a *place*, not just a binding
            // (`MUTABILITY.md` Stage 8) — `lower_place` resolves the same
            // paths `lower_place_assign` accepts and applies the same
            // mutability check to the root.
            let (place, leaf_ty) = self.lower_place(xs_inner, xs_span)?;
            return self.finish_push(place, leaf_ty, xs_span, v_arg, span);
        } else if is_len {
            if c.args.len() != 1 {
                return Err(Spanned::from(TypeError {
                    msg: format!("Wrong number of arguments, expected 1, got {}", c.args.len())
                }, callee_span));
            }
            let arg = self.check_and_lower(c.args.into_iter().next().expect("arity checked just above"))?;
            return self.finish_len(arg, callee_span, span);
        } else if is_get {
            if c.args.len() != 2 {
                return Err(Spanned::from(TypeError {
                    msg: format!("Wrong number of arguments, expected 2, got {}", c.args.len())
                }, callee_span));
            }
            let mut arg_iter = c.args.into_iter();
            let xs_arg = arg_iter.next().expect("arity checked just above");
            let i_arg  = arg_iter.next().expect("arity checked just above");
            return self.finish_get(xs_arg, i_arg, span);
        } else if is_to_list {
            if c.args.len() != 1 {
                return Err(Spanned::from(TypeError {
                    msg: format!("Wrong number of arguments, expected 1, got {}", c.args.len())
                }, callee_span));
            }
            let arg = self.check_and_lower(c.args.into_iter().next().expect("arity checked just above"))?;
            return self.finish_to_list(arg, callee_span, span);
        } else if matches!(&c.callable.item, Expression::FieldAccess(_)) {
            // `x.f(args)` where `f` isn't a struct/union field of
            // `typeof(x)` — resolved by `lower_ufcs_call` per
            // `TRAITS.md` Part 1's three steps.
            let Expression::FieldAccess(fa) = c.callable.item else { unreachable!("matched above") };

            // ...unless the "receiver" is a trait name, in which case this is
            // the prefix form `Ord.compare(a, b)`, not a dot call at all. It
            // has to be caught *before* the target is lowered, since a trait
            // name is not a value and would die as an unbound variable.
            //
            // The prefix form is trait-qualified rather than type-qualified
            // (`Ord.compare`, never `Money.compare`) because that is the form
            // a zero-`Self` member like `zero(): Self` can be spelled in at
            // all — see `TRAITS.md` Part 1, "The prefix form".
            if let Some(trait_name) = fa.target.item.get_identifier()
                .filter(|n| self.traits.contains_key(*n))
                .map(|n| n.to_string())
            {
                return self.lower_trait_prefix_call(&trait_name, fa.field, c.args, callee_span, span);
            }

            return self.lower_ufcs_call(fa, c.args, callee_span, span, None);
        } else {
            let callable = self.check_and_lower(*c.callable)?;
            return self.finish_call(callable, callee_name, c.args, callee_span, span);
        };
        Ok(Spanned::from(TypedExpr { id: 0, ty, kind }, span))
    }

    /// The tail of a `push` call once its first argument is resolved to
    /// an already-lowered, already-mutability-checked list with a known
    /// root binding — shared by `lower_call`'s `is_push` branch (where
    /// `root` comes from an explicit `mut` marker) and `lower_ufcs_call`
    /// (where `xs.push(v)`'s receiver is exempt from that marker, per
    /// `TRAITS.md` Part 2, but `root` still names the same binding).
    fn finish_push(&mut self, place: Place, leaf_ty: Type, xs_span: Span, v_arg: Spanned<Expression>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let root = place.root.clone();
        let xs_ty = self.lookup(&leaf_ty);
        let Some(elem_ty) = xs_ty.as_list_elem() else {
            return Err(Spanned::from(TypeError {
                msg: format!("push's first argument must be a List, got {:?}", xs_ty)
            }, xs_span));
        };

        // Exclusivity: `push(mut xs, xs)` rejected, same reasoning as
        // the generic `mut`-argument path's "no root may also be
        // another argument" rule — just a fixed two-argument shape
        // here, not worth generalizing.
        let v_span = v_arg.span;
        if v_arg.item.get_identifier().is_some_and(|n| n == root) {
            return Err(Spanned::from(TypeError {
                msg: format!("'{}' can't be passed 'mut' and also appear as another argument in the same call", root)
            }, v_span));
        }

        let v_lowered = self.check_and_lower(v_arg)?;
        let resolved_argt = self.lookup(&v_lowered.item.ty);
        let resolved_elem = self.lookup(elem_ty);
        if !widens_to(&resolved_argt, &resolved_elem)
            && !self.unify(&v_lowered.item.ty, elem_ty)
            && !self.unify_with_one_union_member(&v_lowered.item.ty, elem_ty) {
            return Err(Spanned::from(TypeError {
                msg: format!("Can't unify {} and {}", resolved_argt, resolved_elem)
            }, v_span));
        }
        let v_widened = self.lower_widen(v_lowered, elem_ty)?;

        // `push` has no `default_context()` entry to resolve (see `is_push`),
        // so its callable is synthesized here and consulted for nothing but
        // `compile_call`'s dispatch on the literal name — exactly as `print`
        // is dispatched. Its receiver rides in `Arg::Mut`, so nothing about
        // this shape is special to `push`: the next mutating builtin needs
        // only its own arm in `compile_call`.
        let callable = Spanned::from(TypedExpr {
            id: 0,
            ty: Type::Function { params: vec![xs_ty.clone(), (*elem_ty).clone()], result: Box::new(Type::None) },
            kind: TypedExprKind::Var("push".to_string()),
        }, xs_span);

        Ok(Spanned::from(TypedExpr {
            id: 0,
            ty: Type::None,
            kind: TypedExprKind::Call {
                callable: Box::new(callable),
                args: vec![Arg::Mut(place), Arg::Value(v_widened)],
            },
        }, span))
    }

    /// The tail of a `len` call once its argument is already lowered —
    /// shared by `lower_call`'s `is_len` branch (`len(xs)`) and
    /// `lower_ufcs_call` (`xs.len()`). Like `finish_push`, synthesizes the
    /// callable's `TypedExpr` directly rather than resolving it against
    /// `ctx`, since no `default_context()` entry backs `len` either.
    fn finish_len(&mut self, arg: Spanned<TypedExpr>, callee_span: Span, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let arg_span = arg.span;
        let arg_ty = self.lookup(&arg.item.ty);
        if !arg_ty.is_list() && arg_ty != Type::Str {
            return Err(Spanned::from(TypeError {
                msg: format!("len's argument must be a List or Str, got {:?}", arg_ty)
            }, arg_span));
        }
        let callable = Spanned::from(TypedExpr {
            id: 0,
            ty: Type::Function { params: vec![arg_ty.clone()], result: Box::new(Type::Int) },
            kind: TypedExprKind::Var("len".to_string()),
        }, callee_span);
        Ok(Spanned::from(TypedExpr {
            id: 0,
            ty: Type::Int,
            kind: TypedExprKind::Call { callable: Box::new(callable), args: vec![Arg::Value(arg)] },
        }, span))
    }

    /// The tail of a `to_list` call once its argument is already lowered —
    /// the explicit `Range<T> -> List<T>` conversion (see `is_to_list`'s
    /// comment). Same "synthesize the callable directly" shape as
    /// `finish_len`.
    fn finish_to_list(&mut self, arg: Spanned<TypedExpr>, callee_span: Span, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let arg_span = arg.span;
        let arg_ty = self.lookup(&arg.item.ty);
        let Some(elem) = arg_ty.as_range_elem().cloned() else {
            return Err(Spanned::from(TypeError {
                msg: format!("to_list's argument must be a Range, got {}", arg_ty)
            }, arg_span));
        };
        let result_ty = Type::list(elem);
        let callable = Spanned::from(TypedExpr {
            id: 0,
            ty: Type::Function { params: vec![arg_ty.clone()], result: Box::new(result_ty.clone()) },
            kind: TypedExprKind::Var("to_list".to_string()),
        }, callee_span);
        Ok(Spanned::from(TypedExpr {
            id: 0,
            ty: result_ty,
            kind: TypedExprKind::Call { callable: Box::new(callable), args: vec![Arg::Value(arg)] },
        }, span))
    }

    /// `get(xs, i)`'s entire lowering: rather than adding a dedicated
    /// codegen node with hand-rolled bounds-check IR, this builds a small
    /// surface-syntax `Expression::Block` — bind the receiver/index once,
    /// resolve a negative index the same way the runtime's own
    /// `resolve_index` does, bounds-check it, and either index in-bounds or
    /// construct an `IndexError` — then lowers that block through the
    /// ordinary `check_and_lower` path. This is exactly the same strategy
    /// `?`/`!`/`catch` use (`build_try_arms` etc.): reuse the existing
    /// `if`/else union-join and struct-construction machinery instead of
    /// teaching codegen a new node. `xs`/`i` are passed in unlowered (as
    /// `Expression`, not `TypedExpr`) since they're spliced into the
    /// desugared block and lowered there, exactly once.
    fn finish_get(&mut self, xs_arg: Spanned<Expression>, i_arg: Spanned<Expression>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let xs_span = xs_arg.span;
        let ident = |n: &str| Expression::literal(Token::Identifier(n.to_string()));
        let let_stmt = |name: &str, value: Spanned<Expression>, sp: Span| Spanned::from(
            Expression::assign(Spanned::from(ident(name), sp), None, value, Some(Mutability::Immutable)),
            sp,
        );
        let mut stmts = vec![let_stmt("__get_xs", xs_arg, xs_span)];
        stmts.extend(Self::get_rest_stmts(i_arg, span));
        self.check_and_lower(Spanned::from(Expression::Block(stmts), span))
    }

    /// The `xs.get(i)` UFCS form (`lower_ufcs_call`'s `field == "get"`
    /// branch): unlike `finish_get`, the receiver (`xs_lowered`) is already
    /// typed — lowered once by the caller, since it may have side effects
    /// (`get_list().get(i)` must call `get_list()` once, same reasoning as
    /// `push`/`len`'s UFCS forms). So it can't be spliced back into a raw
    /// `Expression::Block` and re-lowered (that would evaluate it twice);
    /// instead its value is bound into scope directly as a synthesized
    /// `TypedExprKind::Assign` (the same trick `lower_match_lowered` uses
    /// for its subject temporary), and only the rest of the desugaring
    /// (`get_rest_stmts`, which only ever refers to `__get_xs` by name) goes
    /// through ordinary raw-`Expression` lowering.
    fn finish_get_ufcs(&mut self, xs_lowered: Spanned<TypedExpr>, i_arg: Spanned<Expression>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let xs_ty = xs_lowered.item.ty.clone();
        let xs_assign = Spanned::from(
            TypedExpr { id: 0, ty: xs_ty.clone(), kind: TypedExprKind::Assign { name: "__get_xs".to_string(), value: Box::new(xs_lowered) } },
            span,
        );
        let rest_block = Spanned::from(Expression::Block(Self::get_rest_stmts(i_arg, span)), span);
        let rest = self.with_context_mut(
            std::iter::once(("__get_xs".to_string(), xs_ty, false)),
            |t| t.check_and_lower(rest_block),
        )?;
        let ty = rest.item.ty.clone();
        Ok(Spanned::from(TypedExpr { id: 0, ty, kind: TypedExprKind::Block(vec![xs_assign, rest]) }, span))
    }

    /// The shared tail of `get`'s desugaring, from `__get_i` onward —
    /// everything after `__get_xs` is bound, referring to it only by name
    /// so it's agnostic to how the caller bound it (`finish_get`'s raw
    /// `let`, or `finish_get_ufcs`'s pre-typed `Assign`): resolve a
    /// negative index the same way the runtime's own `resolve_index` does
    /// (`frog_list_get`'s FFI, `runtime/ffi.rs`), bounds-check it, and
    /// either index in-bounds or construct an `IndexError`. This is the
    /// same strategy `?`/`!`/`catch` use (`build_try_arms` etc.): reuse the
    /// existing `if`/else union-join and struct-construction machinery
    /// instead of teaching codegen a new node.
    fn get_rest_stmts(i_arg: Spanned<Expression>, span: Span) -> Vec<Spanned<Expression>> {
        let ident = |n: &str| Expression::literal(Token::Identifier(n.to_string()));
        let let_stmt = |name: &str, value: Spanned<Expression>, sp: Span| Spanned::from(
            Expression::assign(Spanned::from(ident(name), sp), None, value, Some(Mutability::Immutable)),
            sp,
        );

        let i_span = i_arg.span;
        let mut stmts = Vec::with_capacity(4);
        stmts.push(let_stmt("__get_i", i_arg, i_span));

        let len_call = Spanned::from(
            Expression::call(Spanned::from(ident("len"), span), vec![Spanned::from(ident("__get_xs"), span)]),
            span,
        );
        stmts.push(let_stmt("__get_n", len_call, span));

        // __get_real = if __get_i < 0 then __get_n + __get_i else __get_i
        let is_negative = Spanned::from(Expression::binary(
            Token::Lt, Spanned::from(ident("__get_i"), span), Spanned::from(Expression::literal(Token::Int(0)), span),
        ), span);
        let wrapped = Spanned::from(Expression::binary(
            Token::Plus, Spanned::from(ident("__get_n"), span), Spanned::from(ident("__get_i"), span),
        ), span);
        let real_val = Spanned::from(
            Expression::conditional(is_negative, wrapped, Some(Spanned::from(ident("__get_i"), span))),
            span,
        );
        stmts.push(let_stmt("__get_real", real_val, span));

        // __get_real >= 0 and __get_real < __get_n
        let ge_zero = Spanned::from(Expression::binary(
            Token::GtEq, Spanned::from(ident("__get_real"), span), Spanned::from(Expression::literal(Token::Int(0)), span),
        ), span);
        let lt_len = Spanned::from(Expression::binary(
            Token::Lt, Spanned::from(ident("__get_real"), span), Spanned::from(ident("__get_n"), span),
        ), span);
        let in_bounds = Spanned::from(Expression::binary(Token::And, ge_zero, lt_len), span);

        let index_expr = Spanned::from(
            Expression::index(Spanned::from(ident("__get_xs"), span), Spanned::from(ident("__get_real"), span)),
            span,
        );
        let index_error = Spanned::from(
            Expression::call(Spanned::from(ident("IndexError"), span), vec![
                Spanned::from(Expression::assign(Spanned::from(ident("index"), span), None, Spanned::from(ident("__get_i"), span), None), span),
                Spanned::from(Expression::assign(Spanned::from(ident("len"), span), None, Spanned::from(ident("__get_n"), span), None), span),
            ]),
            span,
        );
        stmts.push(Spanned::from(Expression::conditional(in_bounds, index_expr, Some(index_error)), span));
        stmts
    }

    /// The tail shared by an ordinary call (`f(args)`, callable already
    /// resolved) and a UFCS free-function rewrite (`x.f(args)` →
    /// `f(x, args)`, `lower_ufcs_call`'s step-3 case): arity, `mut`
    /// marking/exclusivity, unification and widening against the
    /// callable's parameter types.
    fn finish_call(&mut self, callable: Spanned<TypedExpr>, callee_name: Option<String>, call_args: Vec<Spanned<Expression>>, callee_span: Span, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let func_type = self.lookup(&callable.item.ty);

        // If the callee is an unbound TypeVar (e.g. a lambda
        // parameter used as a function), bind it to a fresh
        // function type whose arity matches this call site.
        let func_type = if let Type::TypeVar { name, .. } = &func_type {
            let param_types: Vec<Type> = call_args.iter().map(|_| self.fresh_var()).collect();
            let result_type = self.fresh_var();
            let fn_ty = Type::Function { params: param_types, result: Box::new(result_type) };
            self.substitutions.insert(name.clone(), fn_ty.clone());
            fn_ty
        } else {
            func_type
        };

        let Type::Function { params, result } = func_type else {
            return Err(Spanned::from(TypeError {
                msg: format!("Not callable: {}", func_type)
            }, callee_span));
        };
        if call_args.len() != params.len() {
            return Err(Spanned::from(TypeError {
                msg: format!("Wrong number of arguments, expected {}, got {}", params.len(), call_args.len())
            }, callee_span));
        }
        // The callee's declared `mut` parameters, if it's a plain
        // name resolving to one — `None` means either an indirect
        // call or a callee with no `mut` parameters, both of which
        // reject *any* `mut`-marked argument identically below.
        let declared_mut = callee_name.as_ref().and_then(|n| self.func_mut_params.get(n).cloned());

        let mut args = Vec::with_capacity(call_args.len());
        let mut mut_args = Vec::with_capacity(call_args.len());
        // Every argument that's a bare identifier reference, mut or
        // not — used below to reject a `mut` argument's root
        // reappearing as any other argument in the same call
        // (`swap(mut a, mut a)`, `merge(mut xs, xs)`), the exclusivity
        // rule that's cheap here only because nothing else aliases.
        //
        // One entry per argument, `None` for an argument that isn't a
        // bare identifier — the exclusivity loop below indexes this by
        // *argument* position, so pushing only the identifier ones
        // would misalign it. `f(g(x), mut a)` used to push a single
        // entry and then index it at 1.
        let mut all_roots: Vec<Option<(String, Span)>> = Vec::with_capacity(call_args.len());
        for (i, (arg, param)) in call_args.into_iter().zip(params.iter()).enumerate() {
            let declared = declared_mut.as_ref().and_then(|d| d.get(i)).copied();
            let (lowered, is_mut, root) = self.lower_call_arg(i, arg, param, declared)?;
            args.push(lowered);
            mut_args.push(is_mut);
            all_roots.push(root);
        }
        self.check_mut_exclusivity(&mut_args, &all_roots)?;
        let ty = self.lookup(&result);
        Ok(Spanned::from(TypedExpr { id: 0, ty, kind: TypedExprKind::Call { callable: Box::new(callable), args } }, span))
    }

    /// One argument of a call: unwraps a `mut` marker, validates it
    /// against `declared` (the callee's declared mutability for this
    /// position, `None` meaning "no declaration reaches here"), then
    /// unifies/widens the argument's type against `param`. Shared by
    /// `finish_call` (every argument) and `lower_ufcs_call` (every
    /// argument after the exempt receiver).
    fn lower_call_arg(&mut self, i: usize, arg: Spanned<Expression>, param: &Type, declared: Option<bool>) -> Result<(Arg, bool, Option<(String, Span)>), Spanned<TypeError>> {
        let arg_span = arg.span;
        let (is_mut, inner) = match arg.item {
            Expression::MutArg(inner) => (true, *inner),
            other => (false, Spanned::from(other, arg_span)),
        };
        let root = inner.item.get_identifier().map(|r| (r.to_string(), arg_span));
        // `.get(i)`, not `d[i]`: `func_mut_params` is keyed by bare
        // name with no scoping, so a local binding that shadows a
        // `func` of the same name can produce a shorter list than
        // this call has arguments. Treating a missing entry as
        // "not declared mut" is the same answer an indirect call
        // gets, which is the conservative one.
        match declared {
            Some(true) if !is_mut => return Err(Spanned::from(TypeError {
                msg: format!("argument {} must be marked 'mut' — the callee's parameter is 'mut'", i + 1)
            }, arg_span)),
            Some(false) if is_mut => return Err(Spanned::from(TypeError {
                msg: format!("argument {} is marked 'mut', but the callee's parameter isn't", i + 1)
            }, arg_span)),
            None if is_mut => return Err(Spanned::from(TypeError {
                msg: "'mut' arguments are only valid in a direct call to a 'func' declaration".to_string()
            }, arg_span)),
            _ => {},
        }
        // A `mut` argument names storage, not a value: resolve it as a
        // place (which checks the root is mutable) and type-check its leaf
        // against the parameter. No widening — a `mut` parameter is copied
        // back out into the very same storage afterwards, so its type has
        // to match exactly in both directions.
        if is_mut {
            let (place, leaf_ty) = self.lower_place(inner, arg_span)?;
            let resolved_argt  = self.lookup(&leaf_ty);
            let resolved_param = self.lookup(param);
            if !self.unify(&leaf_ty, param) {
                return Err(Spanned::from(TypeError {
                    msg: format!("Can't unify {} and {}", resolved_argt, resolved_param)
                }, arg_span));
            }
            let root = Some((place.root.clone(), arg_span));
            return Ok((Arg::Mut(place), true, root));
        }
        let lowered = self.check_and_lower(inner)?;
        let resolved_argt  = self.lookup(&lowered.item.ty);
        let resolved_param = self.lookup(param);
        // Allow implicit widening coercions at call sites (e.g. Int→Float).
        if !widens_to(&resolved_argt, &resolved_param)
            && !self.unify(&lowered.item.ty, param)
            && !self.unify_with_one_union_member(&lowered.item.ty, param) {
            return Err(Spanned::from(TypeError {
                msg: format!("Can't unify {} and {}", resolved_argt, resolved_param)
            }, arg_span));
        }
        let widened = self.lower_widen(lowered, param)?;
        Ok((Arg::Value(widened), is_mut, root))
    }

    /// A `mut`-marked argument's root may not also be any other
    /// argument's root in the same call (`swap(mut a, mut a)`,
    /// `merge(mut xs, xs)`).
    /// The receiver exemption (`TRAITS.md` Part 2, `MUTABILITY.md`'s
    /// amendment): a mutating method takes no `mut` marker at the dot call
    /// site, so the declaration-site guard that marker would otherwise gate
    /// — "is this root actually a mutable binding" — is applied here to the
    /// receiver directly.
    fn check_mut_receiver(&self, root_name: Option<&str>, target_span: Span) -> Result<(), Spanned<TypeError>> {
        let root = root_name.ok_or_else(|| Spanned::from(TypeError {
            msg: "the receiver of a mutating method must be a plain mutable binding, not an expression".to_string()
        }, target_span))?;
        match self.ctx.is_mutable(root) {
            None => Err(Spanned::from(TypeError {
                msg: format!("'{}' is not declared", root)
            }, target_span)),
            Some(false) => Err(Spanned::from(TypeError {
                msg: format!("'{}' is not mutable — declare it with 'mut {} = ...' to call a mutating method on it", root, root)
            }, target_span)),
            Some(true) => Ok(()),
        }
    }

    fn check_mut_exclusivity(&self, mut_args: &[bool], all_roots: &[Option<(String, Span)>]) -> Result<(), Spanned<TypeError>> {
        for (i, is_mut) in mut_args.iter().enumerate() {
            if !is_mut { continue; }
            // A `mut` argument is always a bare identifier — the check
            // above rejects anything else — so this entry is `Some`.
            let Some((root, root_span)) = &all_roots[i] else { continue };
            let aliases = all_roots.iter().enumerate()
                .any(|(j, other)| j != i && other.as_ref().is_some_and(|(n, _)| n == root));
            if aliases {
                return Err(Spanned::from(TypeError {
                    msg: format!("'{}' can't be passed 'mut' and also appear as another argument in the same call", root)
                }, *root_span));
            }
        }
        Ok(())
    }

    /// The type of a field named `field` on an already-resolved type, or
    /// `None` if there is no such field — used by `lower_ufcs_call` to
    /// decide resolution step 1 (field access) versus falling through to
    /// step 3 (a free function). Mirrors `lower_field_access`'s
    /// struct/union lookup, but takes an already-resolved `Type` instead
    /// of lowering the target itself, since the caller has already done
    /// that once and must not do it again (the target may have side
    /// effects).
    fn field_type_of(&mut self, resolved: &Type, field: &str) -> Option<Type> {
        if resolved.as_struct_name().is_some() {
            self.materialize_struct(resolved).iter()
                .find(|(n, _)| n == field)
                .map(|(_, t)| t.clone())
        } else if let Some((_, def)) = self.resolve_union(resolved) {
            def.common.iter().find(|(n, _)| n == field).map(|(_, t)| t.clone())
        } else {
            None
        }
    }

    /// `x.f(args)` where `f` is not a field of `typeof(x)` directly on the
    /// `Call` node — i.e. resolution steps 1 and 3 of `TRAITS.md` Part 1.
    /// Step 2 (trait members) doesn't exist yet — there is no `provides`
    /// body and no impl registry — so this is the whole of Stage 0.
    ///
    /// `fa.target` is lowered exactly once, up front; both the step-1 and
    /// step-3 branches below reuse that single `TypedExpr` rather than
    /// re-lowering the raw expression, since the target may have side
    /// effects (`get_list().push(x)` must call `get_list()` once).
    /// `Trait.member(args)` — the prefix form (`TRAITS.md` Part 1).
    ///
    /// Rewritten into the dot form over its first argument and handed to
    /// `lower_ufcs_call`, with `expect_trait` set so that naming the wrong
    /// trait is an error rather than a silent call to whichever impl happens
    /// to own that member name. The rewrite is exact for every member with a
    /// `Self` parameter, which is all of them today: a zero-`Self` member has
    /// no argument to dispatch on and is resolved by expected type instead
    /// (`TRAITS.md` Stage 5's zero-`Self` case), reported here rather than
    /// mis-resolved.
    fn lower_trait_prefix_call(
        &mut self,
        trait_name: &str,
        member: String,
        args: Vec<Spanned<Expression>>,
        callee_span: Span,
        span: Span,
    ) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let def = self.traits.get(trait_name).expect("caller checked membership");
        let Some(sig) = def.members.iter().find(|m| m.name == member) else {
            return Err(Spanned::from(TypeError {
                msg: format!("trait '{}' has no member '{}'", trait_name, member)
            }, callee_span));
        };
        let dispatch_on_first = sig.params.first()
            .map(|(_, ty, _)| Self::mentions_self(ty))
            .unwrap_or(false);
        if !dispatch_on_first {
            return Err(Spanned::from(TypeError {
                msg: format!(
                    "'{}.{}' takes no Self argument to dispatch on; resolving it from the expected return type isn't supported yet",
                    trait_name, member
                )
            }, callee_span));
        }
        let mut args = args.into_iter();
        let Some(receiver) = args.next() else {
            return Err(Spanned::from(TypeError {
                msg: format!("'{}.{}' needs at least one argument", trait_name, member)
            }, callee_span));
        };
        let fa = FieldAccessExpr { target: Box::new(receiver), field: member };
        self.lower_ufcs_call(fa, args.collect(), callee_span, span, Some(trait_name.to_string()))
    }

    /// The dispatch arms for a member call on a union, or `None` if this
    /// isn't one — because some member has no impl of `member`, or because
    /// two members implement it from *different* traits, which would make
    /// `u.f()` mean two unrelated things depending on the runtime tag.
    ///
    /// Returning `None` rather than erroring lets the caller fall through to
    /// step 3, so a free function taking the whole union still wins where one
    /// exists and the error, when there is none, is the ordinary "no such
    /// member or function" one.
    ///
    /// `expect_trait` is the prefix form's named trait, checked here rather
    /// than at the caller: the arms are built and returned, so the caller's
    /// own check is past by then, and each arm re-lowers as an *unqualified*
    /// call that would silently accept whatever impl owns the name.
    fn union_member_arms(
        &mut self,
        resolved: &Type,
        member: &str,
        rest_args: &[Spanned<Expression>],
        narrow_target: Option<&str>,
        expect_trait: Option<&str>,
        subject_span: Span,
        span: Span,
    ) -> Result<Option<Vec<MatchArm>>, Spanned<TypeError>> {
        let entries = self.union_entries(resolved, subject_span, span)?;
        if entries.is_empty() { return Ok(None); }
        let mut owner: Option<String> = None;
        for e in &entries {
            let hit = Self::grant_key(&e.ty)
                .and_then(|key| self.member_index.get(&(key, member.to_string())).cloned());
            match (hit, &owner) {
                (None, _) => return Ok(None),
                (Some((tr, _)), None) => owner = Some(tr),
                (Some((tr, _)), Some(prev)) if &tr != prev => return Ok(None),
                (Some(_), Some(_)) => {}
            }
        }
        if let (Some(want), Some(owner)) = (expect_trait, &owner) {
            if owner != want {
                return Err(Spanned::from(TypeError {
                    msg: format!("{} implements '{}' from trait '{}', not '{}'", resolved, member, owner, want)
                }, span));
            }
        }
        // Each arm's receiver has to be the *narrowed* binding, not
        // `union_entries`' reconstructed value: reconstructing a variant
        // yields the union type again (variants are boxed into it), so the
        // arm body would re-enter this same dispatch forever. Narrowing is
        // what gives the arm a receiver typed at the member — and it only
        // works on a name, so a receiver that isn't a plain binding is
        // rejected with the fix rather than mis-dispatched.
        let Some(recv) = narrow_target else {
            return Err(Spanned::from(TypeError {
                msg: format!(
                    "calling '{}' on a union needs the receiver to be a plain binding, so each member can be narrowed — bind it first, e.g. `let x = ...` then `x.{}(...)`",
                    member, member
                )
            }, subject_span));
        };
        let mut arms = Vec::with_capacity(entries.len());
        for e in entries {
            // `rest_args` is cloned per arm, so a side-effecting argument is
            // *written* once per member but still *runs* once — only the
            // matching arm executes. Same duplication `lower_match`'s guard
            // clauses already accept.
            let callee = Spanned::from(
                Expression::field_access(
                    Spanned::from(Expression::literal(Token::Identifier(recv.to_string())), span),
                    member.to_string(),
                ),
                span,
            );
            let body = Expression::call(callee, rest_args.to_vec());
            arms.push(MatchArm {
                pattern: Pattern {
                    path: None,
                    variant: e.pattern_variant,
                    // Deliberately bind-less: `lower_match_lowered` applies
                    // flow narrowing only to an arm that binds no fields
                    // (binding fields is the *other* way to read a variant),
                    // and narrowing the receiver's own name is exactly what
                    // this dispatch needs.
                    binds: Vec::new(),
                    resolved_member: e.member_idx,
                },
                guard: None,
                body: Box::new(Spanned::from(body, span)),
            });
        }
        Ok(Some(arms))
    }

    /// Lower `x.member(...)` where `x`'s type is a *bounded type parameter*
    /// — `func f<T: Shape>(x: T) = x.area()` — and `trait_name` is the bound
    /// that declares `member`.
    ///
    /// Every other member call resolves to an impl's compiled symbol right
    /// here, by looking `(type key, member)` up in `member_index`. This one
    /// cannot: `T` is not a type yet, and does not become one until
    /// `monomorphize_generics` clones this body per instantiation. So the
    /// call is checked against the *trait's* signature with `Self` standing
    /// for `T` — which is exactly what the bound licenses, and what makes
    /// the arity, argument, and result types checkable once at the
    /// declaration rather than once per instantiation — and its callee is
    /// left as a pending symbol (`pending_member_symbol`) carrying the trait
    /// and member name.
    ///
    /// The receiver's own type rides along in the callee node's function
    /// type, so no side table is needed: `substitute_types_deep` rewrites
    /// that type like any other when the clone's binders are substituted,
    /// and `resolve_bound_members` then reads the now-concrete `Self` back
    /// out of it and writes the real symbol in. A pending symbol that
    /// somehow survived to codegen would be an unknown function, so
    /// `monomorphize_generics` checks for leftovers rather than trusting it.
    fn lower_bound_member_call(
        &mut self,
        trait_name: &str,
        member: &str,
        target: Spanned<TypedExpr>,
        self_ty: Type,
        root_name: Option<String>,
        rest_args: Vec<Spanned<Expression>>,
        target_span: Span,
        callee_span: Span,
        span: Span,
    ) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let sig = self.traits.get(trait_name)
            .and_then(|d| d.members.iter().find(|m| m.name == member))
            .cloned()
            .expect("caller found this member on this trait");
        // `Self` -> the binder variable. The stored signature is a template
        // and must never be unified against directly (see `TraitMemberSig`);
        // substituting first is what keeps one instantiation's `Self` out of
        // every other's.
        let subst = HashMap::from([(SELF_BINDER.to_string(), self_ty.clone())]);
        let params: Vec<(String, Type, bool)> = sig.params.iter()
            .map(|(n, t, m)| (n.clone(), t.substitute(&subst), *m))
            .collect();
        let result = sig.return_type.substitute(&subst);
        // A member whose first parameter isn't `Self` has nothing to
        // dispatch on — it is the zero-`Self` shape, called by name or
        // through the prefix form, not on a receiver.
        if !sig.params.first().map(|(_, t, _)| Self::mentions_self(t)).unwrap_or(false) {
            return Err(Spanned::from(TypeError {
                msg: format!(
                    "'{}.{}' takes no Self parameter, so it can't be called on a value — call it as '{}.{}(...)' and annotate the expected type",
                    trait_name, member, trait_name, member
                )
            }, span));
        }
        if rest_args.len() != params.len() - 1 {
            return Err(Spanned::from(TypeError {
                msg: format!("Wrong number of arguments, expected {}, got {}", params.len() - 1, rest_args.len())
            }, callee_span));
        }
        let mut_first = params[0].2;
        // Same receiver exemption as a concrete member call: no `mut` marker
        // at the dot call site, but the root still has to be a mutable
        // binding.
        if mut_first {
            self.check_mut_receiver(root_name.as_deref(), target_span)?;
        }

        let fn_ty = Type::Function {
            params: params.iter().map(|(_, t, _)| t.clone()).collect(),
            result: Box::new(result.clone()),
        };
        let callable = Spanned::from(TypedExpr {
            id: 0,
            ty: fn_ty,
            kind: TypedExprKind::Var(Self::pending_member_symbol(trait_name, member)),
        }, callee_span);

        let mut args: Vec<Arg> = Vec::with_capacity(params.len());
        let mut mut_args = Vec::with_capacity(params.len());
        let mut all_roots: Vec<Option<(String, Span)>> = Vec::with_capacity(params.len());
        all_roots.push(root_name.clone().map(|n| (n, target_span)));
        // The receiver of a mutating method takes no `mut` marker (`TRAITS.md`
        // Part 2), but it is still a `mut` argument, so it rides in `Arg::Mut`
        // like any other. `check_mut_receiver` above has already established
        // that it is a bare mutable binding, hence the empty path — unlike
        // `push(mut b.items, v)`, a user-defined mutating *method* still
        // can't be called on a path receiver. See roadmap.md.
        args.push(if mut_first {
            Arg::Mut(Place { root: root_name.expect("check_mut_receiver requires a root"), path: Vec::new() })
        } else {
            Arg::Value(self.lower_widen(target, &params[0].1)?)
        });
        mut_args.push(mut_first);
        for (i, (arg, (_, param, declared))) in rest_args.into_iter().zip(params[1..].iter()).enumerate() {
            let (lowered, is_mut, root) = self.lower_call_arg(i + 1, arg, param, Some(*declared))?;
            args.push(lowered);
            mut_args.push(is_mut);
            all_roots.push(root);
        }
        self.check_mut_exclusivity(&mut_args, &all_roots)?;

        let ty = self.lookup(&result);
        Ok(Spanned::from(TypedExpr {
            id: 0, ty,
            kind: TypedExprKind::Call { callable: Box::new(callable), args },
        }, span))
    }

    fn lower_ufcs_call(&mut self, fa: FieldAccessExpr, rest_args: Vec<Spanned<Expression>>, callee_span: Span, span: Span, expect_trait: Option<String>) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let target_span = fa.target.span;
        // Captured before the target is lowered — only a bare identifier
        // can be the root of a `mut` receiver, same restriction ordinary
        // `mut` arguments have (`lower_call_arg`).
        let root_name = fa.target.item.get_identifier().map(|s| s.to_string());
        // `push` needs the *untyped* receiver to resolve it as a place
        // (`lower_place`), and which builtin this is isn't known until
        // after the target is lowered — so keep a copy.
        let raw_target = (*fa.target).clone();
        let target = self.check_and_lower(*fa.target)?;
        let resolved = self.lookup(&target.item.ty);

        if let Some(field_ty) = self.field_type_of(&resolved, &fa.field) {
            // Step 1: `f` is a field — e.g. a struct field holding a
            // function value. Build the `FieldAccess` node directly
            // (`target` is already lowered) and hand it to `finish_call`
            // like any other callable expression.
            let enum_name = self.resolve_union(&resolved).map(|(n, _)| n.to_string());
            let callable = Spanned::from(TypedExpr {
                id: 0,
                ty: field_ty,
                kind: TypedExprKind::FieldAccess { target: Box::new(target), field: fa.field, enum_name },
            }, callee_span);
            return self.finish_call(callable, None, rest_args, callee_span, span);
        }

        // Step 2 (`TRAITS.md` Part 1): a trait member implemented for
        // `typeof(target)`. A hash lookup on `(type key, member name)`, never
        // a search — the two ambiguities that would make it one (a duplicate
        // `(trait, type)` impl, and two traits declaring the same member for
        // one type) are both rejected at registration, in `register_impl`.
        //
        // Looked up here, ahead of the `len`/`push`/`get` builtins below,
        // so a user type can name a member `len` and mean it. `List`/`Str`
        // have no impls, so those builtins are unaffected.
        let field = fa.field;
        let member_hit: Option<(String, String)> = Self::grant_key(&resolved)
            .and_then(|key| self.member_index.get(&(key, field.clone())).cloned());
        // From here on `callee` is what gets looked up and named in the typed
        // AST — a member's compiled symbol, or the plain function name for
        // step 3 — while `field` stays what the user wrote, for diagnostics.
        let callee = match &member_hit {
            Some((_, symbol)) => symbol.clone(),
            None => field.clone(),
        };
        // Step 2, union case: a member call on a union is legal iff *every*
        // member implements it, from the same trait — the same
        // "union satisfies a trait iff every member does" rule
        // `type_implements` already applies, and the exact parallel of
        // `lower_field_access` accepting `u.f` only for a common field.
        //
        // It desugars into the dispatch `match` that `?`/`!`/`catch` already
        // build from the same two pieces: `union_entries` reconstructs each
        // member's value from bound fields, and `lower_match_lowered` takes
        // the *already-lowered* subject, so the receiver is still evaluated
        // exactly once. Each arm re-lowers the call against its own narrowed
        // member type, which is what makes step 2 pick that member's impl.
        if member_hit.is_none() && matches!(&resolved, Type::Union(_)) {
            let narrow_target = root_name.clone().filter(|n| self.ctx.contains_key(n));
            if let Some(arms) = self.union_member_arms(
                &resolved, &field, &rest_args, narrow_target.as_deref(),
                expect_trait.as_deref(), target_span, span
            )? {
                return self.lower_match_lowered(target, narrow_target, target_span, arms, None, span);
            }
        }

        // A member call on a *bounded type parameter* — `func f<T: Shape>(x: T) = x.area()`.
        // There is no impl to name yet: `T` only becomes concrete later,
        // when `monomorphize_generics` substitutes into this already-lowered
        // body. So the call is checked here against the *trait's* signature
        // with `Self` standing for `T`, and its callee is left as a pending
        // symbol that monomorphization resolves once `T` is a real type —
        // see `lower_bound_member_call`.
        if member_hit.is_none() {
            if let Type::TypeVar { bounds, .. } = &resolved {
                let bounds = bounds.clone();
                if let Some(tr) = bounds.iter().find(|b| self.traits.get(&b.to_string())
                    .map(|d| d.members.iter().any(|m| m.name == field)).unwrap_or(false))
                {
                    let trait_name = tr.to_string();
                    if let Some(want) = &expect_trait {
                        if want != &trait_name {
                            return Err(Spanned::from(TypeError {
                                msg: format!("type parameter bounded by '{}' doesn't implement '{}.{}'", trait_name, want, field)
                            }, span));
                        }
                    }
                    return self.lower_bound_member_call(
                        &trait_name, &field, target, resolved.clone(), root_name,
                        rest_args, target_span, callee_span, span,
                    );
                }
            }
        }

        // The prefix form named a specific trait; honour it rather than
        // dispatching to whatever impl owns this member name.
        if let Some(want) = &expect_trait {
            match &member_hit {
                Some((owner, _)) if owner == want => {}
                Some((owner, _)) => return Err(Spanned::from(TypeError {
                    msg: format!("{} implements '{}' from trait '{}', not '{}'", resolved, field, owner, want)
                }, span)),
                None => return Err(Spanned::from(TypeError {
                    msg: format!("{} doesn't implement '{}.{}'", resolved, want, field)
                }, span)),
            }
        }

        // Step 3: no such field or member — a global `func` whose first
        // parameter accepts `typeof(target)`, rewritten to
        // `f(target, ...rest_args)`.

        // `len` has no `ctx` entry either (`finish_len`'s comment) — same
        // special-casing as `push` below, minus any `mut` handling since
        // `len` doesn't mutate its receiver.
        if member_hit.is_none() && field == "len" {
            if !rest_args.is_empty() {
                return Err(Spanned::from(TypeError {
                    msg: format!("Wrong number of arguments, expected 0, got {}", rest_args.len())
                }, callee_span));
            }
            return self.finish_len(target, callee_span, span);
        }

        // `push` has no `ctx` entry at all (see `is_push`'s own comment —
        // it's polymorphic over `T` with no generics system to express
        // that), so it's special-cased here the same way `lower_call`
        // special-cases it, with the receiver's `mut` marker exempted.
        if member_hit.is_none() && field == "push" {
            if rest_args.len() != 1 {
                return Err(Spanned::from(TypeError {
                    msg: format!("Wrong number of arguments, expected 1, got {}", rest_args.len())
                }, callee_span));
            }
            let (place, leaf_ty) = self.lower_place(raw_target, target_span)?;
            let v_arg = rest_args.into_iter().next().expect("arity checked just above");
            return self.finish_push(place, leaf_ty, target_span, v_arg, span);
        }

        // `get` has no `ctx` entry either, same reasoning as `len`/`push`
        // above — see `finish_get_ufcs`.
        if member_hit.is_none() && field == "get" {
            if rest_args.len() != 1 {
                return Err(Spanned::from(TypeError {
                    msg: format!("Wrong number of arguments, expected 1, got {}", rest_args.len())
                }, callee_span));
            }
            let i_arg = rest_args.into_iter().next().expect("arity checked just above");
            return self.finish_get_ufcs(target, i_arg, span);
        }

        let Some(func_ty) = self.ctx.get(&callee).cloned() else {
            return Err(Spanned::from(TypeError {
                msg: format!("{} has no field '{}', and there's no function '{}' to call as a method", resolved, field, field)
            }, span));
        };
        // Instantiate `field`'s scheme (`TRAITS.md` Stage 2) before
        // testing receiver acceptance below — this call site doesn't go
        // through `lower_literal`'s `Var` arm (it's synthesizing a callee
        // from a bare name, not lowering an `Expression::Var`), so without
        // this a generic free function's first parameter would bind
        // permanently to whichever type dot-called it first, via the
        // mutating `unify` a few lines down.
        let binders = self.ctx.binders(&callee).expect("just found by get").to_vec();
        let func_ty = self.instantiate(&func_ty, &binders);
        let func_ty = self.lookup(&func_ty);
        let Type::Function { params, result } = func_ty.clone() else {
            return Err(Spanned::from(TypeError {
                msg: format!("{} has no field '{}', and '{}' isn't a function", resolved, field, field)
            }, span));
        };
        if params.is_empty() {
            return Err(Spanned::from(TypeError {
                msg: format!("'{}' takes no arguments, so it can't be called as {}.{}(...)", field, resolved, field)
            }, span));
        }
        let resolved_recv = self.lookup(&params[0]);
        if !widens_to(&resolved, &resolved_recv) && !self.unify(&target.item.ty, &params[0]) {
            return Err(Spanned::from(TypeError {
                msg: format!("{} has no field '{}', and '{}'s first parameter doesn't accept {}", resolved, field, field, resolved)
            }, target_span));
        }
        if rest_args.len() != params.len() - 1 {
            return Err(Spanned::from(TypeError {
                msg: format!("Wrong number of arguments, expected {}, got {}", params.len() - 1, rest_args.len())
            }, callee_span));
        }

        let declared_mut = self.func_mut_params.get(&callee).cloned();
        let mut_first = declared_mut.as_ref().and_then(|d| d.first()).copied().unwrap_or(false);

        if mut_first {
            self.check_mut_receiver(root_name.as_deref(), target_span)?;
        }

        // No `check_and_lower` round-trip for the callee name: the
        // binding is already resolved (`func_ty` above), so the callable
        // is synthesized the same way `push`'s is — including
        // `lower_literal`'s generalized-declaration rename, which this
        // path would otherwise skip, leaving a dot-called generic
        // unresolvable at monomorphization time.
        let callee_sym = self.ctx.symbol(&callee).map(str::to_string).unwrap_or_else(|| callee.clone());
        let callable = Spanned::from(TypedExpr { id: 0, ty: func_ty, kind: TypedExprKind::Var(callee_sym) }, callee_span);

        let mut args: Vec<Arg> = Vec::with_capacity(rest_args.len() + 1);
        let mut mut_args = Vec::with_capacity(rest_args.len() + 1);
        let mut all_roots: Vec<Option<(String, Span)>> = Vec::with_capacity(rest_args.len() + 1);

        all_roots.push(root_name.clone().map(|n| (n, target_span)));
        // The receiver of a mutating method takes no `mut` marker (`TRAITS.md`
        // Part 2), but it is still a `mut` argument, so it rides in `Arg::Mut`
        // like any other. `check_mut_receiver` above has already established
        // that it is a bare mutable binding, hence the empty path — unlike
        // `push(mut b.items, v)`, a user-defined mutating *method* still
        // can't be called on a path receiver. See roadmap.md.
        args.push(if mut_first {
            Arg::Mut(Place { root: root_name.expect("check_mut_receiver requires a root"), path: Vec::new() })
        } else {
            Arg::Value(self.lower_widen(target, &params[0])?)
        });
        mut_args.push(mut_first);

        for (i, (arg, param)) in rest_args.into_iter().zip(params[1..].iter()).enumerate() {
            let declared = declared_mut.as_ref().and_then(|d| d.get(i + 1)).copied();
            let (lowered, is_mut, root) = self.lower_call_arg(i + 1, arg, param, declared)?;
            args.push(lowered);
            mut_args.push(is_mut);
            all_roots.push(root);
        }

        self.check_mut_exclusivity(&mut_args, &all_roots)?;

        let ty = self.lookup(&result);
        Ok(Spanned::from(TypedExpr { id: 0, ty, kind: TypedExprKind::Call { callable: Box::new(callable), args } }, span))
    }

    fn lower_tuple(&mut self, elems: Vec<Spanned<Expression>>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let (kind, ty) = if elems.is_empty() {
            (TypedExprKind::List(Vec::new()), Type::list(self.fresh_var()))
        } else {
            let mut items: Vec<Spanned<TypedExpr>> = Vec::with_capacity(elems.len());
            let mut first_ty: Option<Type> = None;
            for e in elems {
                let elem_span = e.span;
                let lowered = self.check_and_lower(e)?;
                match &first_ty {
                    None => first_ty = Some(lowered.item.ty.clone()),
                    Some(first) => {
                        let first = first.clone();
                        if !self.unify(&first, &lowered.item.ty) {
                            return Err(Spanned::from(TypeError {
                                msg: format!(
                                    "List elements must have the same type, got {} and {}",
                                    self.lookup(&first), self.lookup(&lowered.item.ty)
                                )
                            }, elem_span));
                        }
                    },
                }
                items.push(lowered);
            }
            let elem_ty = self.lookup(&first_ty.expect("elems is non-empty"));
            (TypedExprKind::List(items), Type::list(elem_ty))
        };
        Ok(Spanned::from(TypedExpr { id: 0, ty, kind }, span))
    }

    // A block is its own lexical scope: bindings made by a `let`
    // inside it (directly, or via a nested block/conditional branch)
    // must not leak to whatever follows the block. Without this,
    // codegen can be asked to reference an SSA value that only
    // exists on one control-flow path (e.g. one arm of an `if`),
    // which is invalid IR, not just a stale-name bug.
    //
    // `check_and_lower` is only ever called directly (not via
    // `check_and_lower_entry`) on a *nested* block, since the
    // top-level program/REPL entry goes through
    // `check_and_lower_entry` instead, which does not scope.
    fn lower_block(&mut self, mut stmts: Vec<Spanned<Expression>>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        self.hoist_annotation_decls(&stmts)?;
        self.strip_and_validate_annotations(&mut stmts)?;
        self.hoist_trait_names(&stmts)?;
        self.hoist_data_decls(&stmts)?;
        self.hoist_trait_members(&stmts)?;
        self.expand_impls(&mut stmts)?;
        let lowered = self.in_scope(|t| -> Result<Vec<Spanned<TypedExpr>>, Spanned<TypeError>> {
            let mut lowered = Vec::with_capacity(stmts.len());
            for s in stmts {
                if matches!(s.item, Expression::DataDecl(_) | Expression::TraitDecl(_) | Expression::AnnotationDecl(_)) { continue; }
                lowered.push(t.check_and_lower(s)?);
            }
            Ok(lowered)
        })?;
        // Must-handle: every non-tail statement's value is
        // discarded, so none may be a possible Error — see
        // `check_must_handle`. The tail is exempt; its value
        // propagates to whatever position this Block itself sits
        // in, which is checked there instead.
        if let Some((_, rest)) = lowered.split_last() {
            for t in rest {
                self.check_must_handle(t)?;
            }
        }
        let ty = lowered.last().map(|t| t.item.ty.clone()).unwrap_or(Type::None);
        Ok(Spanned::from(TypedExpr { id: 0, ty, kind: TypedExprKind::Block(lowered) }, span))
    }

    // The annotation is absorbed into this node's own type: lower
    // the inner expression *against* it (see `lower_expected`) and
    // reuse the result directly.
    fn lower_annotated(&mut self, a: AnnotatedExpr, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let annotated_ty = self.resolve_type_expr(&a.ty)?;
        let lowered = self.lower_expected(*a.expr, &annotated_ty)?;
        Ok(Spanned::from(TypedExpr { id: 0, ty: lowered.item.ty, kind: lowered.item.kind }, span))
    }

    fn lower_index(&mut self, idx: IndexExpr, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let target_span = idx.target.span;
        let target = self.check_and_lower(*idx.target)?;
        let target_ty = target.item.ty.clone();
        let resolved_target = self.lookup(&target_ty);
        let elem_ty = match resolved_target.as_list_elem() {
            Some(inner) => inner.clone(),
            None if matches!(&resolved_target, Type::TypeVar { .. }) => {
                let elem = self.fresh_var();
                if !self.unify(&target_ty, &Type::list(elem.clone())) {
                    return Err(Spanned::from(TypeError {
                        msg: format!("Can't index into {}", resolved_target)
                    }, target_span));
                }
                elem
            },
            _ => return Err(Spanned::from(TypeError {
                msg: format!("Can't index into {}, expected a List", resolved_target)
            }, target_span)),
        };

        let index_span = idx.index.span;
        let index = self.check_and_lower(*idx.index)?;
        if !self.unify(&index.item.ty, &Type::Int) {
            return Err(Spanned::from(TypeError {
                msg: format!("List index must be Int, got {}", self.lookup(&index.item.ty))
            }, index_span));
        }

        let ty = self.lookup(&elem_ty);
        Ok(Spanned::from(TypedExpr { id: 0, ty, kind: TypedExprKind::Index { target: Box::new(target), index: Box::new(index) } }, span))
    }

    fn lower_slice(&mut self, s: SliceExpr, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let target_span = s.target.span;
        let target = self.check_and_lower(*s.target)?;
        let target_ty = target.item.ty.clone();
        let resolved_target = self.lookup(&target_ty);
        let list_ty = if resolved_target.is_list() {
            resolved_target.clone()
        } else if matches!(&resolved_target, Type::TypeVar { .. }) {
            let elem = self.fresh_var();
            let list_ty = Type::list(elem);
            if !self.unify(&target_ty, &list_ty) {
                return Err(Spanned::from(TypeError {
                    msg: format!("Can't slice {}", resolved_target)
                }, target_span));
            }
            list_ty
        } else {
            return Err(Spanned::from(TypeError {
                msg: format!("Can't slice {}, expected a List", resolved_target)
            }, target_span));
        };

        let mut bounds = Vec::with_capacity(2);
        for bound in [s.start, s.end] {
            bounds.push(match bound {
                Some(e) => {
                    let bound_span = e.span;
                    let lowered = self.check_and_lower(*e)?;
                    if !self.unify(&lowered.item.ty, &Type::Int) {
                        return Err(Spanned::from(TypeError {
                            msg: format!("Slice bound must be Int, got {}", self.lookup(&lowered.item.ty))
                        }, bound_span));
                    }
                    Some(Box::new(lowered))
                },
                None => None,
            });
        }
        let end = bounds.pop().expect("two bounds pushed");
        let start = bounds.pop().expect("two bounds pushed");

        let ty = self.lookup(&list_ty);
        Ok(Spanned::from(TypedExpr { id: 0, ty, kind: TypedExprKind::Slice { target: Box::new(target), start, end } }, span))
    }

    fn lower_range(&mut self, r: RangeExpr, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let start_span = r.start.span;
        let start = self.check_and_lower(*r.start)?;
        if !self.unify(&start.item.ty, &Type::Int) {
            return Err(Spanned::from(TypeError {
                msg: format!("Range start must be Int, got {}", self.lookup(&start.item.ty))
            }, start_span));
        }
        let end_span = r.end.span;
        let end = self.check_and_lower(*r.end)?;
        if !self.unify(&end.item.ty, &Type::Int) {
            return Err(Spanned::from(TypeError {
                msg: format!("Range end must be Int, got {}", self.lookup(&end.item.ty))
            }, end_span));
        }
        let ty = Type::range(Type::Int);
        // Eagerly register `Range<Int>`'s concrete layout so codegen's
        // `struct_fields` never sees an unregistered instantiation (which
        // would otherwise silently flatten to zero leaves via
        // `unwrap_or_default` — a silent-corruption failure mode, not a
        // panic). Mirrors how a real struct-construction call site always
        // materializes its own layout before lowering can succeed.
        self.materialize_struct(&ty);
        Ok(Spanned::from(TypedExpr { id: 0,
            ty,
            kind: TypedExprKind::Range { start: Box::new(start), end: Box::new(end) },
        }, span))
    }

    fn lower_for_loop_expr(&mut self, fl: ForLoopExpr, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let (var, iterable, cond, body, iter_via) = self.lower_for_loop(fl)?;
        // Each iteration discards the body's value exactly like a
        // non-tail Block statement does — same must-handle rule.
        self.check_must_handle(&body)?;
        Ok(Spanned::from(TypedExpr { id: 0, ty: Type::None, kind: TypedExprKind::ForLoop { var, iterable, cond, body, iter_via } }, span))
    }

    fn lower_comprehension(&mut self, inner: Spanned<Expression>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let fl = match inner.item {
            Expression::ForLoop(fl) => fl,
            _ => unreachable!("Comprehension always wraps a ForLoop — see Grammar::tuple"),
        };
        // A comprehension collects the body's value into the
        // result list rather than discarding it, so must-handle
        // does not apply here — unlike a plain ForLoop.
        let (var, iterable, cond, body, iter_via) = self.lower_for_loop(fl)?;
        let ty = Type::list(self.lookup(&body.item.ty));
        Ok(Spanned::from(TypedExpr { id: 0, ty, kind: TypedExprKind::Comprehension { var, iterable, cond, body, iter_via } }, span))
    }

    fn lower_field_access(&mut self, fa: FieldAccessExpr, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let target_span = fa.target.span;
        let target = self.check_and_lower(*fa.target)?;
        let resolved = self.lookup(&target.item.ty);
        // A member read as a value, not called. Froglang has no runtime
        // representation of a function at all (`TRAITS.md` Stage 3's
        // follow-up), so this can only ever be a call whose parentheses were
        // left off — say that, rather than "no such field", which names the
        // symptom instead of the fix.
        // Gated on the field genuinely being absent: step 1 wins
        // unconditionally, so a member that shares a field's name must not
        // change what `p.n` means.
        if self.field_type_of(&resolved, &fa.field).is_none() {
            if let Some((owner, _)) = Self::grant_key(&resolved)
                .and_then(|k| self.member_index.get(&(k, fa.field.clone())))
            {
                return Err(Spanned::from(TypeError {
                    msg: format!(
                        "'{}' is a member of trait '{}', not a field — call it: x.{}(...)",
                        fa.field, owner, fa.field
                    )
                }, span));
            }
        }
        let (kind, ty) = if resolved.as_struct_name().is_some() {
            let field_ty = self.materialize_struct(&resolved).into_iter()
                .find(|(n, _)| *n == fa.field)
                .map(|(_, t)| t)
                .ok_or_else(|| Spanned::from(TypeError {
                    msg: format!("Struct {} has no field '{}'", resolved, fa.field)
                }, span))?;
            (
                TypedExprKind::FieldAccess { target: Box::new(target), field: fa.field, enum_name: None },
                field_ty,
            )
        } else if let Some((ename, def)) = self.resolve_union(&resolved).map(|(n, d)| (n.to_string(), d.clone())) {
            // Only common fields (declared on the union head) are
            // readable without matching — a variant-only field
            // requires a `match`/`is` to narrow the value first (see
            // DESIGN.md's `shape.r` example).
            if let Some((_, t)) = def.common.iter().find(|(n, _)| *n == fa.field) {
                let field_ty = t.clone();
                (
                    TypedExprKind::FieldAccess { target: Box::new(target), field: fa.field, enum_name: Some(ename) },
                    field_ty,
                )
            } else if def.variants.iter().any(|(_, fs)| fs.iter().any(|(n, _)| *n == fa.field)) {
                return Err(Spanned::from(TypeError {
                    msg: format!("'{}' is a variant-specific field of {} — match on it to access it", fa.field, ename)
                }, span));
            } else {
                return Err(Spanned::from(TypeError {
                    msg: format!("{} has no field '{}'", ename, fa.field)
                }, span));
            }
        } else {
            return Err(Spanned::from(TypeError {
                msg: format!("Can't access field '{}' on {}, expected a struct or union", fa.field, resolved)
            }, target_span));
        };
        Ok(Spanned::from(TypedExpr { id: 0, ty, kind }, span))
    }

    // Standalone (non-if-condition) `subject is Variant`/`subject is
    // Type` — a plain tag test. Binds are rejected: there is no
    // `then`-scope for them to enter (`if subject is P then ...` is
    // intercepted by `lower_conditional`, which routes through
    // `lower_match` so its binds *are* reachable).
    fn lower_is_pattern(&mut self, ip: IsPatternExpr, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let subject_span = ip.subject.span;
        let target = self.check_and_lower(*ip.subject)?;
        let resolved = self.lookup(&target.item.ty);
        let nominal = self.resolve_union(&resolved).map(|(n, d)| (n.to_string(), d.clone()));
        let kind = match nominal {
            Some((enum_name, def)) => {
                let idx = self.check_pattern(&ip.pattern, &enum_name, &def, span)?;
                TypedExprKind::IsVariant {
                    target: Box::new(target), enum_name, variant: ip.pattern.variant.clone(), tag: idx as u32,
                }
            },
            None => {
                let Type::Union(members) = &resolved else {
                    return Err(Spanned::from(TypeError {
                        msg: format!("Can only use 'is' on a union value, got {}", resolved)
                    }, subject_span));
                };
                let (idx, _) = self.check_type_pattern(&ip.pattern, members, span)?;
                TypedExprKind::TypeTag { target: Box::new(target), tag: idx as u32 }
            },
        };
        if !ip.pattern.binds.is_empty() {
            return Err(Spanned::from(TypeError {
                msg: "pattern bindings with 'is' are only allowed as the entire condition of an 'if'".to_string()
            }, span));
        }
        Ok(Spanned::from(TypedExpr { id: 0, ty: Type::Bool, kind }, span))
    }

    // `return`, or `return value`. Always typed `Never` — see
    // `Type::Never` and `TypeChecker::return_types`.
    //
    // With a declared enclosing return type, `lower_expected` is
    // used: it gives subtype acceptance (returning an `Int` into a
    // declared `Int | Str` works), inserts the matching `Widen`, and
    // pushes expected types down into an unannotated lambda literal.
    // But an *unannotated* function's return type is a fresh,
    // still-unbound type var (see `lower_function`), and
    // `lower_expected` assumes its expectation is already concrete —
    // so that case unifies directly instead, exactly like any other
    // site that pins down a fresh var from a synthesized type.
    fn lower_return(&mut self, value: Option<Box<Spanned<Expression>>>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let return_ty = self.return_types.last().cloned().ok_or_else(|| Spanned::from(
            TypeError { msg: "'return' used outside of a function".to_string() }, span
        ))?;
        let resolved_return_ty = self.lookup(&return_ty);
        let still_unbound = matches!(resolved_return_ty, Type::TypeVar { .. });

        let value = match value {
            Some(v) if still_unbound => {
                let lowered = self.check_and_lower(*v)?;
                if !self.unify(&lowered.item.ty, &return_ty) {
                    return Err(Spanned::from(TypeError {
                        msg: format!("Function's return statements disagree: {} vs {}", self.lookup(&return_ty), lowered.item.ty)
                    }, span));
                }
                Some(Box::new(lowered))
            },
            Some(v) => Some(Box::new(self.lower_expected(*v, &resolved_return_ty)?)),
            None if still_unbound => {
                if !self.unify(&Type::None, &return_ty) {
                    return Err(Spanned::from(TypeError {
                        msg: format!("Function's return statements disagree: {} vs None", self.lookup(&return_ty))
                    }, span));
                }
                None
            },
            None => {
                self.check_ty(Type::None, &resolved_return_ty, span)?;
                None
            },
        };
        Ok(Spanned::from(TypedExpr { id: 0, ty: Type::Never, kind: TypedExprKind::Return(value) }, span))
    }

    /// Shared lowering for `for var in iterable (if cond)? body`, used by
    /// both `ForLoop` and `Comprehension`. `var` is bound to the iterable's
    /// element type for `cond`/`body` only, then popped. Also does the
    /// checking a `for` needs: the iterable really is a `List` (unifying
    /// against `List(elem)` if its type is still open), and the optional
    /// guard is a condition.
    fn lower_for_loop(&mut self, fl: ForLoopExpr) -> Result<
        (String, Box<Spanned<TypedExpr>>, Option<Box<Spanned<TypedExpr>>>, Box<Spanned<TypedExpr>>, Option<IterVia>),
        Spanned<TypeError>
    > {
        let iterable_span = fl.iterable.span;
        let iterable = self.check_and_lower(*fl.iterable)?;
        let iter_ty = iterable.item.ty.clone();
        let resolved_iter = self.lookup(&iter_ty);
        let (elem_ty, iter_via) = if let Some(inner) = resolved_iter.as_list_elem() {
            (inner.clone(), None)
        } else if let Some(inner) = resolved_iter.as_range_elem() {
            (inner.clone(), None)
        } else if matches!(&resolved_iter, Type::TypeVar { .. }) {
            let elem = self.fresh_var();
            if !self.unify(&iter_ty, &Type::list(elem.clone())) {
                return Err(Spanned::from(TypeError {
                    msg: format!("Can't iterate over {}", resolved_iter)
                }, iterable_span));
            }
            (elem, None)
        } else if let Some(via) = self.lower_iter_via(&resolved_iter, iterable_span)? {
            let elem = via.next_call.item.ty.clone();
            let elem = Self::optional_item_ty(&elem)
                .unwrap_or_else(|| unreachable!("lower_iter_via's next_call always returns Item?"));
            (elem, Some(via))
        } else {
            return Err(Spanned::from(TypeError {
                msg: format!("Can't iterate over {}, expected a List, Range, or a type providing Iterable", resolved_iter)
            }, iterable_span));
        };

        // The loop variable is bound for the guard and body only. The
        // immediately-invoked closure that used to be needed here so an
        // early `?` couldn't skip the restore is now `in_scope`'s job.
        let (cond, body) = self.with_context(
            std::iter::once((fl.var.clone(), elem_ty)),
            |t| -> Result<_, Spanned<TypeError>> {
                let cond = match fl.cond {
                    Some(c) => {
                        let cond_span = c.span;
                        let lowered = t.check_and_lower(*c)?;
                        t.check_condition(&lowered.item.ty.clone(), cond_span)?;
                        Some(Box::new(t.coerce_truthy(lowered, cond_span)))
                    },
                    None => None,
                };
                let body = t.check_and_lower(*fl.body)?;
                Ok((cond, body))
            },
        )?;
        Ok((fl.var, Box::new(iterable), cond, Box::new(body), iter_via))
    }

    /// The `Item` half of an `Item?` (`Type::Union([Item, None])`) result —
    /// the shape `Iterable::next`'s declared return type is always checked
    /// against at registration (`check_impl_member`), so this is a pure
    /// destructure, not a fallible narrowing.
    fn optional_item_ty(opt_ty: &Type) -> Option<Type> {
        match opt_ty {
            Type::Union(members) if members.len() == 2 => {
                members.iter().find(|m| **m != Type::None).cloned()
            }
            _ => None,
        }
    }

    /// The `Iterable<Item>` trait-dispatch fallback for `for x in y do` —
    /// `Some(IterVia)` iff `resolved_iter` has a registered `Iterable`
    /// impl providing `next`, `None` if it has no impl at all (the ordinary
    /// "not iterable" case, left for the caller to report). Builds a fresh
    /// internal `mut` binding for the iterable's value and a fully-typed
    /// `<binding>.next()` call to its resolved symbol — see `IterVia`'s own
    /// doc comment for why this needs no further codegen support beyond an
    /// ordinary function call.
    fn lower_iter_via(&mut self, resolved_iter: &Type, span: Span) -> Result<Option<IterVia>, Spanned<TypeError>> {
        let Some(type_key) = Self::grant_key(resolved_iter) else { return Ok(None) };
        let Some((trait_name, symbol)) = self.member_index.get(&(type_key, "next".to_string())).cloned() else {
            return Ok(None);
        };
        if trait_name != "Iterable" {
            // Some other trait happens to declare a `next` member — not
            // this one's business; report "not iterable" like any type
            // with no `Iterable` impl at all.
            return Ok(None);
        }
        // `member_index` is populated for the whole program by
        // `expand_impls` before any statement is lowered, but a member's
        // type only lands in `ctx` once its (position-preserving) renamed
        // declaration is itself lowered — so a `for` that textually
        // precedes its iterable's `provides Iterable` sees the symbol here
        // but not yet in `ctx`. Report it like any other forward reference
        // rather than tripping the invariant `register_impl` relies on for
        // every *already-lowered* impl.
        let Some(next_ty) = self.ctx.get(&symbol).cloned() else {
            return Err(Spanned::from(TypeError {
                msg: format!(
                    "'{}' provides 'Iterable', but that impl is declared later in the program — move it before this use",
                    resolved_iter
                )
            }, span));
        };
        let Type::Function { params, result } = next_ty else {
            unreachable!("a trait member's resolved type is always Function")
        };

        let iter_var = format!("#iter{}", self.next_id);
        self.next_id += 1;

        let callable = Spanned::from(TypedExpr {
            id: 0,
            ty: Type::Function { params: params.clone(), result: result.clone() },
            kind: TypedExprKind::Var(symbol),
        }, span);
        let next_call = Spanned::from(TypedExpr {
            id: 0,
            ty: *result,
            kind: TypedExprKind::Call {
                callable: Box::new(callable),
                args: vec![Arg::Mut(Place { root: iter_var.clone(), path: Vec::new() })],
            },
        }, span);
        Ok(Some(IterVia { iter_var, next_call: Box::new(next_call) }))
    }

    /// Lower `match subject { arms... (else default)? }` — and, via
    /// `ConditionalExpr`'s special-case, `if subject is Pattern then A
    /// (else B)?` too (a single synthesized arm). `match` has no runtime
    /// representation of its own: it desugars entirely into ordinary
    /// `Conditional`/`Assign`/`IsVariant`/`VariantField` nodes, built
    /// right-to-left so each arm's "else" is the chain already built for
    /// the arms after it. `subject` is bound to a temporary first (mirrors
    /// `desugar_struct_eq`) so a side-effecting subject expression is only
    /// evaluated once, not once per arm's tag test.
    fn lower_match(&mut self, subject: Spanned<Expression>, arms: Vec<MatchArm>, default: Option<Box<Spanned<Expression>>>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let (subject, narrow_target, subject_span) = self.lower_match_subject(subject)?;
        self.lower_match_lowered(subject, narrow_target, subject_span, arms, default, span)
    }

    /// Lower a match subject, also capturing the flow-narrowing target —
    /// the subject's own name, when it *is* just a name that's already
    /// bound. Has to be read off the untyped expression, before lowering
    /// consumes it; see the narrowing prelude in `lower_match_lowered`.
    ///
    /// Split out so `?`/`!`/`catch` can lower their subject once, read its
    /// type to build their desugared arms, and hand the already-lowered
    /// subject straight to `lower_match_lowered` — rather than lowering it,
    /// then lowering it again inside `lower_match`.
    fn lower_match_subject(&mut self, subject: Spanned<Expression>) -> Result<(Spanned<TypedExpr>, Option<String>, Span), Spanned<TypeError>> {
        let narrow_target = subject.item.get_identifier()
            .filter(|name| self.ctx.contains_key(name))
            .map(|name| name.to_string());
        let subject_span = subject.span;
        let lowered = self.check_and_lower(subject)?;
        Ok((lowered, narrow_target, subject_span))
    }

    /// `lower_match` with the subject already lowered. Validates the arms
    /// against the subject union (pattern names, bind arity, exhaustiveness)
    /// and then desugars.
    fn lower_match_lowered(&mut self, subject: Spanned<TypedExpr>, narrow_target: Option<String>, subject_span: Span, arms: Vec<MatchArm>, default: Option<Box<Spanned<Expression>>>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let resolved_subject = self.lookup(&subject.item.ty);
        let Some((enum_name, def)) = self.resolve_union(&resolved_subject).map(|(n, d)| (n.to_string(), d.clone())) else {
            if !matches!(&resolved_subject, Type::Union(_)) {
                return Err(Spanned::from(TypeError {
                    msg: format!("Can only match on a union value, got {}", resolved_subject)
                }, subject_span));
            }
            return self.lower_anon_match(subject, narrow_target, arms, default, span);
        };
        let arms = self.expand_trait_arms(arms, &enum_name, &def)?;

        // Validate every arm's pattern (and collect what it covers) before
        // building anything. An unguarded arm counts toward exhaustiveness;
        // a guarded one never does, since the guard might not hold at
        // runtime. Doing this up front is what lets the desugaring below —
        // which runs right-to-left, so it sees the arms in the wrong order
        // to report a missing variant sensibly — assume its patterns are
        // sound and its `tail` is non-empty.
        let mut covered: std::collections::HashSet<usize> = std::collections::HashSet::new();
        for arm in &arms {
            let idx = self.check_pattern(&arm.pattern, &enum_name, &def, arm.body.span)?;
            if arm.guard.is_none() { covered.insert(idx); }
        }
        if default.is_none() && covered.len() < def.variants.len() {
            let missing: Vec<&str> = def.variants.iter().enumerate()
                .filter(|(i, _)| !covered.contains(i))
                .map(|(_, (n, _))| n.as_str())
                .collect();
            return Err(Spanned::from(TypeError {
                msg: format!("Non-exhaustive match on {}: missing {} (add an 'else' arm to handle the rest)", enum_name, missing.join(", "))
            }, span));
        }

        let subject_name = format!("__match_subject_{}", self.next_id); self.next_id += 1;
        // The *resolved* union, not the stored `.ty`: the temporary the
        // subject is bound to below, and every `Var` reading it back, must
        // carry the union type codegen dispatches on, never an unresolved
        // `TypeVar` that merely points at it.
        let subject_ty = resolved_subject.clone();
        let subject_assign = Spanned::from(
            TypedExpr { id: 0, ty: subject_ty.clone(), kind: TypedExprKind::Assign { name: subject_name.clone(), value: Box::new(subject) } },
            span,
        );

        let mut tail: Option<Spanned<TypedExpr>> = match default {
            Some(d) => {
                Some(self.in_scope(|t| t.check_and_lower(*d))?)
            }
            None => None,
        };

        for arm in arms.into_iter().rev() {
            let idx = def.variant_index(&arm.pattern.variant).expect("validated by check_pattern above");
            let variant_fields = def.variants[idx].1.clone();

            let subject_var = Spanned::from(
                TypedExpr { id: 0, ty: subject_ty.clone(), kind: TypedExprKind::Var(subject_name.clone()) },
                span,
            );
            let base_cond = Spanned::from(
                TypedExpr { id: 0, ty: Type::Bool, kind: TypedExprKind::IsVariant {
                    target: Box::new(subject_var.clone()), enum_name: enum_name.clone(), variant: arm.pattern.variant.clone(), tag: idx as u32,
                } },
                span,
            );

            let mut prelude = Vec::new();
            let mut bindings = Vec::new();
            if !arm.pattern.binds.is_empty() {
                for (bind, (fname, fty)) in arm.pattern.binds.iter().zip(variant_fields.iter()) {
                    if bind == "_" { continue; }
                    let value = Spanned::from(
                        TypedExpr { id: 0, ty: fty.clone(), kind: TypedExprKind::VariantField {
                            target: Box::new(subject_var.clone()), enum_name: enum_name.clone(), variant: arm.pattern.variant.clone(), field: fname.clone(),
                        } },
                        span,
                    );
                    prelude.push(Spanned::from(
                        TypedExpr { id: 0, ty: fty.clone(), kind: TypedExprKind::Assign { name: bind.clone(), value: Box::new(value) } },
                        span,
                    ));
                    bindings.push((bind.clone(), fty.clone(), false));
                }
            } else if let Some(name) = &narrow_target {
                // Flow narrowing (ERRORS.md Phase 6): rebind the subject's
                // own name, inside this arm only, to a plain `Type::Struct`
                // value of the matched variant's own type — reconstructed
                // by reading every declared field (common fields, via
                // ordinary `FieldAccess`, then the variant's own, via
                // `VariantField`) back out, in the same common-then-own
                // order `hoist_data_decls` registered under this qualified
                // name in `struct_defs`, so `StructInit`'s flattened
                // codegen layout lines up.
                let qualified = format!("{}.{}", enum_name, arm.pattern.variant);
                let common_fields = def.common.iter().map(|(fname, fty)| {
                    (fname.clone(), Box::new(Spanned::from(
                        TypedExpr { id: 0, ty: fty.clone(), kind: TypedExprKind::FieldAccess {
                            target: Box::new(subject_var.clone()), field: fname.clone(), enum_name: Some(enum_name.clone()),
                        } },
                        span,
                    )))
                });
                let own_fields = variant_fields.iter().map(|(fname, fty)| {
                    (fname.clone(), Box::new(Spanned::from(
                        TypedExpr { id: 0, ty: fty.clone(), kind: TypedExprKind::VariantField {
                            target: Box::new(subject_var.clone()), enum_name: enum_name.clone(), variant: arm.pattern.variant.clone(), field: fname.clone(),
                        } },
                        span,
                    )))
                });
                let narrowed_fields: Vec<(String, TypedExprRef)> = common_fields.chain(own_fields).collect();
                let narrowed_ty = Type::strukt(qualified.clone());
                let value = Spanned::from(
                    TypedExpr { id: 0, ty: narrowed_ty.clone(), kind: TypedExprKind::StructInit { name: qualified, fields: narrowed_fields } },
                    span,
                );
                prelude.push(Spanned::from(
                    TypedExpr { id: 0, ty: narrowed_ty.clone(), kind: TypedExprKind::Assign { name: name.clone(), value: Box::new(value) } },
                    span,
                ));
                // Preserve the subject's own mutability across the rebind
                // — narrowing shouldn't turn a `mut` binding immutable for
                // the arm that's using it.
                let mutable = self.ctx.is_mutable(name).unwrap_or(false);
                bindings.push((name.clone(), narrowed_ty, mutable));
            }

            tail = Some(self.fold_match_arm(arm, prelude, bindings, base_cond, tail, span)?);
        }

        Ok(Self::finish_match_chain(subject_assign, tail, span))
    }

    /// Fold one already-validated match arm into the right-to-left
    /// `Conditional` chain that both `lower_match_lowered` and
    /// `lower_anon_match` build. They differ only in how an arm's pattern
    /// is checked and extracted — `prelude` (the extraction statements),
    /// `bindings` (the names it introduces) and `base_cond` (its tag test)
    /// are that difference, already computed; everything after it is shared.
    ///
    /// `tail` is everything to this arm's right, already folded.
    fn fold_match_arm(
        &mut self,
        arm: MatchArm,
        prelude: Vec<Spanned<TypedExpr>>,
        bindings: Vec<(String, Type, bool)>,
        base_cond: Spanned<TypedExpr>,
        tail: Option<Spanned<TypedExpr>>,
        span: Span,
    ) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        // The arm's pattern binds are visible to its guard and body only.
        // The immediately-invoked closure that used to guarantee the restore
        // happened even on an early `?` is now `in_scope`'s job.
        let (guard, body) = self.with_context_mut(
            bindings.into_iter(),
            |t| -> Result<_, Spanned<TypeError>> {
                let body = t.check_and_lower(*arm.body)?;
                let guard = match arm.guard {
                    Some(g) => {
                        let guard_span = g.span;
                        let lowered = t.check_and_lower(*g)?;
                        t.check_condition(&lowered.item.ty.clone(), guard_span)?;
                        Some(t.coerce_truthy(lowered, guard_span))
                    },
                    None => None,
                };
                Ok((guard, body))
            },
        )?;

        // The rightmost arm of an exhaustive `match` needs no tag test at
        // all. `tail` is `None` only when there is no `else` *and* every arm
        // to this one's right has already been folded in (the fold runs
        // right-to-left), and exhaustiveness was checked before lowering
        // began — so if control reaches this arm, the subject holds this
        // arm's member and the test can only answer `true`. Emitting it
        // anyway costs a `band`/`icmp`/`brif`, a block, and a dead
        // placeholder value per match; on a two-member union — `Int | E`,
        // the shape `?`/`!`/`catch` desugar to — that is *half* of all the
        // tag tests in the program, in the hottest error-handling path
        // there is.
        //
        // Note this is the *innermost* arm of the chain, not the first one
        // reached: every arm to its left tests and branches around it, so
        // control arrives here only after all of them have declined.
        //
        // A guarded last arm keeps its test: `tail` being `None` says the
        // guard can only be `true`, but the guard is an arbitrary
        // expression and may have side effects, so it still has to run.
        //
        // `base_cond` is dropped rather than emitted-and-ignored: it is an
        // `IsVariant`/`TypeTag` on a `Var`, so there is nothing in it to
        // observe.
        if guard.is_none() && tail.is_none() {
            return Ok(Self::arm_body_block(prelude, body, span));
        }

        // A guard's binds must be extracted (the `prelude`) *before* the
        // guard itself runs — they can't be folded into a single
        // `base_cond and guard` boolean the way a bindless guard could,
        // since extraction is a statement, not an expression. So a guarded
        // arm nests one level deeper: the tag test's true branch runs the
        // prelude, then re-tests the guard, only falling through to `tail`
        // (cloned — it's the else of both the tag test and, on guard
        // failure, the inner check too) if that also fails.
        // The condition gating whether this arm's *body* runs: the tag
        // test alone for an unguarded arm, or the tag test *and* the guard
        // for a guarded one — the guard re-runs the (side-effect-free,
        // pattern-extraction-only) `prelude` in its own scoped block so it
        // can see the bound names, short-circuiting under `base_cond` via
        // `joined_conditional` so a false tag test never evaluates it.
        //
        // Folding the guard into `cond` this way, rather than nesting a
        // second `Conditional` inside the tag test's true branch, means
        // `tail` is embedded exactly once below — not once as the tag
        // test's own false branch *and* once more as the guard's. That
        // doubling used to compound arm over arm (each arm's `tail` is the
        // one built by every arm to its right), giving O(2^n) tree size —
        // and O(2^n) compiled code, since codegen has no way to know two
        // syntactically distinct subtrees are the same code — for n
        // consecutive guarded arms. `prelude` is duplicated instead (once
        // here, once in `true_branch` below), which costs nothing per arm
        // rather than everything to the arm's right.
        let cond = match guard {
            None => base_cond,
            Some(g) => {
                let guard_check = if prelude.is_empty() {
                    g
                } else {
                    let mut stmts = prelude.clone();
                    stmts.push(g);
                    Spanned::from(TypedExpr { id: 0, ty: Type::Bool, kind: TypedExprKind::Block(stmts) }, span)
                };
                let false_lit = Spanned::from(TypedExpr { id: 0, ty: Type::Bool, kind: TypedExprKind::BoolLit(false) }, span);
                self.joined_conditional(base_cond, guard_check, Some(false_lit), span)?
            }
        };

        let true_branch = Self::arm_body_block(prelude, body, span);

        self.joined_conditional(cond, true_branch, tail, span)
    }

    /// One arm's pattern extraction followed by its body — a plain `Block`,
    /// or the body alone when the pattern binds nothing.
    fn arm_body_block(
        prelude: Vec<Spanned<TypedExpr>>,
        body: Spanned<TypedExpr>,
        span: Span,
    ) -> Spanned<TypedExpr> {
        if prelude.is_empty() { return body; }
        let inner_ty = body.item.ty.clone();
        let mut stmts = prelude;
        stmts.push(body);
        Spanned::from(TypedExpr { id: 0, ty: inner_ty, kind: TypedExprKind::Block(stmts) }, span)
    }

    /// A `Conditional` typed as the join of its two branches, with each
    /// branch widened to that join — the treatment `check_and_lower`'s own
    /// `Conditional` arm applies, which these hand-built ones need too.
    ///
    /// A `None` `false_branch` is `Never`-typed, not `None`-typed: the only
    /// way `fold_match_arm` reaches this with no tail is on a *guarded*
    /// rightmost arm — every remaining arm has already been folded in (the
    /// fold runs right-to-left) and the match was checked exhaustive before
    /// lowering began, so that path is genuinely unreachable, and only the
    /// guard's side effects are keeping the branch alive at all (an
    /// unguarded rightmost arm returns before it gets here). `Never`
    /// vanishes from the join; `None` would widen every guarded arm's type
    /// to `T | None`.
    fn joined_conditional(
        &mut self,
        cond: Spanned<TypedExpr>,
        true_branch: Spanned<TypedExpr>,
        false_branch: Option<Spanned<TypedExpr>>,
        span: Span,
    ) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let true_ty = true_branch.item.ty.clone();
        let false_ty = false_branch.as_ref().map(|t| t.item.ty.clone()).unwrap_or(Type::Never);
        let result_ty = self.join_types(&true_ty, &false_ty);
        let true_branch = self.lower_widen(true_branch, &result_ty)?;
        let false_branch = match false_branch {
            Some(t) => Some(Box::new(self.lower_widen(t, &result_ty)?)),
            None => None,
        };
        Ok(Spanned::from(
            TypedExpr { id: 0, ty: result_ty, kind: TypedExprKind::Conditional {
                cond: Box::new(cond), true_branch: Box::new(true_branch), false_branch,
            } },
            span,
        ))
    }

    /// The finished match: bind the subject to its temporary, then run the
    /// folded conditional chain.
    fn finish_match_chain(
        subject_assign: Spanned<TypedExpr>,
        tail: Option<Spanned<TypedExpr>>,
        span: Span,
    ) -> Spanned<TypedExpr> {
        let chain = tail.expect("an empty match with no arms and no default was already rejected as non-exhaustive");
        let chain_ty = chain.item.ty.clone();
        Spanned::from(TypedExpr { id: 0, ty: chain_ty, kind: TypedExprKind::Block(vec![subject_assign, chain]) }, span)
    }

    /// `lower_match_lowered`'s counterpart for an *anonymous* union subject
    /// (`subject` already lowered, and already checked to be a union but not
    /// a nominal one). Same nested-`Conditional` desugaring, `TypeTag`/
    /// `Narrow` in place of `IsVariant`/`VariantField`, and a bind (there's
    /// at most one, checked by `check_type_pattern`) is the whole narrowed
    /// member rather than one of its fields.
    fn lower_anon_match(&mut self, subject: Spanned<TypedExpr>, narrow_target: Option<String>, arms: Vec<MatchArm>, default: Option<Box<Spanned<Expression>>>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        // Through `lookup`, matching how the caller decided to come here —
        // the stored `.ty` may still be a `TypeVar` that resolves to the
        // union.
        let members = match self.lookup(&subject.item.ty) {
            Type::Union(members) => members,
            other => unreachable!("lower_anon_match subject must be an anonymous union, got {}", other),
        };
        let arms = self.expand_trait_arms_anon(arms, &members)?;

        // Same up-front validation as the nominal case — see
        // `lower_match_lowered`. Exhaustiveness is over the union's member
        // count rather than a variant count, and the pattern names a bare
        // type (`is Int`) rather than a declared variant.
        let mut covered: std::collections::HashSet<usize> = std::collections::HashSet::new();
        for arm in &arms {
            let (idx, _) = self.check_type_pattern(&arm.pattern, &members, arm.body.span)?;
            if arm.guard.is_none() { covered.insert(idx); }
        }
        if default.is_none() && covered.len() < members.len() {
            let missing: Vec<String> = members.iter().enumerate()
                .filter(|(i, _)| !covered.contains(i))
                .map(|(_, m)| m.to_string())
                .collect();
            return Err(Spanned::from(TypeError {
                msg: format!(
                    "Non-exhaustive match on {}: missing {} (add an 'else' arm to handle the rest)",
                    Type::Union(members.clone()), missing.join(", ")
                )
            }, span));
        }

        let subject_name = format!("__match_subject_{}", self.next_id); self.next_id += 1;
        // See the matching note in `lower_match_lowered`.
        let subject_ty = Type::Union(members.clone());
        let subject_assign = Spanned::from(
            TypedExpr { id: 0, ty: subject_ty.clone(), kind: TypedExprKind::Assign { name: subject_name.clone(), value: Box::new(subject) } },
            span,
        );

        let mut tail: Option<Spanned<TypedExpr>> = match default {
            Some(d) => {
                Some(self.in_scope(|t| t.check_and_lower(*d))?)
            }
            None => None,
        };

        for arm in arms.into_iter().rev() {
            let (idx, member_ty) = self.check_type_pattern(&arm.pattern, &members, arm.body.span)?;

            let subject_var = Spanned::from(
                TypedExpr { id: 0, ty: subject_ty.clone(), kind: TypedExprKind::Var(subject_name.clone()) },
                span,
            );
            let base_cond = Spanned::from(
                TypedExpr { id: 0, ty: Type::Bool, kind: TypedExprKind::TypeTag {
                    target: Box::new(subject_var.clone()), tag: idx as u32,
                } },
                span,
            );

            let mut prelude = Vec::new();
            let mut bindings = Vec::new();
            // An explicit bind takes priority; a bindless arm on an
            // already-bound bare-identifier subject narrows that name
            // instead (ERRORS.md Phase 6's flow narrowing) — `Narrow`
            // already yields the whole member value either way.
            let explicit_bind = arm.pattern.binds.first().cloned();
            let bind_name = explicit_bind.clone().or_else(|| narrow_target.clone());
            if let Some(bind) = bind_name {
                if bind != "_" {
                    let value = Spanned::from(
                        TypedExpr { id: 0, ty: member_ty.clone(), kind: TypedExprKind::Narrow {
                            value: Box::new(subject_var.clone()), tag: idx as u32,
                        } },
                        span,
                    );
                    prelude.push(Spanned::from(
                        TypedExpr { id: 0, ty: member_ty.clone(), kind: TypedExprKind::Assign { name: bind.clone(), value: Box::new(value) } },
                        span,
                    ));
                    // An explicit pattern bind is always a fresh, immutable
                    // name; the bindless fallback rebinds the subject's own
                    // name and must preserve whatever mutability it already
                    // had, or e.g. `if x is Str then { x = 0 }` on a `mut x`
                    // would wrongly reject the reassignment.
                    let mutable = if explicit_bind.is_some() { false } else { self.ctx.is_mutable(&bind).unwrap_or(false) };
                    bindings.push((bind.clone(), member_ty.clone(), mutable));
                }
            }

            tail = Some(self.fold_match_arm(arm, prelude, bindings, base_cond, tail, span)?);
        }

        Ok(Self::finish_match_chain(subject_assign, tail, span))
    }

    /// Desugar `left == right` / `left != right` (both already lowered, same
    /// struct type `struct_ty`) into a per-field structural comparison.
    /// `left`/`right` are bound to fresh temporaries first so a
    /// side-effecting operand (e.g. a function call returning a struct)
    /// is only evaluated once, not once per field.
    fn desugar_struct_eq(&mut self, op: Token, left: Spanned<TypedExpr>, right: Spanned<TypedExpr>, struct_ty: &Type, span: Span) -> TypedExprKind {
        let l_name = format!("__struct_eq_l{}", self.next_id); self.next_id += 1;
        let r_name = format!("__struct_eq_r{}", self.next_id); self.next_id += 1;
        let left_ty = left.item.ty.clone();
        let right_ty = right.item.ty.clone();

        let l_assign = Spanned::from(TypedExpr { id: 0, ty: left_ty.clone(), kind: TypedExprKind::Assign { name: l_name.clone(), value: Box::new(left) } }, span);
        let r_assign = Spanned::from(TypedExpr { id: 0, ty: right_ty.clone(), kind: TypedExprKind::Assign { name: r_name.clone(), value: Box::new(right) } }, span);
        let l_var = Spanned::from(TypedExpr { id: 0, ty: left_ty, kind: TypedExprKind::Var(l_name) }, span);
        let r_var = Spanned::from(TypedExpr { id: 0, ty: right_ty, kind: TypedExprKind::Var(r_name) }, span);

        let eq_expr = self.build_struct_eq(struct_ty, l_var, r_var, span);
        let result = if op == Token::NotEq {
            Spanned::from(TypedExpr { id: 0, ty: Type::Bool, kind: TypedExprKind::Unary { op: Token::Not, expr: Box::new(eq_expr) } }, span)
        } else {
            eq_expr
        };

        TypedExprKind::Block(vec![l_assign, r_assign, result])
    }

    /// Build `l.f1 == r.f1 and l.f2 == r.f2 and ...` for every field of the
    /// struct type `struct_ty`, recursing for nested-struct fields. `l`/`r`
    /// are assumed cheap to duplicate (a `Var` or `FieldAccess` chain —
    /// never something that could re-run a side effect).
    ///
    /// Takes the whole resolved `Type`, not the bare declared name: for a
    /// generic struct (`TRAITS.md` Stage 3a) the name alone keys the
    /// *template*, whose field types are still binder `TypeVar`s, and
    /// stamping those onto the synthesized `FieldAccess` nodes would make
    /// codegen compare every field as a raw `I64` — silently wrong for a
    /// `Str` or nested-struct field. `materialize_struct` gives this
    /// instantiation's concrete layout instead (and is what registers it
    /// under `struct_key` for codegen).
    fn build_struct_eq(&mut self, struct_ty: &Type, l: Spanned<TypedExpr>, r: Spanned<TypedExpr>, span: Span) -> Spanned<TypedExpr> {
        let fields = self.materialize_struct(struct_ty);
        let mut chain: Option<Spanned<TypedExpr>> = None;
        for (fname, fty) in &fields {
            let lf = Spanned::from(TypedExpr { id: 0, ty: fty.clone(), kind: TypedExprKind::FieldAccess { target: Box::new(l.clone()), field: fname.clone(), enum_name: None } }, span);
            let rf = Spanned::from(TypedExpr { id: 0, ty: fty.clone(), kind: TypedExprKind::FieldAccess { target: Box::new(r.clone()), field: fname.clone(), enum_name: None } }, span);
            let sub = match fty.as_struct_name() {
                Some(_) => self.build_struct_eq(fty, lf, rf, span),
                None => Spanned::from(TypedExpr { id: 0, ty: Type::Bool, kind: TypedExprKind::Binary { op: Token::EqEq, left: Box::new(lf), right: Box::new(rf) } }, span),
            };
            chain = Some(match chain {
                None => sub,
                Some(prev) => Spanned::from(TypedExpr { id: 0, ty: Type::Bool, kind: TypedExprKind::Binary { op: Token::And, left: Box::new(prev), right: Box::new(sub) } }, span),
            });
        }
        chain.unwrap_or_else(|| Spanned::from(TypedExpr { id: 0, ty: Type::Bool, kind: TypedExprKind::BoolLit(true) }, span))
    }

    // ── `repr` (`plans/DATA.md` stage 5) ───────────────────────────────────
    //
    // `repr(x)` lowers (`is_repr` above) to a placeholder `Call{callable:
    // Var("repr"), args:[x]}` node with `ty: Str` — nothing about `x`'s type
    // is expanded yet. `desugar_notation` is a whole-tree post-pass, run
    // once per entry after `monomorphize_generics` (`state.rs`'s
    // `eval_with_base`, `codegen::compile_and_run`) and before
    // `liveness::number_nodes`, that finds every one of those placeholders
    // and replaces it with `build_repr`'s per-type expansion. It has to run
    // after monomorphization, not during `lower_call`, for two reasons
    // (both concrete failures, not just tidiness):
    //
    //  - A struct/union field's `Type` is only guaranteed fully substituted
    //    (no leftover generic binder `TypeVar`s) once monomorphization has
    //    cloned and specialized every instantiation — `desugar_struct_eq`
    //    gets away with running during ordinary lowering because `==`'s
    //    operand types are already concrete at that point in a way a
    //    generic function *body*'s `repr(x)` is not.
    //  - `self.lookup(&arg.item.ty)` at placeholder-build time would return
    //    an unresolved `TypeVar` for something like `mut xs = []` — whose
    //    binding gets a concrete `List<Int>` type only after the rest of
    //    the entry has been checked. Emitting `"[]"` for that at lowering
    //    time would be a *lie* about a possibly non-empty list; running
    //    after the whole entry is checked and substituted is what makes
    //    `self.lookup` here trustworthy (this is also what fixes
    //    `MUTABILITY.md`'s `mut xs = []; print(xs)` → `<?>` bug for `repr`,
    //    for free — that bug is a stale node `.ty`, not a genuinely
    //    unresolved type).
    //
    /// Walk every node reachable from `expr` (mirroring
    /// `liveness::number_nodes`'s exhaustive structural recursion — nested
    /// `Function` bodies included, since a lambda can call `repr` too),
    /// replacing each `repr(...)` placeholder found along the way with
    /// `build_repr`'s expansion. Runs before `number_nodes`, so every
    /// synthesized node's `id: 0` is fine — the next pass assigns real ones.
    pub fn desugar_notation(&mut self, expr: &mut Spanned<TypedExpr>) -> Result<(), Spanned<TypeError>> {
        self.desugar_notation_children(&mut expr.item.kind)?;
        // Two placeholders, one pass: `repr(x)` and `json.to_str(x)` are
        // the same walk over the same types with different fragments
        // (`DATA.md`'s "one lowering serves repr, json.to_str, ... by
        // swapping the fragments"), so they expand at the same point, under
        // the same post-monomorphization guarantees, and `build_json`'s
        // arms mirror `build_repr`'s one for one.
        let placeholder = match &expr.item.kind {
            TypedExprKind::Call { callable, .. } => match &callable.item.kind {
                TypedExprKind::Var(name) if name == Notation::Repr.callee()   => Notation::Repr,
                TypedExprKind::Var(name) if name == Notation::Json.callee()   => Notation::Json,
                TypedExprKind::Var(name) if name == Notation::Interp.callee() => Notation::Interp,
                _ => return Ok(()),
            },
            _ => return Ok(()),
        };
        let TypedExprKind::Call { args, .. } = &expr.item.kind else { unreachable!("just matched above") };
        let arg = args[0].value().expect("the placeholder always has exactly one Arg::Value").clone();
        let span = expr.span;
        let ty = self.lookup(&arg.item.ty);
        // `is_repr`'s / `lower_json_call`'s own checks defer here, unrun,
        // when the argument's type was still a bare generic `TypeVar` at
        // that point (a call inside a generic function body) — this is
        // where each concrete monomorphized instantiation finally gets
        // checked, exactly once, against its own resolved type.
        match placeholder {
            Notation::Repr | Notation::Interp => {
                if !self.type_implements(&ty, &Trait::Show) {
                    return Err(Spanned::from(TypeError { msg: placeholder.needs_show(&ty) }, arg.span));
                }
                self.check_no_recursive_union(&ty, arg.span, "repr", "formats it field-by-field")?;
            }
            Notation::Json => self.check_json_serializable(&ty, arg.span)?,
        }
        // Interpolating a `Str` inserts it as text — the one place the two
        // notations differ, and the reason this decision waits until here
        // (`Notation`'s doc comment). Everything else formats as `repr`.
        if placeholder == Notation::Interp && ty == Type::Str {
            *expr = arg;
            return Ok(());
        }
        let temp_name = format!("__repr_v{}", self.next_id); self.next_id += 1;
        let temp_assign = Spanned::from(
            TypedExpr { id: 0, ty: ty.clone(), kind: TypedExprKind::Assign { name: temp_name.clone(), value: Box::new(arg) } },
            span,
        );
        let temp_var = Spanned::from(TypedExpr { id: 0, ty: ty.clone(), kind: TypedExprKind::Var(temp_name) }, span);
        let body = match placeholder {
            Notation::Repr | Notation::Interp => self.build_repr(&ty, temp_var, span)?,
            Notation::Json => self.build_json(&ty, temp_var, span)?,
        };
        *expr = Spanned::from(TypedExpr { id: 0, ty: Type::Str, kind: TypedExprKind::Block(vec![temp_assign, body]) }, span);
        Ok(())
    }

    fn desugar_notation_opt(&mut self, expr: &mut Option<Box<Spanned<TypedExpr>>>) -> Result<(), Spanned<TypeError>> {
        match expr {
            Some(e) => self.desugar_notation(e),
            None => Ok(()),
        }
    }

    /// `desugar_notation`'s exhaustive per-kind recursion — every arm below
    /// mirrors `liveness::number_kind`'s traversal shape exactly (that pass
    /// visits the same nodes for the same reason: nothing may be skipped),
    /// with `number`/`number_opt` calls replaced by `self.desugar_notation`/
    /// `self.desugar_notation_opt`, which can fail (an ill-typed `repr`
    /// argument) where numbering never could.
    fn desugar_notation_children(&mut self, kind: &mut TypedExprKind) -> Result<(), Spanned<TypeError>> {
        match kind {
            TypedExprKind::IntLit(_)
            | TypedExprKind::FloatLit(_)
            | TypedExprKind::BoolLit(_)
            | TypedExprKind::StrLit(_)
            | TypedExprKind::NoneLit
            | TypedExprKind::Var(_) => {}

            TypedExprKind::Unary { expr, .. } => self.desugar_notation(expr)?,
            TypedExprKind::Binary { left, right, .. } => {
                self.desugar_notation(left)?;
                self.desugar_notation(right)?;
            }
            TypedExprKind::Conditional { cond, true_branch, false_branch } => {
                self.desugar_notation(cond)?;
                self.desugar_notation(true_branch)?;
                self.desugar_notation_opt(false_branch)?;
            }
            TypedExprKind::Assign { value, .. } => self.desugar_notation(value)?,
            TypedExprKind::Function { body, .. } => self.desugar_notation(body)?,
            TypedExprKind::Call { callable, args, .. } => {
                self.desugar_notation(callable)?;
                for a in args.iter_mut().flat_map(Arg::subexprs_mut) { self.desugar_notation(a)?; }
            }
            TypedExprKind::Index { target, index } => {
                self.desugar_notation(target)?;
                self.desugar_notation(index)?;
            }
            TypedExprKind::Slice { target, start, end } => {
                self.desugar_notation(target)?;
                self.desugar_notation_opt(start)?;
                self.desugar_notation_opt(end)?;
            }
            TypedExprKind::Range { start, end } => {
                self.desugar_notation(start)?;
                self.desugar_notation(end)?;
            }
            TypedExprKind::List(elems) => {
                for e in elems { self.desugar_notation(e)?; }
            }
            TypedExprKind::Block(stmts) => {
                for s in stmts { self.desugar_notation(s)?; }
            }
            TypedExprKind::ForLoop { iterable, cond, body, .. }
            | TypedExprKind::Comprehension { iterable, cond, body, .. } => {
                self.desugar_notation(iterable)?;
                self.desugar_notation_opt(cond)?;
                self.desugar_notation(body)?;
            }
            TypedExprKind::StructInit { fields, .. } => {
                for (_, v) in fields { self.desugar_notation(v)?; }
            }
            TypedExprKind::FieldAccess { target, .. } => self.desugar_notation(target)?,
            TypedExprKind::PlaceAssign { place, value } => {
                for seg in place.path.iter_mut() {
                    if let PlaceSeg::Index { index, .. } = seg {
                        self.desugar_notation(index)?;
                    }
                }
                self.desugar_notation(value)?;
            }
            TypedExprKind::VariantInit { fields, .. } => {
                for (_, v) in fields { self.desugar_notation(v)?; }
            }
            TypedExprKind::IsVariant { target, .. } => self.desugar_notation(target)?,
            TypedExprKind::VariantField { target, .. } => self.desugar_notation(target)?,
            TypedExprKind::Return(value) => self.desugar_notation_opt(value)?,
            TypedExprKind::Widen { value, .. } => self.desugar_notation(value)?,
            TypedExprKind::Narrow { value, .. } => self.desugar_notation(value)?,
            TypedExprKind::TypeTag { target, .. } => self.desugar_notation(target)?,
            TypedExprKind::Truthy(value) => self.desugar_notation(value)?,
            TypedExprKind::Coerce(value) => self.desugar_notation(value)?,
        }
        Ok(())
    }

    fn str_lit(s: impl Into<String>, span: Span) -> Spanned<TypedExpr> {
        Spanned::from(TypedExpr { id: 0, ty: Type::Str, kind: TypedExprKind::StrLit(s.into()) }, span)
    }

    /// `TypedExpr` counterpart of `ConstValue` — the "natural" scalar type
    /// (`Int`/`Float`/`Bool`/`Str`/`None`), never a union. Callers that need
    /// the value in a wider slot (an annotation/struct field typed `T?`, a
    /// union member) route it through `lower_widen` afterward, exactly like
    /// an ordinary construction argument.
    fn const_value_to_typed(v: &ConstValue, span: Span) -> Spanned<TypedExpr> {
        let (ty, kind) = match v {
            ConstValue::Int(n) => (Type::Int, TypedExprKind::IntLit(*n)),
            ConstValue::Float(f) => (Type::Float, TypedExprKind::FloatLit(*f)),
            ConstValue::Bool(b) => (Type::Bool, TypedExprKind::BoolLit(*b)),
            ConstValue::Str(s) => (Type::Str, TypedExprKind::StrLit(s.clone())),
            ConstValue::None => (Type::None, TypedExprKind::NoneLit),
        };
        Spanned::from(TypedExpr { id: 0, ty, kind }, span)
    }

    /// Check `v` against `target` the same way an ordinary construction
    /// argument is checked (`lower_record_args`'s own compatibility test:
    /// `widens_to` / `unify` / `unify_with_one_union_member`), then widen
    /// it into `target`'s representation. `lower_widen` alone is *not*
    /// a type check — for a non-union `target`, it silently returns its
    /// input unchanged if the types don't match (it exists to be called
    /// only after a value has already been checked compatible some other
    /// way), so every one of this stage's own validation sites
    /// (`register_field_extras`, `hoist_annotation_decls`,
    /// `validate_annotation_use`) must go through this, never
    /// `lower_widen` directly, or a mismatched literal (`#json(name=5)`
    /// against a `Str` field) would validate silently.
    fn widen_const_checked(&mut self, v: &ConstValue, target: &Type, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let base = Self::const_value_to_typed(v, span);
        let resolved_value_ty = self.lookup(&base.item.ty);
        let resolved_target_ty = self.lookup(target);
        // `unify`/`unify_with_one_union_member` are used here as
        // *predicates*, but both write to `substitutions` when they
        // succeed. Every caller is a declaration-time check of a literal
        // against a declared type, so any binding they produce would pin a
        // type variable belonging to the *declaration* (a generic struct's
        // binder, say) for the rest of the program, on the strength of one
        // default value. `v` is always a concrete `ConstValue`, so nothing
        // below — `lower_widen` included — needs those bindings; take a
        // snapshot and put it back either way.
        let snapshot = self.substitutions.clone();
        let accepted = widens_to(&resolved_value_ty, &resolved_target_ty)
            || self.unify(&base.item.ty, target)
            || self.unify_with_one_union_member(&base.item.ty, target);
        self.substitutions = snapshot;
        if !accepted {
            return Err(Spanned::from(TypeError {
                msg: format!("expected {}, got {}", resolved_target_ty, resolved_value_ty)
            }, span));
        }
        self.lower_widen(base, target)
    }

    /// Left-fold `parts` (every one `Str`-typed) into a `Str + Str + ...`
    /// chain, merging adjacent `StrLit`s at build time first instead of
    /// emitting a `Binary` node for them — halves the fragment/allocation
    /// count for a struct's field-name/punctuation literals at zero
    /// runtime cost (`plans/DATA.md` stage 5's known O(n^2)-in-fragment-
    /// count risk; this doesn't fix the underlying shape, `Sink`/`StrBuf`
    /// does, but it's a free partial mitigation). `parts` must be non-empty.
    fn str_cat(parts: Vec<Spanned<TypedExpr>>, span: Span) -> Spanned<TypedExpr> {
        let mut folded: Vec<Spanned<TypedExpr>> = Vec::with_capacity(parts.len());
        for p in parts {
            let mut merged = false;
            if let TypedExprKind::StrLit(s) = &p.item.kind {
                if let Some(last) = folded.last_mut() {
                    if let TypedExprKind::StrLit(prev) = &mut last.item.kind {
                        prev.push_str(s);
                        merged = true;
                    }
                }
            }
            if !merged { folded.push(p); }
        }
        let mut iter = folded.into_iter();
        let first = iter.next().expect("str_cat: parts must be non-empty");
        iter.fold(first, |acc, p| {
            Spanned::from(TypedExpr { id: 0, ty: Type::Str, kind: TypedExprKind::Binary { op: Token::Plus, left: Box::new(acc), right: Box::new(p) } }, span)
        })
    }

    /// `Call{Var(rt_name), [v]}` — one of the allocating scalar `repr`
    /// leaves (`runtime/ffi.rs`'s `frog_{int,float,bool,str}_repr`,
    /// registered under these exact `func_ids` keys in `codegen/mod.rs`).
    fn repr_leaf_call(rt_name: &str, param_ty: Type, v: Spanned<TypedExpr>, span: Span) -> Spanned<TypedExpr> {
        let callable = Spanned::from(TypedExpr {
            id: 0,
            ty: Type::Function { params: vec![param_ty], result: Box::new(Type::Str) },
            kind: TypedExprKind::Var(rt_name.to_string()),
        }, span);
        Spanned::from(TypedExpr {
            id: 0, ty: Type::Str,
            kind: TypedExprKind::Call { callable: Box::new(callable), args: vec![Arg::Value(v)] },
        }, span)
    }

    /// `Call{Var("__str_join"), [list, sep]}` — `List<T>`'s `repr` arm
    /// reduces its per-element fragments this way instead of an O(depth)
    /// `+` chain (`runtime/ffi.rs::frog_str_join`).
    fn build_str_join(list: Spanned<TypedExpr>, sep: Spanned<TypedExpr>, span: Span) -> Spanned<TypedExpr> {
        let callable = Spanned::from(TypedExpr {
            id: 0,
            ty: Type::Function { params: vec![Type::list(Type::Str), Type::Str], result: Box::new(Type::Str) },
            kind: TypedExprKind::Var("__str_join".to_string()),
        }, span);
        Spanned::from(TypedExpr {
            id: 0, ty: Type::Str,
            kind: TypedExprKind::Call { callable: Box::new(callable), args: vec![Arg::Value(list), Arg::Value(sep)] },
        }, span)
    }

    /// `repr`'s per-type dispatch. `ty` must already be `self.lookup`-
    /// resolved (every caller either resolves it just before calling, or
    /// receives it already resolved from a caller that did) — this never
    /// re-resolves `v`'s own stored `.ty`, so a caller that skips that step
    /// gets whatever `TypeVar` was in `ty` back out, unhelpfully.
    fn build_repr(&mut self, ty: &Type, v: Spanned<TypedExpr>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        // `Range` first: it is structurally a 2-field struct to
        // `as_struct_name` below, exactly the ordering `print_value`
        // itself needs and explains (`codegen/mod.rs`).
        if let Some(elem_ty) = ty.as_range_elem().cloned() {
            // Registers `Range<elem>`'s flattened layout in `struct_defs`
            // (if this is the first time this instantiation is seen) —
            // needed for codegen to resolve the `FieldAccess` nodes below,
            // the same reason `build_struct_eq` calls it for a struct.
            self.materialize_struct(ty);
            let start = Spanned::from(TypedExpr { id: 0, ty: elem_ty.clone(), kind: TypedExprKind::FieldAccess { target: Box::new(v.clone()), field: "start".to_string(), enum_name: None } }, span);
            let end   = Spanned::from(TypedExpr { id: 0, ty: elem_ty.clone(), kind: TypedExprKind::FieldAccess { target: Box::new(v),         field: "end".to_string(),   enum_name: None } }, span);
            let start_repr = self.build_repr(&elem_ty, start, span)?;
            let end_repr   = self.build_repr(&elem_ty, end, span)?;
            return Ok(Self::str_cat(vec![start_repr, Self::str_lit("..", span), end_repr], span));
        }
        if let Some(name) = ty.as_struct_name().map(str::to_string) {
            return self.build_repr_struct(&name, ty, v, span);
        }
        if let Some(elem_ty) = ty.as_list_elem().cloned() {
            return self.build_repr_list(&elem_ty, v, span);
        }
        match ty {
            Type::Union(members) => {
                let members = members.clone();
                self.build_repr_union(&members, v, span)
            }
            Type::Str   => Ok(Self::repr_leaf_call("__repr_str", Type::Str, v, span)),
            Type::None  => Ok(Self::str_lit("none", span)),
            Type::Int   => Ok(Self::repr_leaf_call("__repr_int", Type::Int, v, span)),
            Type::Float => Ok(Self::repr_leaf_call("__repr_float", Type::Float, v, span)),
            Type::Bool  => Ok(Self::repr_leaf_call("__repr_bool", Type::Bool, v, span)),
            // Only reachable for a bare (non-list-element) argument whose
            // type inference never pinned down — which, for a `Show`-
            // checked argument, means it was never actually observed at a
            // concrete type anywhere in the program. `List<T>`'s own arm
            // (`build_repr_list`) handles the one case that legitimately
            // happens in working programs (an empty list literal) before
            // ever calling back in here with a bare `TypeVar`.
            Type::TypeVar { .. } => Err(Spanned::from(TypeError {
                msg: "cannot infer the type of repr's argument; annotate the expected type".to_string()
            }, span)),
            other => Err(Spanned::from(TypeError { msg: format!("{} has no notation", other) }, span)),
        }
    }

    /// `Name(f=repr(v.f), ...)` / `Name(repr(v.0), ...)` — struct notation,
    /// matching `print_value`'s exact struct arm (`codegen/mod.rs`):
    /// positional fields (`is_positional_fields`) print bare, named fields
    /// print `f=`-prefixed.
    fn build_repr_struct(&mut self, name: &str, ty: &Type, v: Spanned<TypedExpr>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let fields = self.materialize_struct(ty);
        let positional = is_positional_fields(&fields);
        let mut parts = vec![Self::str_lit(format!("{}(", name), span)];
        for (i, (fname, fty)) in fields.iter().enumerate() {
            if i != 0 { parts.push(Self::str_lit(", ", span)); }
            if !positional { parts.push(Self::str_lit(format!("{}=", fname), span)); }
            let fv = Spanned::from(TypedExpr {
                id: 0, ty: fty.clone(),
                kind: TypedExprKind::FieldAccess { target: Box::new(v.clone()), field: fname.clone(), enum_name: None },
            }, span);
            parts.push(self.build_repr(fty, fv, span)?);
        }
        parts.push(Self::str_lit(")", span));
        Ok(Self::str_cat(parts, span))
    }

    /// `[repr(e0), repr(e1), ...]` — `List<T>` notation. Binds `v` to a
    /// temporary first (`__repr_l{n}`), since the loop reads it as an
    /// iterable and it may otherwise be an arbitrary (possibly
    /// side-effecting) expression; the per-element loop body reads a
    /// second temporary (`__repr_e{n}`), the `Comprehension`'s own bound
    /// variable. The iterable read is an ordinary `for`-loop-shaped one
    /// (`Var` fed straight into `Comprehension::iterable`), so it is
    /// transient under the existing liveness analysis exactly like any
    /// user-written `[for x in xs do ...]` — no `repr`-specific plumbing
    /// needed there (`MUTABILITY.md`'s O(n^2) trap this avoids).
    fn build_repr_list(&mut self, elem_ty: &Type, v: Spanned<TypedExpr>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        // An element type inference never fixed — only possible for a
        // provably empty list (`repr([])`), since any element would have
        // fixed the variable. The comprehension body below would be dead
        // code, so skip it and emit the same `[]` a literal empty list
        // reprs as, rather than building a loop over a type that doesn't
        // exist yet — mirrors `print_value`'s `TypeVar` arm's reasoning.
        if matches!(elem_ty, Type::TypeVar { .. }) {
            return Ok(Self::str_lit("[]", span));
        }
        let list_name = format!("__repr_l{}", self.next_id); self.next_id += 1;
        let elem_name = format!("__repr_e{}", self.next_id); self.next_id += 1;
        let list_ty = Type::list(elem_ty.clone());

        let list_assign = Spanned::from(TypedExpr { id: 0, ty: list_ty.clone(), kind: TypedExprKind::Assign { name: list_name.clone(), value: Box::new(v) } }, span);
        let list_var = Spanned::from(TypedExpr { id: 0, ty: list_ty, kind: TypedExprKind::Var(list_name) }, span);
        let elem_var = Spanned::from(TypedExpr { id: 0, ty: elem_ty.clone(), kind: TypedExprKind::Var(elem_name.clone()) }, span);
        let elem_repr = self.build_repr(elem_ty, elem_var, span)?;

        let comprehension = Spanned::from(TypedExpr {
            id: 0, ty: Type::list(Type::Str),
            kind: TypedExprKind::Comprehension { var: elem_name, iterable: Box::new(list_var), cond: None, body: Box::new(elem_repr), iter_via: None },
        }, span);
        let joined = Self::build_str_join(comprehension, Self::str_lit(", ", span), span);
        let cat = Self::str_cat(vec![Self::str_lit("[", span), joined, Self::str_lit("]", span)], span);
        Ok(Spanned::from(TypedExpr { id: 0, ty: Type::Str, kind: TypedExprKind::Block(vec![list_assign, cat]) }, span))
    }

    /// Union notation, nominal or anonymous. Binds `v` to a temporary
    /// first (`__repr_u{n}`) — every member arm below reads the subject at
    /// least once, some (a nominal variant's own fields) more than once,
    /// so it must be cheap to duplicate — then dispatches on whether the
    /// (already-normalized) member list resolves to a declared `data ...
    /// is ...` union.
    fn build_repr_union(&mut self, members: &[Type], v: Spanned<TypedExpr>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let union_ty = Type::Union(members.to_vec());
        let subj_name = format!("__repr_u{}", self.next_id); self.next_id += 1;
        let subj_assign = Spanned::from(TypedExpr { id: 0, ty: union_ty.clone(), kind: TypedExprKind::Assign { name: subj_name.clone(), value: Box::new(v) } }, span);

        let resolved = self.resolve_union(&union_ty).map(|(n, d)| (n.to_string(), d.clone()));
        let body = match resolved {
            Some((enum_name, def)) => self.build_repr_nominal_union(&enum_name, &def, &subj_name, &union_ty, span)?,
            None => self.build_repr_anon_union(members, &subj_name, &union_ty, span)?,
        };
        Ok(Spanned::from(TypedExpr { id: 0, ty: Type::Str, kind: TypedExprKind::Block(vec![subj_assign, body]) }, span))
    }

    /// `Enum.Variant(f=repr(v.f), ...)`, folded right-to-left into a nested
    /// `Conditional` over `IsVariant` — exactly `fold_match_arm`'s "the
    /// rightmost arm needs no tag test" shape (`lower_match_lowered`) and
    /// `print_union_body`'s declared-order dispatch (`codegen/mod.rs`),
    /// since every branch here already agrees on its result type (`Str`)
    /// there is no need for `fold_match_arm`'s own `joined_conditional`
    /// widening machinery — a plain `Conditional` node suffices.
    fn build_repr_nominal_union(&mut self, enum_name: &str, def: &UnionDef, subj_name: &str, subj_ty: &Type, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let common = def.common.clone();
        let variants = def.variants.clone();
        let mut tail: Option<Spanned<TypedExpr>> = None;
        for (idx, (vname, vfields)) in variants.iter().enumerate().rev() {
            let subject_var = Spanned::from(TypedExpr { id: 0, ty: subj_ty.clone(), kind: TypedExprKind::Var(subj_name.to_string()) }, span);

            let flat: Vec<(String, Type)> = common.iter().chain(vfields.iter()).cloned().collect();
            let positional = is_positional_fields(&flat);
            let mut parts = vec![Self::str_lit(format!("{}.{}(", enum_name, vname), span)];
            for (i, (fname, fty)) in flat.iter().enumerate() {
                if i != 0 { parts.push(Self::str_lit(", ", span)); }
                if !positional { parts.push(Self::str_lit(format!("{}=", fname), span)); }
                let fv = if i < common.len() {
                    Spanned::from(TypedExpr {
                        id: 0, ty: fty.clone(),
                        kind: TypedExprKind::FieldAccess { target: Box::new(subject_var.clone()), field: fname.clone(), enum_name: Some(enum_name.to_string()) },
                    }, span)
                } else {
                    Spanned::from(TypedExpr {
                        id: 0, ty: fty.clone(),
                        kind: TypedExprKind::VariantField { target: Box::new(subject_var.clone()), enum_name: enum_name.to_string(), variant: vname.clone(), field: fname.clone() },
                    }, span)
                };
                parts.push(self.build_repr(fty, fv, span)?);
            }
            parts.push(Self::str_lit(")", span));
            let body = Self::str_cat(parts, span);

            if tail.is_none() {
                tail = Some(body);
                continue;
            }
            let cond = Spanned::from(TypedExpr {
                id: 0, ty: Type::Bool,
                kind: TypedExprKind::IsVariant { target: Box::new(subject_var), enum_name: enum_name.to_string(), variant: vname.clone(), tag: idx as u32 },
            }, span);
            tail = Some(Spanned::from(TypedExpr {
                id: 0, ty: Type::Str,
                kind: TypedExprKind::Conditional { cond: Box::new(cond), true_branch: Box::new(body), false_branch: tail.map(Box::new) },
            }, span));
        }
        Ok(tail.expect("a nominal union always declares at least one variant"))
    }

    /// Anonymous union notation — `TypeTag`/`Narrow` in place of
    /// `IsVariant`/`VariantField`, same right-to-left fold as the nominal
    /// case above. A `None` member needs no `Narrow`: there is no payload
    /// to unbox, and its own tag test already proved which member it is.
    fn build_repr_anon_union(&mut self, members: &[Type], subj_name: &str, subj_ty: &Type, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let mut tail: Option<Spanned<TypedExpr>> = None;
        for (idx, member_ty) in members.iter().enumerate().rev() {
            let subject_var = Spanned::from(TypedExpr { id: 0, ty: subj_ty.clone(), kind: TypedExprKind::Var(subj_name.to_string()) }, span);
            let narrowed = if *member_ty == Type::None {
                Self::str_lit("none", span)
            } else {
                let narrow_val = Spanned::from(TypedExpr {
                    id: 0, ty: member_ty.clone(),
                    kind: TypedExprKind::Narrow { value: Box::new(subject_var.clone()), tag: idx as u32 },
                }, span);
                self.build_repr(member_ty, narrow_val, span)?
            };

            if tail.is_none() {
                tail = Some(narrowed);
                continue;
            }
            let cond = Spanned::from(TypedExpr {
                id: 0, ty: Type::Bool,
                kind: TypedExprKind::TypeTag { target: Box::new(subject_var), tag: idx as u32 },
            }, span);
            tail = Some(Spanned::from(TypedExpr {
                id: 0, ty: Type::Str,
                kind: TypedExprKind::Conditional { cond: Box::new(cond), true_branch: Box::new(narrowed), false_branch: tail.map(Box::new) },
            }, span));
        }
        Ok(tail.expect("an anonymous union always has at least two members"))
    }

    // ── `json.to_str` (`plans/DATA.md` stage 8) ─────────────────────────────
    //
    // The same walk as `build_repr` above, arm for arm, with different
    // fragments — which is the concrete cash value of DATA.md's "one
    // lowering serves `repr`, `json.to_str`, ... by swapping the
    // fragments", and the reason `repr` was built as a typed-AST desugar
    // rather than a fourth `print_value`-shaped codegen walk. Nothing here
    // is a new mechanism: `str_cat`, `str_lit`, `build_str_join`,
    // `Comprehension`, `IsVariant`/`VariantField`, `TypeTag`/`Narrow` are
    // all `build_repr`'s, unchanged.
    //
    // Where the two genuinely differ, they differ because JSON is a
    // different format, not because this is a different compiler:
    //
    //  - `Str` escapes with JSON's table (`__json_str`), never frog
    //    notation's (`DATA.md`'s two tiers must not share a mechanism).
    //  - `Float` has no spelling for `inf`/`nan`, so `__json_float` emits
    //    `null` — `json.to_str` returns `Str`, not `Str | JsonError`.
    //  - A struct is an object (`{"f":...}`) or, if its fields are
    //    positional, an array.
    //  - A nominal union is **externally tagged**: `{"Circle":{"r":1}}`.
    //    Uniform across named, positional and nullary variants, ambiguous
    //    for none of them, and it reserves no field name — internal
    //    tagging cannot represent a positional or non-object payload at
    //    all, so it could not be the default without there being two
    //    incompatible default shapes. (`#json(tag="kind")` is the deferred
    //    way to ask for the other one.)
    //  - `Int` and `Bool` reuse `__repr_int`/`__repr_bool` outright: their
    //    output is already JSON-legal, and a second pair of leaves that
    //    happened to agree would just be two things to keep in agreement.

    /// `json.to_str`'s per-type dispatch, mirroring `build_repr`'s
    /// (including its arm *order*, which `Range`-before-struct depends on).
    /// `ty` must already be `self.lookup`-resolved — same contract.
    fn build_json(&mut self, ty: &Type, v: Spanned<TypedExpr>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        if let Some(elem_ty) = ty.as_range_elem().cloned() {
            self.materialize_struct(ty);
            let start = Spanned::from(TypedExpr { id: 0, ty: elem_ty.clone(), kind: TypedExprKind::FieldAccess { target: Box::new(v.clone()), field: "start".to_string(), enum_name: None } }, span);
            let end   = Spanned::from(TypedExpr { id: 0, ty: elem_ty.clone(), kind: TypedExprKind::FieldAccess { target: Box::new(v),         field: "end".to_string(),   enum_name: None } }, span);
            let start_json = self.build_json(&elem_ty, start, span)?;
            let end_json   = self.build_json(&elem_ty, end, span)?;
            return Ok(Self::str_cat(vec![
                Self::str_lit("{\"start\":", span), start_json,
                Self::str_lit(",\"end\":", span), end_json,
                Self::str_lit("}", span),
            ], span));
        }
        // Unlike `build_repr_struct`, the name is not needed: JSON writes
        // an object or an array, never `Name(...)`.
        if ty.as_struct_name().is_some() {
            return self.build_json_struct(ty, v, span);
        }
        if let Some(elem_ty) = ty.as_list_elem().cloned() {
            return self.build_json_list(&elem_ty, v, span);
        }
        match ty {
            Type::Union(members) => {
                let members = members.clone();
                self.build_json_union(&members, v, span)
            }
            Type::Str   => Ok(Self::repr_leaf_call("__json_str", Type::Str, v, span)),
            Type::None  => Ok(Self::str_lit("null", span)),
            Type::Int   => Ok(Self::repr_leaf_call("__repr_int", Type::Int, v, span)),
            Type::Float => Ok(Self::repr_leaf_call("__json_float", Type::Float, v, span)),
            Type::Bool  => Ok(Self::repr_leaf_call("__repr_bool", Type::Bool, v, span)),
            // An empty list's element type, reached from `build_json_list`
            // only when there provably are no elements — same reasoning as
            // `build_repr`'s arm, which explains the asymmetry.
            Type::TypeVar { .. } => Err(Spanned::from(TypeError {
                msg: "cannot infer the type of json.to_str's argument; annotate the expected type".to_string()
            }, span)),
            other => Err(Spanned::from(TypeError { msg: format!("{} has no JSON form", other) }, span)),
        }
    }

    /// `{"f":...,"g":...}`, or `[...]` if the fields are positional — a
    /// "tuple struct" (`data Lit(Int)`) has no field names to key an object
    /// by, and inventing `"0"`/`"1"` would put froglang's own internal
    /// synthetic field-name keys on the wire.
    fn build_json_struct(&mut self, ty: &Type, v: Spanned<TypedExpr>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let fields = self.materialize_struct(ty);
        let accessors = fields.iter().map(|(fname, fty)| {
            let fv = Spanned::from(TypedExpr {
                id: 0, ty: fty.clone(),
                kind: TypedExprKind::FieldAccess { target: Box::new(v.clone()), field: fname.clone(), enum_name: None },
            }, span);
            (fname.clone(), fty.clone(), fv)
        }).collect::<Vec<_>>();
        self.build_json_fields(&accessors, span)
    }

    /// The shared "these fields, as an object or an array" body — used by
    /// both an ordinary struct and a nominal union variant's payload, which
    /// is the whole reason external tagging is uniform across variant
    /// shapes.
    fn build_json_fields(&mut self, fields: &[(String, Type, Spanned<TypedExpr>)], span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let names: Vec<(String, Type)> = fields.iter().map(|(n, t, _)| (n.clone(), t.clone())).collect();
        // `is_positional_fields` is vacuously true for a nullary variant
        // (`data Shape is ... | Blank()`), which would put `[]` on the wire
        // for something that is conceptually a record with no fields. `{}`
        // is the right empty payload, and keeps every *named* variant's
        // shape an object whether or not it happens to have fields today.
        let positional = !names.is_empty() && is_positional_fields(&names);
        let (open, close) = if positional { ("[", "]") } else { ("{", "}") };
        let mut parts = vec![Self::str_lit(open, span)];
        for (i, (fname, fty, fv)) in fields.iter().enumerate() {
            if i != 0 { parts.push(Self::str_lit(",", span)); }
            if !positional {
                // The key is JSON-escaped at build time, not at runtime:
                // it is a compile-time-known declared name, so escaping it
                // here folds into the surrounding `StrLit` and costs
                // nothing at all at run time.
                parts.push(Self::str_lit(format!("{}:", crate::runtime::json::dom::escape(fname)), span));
            }
            parts.push(self.build_json(fty, fv.clone(), span)?);
        }
        parts.push(Self::str_lit(close, span));
        Ok(Self::str_cat(parts, span))
    }

    /// `[e0,e1,...]` — `build_repr_list`'s twin, down to the two
    /// temporaries and the transient comprehension read. See its doc
    /// comment for why both exist.
    fn build_json_list(&mut self, elem_ty: &Type, v: Spanned<TypedExpr>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        if matches!(elem_ty, Type::TypeVar { .. }) {
            return Ok(Self::str_lit("[]", span));
        }
        let list_name = format!("__json_l{}", self.next_id); self.next_id += 1;
        let elem_name = format!("__json_e{}", self.next_id); self.next_id += 1;
        let list_ty = Type::list(elem_ty.clone());

        let list_assign = Spanned::from(TypedExpr { id: 0, ty: list_ty.clone(), kind: TypedExprKind::Assign { name: list_name.clone(), value: Box::new(v) } }, span);
        let list_var = Spanned::from(TypedExpr { id: 0, ty: list_ty, kind: TypedExprKind::Var(list_name) }, span);
        let elem_var = Spanned::from(TypedExpr { id: 0, ty: elem_ty.clone(), kind: TypedExprKind::Var(elem_name.clone()) }, span);
        let elem_json = self.build_json(elem_ty, elem_var, span)?;

        let comprehension = Spanned::from(TypedExpr {
            id: 0, ty: Type::list(Type::Str),
            kind: TypedExprKind::Comprehension { var: elem_name, iterable: Box::new(list_var), cond: None, body: Box::new(elem_json), iter_via: None },
        }, span);
        let joined = Self::build_str_join(comprehension, Self::str_lit(",", span), span);
        let cat = Self::str_cat(vec![Self::str_lit("[", span), joined, Self::str_lit("]", span)], span);
        Ok(Spanned::from(TypedExpr { id: 0, ty: Type::Str, kind: TypedExprKind::Block(vec![list_assign, cat]) }, span))
    }

    /// `build_repr_union`'s twin — same temp-binding, same nominal /
    /// anonymous split.
    fn build_json_union(&mut self, members: &[Type], v: Spanned<TypedExpr>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let union_ty = Type::Union(members.to_vec());
        let subj_name = format!("__json_u{}", self.next_id); self.next_id += 1;
        let subj_assign = Spanned::from(TypedExpr { id: 0, ty: union_ty.clone(), kind: TypedExprKind::Assign { name: subj_name.clone(), value: Box::new(v) } }, span);

        let resolved = self.resolve_union(&union_ty).map(|(n, d)| (n.to_string(), d.clone()));
        let body = match resolved {
            Some((enum_name, def)) => self.build_json_nominal_union(&enum_name, &def, &subj_name, &union_ty, span)?,
            None => self.build_json_anon_union(members, &subj_name, &union_ty, span)?,
        };
        Ok(Spanned::from(TypedExpr { id: 0, ty: Type::Str, kind: TypedExprKind::Block(vec![subj_assign, body]) }, span))
    }

    /// `{"Variant":<payload>}` — external tagging. The payload is the
    /// variant's common-then-own fields through `build_json_fields`, so a
    /// named variant is an object, a positional one an array, and a nullary
    /// one the empty object `{}`. Right-to-left fold over `IsVariant` with
    /// the last variant as the bare else, exactly `build_repr_nominal_union`.
    fn build_json_nominal_union(&mut self, enum_name: &str, def: &UnionDef, subj_name: &str, subj_ty: &Type, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let common = def.common.clone();
        let variants = def.variants.clone();
        let mut tail: Option<Spanned<TypedExpr>> = None;
        for (idx, (vname, vfields)) in variants.iter().enumerate().rev() {
            let subject_var = Spanned::from(TypedExpr { id: 0, ty: subj_ty.clone(), kind: TypedExprKind::Var(subj_name.to_string()) }, span);

            let flat: Vec<(String, Type)> = common.iter().chain(vfields.iter()).cloned().collect();
            let accessors = flat.iter().enumerate().map(|(i, (fname, fty))| {
                let fv = if i < common.len() {
                    Spanned::from(TypedExpr {
                        id: 0, ty: fty.clone(),
                        kind: TypedExprKind::FieldAccess { target: Box::new(subject_var.clone()), field: fname.clone(), enum_name: Some(enum_name.to_string()) },
                    }, span)
                } else {
                    Spanned::from(TypedExpr {
                        id: 0, ty: fty.clone(),
                        kind: TypedExprKind::VariantField { target: Box::new(subject_var.clone()), enum_name: enum_name.to_string(), variant: vname.clone(), field: fname.clone() },
                    }, span)
                };
                (fname.clone(), fty.clone(), fv)
            }).collect::<Vec<_>>();
            let payload = self.build_json_fields(&accessors, span)?;
            let body = Self::str_cat(vec![
                Self::str_lit(format!("{{{}:", crate::runtime::json::dom::escape(vname)), span),
                payload,
                Self::str_lit("}", span),
            ], span);

            if tail.is_none() {
                tail = Some(body);
                continue;
            }
            let cond = Spanned::from(TypedExpr {
                id: 0, ty: Type::Bool,
                kind: TypedExprKind::IsVariant { target: Box::new(subject_var), enum_name: enum_name.to_string(), variant: vname.clone(), tag: idx as u32 },
            }, span);
            tail = Some(Spanned::from(TypedExpr {
                id: 0, ty: Type::Str,
                kind: TypedExprKind::Conditional { cond: Box::new(cond), true_branch: Box::new(body), false_branch: tail.map(Box::new) },
            }, span));
        }
        Ok(tail.expect("a nominal union always declares at least one variant"))
    }

    /// Anonymous unions serialize **permissively** — each member as its own
    /// JSON, with no tag at all (`Int | None` is a number or `null`). Only
    /// `json.parse` restricts them (`check_json_readable`), because only
    /// reading needs to tell them apart again; writing does not, and
    /// refusing to write a value froglang can perfectly well describe would
    /// be a restriction with nothing behind it. `DATA.md` stage 8's
    /// "serialization stays permissive".
    fn build_json_anon_union(&mut self, members: &[Type], subj_name: &str, subj_ty: &Type, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let mut tail: Option<Spanned<TypedExpr>> = None;
        for (idx, member_ty) in members.iter().enumerate().rev() {
            let subject_var = Spanned::from(TypedExpr { id: 0, ty: subj_ty.clone(), kind: TypedExprKind::Var(subj_name.to_string()) }, span);
            let narrowed = if *member_ty == Type::None {
                Self::str_lit("null", span)
            } else {
                let narrow_val = Spanned::from(TypedExpr {
                    id: 0, ty: member_ty.clone(),
                    kind: TypedExprKind::Narrow { value: Box::new(subject_var.clone()), tag: idx as u32 },
                }, span);
                self.build_json(member_ty, narrow_val, span)?
            };

            if tail.is_none() {
                tail = Some(narrowed);
                continue;
            }
            let cond = Spanned::from(TypedExpr {
                id: 0, ty: Type::Bool,
                kind: TypedExprKind::TypeTag { target: Box::new(subject_var), tag: idx as u32 },
            }, span);
            tail = Some(Spanned::from(TypedExpr {
                id: 0, ty: Type::Str,
                kind: TypedExprKind::Conditional { cond: Box::new(cond), true_branch: Box::new(narrowed), false_branch: tail.map(Box::new) },
            }, span));
        }
        Ok(tail.expect("an anonymous union always has at least two members"))
    }

    // ── `json.parse` (`plans/DATA.md` stage 8) ──────────────────────────────
    //
    // `build_read_json` is to `build_json` what `build_read` is to
    // `build_repr`, and it is built against the same runtime contract:
    // `runtime/json`'s accessors are sticky-error-and-continue, so this
    // synthesizes one unconditional happy-path expression per type with no
    // early-exit control flow, and `lower_json_parse` checks
    // `frog_json_failed()` exactly once at the top.
    //
    // Two things here have no counterpart in `build_read`:
    //
    //  - **Absent vs. `null`** (DATA.md stage 8's semantics decision 1).
    //    A named field whose type admits `None` may be missing entirely;
    //    `frog_json_has` (the one probe that does *not* mark failure) is
    //    what asks. A *present* `null` needs no special case at all — the
    //    field's own `T | None` dispatch already tests `frog_json_is_null`
    //    — so the whole rule is one `Conditional` around the ordinary read.
    //  - **External tagging**, which makes "is this the `Circle` variant?"
    //    into "does this object have a `Circle` key?" — `frog_json_has`
    //    again, in place of `read`'s `frog_read_is_call`.

    /// `json.parse`'s per-type dispatch, `build_json`'s inverse and
    /// `build_read`'s twin. `node` is a `Type::Int`-typed opaque handle
    /// into `runtime::json`'s parsed document.
    fn build_read_json(&mut self, ty: &Type, node: Spanned<TypedExpr>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        if let Some(elem_ty) = ty.as_range_elem().cloned() {
            self.materialize_struct(ty);
            let lo_node = Self::read_call2("frog_json_get", Type::Int, Type::Str, Type::Int, node.clone(), Self::str_lit("start", span), span);
            let hi_node = Self::read_call2("frog_json_get", Type::Int, Type::Str, Type::Int, node, Self::str_lit("end", span), span);
            let lo = self.build_read_json(&elem_ty, lo_node, span)?;
            let hi = self.build_read_json(&elem_ty, hi_node, span)?;
            let name = ty.as_struct_name().expect("Range is a struct name").to_string();
            return Ok(Spanned::from(TypedExpr {
                id: 0, ty: ty.clone(),
                kind: TypedExprKind::StructInit { name, fields: vec![("start".to_string(), Box::new(lo)), ("end".to_string(), Box::new(hi))] },
            }, span));
        }
        if let Some(name) = ty.as_struct_name().map(str::to_string) {
            return self.build_read_json_struct(&name, ty, node, span);
        }
        if let Some(elem_ty) = ty.as_list_elem().cloned() {
            return self.build_read_json_list(&elem_ty, node, span);
        }
        match ty {
            Type::Union(members) => {
                let members = members.clone();
                let union_ty = Type::Union(members.clone());
                self.build_read_json_union_as(&members, node, &union_ty, span)
            }
            Type::Str   => Ok(Self::read_leaf_call("frog_json_str", Type::Int, Type::Str, node, span)),
            Type::None  => Ok(Spanned::from(TypedExpr { id: 0, ty: Type::None, kind: TypedExprKind::NoneLit }, span)),
            Type::Int   => Ok(Self::read_leaf_call("frog_json_int", Type::Int, Type::Int, node, span)),
            Type::Float => Ok(Self::read_leaf_call("frog_json_float", Type::Int, Type::Float, node, span)),
            Type::Bool  => Ok(Self::read_leaf_call("frog_json_bool", Type::Int, Type::Bool, node, span)),
            other => Err(Spanned::from(TypeError { msg: format!("{} has no JSON form to parse", other) }, span)),
        }
    }

    /// An object keyed by declared field name, or an array if the fields
    /// are positional — `build_json_struct`'s inverse. Keys are fetched by
    /// name, so key order in the input is irrelevant for free.
    fn build_read_json_struct(&mut self, name: &str, ty: &Type, node: Spanned<TypedExpr>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let fields = self.materialize_struct(ty);
        // The node is read once per field, so bind it — `node` may be an
        // arbitrary accessor call, and re-evaluating `frog_json_get(...)`
        // per field would re-navigate (and re-mark) each time.
        let node_name = format!("__json_s{}", self.next_id); self.next_id += 1;
        let node_assign = Spanned::from(TypedExpr { id: 0, ty: Type::Int, kind: TypedExprKind::Assign { name: node_name.clone(), value: Box::new(node) } }, span);
        let out = self.build_read_json_fields(&fields, &node_name, span)?;
        let init = Spanned::from(TypedExpr { id: 0, ty: ty.clone(), kind: TypedExprKind::StructInit { name: name.to_string(), fields: out } }, span);
        Ok(Spanned::from(TypedExpr { id: 0, ty: ty.clone(), kind: TypedExprKind::Block(vec![node_assign, init]) }, span))
    }

    /// The shared "read these fields off this node" body — `build_json_fields`'
    /// inverse, used by both a struct and a nominal variant's payload, and
    /// where DATA.md stage 8's absent-vs-`null` rule lives.
    fn build_read_json_fields(&mut self, fields: &[(String, Type)], node_name: &str, span: Span) -> Result<Vec<(String, Box<Spanned<TypedExpr>>)>, Spanned<TypeError>> {
        let positional = !fields.is_empty() && is_positional_fields(fields);
        let mut out = Vec::with_capacity(fields.len());
        for (i, (fname, fty)) in fields.iter().enumerate() {
            let fnode = if positional {
                Self::read_call2("frog_json_at", Type::Int, Type::Int, Type::Int, Self::node_var(node_name, span), Self::int_lit(i as i64, span), span)
            } else {
                Self::read_call2("frog_json_get", Type::Int, Type::Str, Type::Int, Self::node_var(node_name, span), Self::str_lit(fname.clone(), span), span)
            };
            let fval = self.build_read_json(fty, fnode, span)?;
            // Absent is permitted iff the type admits `None`, and yields
            // `none` — serde's rule, and the only one under which an
            // optional field is actually optional. A *present* `null` is
            // already handled by `fty`'s own union dispatch, so this is
            // only about absence. A positional field can't be absent
            // (there is no name to be missing) — the array either has the
            // element or it doesn't, which is a shape error.
            let fval = if !positional && Self::admits_none(fty) {
                let has = Self::read_call2("frog_json_has", Type::Int, Type::Str, Type::Bool, Self::node_var(node_name, span), Self::str_lit(fname.clone(), span), span);
                let none_lit = Spanned::from(TypedExpr { id: 0, ty: Type::None, kind: TypedExprKind::NoneLit }, span);
                let absent = self.lower_widen(none_lit, fty)?;
                Spanned::from(TypedExpr {
                    id: 0, ty: fty.clone(),
                    kind: TypedExprKind::Conditional { cond: Box::new(has), true_branch: Box::new(fval), false_branch: Some(Box::new(absent)) },
                }, span)
            } else {
                fval
            };
            out.push((fname.clone(), Box::new(fval)));
        }
        Ok(out)
    }

    /// Does `ty` have `none` among its possible values? Union
    /// normalization flattens and dedups, so this is the whole question —
    /// froglang structurally cannot express serde's `Option<Option<T>>`,
    /// which is exactly why absent and `null` have to mean the same thing
    /// (DATA.md stage 8's decision 1 and its rejection of a distinct
    /// `missing`).
    fn admits_none(ty: &Type) -> bool {
        matches!(ty, Type::Union(members) if members.contains(&Type::None))
    }

    /// `[for i in 0..len(node) do read(at(node, i))]` — `build_read_list`'s
    /// twin, including the `Range<Int>` (not `List<Int>`) iterable.
    fn build_read_json_list(&mut self, elem_ty: &Type, node: Spanned<TypedExpr>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let node_name = format!("__json_n{}", self.next_id); self.next_id += 1;
        let idx_name = format!("__json_i{}", self.next_id); self.next_id += 1;
        let node_assign = Spanned::from(TypedExpr { id: 0, ty: Type::Int, kind: TypedExprKind::Assign { name: node_name.clone(), value: Box::new(node) } }, span);

        let len_call = Self::read_leaf_call("frog_json_len", Type::Int, Type::Int, Self::node_var(&node_name, span), span);
        let range = Spanned::from(TypedExpr {
            id: 0, ty: Type::range(Type::Int),
            kind: TypedExprKind::Range { start: Box::new(Self::int_lit(0, span)), end: Box::new(len_call) },
        }, span);
        let elem_node = Self::read_call2("frog_json_at", Type::Int, Type::Int, Type::Int, Self::node_var(&node_name, span), Self::node_var(&idx_name, span), span);
        let elem_val = self.build_read_json(elem_ty, elem_node, span)?;

        let comprehension = Spanned::from(TypedExpr {
            id: 0, ty: Type::list(elem_ty.clone()),
            kind: TypedExprKind::Comprehension { var: idx_name, iterable: Box::new(range), cond: None, body: Box::new(elem_val), iter_via: None },
        }, span);
        Ok(Spanned::from(TypedExpr { id: 0, ty: Type::list(elem_ty.clone()), kind: TypedExprKind::Block(vec![node_assign, comprehension]) }, span))
    }

    /// `build_read_union_as`'s twin, and it exists for the same reason —
    /// see that function's doc comment for why `json.parse(s): Shape |
    /// JsonError` has to be built against the *wide* type from the start
    /// rather than widened into it afterward.
    fn build_read_json_union_as(&mut self, members: &[Type], node: Spanned<TypedExpr>, target_ty: &Type, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let union_ty = Type::Union(members.to_vec());
        let node_name = format!("__json_u{}", self.next_id); self.next_id += 1;
        let node_assign = Spanned::from(TypedExpr { id: 0, ty: Type::Int, kind: TypedExprKind::Assign { name: node_name.clone(), value: Box::new(node) } }, span);

        let resolved = self.resolve_union(&union_ty).map(|(n, d)| (n.to_string(), d.clone()));
        let body = match resolved {
            Some((enum_name, def)) => self.build_read_json_nominal_union(&enum_name, &def, &node_name, target_ty, span)?,
            None => self.build_read_json_anon_union(members, &node_name, target_ty, span)?,
        };
        Ok(Spanned::from(TypedExpr { id: 0, ty: target_ty.clone(), kind: TypedExprKind::Block(vec![node_assign, body]) }, span))
    }

    /// External tagging's inverse: `frog_json_has(node, "Variant")` picks
    /// the arm, `frog_json_get(node, "Variant")` is the payload. Every
    /// variant is tested (the input might name none of them) with a real
    /// `frog_json_expect` fallback, exactly as `build_read_nominal_union`
    /// does and for the same reason. The `nominal_target` split is also
    /// that function's — see its comment for why a `VariantInit` cannot
    /// simply be stamped with a wider union.
    fn build_read_json_nominal_union(&mut self, enum_name: &str, def: &UnionDef, node_name: &str, union_ty: &Type, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let common = def.common.clone();
        let variants = def.variants.clone();
        let nominal_target = self.resolve_union(union_ty).map(|(n, _)| n) == Some(enum_name);
        let Type::Union(target_members) = union_ty else {
            unreachable!("build_read_json_nominal_union's union_ty is always a Type::Union")
        };
        let target_members = target_members.clone();

        let mut constructions: Vec<Spanned<TypedExpr>> = Vec::with_capacity(variants.len());
        for (idx, (vname, vfields)) in variants.iter().enumerate() {
            let flat: Vec<(String, Type)> = common.iter().chain(vfields.iter()).cloned().collect();
            // The payload object is bound once per arm: every field of the
            // variant reads it, and re-navigating to it per field would
            // re-mark on a malformed input as well as being wasteful.
            let payload_name = format!("__json_p{}", self.next_id); self.next_id += 1;
            let payload = Self::read_call2("frog_json_get", Type::Int, Type::Str, Type::Int, Self::node_var(node_name, span), Self::str_lit(vname.clone(), span), span);
            let payload_assign = Spanned::from(TypedExpr { id: 0, ty: Type::Int, kind: TypedExprKind::Assign { name: payload_name.clone(), value: Box::new(payload) } }, span);
            let fields_out = self.build_read_json_fields(&flat, &payload_name, span)?;

            let variant_ty = Type::strukt(format!("{}.{}", enum_name, vname));
            let init = Spanned::from(TypedExpr {
                id: 0, ty: if nominal_target { union_ty.clone() } else { variant_ty.clone() },
                kind: TypedExprKind::VariantInit { enum_name: enum_name.to_string(), variant: vname.clone(), tag: idx as u32, fields: fields_out },
            }, span);
            let built = if nominal_target { init } else {
                let tag = target_members.iter().position(|m| *m == variant_ty)
                    .expect("every variant of the union being parsed is a member of the target union") as u32;
                Spanned::from(TypedExpr { id: 0, ty: union_ty.clone(), kind: TypedExprKind::Widen { value: Box::new(init), tag } }, span)
            };
            constructions.push(Spanned::from(TypedExpr {
                id: 0, ty: union_ty.clone(), kind: TypedExprKind::Block(vec![payload_assign, built]),
            }, span));
        }

        let last = constructions.len() - 1;
        let fail = Self::read_call2("frog_json_expect", Type::Int, Type::Str, Type::Int, Self::node_var(node_name, span), Self::str_lit(format!("expected an object tagged with a variant of {}", enum_name), span), span);
        let mut tail = Spanned::from(TypedExpr {
            id: 0, ty: union_ty.clone(), kind: TypedExprKind::Block(vec![fail, constructions[last].clone()]),
        }, span);
        for idx in (0..variants.len()).rev() {
            let vname = &variants[idx].0;
            let test = Self::read_call2("frog_json_has", Type::Int, Type::Str, Type::Bool, Self::node_var(node_name, span), Self::str_lit(vname.clone(), span), span);
            tail = Spanned::from(TypedExpr {
                id: 0, ty: union_ty.clone(),
                kind: TypedExprKind::Conditional { cond: Box::new(test), true_branch: Box::new(constructions[idx].clone()), false_branch: Some(Box::new(tail)) },
            }, span);
        }
        Ok(tail)
    }

    /// Anonymous unions dispatch on the JSON *kind* of the node, one
    /// `frog_json_is_*` per member — which only works while no two members
    /// share a kind (`check_json_union_readable`).
    fn build_read_json_anon_union(&mut self, members: &[Type], node_name: &str, union_ty: &Type, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let Type::Union(target_members) = union_ty else {
            unreachable!("build_read_json_anon_union's union_ty is always a Type::Union")
        };
        let target_members = target_members.clone();
        // `members`, not `union_ty`: the *dispatch set* is what has to be
        // unambiguous, and `lower_json_parse`'s `T | JsonError` hatch
        // additionally carries `JsonError`, which is never dispatched on.
        // Checked here, at the point of descent, rather than by walking the
        // type up front — same reasoning as `check_union_readable`.
        self.check_json_union_readable(&Type::Union(members.to_vec()), span)?;

        let mut constructions: Vec<Spanned<TypedExpr>> = Vec::with_capacity(members.len());
        for member_ty in members {
            let value = if *member_ty == Type::None {
                Spanned::from(TypedExpr { id: 0, ty: Type::None, kind: TypedExprKind::NoneLit }, span)
            } else {
                self.build_read_json(member_ty, Self::node_var(node_name, span), span)?
            };
            constructions.push(value);
        }
        let widened: Vec<Spanned<TypedExpr>> = constructions.into_iter().zip(members.iter())
            .map(|(v, mty)| {
                let tag = target_members.iter().position(|m| m == mty)
                    .expect("every dispatch member is present in its own target union") as u32;
                Spanned::from(TypedExpr { id: 0, ty: union_ty.clone(), kind: TypedExprKind::Widen { value: Box::new(v), tag } }, span)
            })
            .collect();

        let last = widened.len() - 1;
        let fail = Self::read_call2("frog_json_expect", Type::Int, Type::Str, Type::Int, Self::node_var(node_name, span), Self::str_lit(format!("expected {}", union_ty), span), span);
        let mut tail = Spanned::from(TypedExpr {
            id: 0, ty: union_ty.clone(), kind: TypedExprKind::Block(vec![fail, widened[last].clone()]),
        }, span);
        for idx in (0..members.len()).rev() {
            let predicate = self.json_kind_predicate(&members[idx]);
            let test = Self::read_leaf_call(predicate, Type::Int, Type::Bool, Self::node_var(node_name, span), span);
            tail = Spanned::from(TypedExpr {
                id: 0, ty: union_ty.clone(),
                kind: TypedExprKind::Conditional { cond: Box::new(test), true_branch: Box::new(widened[idx].clone()), false_branch: Some(Box::new(tail)) },
            }, span);
        }
        Ok(tail)
    }

    /// Does `ty` occupy JSON's *array* shape rather than its object shape?
    /// A list does — and so does a positional ("tuple") struct, which
    /// `build_json_fields` writes as `[...]` because it has no field names
    /// to key an object by. The two must agree exactly: a value that
    /// dispatches as an object but was written as an array cannot parse
    /// back from its own `json.to_str` output.
    ///
    /// Field *names* decide it, and substituting a generic struct's
    /// arguments never renames a field, so the stored template answers for
    /// every instantiation — no `&mut self` materialization needed. A
    /// nominal union's *variant* type (`Shape.Circle`) is a registered
    /// struct here too, and correctly so: reached on its own it is written
    /// as its bare payload by the same `build_json_fields`. Reached as a
    /// member of its own union it is externally tagged and never asks this
    /// question, because `build_read_json_nominal_union` dispatches on the
    /// tag key instead.
    fn json_is_array_shape(&self, ty: &Type) -> bool {
        if ty.as_list_elem().is_some() { return true }
        let Some(name) = ty.as_struct_name() else { return false };
        // Vacuously positional for a nullary struct, which
        // `build_json_fields` writes as `{}` — same guard, same reason.
        matches!(self.struct_templates.get(name), Some(f) if !f.is_empty() && is_positional_fields(f))
    }

    /// Which `frog_json_is_*` predicate identifies `ty` on the wire. Two
    /// members of an anonymous union that answer to the same predicate
    /// cannot be told apart, so this doubles as the ambiguity key for
    /// `check_json_union_readable`.
    ///
    /// `Int` and `Float` deliberately share `"number"` as an ambiguity key
    /// even though `frog_json_is_int`/`_is_float` can distinguish them:
    /// they do so by whether the *producer* wrote a `.`, and JSON has one
    /// number type, so treating `1` in an `Int | Float` slot as decisive
    /// would make the result depend on a spelling choice no schema
    /// constrains. DATA.md stage 8's decision 3 rejects `Int | Float` for
    /// exactly this. `Float` therefore dispatches on the *whole* number
    /// kind (`frog_json_is_number`), matching what `frog_json_float`
    /// accepts; `Int` keeps the strict `frog_json_is_int`, matching
    /// `frog_json_int`. Since no union may hold both, the two never meet.
    fn json_kind_predicate(&self, ty: &Type) -> &'static str {
        match ty {
            Type::Int   => "frog_json_is_int",
            Type::Float => "frog_json_is_number",
            Type::Bool  => "frog_json_is_bool",
            Type::Str   => "frog_json_is_str",
            Type::None  => "frog_json_is_null",
            t if self.json_is_array_shape(t) => "frog_json_is_array",
            _ => "frog_json_is_object",
        }
    }

    /// The ambiguity key `check_json_union_readable` groups by — the
    /// predicate name, except that every number is one group. See
    /// `json_kind_predicate`.
    fn json_dispatch_key(&self, ty: &Type) -> &'static str {
        match ty {
            Type::Int | Type::Float => "a number",
            Type::Bool => "a boolean",
            Type::Str  => "a string",
            Type::None => "null",
            t if self.json_is_array_shape(t) => "an array",
            _ => "an object",
        }
    }

    /// `json.parse`'s gate, `check_readable`'s twin.
    fn check_json_readable(&self, ty: &Type, span: Span) -> Result<(), Spanned<TypeError>> {
        self.check_no_recursive_union(ty, span, "json.parse", "constructs it field-by-field")?;
        self.check_json_union_readable(ty, span)
    }

    /// An anonymous union is parseable only while its members occupy
    /// distinct JSON shapes — `Int?` and `Int | Str` are fine, `Int |
    /// Float` and `Circle | Rect` are not. Nominal unions are exempt: they
    /// are externally tagged, so `frog_json_has` tells them apart by name
    /// no matter how alike their payloads are, which is precisely what
    /// DATA.md stage 8's "make Deserialize a property of nominal unions"
    /// is pointing at.
    fn check_json_union_readable(&self, ty: &Type, span: Span) -> Result<(), Spanned<TypeError>> {
        let Type::Union(members) = ty else { return Ok(()) };
        if self.resolve_union(ty).is_some() { return Ok(()) }
        for (i, a) in members.iter().enumerate() {
            for b in members.iter().skip(i + 1) {
                if self.json_dispatch_key(a) == self.json_dispatch_key(b) {
                    return Err(Spanned::from(TypeError {
                        msg: format!(
                            "json.parse can't tell {} and {} apart in {} — both are {} in JSON; wrap them in a 'data ... is ...' union instead",
                            a, b, ty, self.json_dispatch_key(a)
                        )
                    }, span));
                }
            }
        }
        Ok(())
    }

    /// `json.parse(s): T | JsonError` — `lower_read`'s twin, structurally
    /// identical down to the temp-binding that forces the happy path to run
    /// (and so discover any sticky failure) *before* `frog_json_failed()`
    /// is consulted. See `lower_read` for the reasoning behind each step.
    fn lower_json_parse(&mut self, c: CallExpr, expected: &Type, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        if c.args.len() != 1 {
            return Err(Spanned::from(TypeError {
                msg: format!("Wrong number of arguments, expected 1, got {}", c.args.len())
            }, span));
        }
        let arg = self.check_and_lower(c.args.into_iter().next().expect("arity checked just above"))?;
        let arg_span = arg.span;
        let arg_ty = self.lookup(&arg.item.ty);
        if arg_ty != Type::Str {
            return Err(Spanned::from(TypeError {
                msg: format!("json.parse's argument must be Str, got {}", arg_ty)
            }, arg_span));
        }

        let json_error_ty = Type::strukt("JsonError");
        let members = match expected {
            Type::Union(ms) if ms.contains(&json_error_ty) => ms.clone(),
            _ => return Err(Spanned::from(TypeError {
                msg: "json.parse returns 'T | JsonError'; annotate e.g. 'let x: Person | JsonError = json.parse(s)'".to_string()
            }, span)),
        };
        let t_members: Vec<Type> = members.into_iter().filter(|m| *m != json_error_ty).collect();
        if t_members.is_empty() {
            return Err(Spanned::from(TypeError {
                msg: "json.parse needs a type to parse besides JsonError itself".to_string()
            }, span));
        }

        let node_name = format!("__json_root{}", self.next_id); self.next_id += 1;
        let result_name = format!("__json_result{}", self.next_id); self.next_id += 1;

        let open_call = Self::read_leaf_call("frog_json_open", Type::Str, Type::Int, arg, span);
        let open_assign = Spanned::from(TypedExpr { id: 0, ty: Type::Int, kind: TypedExprKind::Assign { name: node_name.clone(), value: Box::new(open_call) } }, span);

        let happy_widened = if t_members.len() == 1 {
            let t = t_members.into_iter().next().expect("len checked above");
            self.check_json_readable(&t, span)?;
            let happy = self.build_read_json(&t, Self::node_var(&node_name, span), span)?;
            self.lower_widen(happy, expected)?
        } else {
            let t = Type::Union(t_members.clone()).normalize();
            self.check_json_readable(&t, span)?;
            let Type::Union(normalized_members) = t else { unreachable!("normalize of >1 members is always a Union") };
            self.build_read_json_union_as(&normalized_members, Self::node_var(&node_name, span), expected, span)?
        };

        let msg_call = Self::read_call0("frog_json_msg", Type::Str, span);
        let offset_call = Self::read_call0("frog_json_offset", Type::Int, span);
        let error_value = Spanned::from(TypedExpr {
            id: 0, ty: json_error_ty.clone(),
            kind: TypedExprKind::StructInit { name: "JsonError".to_string(), fields: vec![
                ("msg".to_string(), Box::new(msg_call)), ("offset".to_string(), Box::new(offset_call)),
            ] },
        }, span);
        let error_widened = self.lower_widen(error_value, expected)?;

        let happy_name = format!("__json_happy{}", self.next_id); self.next_id += 1;
        let happy_assign = Spanned::from(TypedExpr { id: 0, ty: expected.clone(), kind: TypedExprKind::Assign { name: happy_name.clone(), value: Box::new(happy_widened) } }, span);
        let happy_var = Spanned::from(TypedExpr { id: 0, ty: expected.clone(), kind: TypedExprKind::Var(happy_name) }, span);

        let failed_test = Self::read_call0("frog_json_failed", Type::Bool, span);
        let dispatch = Spanned::from(TypedExpr {
            id: 0, ty: expected.clone(),
            kind: TypedExprKind::Conditional { cond: Box::new(failed_test), true_branch: Box::new(error_widened), false_branch: Some(Box::new(happy_var)) },
        }, span);
        let result_assign = Spanned::from(TypedExpr { id: 0, ty: expected.clone(), kind: TypedExprKind::Assign { name: result_name.clone(), value: Box::new(dispatch) } }, span);
        let close_call = Self::read_call0("frog_json_close", Type::None, span);
        let result_var = Spanned::from(TypedExpr { id: 0, ty: expected.clone(), kind: TypedExprKind::Var(result_name) }, span);

        Ok(Spanned::from(TypedExpr {
            id: 0, ty: expected.clone(),
            kind: TypedExprKind::Block(vec![open_assign, happy_assign, result_assign, close_call, result_var]),
        }, span))
    }

    // ── `read` (`plans/DATA.md` stage 5) ────────────────────────────────────
    //
    // `read`'s inverse relationship to `repr` is exact at the desugar layer
    // too: `build_read(ty, node, span)` has the same per-type dispatch shape
    // as `build_repr(ty, v, span)`, with the direction of data flow
    // reversed — instead of consuming a real froglang *value* and producing
    // `Str` fragments, it consumes a *node handle* (always `Type::Int` at
    // the froglang level — an opaque pointer into `runtime::read`'s parsed
    // `Expression` tree, `runtime/read.rs`'s own doc comment) and produces
    // a real froglang value. Every `frog_read_*` accessor is
    // sticky-error-tolerant (records the *first* mismatch, keeps going with
    // some always-valid placeholder — `runtime/read.rs` again), which is
    // what lets this build one unconditional "happy path" expression per
    // type with no early-exit control flow of its own: the caller
    // (`lower_read`, below) checks `frog_read_failed()` exactly once, after
    // the whole tree has been built, and discards the result in favor of a
    // `ReadError` if it has.
    //
    // Unlike `build_repr`, a union's dispatch here cannot skip a test for
    // the "last" alternative: `repr`'s own output is exhaustive by
    // construction (whichever member `desugar_notation` started from), but
    // `read`'s input is an arbitrary string that might name none of a
    // union's members at all, so every alternative is tested and a real
    // "nothing matched" fallback marks failure (`frog_read_expect`) before
    // falling through to a placeholder construction — see
    // `build_read_nominal_union`/`build_read_anon_union`.

    fn read_leaf_call(rt_name: &str, param: Type, ret: Type, node: Spanned<TypedExpr>, span: Span) -> Spanned<TypedExpr> {
        let callable = Spanned::from(TypedExpr {
            id: 0,
            ty: Type::Function { params: vec![param], result: Box::new(ret.clone()) },
            kind: TypedExprKind::Var(rt_name.to_string()),
        }, span);
        Spanned::from(TypedExpr {
            id: 0, ty: ret,
            kind: TypedExprKind::Call { callable: Box::new(callable), args: vec![Arg::Value(node)] },
        }, span)
    }

    fn read_call2(rt_name: &str, p0: Type, p1: Type, ret: Type, a: Spanned<TypedExpr>, b: Spanned<TypedExpr>, span: Span) -> Spanned<TypedExpr> {
        let callable = Spanned::from(TypedExpr {
            id: 0,
            ty: Type::Function { params: vec![p0, p1], result: Box::new(ret.clone()) },
            kind: TypedExprKind::Var(rt_name.to_string()),
        }, span);
        Spanned::from(TypedExpr {
            id: 0, ty: ret,
            kind: TypedExprKind::Call { callable: Box::new(callable), args: vec![Arg::Value(a), Arg::Value(b)] },
        }, span)
    }

    fn int_lit(v: i64, span: Span) -> Spanned<TypedExpr> {
        Spanned::from(TypedExpr { id: 0, ty: Type::Int, kind: TypedExprKind::IntLit(v) }, span)
    }

    fn node_var(name: &str, span: Span) -> Spanned<TypedExpr> {
        Spanned::from(TypedExpr { id: 0, ty: Type::Int, kind: TypedExprKind::Var(name.to_string()) }, span)
    }

    /// `read`'s per-type dispatch, `build_repr`'s inverse. `ty` must
    /// already be `self.lookup`-resolved (same contract as `build_repr`).
    fn build_read(&mut self, ty: &Type, node: Spanned<TypedExpr>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        if let Some(elem_ty) = ty.as_range_elem().cloned() {
            self.materialize_struct(ty);
            let lo_node = Self::read_leaf_call("frog_read_range_lo", Type::Int, Type::Int, node.clone(), span);
            let hi_node = Self::read_leaf_call("frog_read_range_hi", Type::Int, Type::Int, node, span);
            let lo = self.build_read(&elem_ty, lo_node, span)?;
            let hi = self.build_read(&elem_ty, hi_node, span)?;
            let name = ty.as_struct_name().expect("Range is a struct name").to_string();
            return Ok(Spanned::from(TypedExpr {
                id: 0, ty: ty.clone(),
                kind: TypedExprKind::StructInit { name, fields: vec![("start".to_string(), Box::new(lo)), ("end".to_string(), Box::new(hi))] },
            }, span));
        }
        if let Some(name) = ty.as_struct_name().map(str::to_string) {
            return self.build_read_struct(&name, ty, node, span);
        }
        if let Some(elem_ty) = ty.as_list_elem().cloned() {
            return self.build_read_list(&elem_ty, node, span);
        }
        match ty {
            Type::Union(members) => {
                let members = members.clone();
                self.build_read_union(&members, node, span)
            }
            Type::Str   => Ok(Self::read_leaf_call("frog_read_str", Type::Int, Type::Str, node, span)),
            Type::None  => Ok(Spanned::from(TypedExpr { id: 0, ty: Type::None, kind: TypedExprKind::NoneLit }, span)),
            Type::Int   => Ok(Self::read_leaf_call("frog_read_int", Type::Int, Type::Int, node, span)),
            Type::Float => Ok(Self::read_leaf_call("frog_read_float", Type::Int, Type::Float, node, span)),
            Type::Bool  => Ok(Self::read_leaf_call("frog_read_bool", Type::Int, Type::Bool, node, span)),
            other => Err(Spanned::from(TypeError { msg: format!("{} has no notation to read", other) }, span)),
        }
    }

    /// `Name(f=read(field), ...)` / `Name(read(arg), ...)` — struct
    /// notation's inverse, `build_repr_struct`'s mirror. Does not verify
    /// the call's own callee name against `name`: `read`'s error surface
    /// is field-by-field (a missing/mistyped field fails there), which is
    /// enough to make the law hold — verifying the outer shape too is a
    /// real but secondary quality gap on malformed/adversarial input, not
    /// on `repr`'s own output.
    fn build_read_struct(&mut self, name: &str, ty: &Type, node: Spanned<TypedExpr>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let fields = self.materialize_struct(ty);
        let positional = is_positional_fields(&fields);
        let mut out = Vec::with_capacity(fields.len());
        for (i, (fname, fty)) in fields.iter().enumerate() {
            let fnode = if positional {
                Self::read_call2("frog_read_arg", Type::Int, Type::Int, Type::Int, node.clone(), Self::int_lit(i as i64, span), span)
            } else {
                Self::read_call2("frog_read_field", Type::Int, Type::Str, Type::Int, node.clone(), Self::str_lit(fname.clone(), span), span)
            };
            let fval = self.build_read(fty, fnode, span)?;
            out.push((fname.clone(), Box::new(fval)));
        }
        Ok(Spanned::from(TypedExpr { id: 0, ty: ty.clone(), kind: TypedExprKind::StructInit { name: name.to_string(), fields: out } }, span))
    }

    /// `[for i in 0..len(node) do read(list_at(node, i))]` — `List<T>`
    /// notation's inverse. `node` is bound to a temporary first, matching
    /// `build_repr_list`'s reasoning (the loop reads it more than once).
    fn build_read_list(&mut self, elem_ty: &Type, node: Spanned<TypedExpr>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let node_name = format!("__read_n{}", self.next_id); self.next_id += 1;
        let idx_name = format!("__read_i{}", self.next_id); self.next_id += 1;
        let node_assign = Spanned::from(TypedExpr { id: 0, ty: Type::Int, kind: TypedExprKind::Assign { name: node_name.clone(), value: Box::new(node) } }, span);

        let len_call = Self::read_leaf_call("frog_read_list_len", Type::Int, Type::Int, Self::node_var(&node_name, span), span);
        // `Type::range(Int)`, not `Type::list(Int)`: `TypedExprKind::Range`'s
        // own doc comment ("eagerly materialized as List<Int>") is stale —
        // `lower_range` types a bare `a..b` as `Range<Int>` (RANGES.md's
        // "Range as a real builtin type"), and codegen's `Range` arm
        // compiles it as the flat `(start, end)` pair that type's unboxed
        // representation is, not an allocated list. `Comprehension`
        // already iterates a `Range<Int>` iterable directly (the same
        // shape any `[for i in 0..n do ...]` produces), so this just needs
        // the matching type.
        let range = Spanned::from(TypedExpr {
            id: 0, ty: Type::range(Type::Int),
            kind: TypedExprKind::Range { start: Box::new(Self::int_lit(0, span)), end: Box::new(len_call) },
        }, span);
        let elem_node = Self::read_call2("frog_read_list_at", Type::Int, Type::Int, Type::Int, Self::node_var(&node_name, span), Self::node_var(&idx_name, span), span);
        let elem_val = self.build_read(elem_ty, elem_node, span)?;

        let comprehension = Spanned::from(TypedExpr {
            id: 0, ty: Type::list(elem_ty.clone()),
            kind: TypedExprKind::Comprehension { var: idx_name, iterable: Box::new(range), cond: None, body: Box::new(elem_val), iter_via: None },
        }, span);
        Ok(Spanned::from(TypedExpr { id: 0, ty: Type::list(elem_ty.clone()), kind: TypedExprKind::Block(vec![node_assign, comprehension]) }, span))
    }

    /// Union notation's inverse. `node` is bound to a temporary first
    /// (every dispatch arm reads it, most more than once).
    fn build_read_union(&mut self, members: &[Type], node: Spanned<TypedExpr>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let union_ty = Type::Union(members.to_vec());
        self.build_read_union_as(members, node, &union_ty, span)
    }

    /// `build_read_union`, but every constructed node is stamped
    /// `target_ty` rather than `Type::Union(members)` — `lower_read`'s own
    /// escape hatch for `read(s): T | ReadError` where `T` is *itself*
    /// already a union (`Shape | ReadError`, not just `Int | ReadError`).
    ///
    /// `lower_widen` can't take an already-union-typed value (`Shape`, from
    /// an ordinary `build_read_union` call) and widen it into a *wider*
    /// union (`Shape | ReadError`) after the fact — it explicitly rejects
    /// union-into-union widening, since the two have different tag
    /// numbering and (once member count crosses `MAX_INLINE_UNION_MEMBERS`)
    /// potentially different representations entirely (inline columns vs.
    /// boxed). But the *construction* nodes don't need the narrower type —
    /// `Widen`'s own `tag` is defined as "position in *this* node's target
    /// union" in both layouts — so building directly against `expected`
    /// from the start, instead of building against the narrower `T` and
    /// widening afterward, sidesteps the limitation rather than needing to
    /// lift it.
    ///
    /// The one node that is *not* target-type-agnostic is `VariantInit`:
    /// its tag is a nominal union's declaration index, which only the
    /// inline layout reconciles with an anonymous union's normalized
    /// member positions. `build_read_nominal_union` handles that — see its
    /// `nominal_target` split — rather than stamping a wider union onto a
    /// `VariantInit`.
    fn build_read_union_as(&mut self, members: &[Type], node: Spanned<TypedExpr>, target_ty: &Type, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let union_ty = Type::Union(members.to_vec());
        let node_name = format!("__read_u{}", self.next_id); self.next_id += 1;
        let node_assign = Spanned::from(TypedExpr { id: 0, ty: Type::Int, kind: TypedExprKind::Assign { name: node_name.clone(), value: Box::new(node) } }, span);

        let resolved = self.resolve_union(&union_ty).map(|(n, d)| (n.to_string(), d.clone()));
        let body = match resolved {
            Some((enum_name, def)) => self.build_read_nominal_union(&enum_name, &def, &node_name, target_ty, span)?,
            None => self.build_read_anon_union(members, &node_name, target_ty, span)?,
        };
        Ok(Spanned::from(TypedExpr { id: 0, ty: target_ty.clone(), kind: TypedExprKind::Block(vec![node_assign, body]) }, span))
    }

    /// Every variant is tested — unlike `build_repr_nominal_union`, the
    /// last one gets no "it must be this one" exemption, since the input
    /// might match none of them. The final fallback marks failure
    /// (`frog_read_expect`) and reconstructs the last variant anyway, off
    /// the same (mismatched) node — a type-correct placeholder, discarded
    /// by `lower_read` once it sees `frog_read_failed()`.
    fn build_read_nominal_union(&mut self, enum_name: &str, def: &UnionDef, node_name: &str, union_ty: &Type, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let common = def.common.clone();
        let variants = def.variants.clone();

        // Is `union_ty` the enum's *own* union type, or a wider anonymous
        // one (`build_read_union_as`'s escape hatch — `lower_read` reading
        // a nominal `T` straight into `T | ReadError`)? The two need
        // different construction nodes, because a boxed union's tag means
        // different things in each: `VariantInit` boxes the *declaration*
        // index, which is what `IsVariant`/`emit_is_variant` compares
        // against, while an anonymous union discriminates by normalized
        // member position (`TypeTag`/`emit_tag_test`). Only the inline
        // layout reconciles them (`compile_variant_init` converts through
        // `nominal_member_index` there), so a `VariantInit` stamped with
        // the wider union is correct only while that union stays inline —
        // past `MAX_INLINE_UNION_MEMBERS` it boxes a declaration index
        // into a value everything downstream reads as a member position,
        // and the mismatch is a wrong-member read, not a clean failure.
        //
        // So for a wider target, build the variant at its own exact
        // qualified type (`Enum.Variant` — `compile_variant_init`'s
        // no-tag, no-box "the expected type *was* the variant" case) and
        // `Widen` that into the target, exactly as `build_read_anon_union`
        // does for its own members. `Widen`'s tag is defined as normalized
        // member position in both layouts, so this needs no special case
        // for either.
        let nominal_target = self.resolve_union(union_ty).map(|(n, _)| n) == Some(enum_name);
        let Type::Union(target_members) = union_ty else {
            unreachable!("build_read_nominal_union's union_ty is always a Type::Union")
        };
        let target_members = target_members.clone();

        let mut constructions: Vec<Spanned<TypedExpr>> = Vec::with_capacity(variants.len());
        for (idx, (vname, vfields)) in variants.iter().enumerate() {
            // Field order and naming mirror `build_repr_nominal_union`'s
            // exactly — common fields first, then the variant's own, and
            // positional ("tuple struct") fields read back bare by
            // position (`frog_read_arg`) since that is how `repr` wrote
            // them. Same split `build_read_struct` makes.
            let flat: Vec<(String, Type)> = common.iter().chain(vfields.iter()).cloned().collect();
            let positional = is_positional_fields(&flat);
            let mut fields_out = Vec::new();
            for (i, (fname, fty)) in flat.iter().enumerate() {
                let fnode = if positional {
                    Self::read_call2("frog_read_arg", Type::Int, Type::Int, Type::Int, Self::node_var(node_name, span), Self::int_lit(i as i64, span), span)
                } else {
                    Self::read_call2("frog_read_field", Type::Int, Type::Str, Type::Int, Self::node_var(node_name, span), Self::str_lit(fname.clone(), span), span)
                };
                let fval = self.build_read(fty, fnode, span)?;
                fields_out.push((fname.clone(), Box::new(fval)));
            }
            let variant_ty = Type::strukt(format!("{}.{}", enum_name, vname));
            let init = Spanned::from(TypedExpr {
                id: 0, ty: if nominal_target { union_ty.clone() } else { variant_ty.clone() },
                kind: TypedExprKind::VariantInit { enum_name: enum_name.to_string(), variant: vname.clone(), tag: idx as u32, fields: fields_out },
            }, span);
            constructions.push(if nominal_target { init } else {
                let tag = target_members.iter().position(|m| *m == variant_ty)
                    .expect("every variant of the union being read is a member of the target union") as u32;
                Spanned::from(TypedExpr { id: 0, ty: union_ty.clone(), kind: TypedExprKind::Widen { value: Box::new(init), tag } }, span)
            });
        }

        let last = constructions.len() - 1;
        let fail = Self::read_call2("frog_read_expect", Type::Int, Type::Str, Type::Int, Self::node_var(node_name, span), Self::str_lit(format!("expected a variant of {}", enum_name), span), span);
        let mut tail = Spanned::from(TypedExpr {
            id: 0, ty: union_ty.clone(), kind: TypedExprKind::Block(vec![fail, constructions[last].clone()]),
        }, span);
        for idx in (0..variants.len()).rev() {
            let vname = &variants[idx].0;
            let test = Self::read_call2("frog_read_is_call", Type::Int, Type::Str, Type::Bool, Self::node_var(node_name, span), Self::str_lit(vname.clone(), span), span);
            tail = Spanned::from(TypedExpr {
                id: 0, ty: union_ty.clone(),
                kind: TypedExprKind::Conditional { cond: Box::new(test), true_branch: Box::new(constructions[idx].clone()), false_branch: Some(Box::new(tail)) },
            }, span);
        }
        Ok(tail)
    }

    /// Anonymous union notation's inverse — one `frog_read_is_*` kind
    /// predicate tested per member (a union with more than one
    /// struct-shaped member, the one case a syntactic kind test can't
    /// disambiguate, is rejected below by `check_union_readable`), same
    /// "every alternative tested, real fallback" shape as the nominal case
    /// above.
    fn build_read_anon_union(&mut self, members: &[Type], node_name: &str, union_ty: &Type, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        // `Widen`'s own `tag` is "position in *this node's* target union"
        // (`TypedExprKind::Widen`'s doc comment) — `union_ty`'s member
        // list, which is `members` for an ordinary (recursive-field) call
        // but a *wider* list for `build_read_union_as`'s escape hatch
        // (`lower_read` widening `T` directly into `T | ReadError`), so the
        // tag has to come from there, not from `members`' own position.
        let Type::Union(target_members) = union_ty else {
            unreachable!("build_read_anon_union's union_ty is always a Type::Union")
        };
        let target_members = target_members.clone();

        // The kind tests below can't tell two struct-shaped members apart
        // — `lower_read` runs this on the type it was annotated with, but
        // a union reached recursively (a struct field, a list element)
        // arrives here without ever having passed through that check.
        // `members`, not `union_ty`: the dispatch set is what has to be
        // unambiguous, and for `lower_read`'s `T | ReadError` hatch the
        // target additionally carries `ReadError`, which is never
        // dispatched on.
        self.check_union_readable(&Type::Union(members.to_vec()), span)?;

        let mut constructions: Vec<Spanned<TypedExpr>> = Vec::with_capacity(members.len());
        for member_ty in members {
            let value = if *member_ty == Type::None {
                Spanned::from(TypedExpr { id: 0, ty: Type::None, kind: TypedExprKind::NoneLit }, span)
            } else {
                self.build_read(member_ty, Self::node_var(node_name, span), span)?
            };
            constructions.push(value);
        }
        let widened: Vec<Spanned<TypedExpr>> = constructions.into_iter().zip(members.iter())
            .map(|(v, mty)| {
                let tag = target_members.iter().position(|m| m == mty)
                    .expect("every dispatch member is present in its own target union") as u32;
                Spanned::from(TypedExpr { id: 0, ty: union_ty.clone(), kind: TypedExprKind::Widen { value: Box::new(v), tag } }, span)
            })
            .collect();

        let last = widened.len() - 1;
        let fail = Self::read_call2("frog_read_expect", Type::Int, Type::Str, Type::Int, Self::node_var(node_name, span), Self::str_lit(format!("expected {}", union_ty), span), span);
        let mut tail = Spanned::from(TypedExpr {
            id: 0, ty: union_ty.clone(), kind: TypedExprKind::Block(vec![fail, widened[last].clone()]),
        }, span);
        for idx in (0..members.len()).rev() {
            let predicate = match &members[idx] {
                Type::Int   => "frog_read_is_int",
                Type::Float => "frog_read_is_float",
                Type::Bool  => "frog_read_is_bool",
                Type::Str   => "frog_read_is_str",
                Type::None  => "frog_read_is_none",
                t if t.as_list_elem().is_some()  => "frog_read_is_list",
                t if t.as_range_elem().is_some() => "frog_read_is_range",
                _ => "frog_read_is_struct",
            };
            let test = Self::read_leaf_call(predicate, Type::Int, Type::Bool, Self::node_var(node_name, span), span);
            tail = Spanned::from(TypedExpr {
                id: 0, ty: union_ty.clone(),
                kind: TypedExprKind::Conditional { cond: Box::new(test), true_branch: Box::new(widened[idx].clone()), false_branch: Some(Box::new(tail)) },
            }, span);
        }
        Ok(tail)
    }

    /// `read`'s counterpart to `check_reprable`: the same recursive-union
    /// prediction (`build_read` can't emit unbounded branch trees any more
    /// than `build_repr` can), plus a check `repr` doesn't need — an
    /// anonymous union with more than one struct-shaped member has no
    /// syntactic feature `frog_read_is_struct` can use to tell them apart
    /// (both `repr` to a bare `Name(...)` call), so `read` at that type
    /// would silently and unpredictably pick one. Named per DATA.md
    /// Stage 8's identical ruling for JSON (`Int | Float` there; the struct
    /// case here), stated once so both can point at it.
    fn check_readable(&self, ty: &Type, span: Span) -> Result<(), Spanned<TypeError>> {
        self.check_no_recursive_union(ty, span, "read", "constructs it field-by-field")?;
        self.check_union_readable(ty, span)
    }

    /// The struct-shaped-members half of `check_readable`, on its own so
    /// it can also run at every union `build_read` *descends into* — a
    /// union reached through a struct field or a list element is dispatched
    /// by exactly the same `frog_read_is_*` kind tests as a top-level one,
    /// so it is ambiguous for exactly the same reason. Checked at the point
    /// of descent (`build_read_anon_union`) rather than by walking `ty`
    /// up front, so the diagnostic can't disagree with what actually gets
    /// built: the recursion that reaches a nested union and the recursion
    /// that would validate it are then the same recursion.
    ///
    /// Nominal unions are exempt (`resolve_union`): their variants `repr`
    /// with a `Enum.Variant(...)` head, which `frog_read_is_call` tells
    /// apart by name.
    fn check_union_readable(&self, ty: &Type, span: Span) -> Result<(), Spanned<TypeError>> {
        if let Type::Union(members) = ty {
            if self.resolve_union(ty).is_none() {
                let struct_members = members.iter().filter(|m| {
                    m.as_struct_name().is_some() && m.as_range_elem().is_none() && m.as_list_elem().is_none()
                }).count();
                if struct_members > 1 {
                    return Err(Spanned::from(TypeError {
                        msg: format!(
                            "read can't tell apart the struct-shaped members of {} — wrap them in a 'data ... is ...' union instead",
                            ty
                        )
                    }, span));
                }
            }
        }
        Ok(())
    }

    /// Resolve a parsed type annotation (`crate::frontend::type_expr`) into a
    /// `Type`. Total over the type grammar — every `TypeExpr` variant is
    /// handled here, so an unresolvable annotation is a *name* problem, never
    /// a shape problem.
    ///
    /// Note that a type name is looked up only among the built-ins and the
    /// declared data types. It deliberately does *not* fall back to the value
    /// environment, which the old `Expression`-sniffing version did — that let
    /// a local variable shadow a type name.
    fn resolve_type_expr(&self, ann: &Spanned<TypeExpr>) -> TypeResult {
        let span = ann.span;
        match &ann.item {
            TypeExpr::Name(name) => self.resolve_type_name(name)
                .ok_or_else(|| Spanned::from(TypeError { msg: format!("Unknown type '{}'", name) }, span)),

            TypeExpr::Apply(name, args) => {
                let arg_types: Vec<Type> = args.iter()
                    .map(|a| self.resolve_type_expr(a))
                    .collect::<Result<_, _>>()?;
                // Declared arity of a named type constructor: every
                // registered generic name — including `List`, registered at
                // arity 1 by `TypeChecker::initial_struct_type_params`
                // (`TRAITS.md` Stage 3c) — is arity-checked through this one
                // `struct_type_params` lookup. `List` still gets its own
                // dedicated `Type::list` construction below (never a plain
                // `Type::Named` built by this generic branch), since it has
                // no field template and stays builtin-represented — this is
                // purely a shared arity check, not a representation change.
                if let Some(binder_names) = self.struct_type_params.get(name) {
                    if arg_types.len() != binder_names.len() {
                        let msg = if name == LIST_NAME {
                            format!("List takes exactly 1 type argument, got {}", arg_types.len())
                        } else {
                            format!(
                                "{} takes exactly {} type argument(s), got {}",
                                name, binder_names.len(), arg_types.len()
                            )
                        };
                        return Err(Spanned::from(TypeError { msg }, span));
                    }
                    if name == LIST_NAME {
                        return Ok(Type::list(arg_types.into_iter().next().expect("arity checked above")));
                    }
                    return Ok(Type::Named { name: name.clone(), args: arg_types });
                }
                if arg_types.is_empty() && (self.struct_templates.contains_key(name) || self.union_defs.contains_key(name)) {
                    return self.resolve_type_name(name)
                        .ok_or_else(|| Spanned::from(TypeError { msg: format!("Unknown type '{}'", name) }, span));
                }
                Err(Spanned::from(
                    TypeError { msg: format!("Type '{}' does not take type arguments", name) }, span))
            }

            TypeExpr::Union(members) => {
                let member_types: Vec<Type> = members.iter()
                    .map(|m| self.resolve_type_expr(m))
                    .collect::<Result<_, _>>()?;
                Ok(Type::Union(member_types).normalize())
            }

            // `T?` is `T | None` and nothing more — the abbreviation exists
            // for readability, not as a distinct type.
            TypeExpr::Optional(inner) => {
                let inner_ty = self.resolve_type_expr(inner)?;
                Ok(Type::Union(vec![inner_ty, Type::None]).normalize())
            }

            TypeExpr::Func(params, result) => {
                let param_types: Vec<Type> = params.iter()
                    .map(|p| self.resolve_type_expr(p))
                    .collect::<Result<_, _>>()?;
                let result_type = self.resolve_type_expr(result)?;
                Ok(Type::Function { params: param_types, result: Box::new(result_type) })
            }
        }
    }
}

// ── unit tests for the pure type-level helpers ───────────────────────────────
//
// The integration suite exercises these only indirectly, through whole
// programs, which makes a failure here surface as a confusing error about
// some unrelated construct. They're small, total functions over `Type`, so
// testing them directly is cheap and pins the exact algebra the rest of the
// checker leans on.
#[cfg(test)]
mod helper_tests {
    use super::*;

    fn union(members: Vec<Type>) -> Type { Type::Union(members) }

    // ── Type::normalize ──────────────────────────────────────────────────────

    #[test]
    fn normalize_flattens_nested_unions() {
        // Member order is `Ord for Type`'s (Int, Float, Bool, Str, ...), not
        // the alphabetical-by-`Display` order this used to assert — see that
        // impl for why the canonical order is no longer a rendering.
        let nested = union(vec![Type::Int, union(vec![Type::Str, Type::Bool])]);
        assert_eq!(nested.normalize(), union(vec![Type::Int, Type::Bool, Type::Str]));
    }

    /// The canonical order is what assigns runtime tags, so pin it directly
    /// rather than only through whatever `normalize` happens to produce.
    #[test]
    fn canonical_order_is_scalars_first_then_composites() {
        let mut ts = vec![
            Type::list(Type::Int),
            tvar("t0"),
            Type::Str,
            Type::Never,
            Type::Int,
            union(vec![Type::Int, Type::Str]),
            Type::None,
            Type::Bool,
            Type::Function { params: vec![Type::Int], result: Box::new(Type::Int) },
            Type::Float,
        ];
        ts.sort();
        assert_eq!(ts, vec![
            Type::Int,
            Type::Float,
            Type::Bool,
            Type::Str,
            Type::None,
            Type::Never,
            Type::list(Type::Int),
            union(vec![Type::Int, Type::Str]),
            Type::Function { params: vec![Type::Int], result: Box::new(Type::Int) },
            tvar("t0"),
        ]);
    }

    /// Two `Named`s at the same rank order by name first, then by argument —
    /// so two instantiations of one generic stay adjacent and in a stable
    /// order regardless of how the program mentions them.
    #[test]
    fn named_types_order_by_name_then_arguments() {
        let mut ts = vec![
            Type::list(Type::Str),
            Type::strukt("Point"),
            Type::list(Type::Int),
            Type::Named { name: "Pair".to_string(), args: vec![Type::Int, Type::Str] },
            Type::Named { name: "Pair".to_string(), args: vec![Type::Int, Type::Int] },
        ];
        ts.sort();
        assert_eq!(ts, vec![
            Type::list(Type::Int),
            Type::list(Type::Str),
            Type::Named { name: "Pair".to_string(), args: vec![Type::Int, Type::Int] },
            Type::Named { name: "Pair".to_string(), args: vec![Type::Int, Type::Str] },
            Type::strukt("Point"),
        ]);
    }

    #[test]
    fn normalize_deduplicates_and_collapses_a_singleton() {
        assert_eq!(union(vec![Type::Int, Type::Int]).normalize(), Type::Int);
    }

    #[test]
    fn normalize_sorts_canonically_so_member_order_is_not_significant() {
        // Two spellings of the same union must produce the same `Type`, or
        // `union_names` lookups and `Widen`'s member-position tags would
        // depend on how the union was written.
        let a = union(vec![Type::Str, Type::Int]).normalize();
        let b = union(vec![Type::Int, Type::Str]).normalize();
        assert_eq!(a, b);
    }

    #[test]
    fn normalize_drops_never_unless_it_stands_alone() {
        // This is what makes `if c then return 1 else 2` type as plain
        // `Int` rather than `Never | Int`.
        assert_eq!(union(vec![Type::Never, Type::Int]).normalize(), Type::Int);
        assert_eq!(union(vec![Type::Never, Type::Never]).normalize(), Type::Never);
    }

    #[test]
    fn normalize_leaves_non_unions_alone() {
        assert_eq!(Type::Int.normalize(), Type::Int);
        assert_eq!(
            Type::list(Type::Int).normalize(),
            Type::list(Type::Int),
        );
    }

    // ── Type::renormalize (canonical form survives resolution) ───────────────
    //
    // `normalize` is only ever run where a union is *built*. Both passes that
    // rewrite an existing union's members in place — `Type::substitute` and
    // `TypeChecker::lookup` — can turn a canonical member list into a
    // non-canonical one, and a member's index in that list is the runtime tag
    // (`lower_widen`, `codegen::union_dispatch_cases`). These pin that they
    // re-canonicalize.

    fn tvar(name: &str) -> Type { Type::TypeVar { name: name.to_string(), bounds: vec![] } }

    #[test]
    fn substitute_recanonicalizes_a_union_whose_member_order_it_changed() {
        // `Str | ~t0` is canonical as written (`Display` sorts "Str" before
        // "~t0"), but binding `~t0` to `Int` makes it `[Str, Int]` — which
        // must come back as the canonical `Int | Str`, or this same union
        // spelled out longhand would disagree with it about every tag.
        let before = union(vec![Type::Str, tvar("t0")]).normalize();
        assert_eq!(before, union(vec![Type::Str, tvar("t0")]), "premise: already canonical");

        let bindings: HashMap<String, Type> = [("t0".to_string(), Type::Int)].into_iter().collect();
        assert_eq!(before.substitute(&bindings), union(vec![Type::Int, Type::Str]));
    }

    #[test]
    fn substitute_collapses_a_union_a_binding_made_redundant() {
        // Two members that resolve to the same type are one member, and a
        // one-member union is not a union at all — the same rule `normalize`
        // applies at construction.
        let bindings: HashMap<String, Type> = [("t0".to_string(), Type::Str)].into_iter().collect();
        assert_eq!(union(vec![Type::Str, tvar("t0")]).substitute(&bindings), Type::Str);
    }

    #[test]
    fn substitute_leaves_an_untouched_union_exactly_as_it_found_it() {
        // The fast path: nothing was rewritten, so nothing needs re-sorting.
        let bindings: HashMap<String, Type> = [("t9".to_string(), Type::Int)].into_iter().collect();
        let u = union(vec![Type::Int, Type::Str]);
        assert_eq!(u.clone().substitute(&bindings), u);
    }

    #[test]
    fn lookup_recanonicalizes_a_union_whose_member_order_it_changed() {
        // `lookup`'s half of the same rule — it resolves through
        // `substitutions` rather than an explicit mapping, but rewrites
        // members exactly the same way.
        let mut tc = TypeChecker::empty();
        tc.substitutions.insert("t0".to_string(), Type::Int);
        assert_eq!(
            tc.lookup(&union(vec![Type::Str, tvar("t0")])),
            union(vec![Type::Int, Type::Str]),
        );
    }

    #[test]
    fn lookup_recanonicalizes_a_union_nested_inside_another_type() {
        // The rewrite is structural, so the hazard reaches a union that is a
        // list's element type or a function's parameter just as much as a
        // top-level one.
        let mut tc = TypeChecker::empty();
        tc.substitutions.insert("t0".to_string(), Type::Int);
        assert_eq!(
            tc.lookup(&Type::list(union(vec![Type::Str, tvar("t0")]))),
            Type::list(union(vec![Type::Int, Type::Str])),
        );
    }

    // ── Display round-trips through the parser ───────────────────────────────
    //
    // The counterpart of `notation`'s `read(repr(x)) == x` for values, and it
    // exists for the same reason: `Display` and the grammar are two halves of
    // one agreement, and nothing had been holding them to it. `TRAITS.md`
    // Stage 3c moved type arguments to `Name<A, B>` and every diagnostic went
    // on printing `Pair(Int, Str)` and `[Int]` — unparseable, and silently so,
    // because no test rendered a type and read it back.
    //
    // Written as a round-trip rather than as expected strings so the two
    // sides cannot drift without a failure here. Anything added to `Display`
    // belongs in `TYPES_THAT_ROUND_TRIP` in the same change.

    /// Parse `text` as a type annotation and resolve it, the way a real
    /// `let x: T = ...` would — `Parser` for the syntax, `resolve_type_expr`
    /// for the `TypeExpr` -> `Type` step.
    fn parse_and_resolve(tc: &mut TypeChecker, text: &str) -> Type {
        use crate::frontend::expression::Expression;
        use crate::frontend::parser::Parser;
        let ast = Parser::parse(&format!("let x: {} = 0", text))
            .unwrap_or_else(|e| panic!("{}: parse error: {:?}", text, e));
        let stmts = match ast.item {
            Expression::Block(s) => s,
            other => vec![Spanned::from(other, ast.span)],
        };
        let ann = match &stmts[0].item {
            Expression::Assign(a) => a.typ.as_ref().expect("annotation present").clone(),
            other => panic!("{}: expected an assignment, got {:?}", text, other),
        };
        tc.resolve_type_expr(&ann)
            .unwrap_or_else(|e| panic!("{}: could not resolve: {:?}", text, e))
    }

    /// Every closed type shape `Display` can produce. `TypeVar` is excluded
    /// on purpose — `~t0` has no source syntax, which is the one documented
    /// exception on `Display for Type`.
    fn types_that_round_trip() -> Vec<Type> {
        let pair = |a: Type, b: Type| Type::Named { name: "Pair".to_string(), args: vec![a, b] };
        vec![
            Type::Int, Type::Float, Type::Bool, Type::Str, Type::None,
            Type::strukt("Point"),
            Type::list(Type::Int),
            Type::list(Type::list(Type::Str)),
            pair(Type::Int, Type::Str),
            pair(Type::Int, Type::list(Type::Str)),
            Type::list(pair(Type::Int, Type::Bool)),
            union(vec![Type::Int, Type::Str]).normalize(),
            union(vec![Type::Int, Type::None]).normalize(),
            union(vec![Type::Int, Type::list(Type::Str)]).normalize(),
            Type::list(union(vec![Type::Int, Type::Str]).normalize()),
            Type::Function { params: vec![Type::Int], result: Box::new(Type::Int) },
            Type::Function { params: vec![Type::Int, Type::Str], result: Box::new(Type::Bool) },
            // A function type inside a union is exactly the case the parens
            // around `->` exist for.
            union(vec![
                Type::Str,
                Type::Function { params: vec![Type::Int], result: Box::new(Type::Int) },
            ]).normalize(),
        ]
    }

    #[test]
    fn every_rendered_type_parses_back_to_itself() {
        let mut tc = TypeChecker::new();
        // `Point` and `Pair<A, B>` have to exist as declared names before an
        // annotation mentioning them resolves.
        tc.struct_templates.insert("Point".to_string(), Vec::new());
        tc.struct_defs.insert(Type::strukt("Point"), Vec::new());
        tc.struct_templates.insert("Pair".to_string(), Vec::new());
        tc.struct_type_params.insert("Pair".to_string(), vec!["A".to_string(), "B".to_string()]);

        for ty in types_that_round_trip() {
            let rendered = ty.to_string();
            let parsed = parse_and_resolve(&mut tc, &rendered);
            assert_eq!(parsed, ty, "rendered as {:?}, parsed back as {}", rendered, parsed);
        }
    }

    /// The specific spellings Stage 3c broke, pinned as literals as well —
    /// the round-trip above would still pass if both sides moved together to
    /// something nobody wants to read.
    #[test]
    fn rendered_types_use_the_current_source_syntax() {
        assert_eq!(Type::list(Type::Int).to_string(), "List<Int>");
        assert_eq!(
            Type::Named { name: "Pair".to_string(), args: vec![Type::Int, Type::Str] }.to_string(),
            "Pair<Int, Str>",
        );
        assert_eq!(
            Type::Function { params: vec![Type::Int], result: Box::new(Type::Int) }.to_string(),
            "(Int -> Int)",
        );
        assert_eq!(union(vec![Type::Int, Type::Str]).normalize().to_string(), "Int | Str");
    }

    // ── widens_to / numeric_join ─────────────────────────────────────────────

    #[test]
    fn widening_is_directed_and_lossless_only() {
        assert!(widens_to(&Type::Int, &Type::Float));
        assert!(!widens_to(&Type::Float, &Type::Int), "Float -> Int is lossy");
        assert!(!widens_to(&Type::Int, &Type::Int), "reflexive case is not a widening");
        assert!(!widens_to(&Type::Int, &Type::Str));
        assert!(!widens_to(&Type::Bool, &Type::Int));
    }

    #[test]
    fn numeric_join_picks_the_wider_type_in_either_argument_order() {
        assert_eq!(numeric_join(&Type::Int, &Type::Float), Some(Type::Float));
        assert_eq!(numeric_join(&Type::Float, &Type::Int), Some(Type::Float));
        assert_eq!(numeric_join(&Type::Int, &Type::Int), Some(Type::Int));
    }

    #[test]
    fn numeric_join_rejects_incompatible_operands() {
        assert_eq!(numeric_join(&Type::Int, &Type::Str), None);
        assert_eq!(numeric_join(&Type::Bool, &Type::Float), None);
    }

    // ── is_positional_fields ─────────────────────────────────────────────────

    #[test]
    fn positional_fields_are_recognised_by_their_index_keys() {
        let positional = vec![("0".to_string(), Type::Int), ("1".to_string(), Type::Str)];
        assert!(is_positional_fields(&positional));
    }

    #[test]
    fn named_fields_are_never_mistaken_for_positional_ones() {
        // A lexed identifier can never be all digits, which is exactly what
        // makes the index marker unambiguous.
        let named = vec![("x".to_string(), Type::Int), ("y".to_string(), Type::Str)];
        assert!(!is_positional_fields(&named));
    }

    #[test]
    fn index_keys_must_be_in_order_to_count_as_positional() {
        // Out-of-order keys mean these indices were assigned by two
        // different field lists and then concatenated — not a single
        // positional list. (This is the shape a union's common fields plus
        // a variant's own positional fields currently produce.)
        let concatenated = vec![("0".to_string(), Type::Int), ("0".to_string(), Type::Str)];
        assert!(!is_positional_fields(&concatenated));
    }

    #[test]
    fn an_empty_field_list_is_vacuously_positional() {
        // A nullary struct/variant takes no arguments under either calling
        // convention, so which style it "is" doesn't matter — but callers
        // must still guard on emptiness before choosing the positional path.
        assert!(is_positional_fields(&[]));
    }

    // ── ScopeStack ───────────────────────────────────────────────────────────

    #[test]
    fn a_binding_made_in_a_scope_is_gone_once_it_closes() {
        let mut s = ScopeStack::new(HashMap::new());
        let mark = s.open();
        s.insert("x".into(), Type::Int);
        assert_eq!(s.get("x"), Some(&Type::Int));
        s.close(mark);
        assert_eq!(s.get("x"), None);
    }

    #[test]
    fn closing_a_scope_restores_a_shadowed_outer_binding() {
        let mut s = ScopeStack::new(HashMap::new());
        let outer = s.open();
        s.insert("x".into(), Type::Int);
        let inner = s.open();
        s.insert("x".into(), Type::Str);
        assert_eq!(s.get("x"), Some(&Type::Str));
        s.close(inner);
        assert_eq!(s.get("x"), Some(&Type::Int), "the outer binding must come back");
        s.close(outer);
        assert_eq!(s.get("x"), None);
    }

    #[test]
    fn repeated_writes_to_one_name_in_one_scope_unwind_to_the_original() {
        // The case that makes replay order matter: the log holds
        // [(x, None), (x, Some(Int))], and only unwinding *backwards*
        // restores Int and then removes x. Replaying forwards would leave
        // `x` bound to Int after the scope closed.
        let mut s = ScopeStack::new(HashMap::new());
        let outer = s.open();
        s.insert("x".into(), Type::Int);
        let inner = s.open();
        s.insert("x".into(), Type::Str);
        s.insert("x".into(), Type::Bool);
        s.insert("x".into(), Type::Float);
        assert_eq!(s.get("x"), Some(&Type::Float));
        s.close(inner);
        assert_eq!(s.get("x"), Some(&Type::Int));
        s.close(outer);
        assert_eq!(s.get("x"), None);
    }

    #[test]
    fn sibling_scopes_do_not_see_each_others_bindings() {
        let mut s = ScopeStack::new(HashMap::new());
        let first = s.open();
        s.insert("a".into(), Type::Int);
        s.close(first);
        let second = s.open();
        assert!(!s.contains_key("a"), "the first scope's binding must not leak into the second");
        s.insert("b".into(), Type::Str);
        s.close(second);
        assert!(!s.contains_key("b"));
    }

    #[test]
    fn top_level_bindings_survive_and_are_not_logged() {
        // Nothing at depth 0 has a scope to unwind to, so logging there
        // would just grow forever in a long-lived REPL session.
        let mut s = ScopeStack::new(HashMap::new());
        s.insert("top".into(), Type::Int);
        assert!(s.log.is_empty(), "a top-level write has nothing to undo");
        let mark = s.open();
        s.insert("scoped".into(), Type::Str);
        s.close(mark);
        assert!(s.log.is_empty(), "the log is empty again once every scope has closed");
        assert_eq!(s.get("top"), Some(&Type::Int), "top-level bindings persist");
    }

    #[test]
    fn a_scope_that_binds_nothing_costs_nothing_to_close() {
        let mut s = ScopeStack::new(HashMap::new());
        s.insert("outer".into(), Type::Int);
        let mark = s.open();
        s.close(mark);
        assert_eq!(s.get("outer"), Some(&Type::Int));
    }

    #[test]
    fn seeded_bindings_are_visible_and_shadowable() {
        // `TypeChecker::new` seeds the stack with `default_context()`
        // (`print`, etc), which must behave like any other top-level binding.
        let mut seed = HashMap::new();
        seed.insert("print".to_string(), Type::Str);
        let mut s = ScopeStack::new(seed);
        assert_eq!(s.get("print"), Some(&Type::Str));
        let mark = s.open();
        s.insert("print".into(), Type::Int);
        assert_eq!(s.get("print"), Some(&Type::Int));
        s.close(mark);
        assert_eq!(s.get("print"), Some(&Type::Str));
    }

    // ── Trait::Show ─────────────────────────────────────────────────────────

    #[test]
    fn show_is_structural_over_lists_and_struct_fields_like_eq() {
        let mut tc = TypeChecker::empty();
        tc.struct_templates.insert("W".to_string(), vec![("xs".to_string(), Type::list(Type::Int))]);
        assert!(tc.type_implements(&Type::strukt("W"), &Trait::Show));
        assert!(tc.type_implements(&Type::list(Type::Str), &Trait::Show));
    }

    #[test]
    fn a_function_typed_field_is_not_show() {
        let mut tc = TypeChecker::empty();
        let f = Type::Function { params: vec![Type::Int], result: Box::new(Type::Int) };
        tc.struct_templates.insert("F".to_string(), vec![("f".to_string(), f)]);
        assert!(!tc.type_implements(&Type::strukt("F"), &Trait::Show));
    }

    // ── join_types ───────────────────────────────────────────────────────────

    #[test]
    fn join_of_identical_types_is_that_type() {
        let mut tc = TypeChecker::empty();
        assert_eq!(tc.join_types(&Type::Int, &Type::Int), Type::Int);
    }

    #[test]
    fn join_of_unrelated_types_is_their_union() {
        let mut tc = TypeChecker::empty();
        assert_eq!(
            tc.join_types(&Type::Int, &Type::Str),
            union(vec![Type::Int, Type::Str]),
        );
    }

    #[test]
    fn join_of_a_member_with_its_own_union_stays_the_union() {
        // The case `join_types` exists for. `unify` returns `true` here via
        // its union-membership shortcut without binding anything, so trusting
        // that boolean would collapse the result to the narrower `Int` —
        // leaving a typed-AST node whose `.ty` disagrees with the multi-value
        // shape one of its branches actually produces, which codegen then
        // reads as a bare scalar.
        let mut tc = TypeChecker::empty();
        let u = union(vec![Type::Int, Type::Str]);
        assert_eq!(tc.join_types(&Type::Int, &u), u);
        assert_eq!(tc.join_types(&u, &Type::Int), u);
    }

    #[test]
    fn join_drops_never() {
        let mut tc = TypeChecker::empty();
        assert_eq!(tc.join_types(&Type::Never, &Type::Int), Type::Int);
        assert_eq!(tc.join_types(&Type::Int, &Type::Never), Type::Int);
    }

    #[test]
    fn join_binds_an_unconstrained_type_var_to_the_concrete_side() {
        // `unify`'s binding side effect is load-bearing: it's how an
        // unannotated lambda parameter picks up a concrete type from the
        // branch it's joined against.
        let mut tc = TypeChecker::empty();
        let v = tc.fresh_var();
        assert_eq!(tc.join_types(&v, &Type::Int), Type::Int);
        assert_eq!(tc.lookup(&v), Type::Int);
    }
}
