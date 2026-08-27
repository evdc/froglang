use std::{collections::HashMap, fmt::Display, vec};

use crate::frontend::{
    expression::{
        AnnotatedExpr, AssignExpr, BinaryExpr, CallExpr, ConditionalExpr, Expression,
        FieldAccessExpr, ForLoopExpr, FunctionExpr, IndexExpr, IsPatternExpr, LiteralExpr,
        MatchArm, Mutability, Pattern, RangeExpr, SliceExpr, UnaryExpr,
    },
    tokens::{Span, Spanned, Token},
};
use crate::frontend::type_expr::TypeExpr;
use crate::frontend::typed_ast::{PlaceSeg, TypedExpr, TypedExprKind, TypedExprRef};
use crate::utils::format_vec;

/// Reserved name for the builtin `panic` alias that `!` desugars to.
/// Contains `!`, which the lexer never produces inside an identifier, so
/// no user-written name can ever collide with or shadow it. See
/// `TypeChecker::default_context` and `build_unwrap_arms`.
const UNWRAP_PANIC_NAME: &str = "panic!builtin";

#[derive(Debug)]
pub struct TypeError {
    pub msg: String
}

type TypeResult = Result<Type, Spanned<TypeError>>;

/// Traits constrain type variables. A type must implement a trait to be bound
/// to a TypeVar that carries that bound.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Trait {
    Num,   // Int, Float — arithmetic operators
    Eq,    // Int, Float, Bool, Str — == and !=
    Ord,   // Int, Float, Str — <, >, <=, >=
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
}

impl Display for Trait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Trait::Num    => write!(f, "Num"),
            Trait::Eq     => write!(f, "Eq"),
            Trait::Ord    => write!(f, "Ord"),
            Trait::Error  => write!(f, "Error"),
            Trait::Truthy => write!(f, "Truthy"),
        }
    }
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

/// The `List` type constructor's name, as it appears in `Type::Named`.
pub const LIST_NAME: &str = "List";

impl Type {
    /// `Type::Named { name: LIST_NAME, args: vec![elem] }`. Prefer this
    /// over constructing `Type::Named` directly for a list.
    pub fn list(elem: Type) -> Type {
        Type::Named { name: LIST_NAME.to_string(), args: vec![elem] }
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

    /// `Some(name)` iff this is a zero-argument `Named` type other than
    /// `List` — i.e. a plain struct. (`List` is excluded so a caller that
    /// wants "the struct name" never mistakes a bare `List` for one; no
    /// zero-arg `List` value exists anyway since it's always applied to
    /// exactly one element type.)
    pub fn as_struct_name(&self) -> Option<&str> {
        match self {
            Type::Named { name, args } if args.is_empty() && name != LIST_NAME => Some(name.as_str()),
            _ => None,
        }
    }

    pub fn is_list(&self) -> bool {
        matches!(self, Type::Named { name, .. } if name == LIST_NAME)
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
                // 3. Sort canonically by display string (stable, readable).
                // `sort_by_cached_key` renders each element's key once, not
                // on every comparison the sort makes.
                seen.sort_by_cached_key(|t| t.to_string());
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

impl Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Type::Int    => write!(f, "Int"),
            Type::Float  => write!(f, "Float"),
            Type::Bool   => write!(f, "Bool"),
            Type::Str    => write!(f, "Str"),
            Type::None   => write!(f, "None"),
            Type::Function { params, result } =>
                write!(f, "{} -> {}", format_vec(params), result),
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
            Type::Named { name, args } if name == LIST_NAME => {
                write!(f, "[{}]", args.first().expect("List always has exactly one arg"))
            }
            Type::Named { name, args } if args.is_empty() => write!(f, "{}", name),
            Type::Named { name, args } => {
                let strs: Vec<String> = args.iter().map(|t| t.to_string()).collect();
                write!(f, "{}({})", name, strs.join(", "))
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
pub type StructDefs = HashMap<String, Vec<(String, Type)>>;

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
            .map(|(k, ty)| (k, Binding { ty, mutable: false, binders: Vec::new() }))
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
        self.insert_raw(name, Binding { ty, mutable, binders: Vec::new() });
    }

    /// A generalized (`TRAITS.md` Stage 2) immutable binding — a `func` or
    /// let-bound-lambda whose type scheme quantifies over `binders`. The
    /// value restriction: only ever called for a syntactic function value,
    /// never for a `mut` binding.
    fn insert_generalized(&mut self, name: String, ty: Type, binders: Vec<(String, Vec<Trait>)>) {
        self.insert_raw(name, Binding { ty, mutable: false, binders });
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
    /// Every name ever bound to a generalized scheme (non-empty `binders`)
    /// during this compilation — `TRAITS.md` Stage 2's codegen gate
    /// consults this in `check_generic_monomorphism` to know which
    /// `Var(name)` references to watch. Monomorphization (Stage 3) is what
    /// eventually makes this field unnecessary; until then, a name in this
    /// set that resolves to 2+ distinct concrete types across the program
    /// is rejected rather than silently miscompiled — a Cranelift function
    /// has exactly one signature.
    generalized_names: std::collections::HashSet<String>,
    /// The single concrete type each generalized name has been observed
    /// instantiated at, across the *whole session* — not just the current
    /// entry. `check_generic_monomorphism`'s own per-entry walk only sees
    /// that entry's typed AST, which is exactly the REPL hazard
    /// `TRAITS.md` Stage 2 flags: a scheme minted at entry 1 and
    /// instantiated at entry 5 needs a check that spans entries, or a
    /// genuinely conflicting second instantiation reaches codegen as a raw
    /// Cranelift verifier panic instead of this gate's clean error —
    /// `f`'s declaration was already frozen concrete by its first use (via
    /// `resolve_single_instantiations` writing straight into
    /// `substitutions`, which persists for the rest of the session), so a
    /// later, different-typed call doesn't even go through the ordinary
    /// occurs-check/unify path that would catch it. Checkpointed like
    /// `substitutions`: a failed entry's would-be instantiation must not
    /// stick, but a successful one persists for the rest of the session,
    /// same as any other type-checking fact learned so far.
    generic_instantiations: HashMap<String, Type>,
}

pub struct TypeCheckerCheckpoint {
    ctx: ScopeStack,
    substitutions: HashMap<String, Type>,
    next_id: u32,
    struct_defs: StructDefs,
    union_defs: UnionDefs,
    union_names: HashMap<Vec<Type>, String>,
    variant_owners: HashMap<String, Vec<String>>,
    return_types: Vec<Type>,
    provides: HashMap<String, Vec<Trait>>,
    func_mut_params: HashMap<String, Vec<bool>>,
    generalized_names: std::collections::HashSet<String>,
    generic_instantiations: HashMap<String, Type>,
}

impl TypeChecker {
    pub fn empty() -> Self {
        TypeChecker { ctx: ScopeStack::new(HashMap::new()), substitutions: HashMap::new(), next_id: 0, struct_defs: HashMap::new(), union_defs: HashMap::new(), union_names: HashMap::new(), variant_owners: HashMap::new(), return_types: Vec::new(), provides: HashMap::new(), func_mut_params: HashMap::new(), host_names: std::collections::HashSet::new(), generalized_names: std::collections::HashSet::new(), generic_instantiations: HashMap::new() }
    }

    pub fn new() -> Self {
        TypeChecker { ctx: ScopeStack::new(TypeChecker::default_context()), substitutions: HashMap::new(), next_id: 0, struct_defs: HashMap::new(), union_defs: HashMap::new(), union_names: HashMap::new(), variant_owners: HashMap::new(), return_types: Vec::new(), provides: HashMap::new(), func_mut_params: HashMap::new(), host_names: std::collections::HashSet::new(), generalized_names: std::collections::HashSet::new(), generic_instantiations: HashMap::new() }
    }

    /// Check whether a concrete type implements the given trait. Only makes
    /// sense for non-TypeVar types; TypeVar-TypeVar unification is handled
    /// separately so this is never called on a TypeVar.
    fn type_implements(&self, ty: &Type, tr: &Trait) -> bool {
        match ty {
            // A union satisfies a trait iff every variant does.
            Type::Union(variants) => variants.iter().all(|v| self.type_implements(v, tr)),
            // Structs get structural `==`/`!=` (desugared into a per-field
            // conjunction at lowering time — see `TypeChecker::desugar_struct_eq`
            // in `check_and_lower`'s `Binary` arm), so they satisfy `Eq`. A
            // struct with a field type that itself doesn't implement `Eq` (e.g.
            // a `List` field — lists don't support `==` at all currently) will
            // fail type-checking when the desugared per-field comparison is
            // itself inferred, which is the correct place for that error to
            // surface, not here.
            Type::Named { name, args } if args.is_empty() && name != LIST_NAME && *tr == Trait::Eq => true,
            // `Error` is granted, not structural — see `provides`.
            Type::Named { name, args } if args.is_empty() && name != LIST_NAME && *tr == Trait::Error => {
                self.provides.get(name).map(|ts| ts.contains(tr)).unwrap_or(false)
            },
            _ => match tr {
                Trait::Num    => matches!(ty, Type::Int | Type::Float),
                Trait::Eq     => matches!(ty, Type::Int | Type::Float | Type::Bool | Type::Str),
                Trait::Ord    => matches!(ty, Type::Int | Type::Float | Type::Str),
                Trait::Error  => false,
                Trait::Truthy => matches!(ty, Type::Int | Type::Float | Type::Bool | Type::Str | Type::None) || ty.is_list(),
            }
        }
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
        Spanned::from(TypedExpr { id: 0, ty: Type::Bool, kind: TypedExprKind::Truthy(Box::new(e)) }, span)
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
        fn subst(ty: &Type, mapping: &HashMap<String, Type>) -> Type {
            match ty {
                Type::TypeVar { name, .. } => mapping.get(name).cloned().unwrap_or_else(|| ty.clone()),
                Type::Function { params, result } => Type::Function {
                    params: params.iter().map(|p| subst(p, mapping)).collect(),
                    result: Box::new(subst(result, mapping)),
                },
                Type::Named { name, args } => Type::Named {
                    name: name.clone(),
                    args: args.iter().map(|a| subst(a, mapping)).collect(),
                },
                Type::Union(variants) => Type::Union(variants.iter().map(|v| subst(v, mapping)).collect()),
                _ => ty.clone(),
            }
        }
        subst(ty, &mapping)
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
    fn generalize(&self, ty: &Type) -> Vec<(String, Vec<Trait>)> {
        let mut ty_vars = Vec::new();
        self.free_vars(ty, &mut ty_vars);
        if ty_vars.is_empty() { return Vec::new(); }
        let mut env_vars = Vec::new();
        for bound in self.ctx.bound_types() {
            self.free_vars(bound, &mut env_vars);
        }
        ty_vars.into_iter().filter(|(n, _)| !env_vars.iter().any(|(en, _)| en == n)).collect()
    }

    pub fn checkpoint(&self) -> TypeCheckerCheckpoint {
        TypeCheckerCheckpoint {
            ctx: self.ctx.clone(),
            substitutions: self.substitutions.clone(),
            next_id: self.next_id,
            struct_defs: self.struct_defs.clone(),
            union_defs: self.union_defs.clone(),
            union_names: self.union_names.clone(),
            variant_owners: self.variant_owners.clone(),
            return_types: self.return_types.clone(),
            provides: self.provides.clone(),
            func_mut_params: self.func_mut_params.clone(),
            generalized_names: self.generalized_names.clone(),
            generic_instantiations: self.generic_instantiations.clone(),
        }
    }

    pub fn restore(&mut self, cp: TypeCheckerCheckpoint) {
        self.ctx = cp.ctx;
        self.substitutions = cp.substitutions;
        self.next_id = cp.next_id;
        self.struct_defs = cp.struct_defs;
        self.union_defs = cp.union_defs;
        self.union_names = cp.union_names;
        self.variant_owners = cp.variant_owners;
        self.return_types = cp.return_types;
        self.provides = cp.provides;
        self.generalized_names = cp.generalized_names;
        self.generic_instantiations = cp.generic_instantiations;
        self.func_mut_params = cp.func_mut_params;
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
        match name {
            "Int"   => Some(Type::Int),
            "Float" => Some(Type::Float),
            "Bool"  => Some(Type::Bool),
            "Str"   => Some(Type::Str),
            "None"  => Some(Type::None),
            _ if self.struct_defs.contains_key(name) => Some(Type::strukt(name)),
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
    fn lower_widen(&self, lowered: Spanned<TypedExpr>, target: &Type) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let from = lowered.item.ty.clone();
        if from == *target || from == Type::Never {
            return Ok(lowered);
        }
        // A non-lossy numeric promotion into a wider declared slot — see
        // `TypedExprKind::Coerce`. This has to happen here rather than at
        // each call site because `lower_widen` is already the single funnel
        // every such value passes through (struct/variant fields, list
        // elements, annotations, declared return types).
        if widens_to(&from, target) {
            let span = lowered.span;
            return Ok(Spanned::from(
                TypedExpr { id: 0, ty: target.clone(), kind: TypedExprKind::Coerce(Box::new(lowered)) },
                span,
            ));
        }
        let Type::Union(members) = target else { return Ok(lowered); };
        let Some(tag) = members.iter().position(|m| *m == from) else { return Ok(lowered); };
        let span = lowered.span;
        if matches!(from, Type::Union(_)) {
            return Err(Spanned::from(TypeError {
                msg: format!("widening a union-typed value ({}) into a different union ({}) is not yet supported", from, target)
            }, span));
        }
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
    ///    variables, so `let f: Function(Int, Int) = [x] -> x + 1` types
    ///    `x` without an annotation on the lambda itself.
    /// 2. **A list literal against a `List` type** — the expected *element*
    ///    type is pushed into each element (recursively, so
    ///    `List(List(Int))` works too). Bottom-up can't type either of the
    ///    two cases an annotation exists to resolve:
    ///      `let xs: List(Str) = []`          — bare `[]` synthesizes
    ///                                          `List(~t0)`, and `is_subtype`
    ///                                          can't see through the `List`
    ///                                          to bind it
    ///      `let xs: List(Int | Str) = [1, "a"]`
    ///                                        — the elements only agree once
    ///                                          each is widened to the
    ///                                          annotated union, which the
    ///                                          literal's own same-type
    ///                                          unification rejects first
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
                let lowered = self.check_and_lower(Spanned::from(other, span))?;
                let resolved = self.lookup(&lowered.item.ty);
                let accepted = self.is_subtype(&resolved, &expected)
                    || widens_to(&resolved, &expected)
                    || (matches!(resolved, Type::TypeVar { .. }) && self.unify(&resolved, &expected));
                if !accepted {
                    return Err(Spanned::from(TypeError {
                        msg: format!("Expected {} got {}", expected, resolved)
                    }, span));
                }
                self.lower_widen(lowered, &expected)
            },
        }
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

    /// `e?`'s match arms: an `Error`-providing member returns its (whole,
    /// reconstructed) value early; every other member passes its value
    /// through unchanged. The join across arms (`lower_match`'s own
    /// machinery) is exactly the doc's "set subtraction" type rule for
    /// free — a `return` arm is `Never`-typed and vanishes from the join,
    /// leaving only the non-`Error` members' union (or a single type, or
    /// `Never` if every member is an `Error`).
    fn build_try_arms(&mut self, subject_ty: &Type, subject_span: Span, span: Span) -> Result<Vec<MatchArm>, Spanned<TypeError>> {
        let entries = self.union_entries(subject_ty, subject_span, span)?;
        if !entries.iter().any(|e| e.is_error) {
            return Err(Spanned::from(TypeError {
                msg: "'?' requires a union with at least one Error-providing member".to_string()
            }, subject_span));
        }
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
        let mut arms = Vec::with_capacity(entries.len());
        for e in entries {
            let pattern = Pattern { path: None, variant: e.pattern_variant, binds: e.bind_names, resolved_member: e.member_idx };
            let body_expr = if e.is_error {
                Expression::return_value(Some(Spanned::from(e.whole_value, span)))
            } else {
                e.whole_value
            };
            arms.push(MatchArm { pattern, guard: None, body: Box::new(Spanned::from(body_expr, span)) });
        }
        Ok(arms)
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
        let entries = self.union_entries(subject_ty, subject_span, span)?;
        if !entries.iter().any(|e| e.is_error) {
            return Err(Spanned::from(TypeError {
                msg: "'!' requires a union with at least one Error-providing member".to_string()
            }, subject_span));
        }
        let mut arms = Vec::with_capacity(entries.len());
        for e in entries {
            let pattern = Pattern { path: None, variant: e.pattern_variant, binds: e.bind_names, resolved_member: e.member_idx };
            let body_expr = if e.is_error {
                let callee = Spanned::from(Expression::literal(Token::Identifier(UNWRAP_PANIC_NAME.to_string())), span);
                let msg = Spanned::from(Expression::literal(Token::String("unwrapped an error value with '!'".to_string())), span);
                Expression::call(callee, vec![msg])
            } else {
                e.whole_value
            };
            arms.push(MatchArm { pattern, guard: None, body: Box::new(Spanned::from(body_expr, span)) });
        }
        Ok(arms)
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
        let entries = self.union_entries(subject_ty, subject_span, span)?;
        if !entries.iter().any(|e| e.is_error) {
            return Err(Spanned::from(TypeError {
                msg: "'catch' requires a union with at least one Error-providing member".to_string()
            }, subject_span));
        }
        let (handler_bind, handler_body): (Option<String>, Spanned<Expression>) = match &handler.item {
            Expression::Function(f) if f.params.len() == 1 => (Some(f.params[0].name.clone()), (*f.body).clone()),
            _ => (None, handler.clone()),
        };
        let mut arms = Vec::with_capacity(entries.len());
        for e in entries {
            let pattern = Pattern { path: None, variant: e.pattern_variant, binds: e.bind_names, resolved_member: e.member_idx };
            let body = if e.is_error {
                match &handler_bind {
                    None => handler_body.clone(),
                    Some(bind) => {
                        let assign = Spanned::from(
                            Expression::assign(
                                Spanned::from(Expression::literal(Token::Identifier(bind.clone())), span),
                                None,
                                Spanned::from(e.whole_value, span),
                                Some(Mutability::Immutable),
                            ),
                            span,
                        );
                        Spanned::from(Expression::Block(vec![assign, handler_body.clone()]), span)
                    }
                }
            } else {
                Spanned::from(e.whole_value, span)
            };
            arms.push(MatchArm { pattern, guard: None, body: Box::new(body) });
        }
        Ok(arms)
    }

    fn lower_catch(&mut self, value: Spanned<Expression>, handler: Spanned<Expression>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let (subject, narrow_target, subject_span) = self.lower_match_subject(value)?;
        let arms = self.build_catch_arms(&subject.item.ty, subject_span, &handler, span)?;
        self.lower_match_lowered(subject, narrow_target, subject_span, arms, None, span)
    }

    /// Register every `data Name(field: Type, ...)` declaration found
    /// directly in `stmts` into `self.struct_defs`, in three phases so
    /// declarations can reference each other regardless of source order:
    /// (1) register every name, so forward references resolve; (2) resolve
    /// every field list to concrete `Type`s; (3) check the resulting
    /// field-type graph for direct/transitive self-reference, which would
    /// make an unboxed struct infinite size.
    fn hoist_data_decls(&mut self, stmts: &[Spanned<Expression>]) -> Result<(), Spanned<TypeError>> {
        for s in stmts {
            if let Expression::DataDecl(d) = &s.item {
                if self.struct_defs.contains_key(&d.name) || self.union_defs.contains_key(&d.name) {
                    return Err(Spanned::from(TypeError {
                        msg: format!("'{}' is already declared", d.name)
                    }, s.span));
                }
                if d.variants.is_empty() {
                    self.struct_defs.insert(d.name.clone(), Vec::new());
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
                    member_types.sort_by_cached_key(|t| t.to_string());
                    let ty = Type::Union(member_types.clone());
                    self.union_names.insert(member_types, d.name.clone());
                    self.union_defs.insert(d.name.clone(), UnionDef { common: Vec::new(), variants: Vec::new(), ty });
                }
            }
        }
        for s in stmts {
            if let Expression::DataDecl(d) = &s.item {
                let mut fields = Vec::with_capacity(d.fields.len());
                for (i, p) in d.fields.iter().enumerate() {
                    let ty = self.resolve_type_expr(&p.ty)?;
                    fields.push((field_name_or_positional(&p.name, i), ty));
                }
                if d.variants.is_empty() {
                    self.struct_defs.insert(d.name.clone(), fields);
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
                        let mut vfields = Vec::with_capacity(v.fields.len());
                        for (i, p) in v.fields.iter().enumerate() {
                            let ty = self.resolve_type_expr(&p.ty)?;
                            vfields.push((field_name_or_positional(&p.name, i), ty));
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
                    for (vn, vfields) in &variants {
                        let mut flat = fields.clone();
                        flat.extend(vfields.clone());
                        self.struct_defs.insert(format!("{}.{}", d.name, vn), flat);
                    }
                    let def = self.union_defs.get_mut(&d.name).expect("registered in the first pass, above");
                    def.common = fields;
                    def.variants = variants;
                }

                if !d.provides.is_empty() {
                    let mut traits = Vec::with_capacity(d.provides.len());
                    for name in &d.provides {
                        match name.as_str() {
                            "Error" => traits.push(Trait::Error),
                            other => return Err(Spanned::from(TypeError {
                                msg: format!("Unknown trait '{}' in provides clause", other)
                            }, s.span)),
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
        for s in stmts {
            if let Expression::DataDecl(d) = &s.item {
                self.check_struct_acyclic(&d.name, &mut Vec::new(), s.span)?;
            }
        }
        Ok(())
    }

    /// DFS over the struct field-type graph, following only direct
    /// `Type::Struct` fields (a `List(Struct(_))` field is fine — a list is
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
        if let Some(fields) = self.struct_defs.get(name).cloned() {
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
    fn lower_record_args(&mut self, kind_name: &str, field_defs: &[(String, Type)], args: Vec<Spanned<Expression>>, span: Span) -> Result<Vec<(String, Box<Spanned<TypedExpr>>)>, Spanned<TypeError>> {
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
            if !(widens_to(&resolved_value_ty, &resolved_field_ty) || self.unify(&value.item.ty, &field_ty)) {
                return Err(Spanned::from(TypeError {
                    msg: format!("Field '{}' of {} expects {}, got {}", fname, kind_name, resolved_field_ty, resolved_value_ty)
                }, value_span));
            }
            fields.push((fname, Box::new(value)));
        }
        let mut ordered = Vec::with_capacity(field_defs.len());
        for (fname, fty) in field_defs {
            let idx = fields.iter().position(|(n, _)| n == fname)
                .ok_or_else(|| Spanned::from(TypeError {
                    msg: format!("Missing field '{}' in construction of {}", fname, kind_name)
                }, span))?;
            let (fname, value) = fields.remove(idx);
            let value = Box::new(self.lower_widen(*value, fty)?);
            ordered.push((fname, value));
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
    fn lower_positional_record_args(&mut self, kind_name: &str, field_defs: &[(String, Type)], args: Vec<Spanned<Expression>>, span: Span) -> Result<Vec<(String, Box<Spanned<TypedExpr>>)>, Spanned<TypeError>> {
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
        if pattern.path.is_some() {
            return Err(Spanned::from(TypeError {
                msg: "a qualifier ('X.Y') only applies to a nominal union's variant name".to_string()
            }, span));
        }
        // A pattern the compiler synthesized itself (`union_entries`'s
        // anonymous-union branch) already knows its member's index — use it
        // directly rather than re-deriving it from `variant`, which for a
        // synthesized pattern is only `Type::to_string()`'s *display* form
        // and may not be a resolvable (or even parseable) type name at all,
        // e.g. `List(Int)`. See `Pattern::resolved_member`'s doc comment.
        let (idx, member_ty) = if let Some(idx) = pattern.resolved_member {
            let member_ty = members.get(idx).cloned().ok_or_else(|| Spanned::from(TypeError {
                msg: format!("internal error: resolved_member index {} out of range for {}", idx, Type::Union(members.to_vec()))
            }, span))?;
            (idx, member_ty)
        } else {
            let member_ty = self.resolve_type_name(&pattern.variant).ok_or_else(|| Spanned::from(TypeError {
                msg: format!("Unknown type '{}'", pattern.variant)
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
        } else if matches!(resolved, Type::TypeVar { .. }) && self.unify(&resolved, expected) {
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
                for (argt, arg_span) in args {
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
                result: Box::new(self.lookup(&result)),
            },
            Type::Named { name, args } => Type::Named {
                name: name.clone(),
                args: args.iter().map(|a| self.lookup(a)).collect(),
            },
            Type::Union(variants)  => Type::Union(variants.iter().map(|t| self.lookup(t)).collect()),
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
            (_, Type::Union(variants)) => variants.iter().any(|v| t1 == *v),
            // Structural unification for functions.
            (Type::Function { params: p1, result: r1 }, Type::Function { params: p2, result: r2 }) => {
                if p1.len() != p2.len() {
                    return false;
                }
                p1.iter().zip(p2.iter()).all(|(l, r)| self.unify(l, r)) &&
                self.unify(&r1, &r2)
            },
            // Structural unification for named type constructors — invariant
            // in every argument position (`TRAITS.md` Part 4, "Variance"):
            // `List(Int)` does not unify with `List(Int | Str)` in either
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
            Expression::Block(stmts) => {
                self.hoist_data_decls(&stmts)?;
                let mut lowered = Vec::with_capacity(stmts.len());
                let mut ty = Type::None;
                for s in stmts {
                    if matches!(s.item, Expression::DataDecl(_)) { continue; }
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
            | TypedExprKind::StrLit(_) | TypedExprKind::NoneLit | TypedExprKind::Var(_) => Ok(()),

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

            TypedExprKind::Assign { value, .. } => self.validate_codegen_constraints(value),

            TypedExprKind::Function { body, .. } => self.validate_codegen_constraints(body),

            TypedExprKind::Call { callable, args, .. } => {
                self.validate_codegen_constraints(callable)?;
                for a in args { self.validate_codegen_constraints(a)?; }
                // `print`'s argument is dispatched at runtime by
                // `codegen::print_union`, which recurses through struct
                // fields and nested unions to render whichever member
                // actually matched — reject anything that would recurse
                // into itself before codegen has to discover that the hard
                // way (see `print_union`'s own guard, which this mirrors).
                if let (TypedExprKind::Var(name), [arg, ..]) = (&callable.item.kind, args.as_slice()) {
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
            TypedExprKind::PlaceAssign { path, value, .. } => {
                for seg in path {
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

    /// `TRAITS.md` Stage 2's codegen gate: monomorphization is Stage 3, and
    /// a Cranelift function has exactly one signature, so a generalized
    /// binding (`generalized_names`) that's actually instantiated at 2+
    /// distinct concrete types across this program can type-check but
    /// can't yet be compiled. Reject that here — after
    /// `validate_codegen_constraints`, since both walk the same
    /// fully-resolved typed AST and this one is the rarer case — naming
    /// both types, rather than let it reach codegen and either panic or
    /// (worse) silently compile one instantiation's shape and miscompile
    /// the other's call sites.
    ///
    /// A generic instantiated at exactly one concrete type anywhere in the
    /// program is unaffected and compiles normally — that's what makes a
    /// same-shaped conversion of `push`/`len`/`get` viable once Stage 3
    /// lands monomorphization proper.
    ///
    /// Name-keyed rather than scope-keyed: two *unrelated* bindings that
    /// happen to share a name (one shadowing the other) and are each
    /// individually monomorphic would be lumped together and could trip
    /// this check unnecessarily. Accepted for this stage — the failure
    /// direction is "reject a program that would actually have been fine",
    /// never "silently miscompile", which is the property this gate exists
    /// to guarantee.
    ///
    /// On success, returns the single concrete type each generalized name
    /// was actually instantiated at (empty if none were used at all) —
    /// `resolve_single_instantiations` needs exactly this map to make that
    /// one instantiation compile.
    ///
    /// Also cross-checks against `generic_instantiations`, which spans the
    /// whole session rather than just this entry — see its doc comment for
    /// why a per-entry-only check misses a scheme minted at one REPL entry
    /// and instantiated at a genuinely different type by a later one.
    pub fn check_generic_monomorphism(&mut self, expr: &Spanned<TypedExpr>) -> Result<HashMap<String, Type>, Spanned<TypeError>> {
        if self.generalized_names.is_empty() { return Ok(HashMap::new()); }
        let mut seen: HashMap<String, (Type, Span)> = HashMap::new();
        self.collect_generic_var_types(expr, &mut seen)?;
        for (name, (ty, span)) in &seen {
            if let Some(prev) = self.generic_instantiations.get(name) {
                if prev != ty {
                    return Err(Spanned::from(TypeError {
                        msg: format!(
                            "'{}' is generic and is used at two different types ({} and {}) across this session — \
                             not yet supported (TRAITS.md Stage 3, monomorphization); \
                             give it a type annotation to pin it to one type, or write a second function",
                            name, prev, ty
                        )
                    }, *span));
                }
            }
        }
        for (name, (ty, _)) in &seen {
            self.generic_instantiations.entry(name.clone()).or_insert_with(|| ty.clone());
        }
        Ok(seen.into_iter().map(|(k, (ty, _))| (k, ty)).collect())
    }

    fn collect_generic_var_types(&self, expr: &Spanned<TypedExpr>, seen: &mut HashMap<String, (Type, Span)>) -> Result<(), Spanned<TypeError>> {
        if let TypedExprKind::Var(name) = &expr.item.kind {
            if self.generalized_names.contains(name) {
                // `expr.item.ty` is the type as it stood at the moment
                // this `Var` node was built — a freshly-instantiated,
                // still-unbound `TypeVar` at that point, since unification
                // against this call's actual arguments happens afterward.
                // `lookup` resolves it to what it was actually pinned to,
                // which is the comparison that matters here; two
                // instantiations that both happened to resolve to `Int`
                // must not be flagged just because their fresh `TypeVar`
                // names differ.
                let resolved = self.lookup(&expr.item.ty);
                match seen.get(name) {
                    Some((prev_ty, _)) if *prev_ty != resolved => {
                        return Err(Spanned::from(TypeError {
                            msg: format!(
                                "'{}' is generic and is used at two different types ({} and {}) in this program — \
                                 not yet supported (TRAITS.md Stage 3, monomorphization); \
                                 give it a type annotation to pin it to one type, or write a second function",
                                name, prev_ty, resolved
                            )
                        }, expr.span));
                    },
                    Some(_) => {},
                    None => { seen.insert(name.clone(), (resolved, expr.span)); },
                }
            }
        }
        match &expr.item.kind {
            TypedExprKind::IntLit(_) | TypedExprKind::FloatLit(_) | TypedExprKind::BoolLit(_)
            | TypedExprKind::StrLit(_) | TypedExprKind::NoneLit | TypedExprKind::Var(_) => Ok(()),

            TypedExprKind::Unary { expr: inner, .. } => self.collect_generic_var_types(inner, seen),

            TypedExprKind::Binary { left, right, .. } => {
                self.collect_generic_var_types(left, seen)?;
                self.collect_generic_var_types(right, seen)
            },

            TypedExprKind::Conditional { cond, true_branch, false_branch } => {
                self.collect_generic_var_types(cond, seen)?;
                self.collect_generic_var_types(true_branch, seen)?;
                if let Some(fb) = false_branch { self.collect_generic_var_types(fb, seen)?; }
                Ok(())
            },

            TypedExprKind::Assign { value, .. } => self.collect_generic_var_types(value, seen),

            TypedExprKind::Function { body, .. } => self.collect_generic_var_types(body, seen),

            TypedExprKind::Call { callable, args, .. } => {
                self.collect_generic_var_types(callable, seen)?;
                for a in args { self.collect_generic_var_types(a, seen)?; }
                Ok(())
            },

            TypedExprKind::Index { target, index } => {
                self.collect_generic_var_types(target, seen)?;
                self.collect_generic_var_types(index, seen)
            },

            TypedExprKind::Slice { target, start, end } => {
                self.collect_generic_var_types(target, seen)?;
                if let Some(s) = start { self.collect_generic_var_types(s, seen)?; }
                if let Some(e) = end { self.collect_generic_var_types(e, seen)?; }
                Ok(())
            },

            TypedExprKind::Range { start, end } => {
                self.collect_generic_var_types(start, seen)?;
                self.collect_generic_var_types(end, seen)
            },

            TypedExprKind::List(elems) => {
                for e in elems { self.collect_generic_var_types(e, seen)?; }
                Ok(())
            },

            TypedExprKind::Block(stmts) => {
                for s in stmts { self.collect_generic_var_types(s, seen)?; }
                Ok(())
            },

            TypedExprKind::ForLoop { iterable, cond, body, .. } => {
                self.collect_generic_var_types(iterable, seen)?;
                if let Some(c) = cond { self.collect_generic_var_types(c, seen)?; }
                self.collect_generic_var_types(body, seen)
            },

            TypedExprKind::Comprehension { iterable, cond, body, .. } => {
                self.collect_generic_var_types(iterable, seen)?;
                if let Some(c) = cond { self.collect_generic_var_types(c, seen)?; }
                self.collect_generic_var_types(body, seen)
            },

            TypedExprKind::StructInit { fields, .. } => {
                for (_, v) in fields { self.collect_generic_var_types(v, seen)?; }
                Ok(())
            },

            TypedExprKind::FieldAccess { target, .. } => self.collect_generic_var_types(target, seen),
            TypedExprKind::PlaceAssign { path, value, .. } => {
                for seg in path {
                    if let PlaceSeg::Index { index, .. } = seg {
                        self.collect_generic_var_types(index, seen)?;
                    }
                }
                self.collect_generic_var_types(value, seen)
            },

            TypedExprKind::VariantInit { fields, .. } => {
                for (_, v) in fields { self.collect_generic_var_types(v, seen)?; }
                Ok(())
            },

            TypedExprKind::IsVariant { target, .. } => self.collect_generic_var_types(target, seen),
            TypedExprKind::VariantField { target, .. } => self.collect_generic_var_types(target, seen),

            TypedExprKind::Return(value) => {
                if let Some(v) = value { self.collect_generic_var_types(v, seen)?; }
                Ok(())
            },

            TypedExprKind::Widen { value, .. } => self.collect_generic_var_types(value, seen),

            TypedExprKind::Narrow { value, .. } => self.collect_generic_var_types(value, seen),
            TypedExprKind::TypeTag { target, .. } => self.collect_generic_var_types(target, seen),
            TypedExprKind::Truthy(value) => self.collect_generic_var_types(value, seen),
            TypedExprKind::Coerce(value) => self.collect_generic_var_types(value, seen),
        }
    }

    /// Makes a single-instantiation generic actually compile. This is
    /// deliberately a narrow slice of monomorphization (`TRAITS.md` Stage
    /// 3's real job), viable only because `check_generic_monomorphism` has
    /// already proven there is exactly one concrete instantiation per name
    /// in `resolved` — no duplication, no mangled symbols, nothing Stage 3
    /// still needs to add.
    ///
    /// Why this is needed at all: a generalized declaration's own
    /// `params`/`return_type` are never resolved to a concrete type by
    /// ordinary checking. `instantiate` mints a *fresh* `TypeVar` per call
    /// site precisely so calls don't contaminate the declaration or each
    /// other — so the declaration's own binder (say `~t3`) never gets a
    /// `substitutions` entry, even after the one call site that used it
    /// (via its own fresh `~t4`) fully resolved. Codegen's `make_sig`
    /// builds a Cranelift signature straight from the declaration's stored
    /// `params`/`return_type` with no resolution step of its own — see
    /// `TypedExpr`'s doc comment, "all TypeVars reachable from `ty` are
    /// fully resolved", which generalization is the one thing that
    /// violates without this pass.
    ///
    /// For each generalized name, structurally zips its declared
    /// (abstract) type against the one concrete type it was used at to
    /// recover `binder name -> concrete type`, writes that into
    /// `substitutions`, then rewrites every node's stored type throughout
    /// the whole typed AST via `lookup` — including `Function` nodes' own
    /// `params`/`return_type`, which aren't reachable through any node's
    /// plain `.ty` field.
    pub fn resolve_single_instantiations(&mut self, typed: &mut Spanned<TypedExpr>, resolved: &HashMap<String, Type>) {
        for (name, concrete) in resolved {
            let Some(binders) = self.ctx.binders(name) else { continue };
            if binders.is_empty() { continue; }
            let binders = binders.to_vec();
            let Some(declared) = self.ctx.get(name).cloned() else { continue };
            let mut mapping = HashMap::new();
            Self::zip_binder_types(&declared, concrete, &binders, &mut mapping);
            for (var_name, ty) in mapping {
                self.substitutions.insert(var_name, ty);
            }
        }
        self.resolve_types_deep(typed);
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

    /// Rewrite `expr.item.ty`, recursively, to `lookup(expr.item.ty)` —
    /// and, for a `Function` node, its `params`/`return_type` too, since
    /// those live outside any node's own `.ty` field. Mutates in place
    /// rather than rebuilding, since every other field of every node is
    /// already correct; only the `Type`s themselves may be stale.
    fn resolve_types_deep(&self, expr: &mut Spanned<TypedExpr>) {
        expr.item.ty = self.lookup(&expr.item.ty);
        match &mut expr.item.kind {
            TypedExprKind::IntLit(_) | TypedExprKind::FloatLit(_) | TypedExprKind::BoolLit(_)
            | TypedExprKind::StrLit(_) | TypedExprKind::NoneLit | TypedExprKind::Var(_) => {},

            TypedExprKind::Unary { expr: inner, .. } => self.resolve_types_deep(inner),

            TypedExprKind::Binary { left, right, .. } => {
                self.resolve_types_deep(left);
                self.resolve_types_deep(right);
            },

            TypedExprKind::Conditional { cond, true_branch, false_branch } => {
                self.resolve_types_deep(cond);
                self.resolve_types_deep(true_branch);
                if let Some(fb) = false_branch { self.resolve_types_deep(fb); }
            },

            TypedExprKind::Assign { value, .. } => self.resolve_types_deep(value),

            TypedExprKind::Function { params, return_type, body } => {
                for (_, ty, _) in params.iter_mut() { *ty = self.lookup(ty); }
                *return_type = self.lookup(return_type);
                self.resolve_types_deep(body);
            },

            TypedExprKind::Call { callable, args, .. } => {
                self.resolve_types_deep(callable);
                for a in args.iter_mut() { self.resolve_types_deep(a); }
            },

            TypedExprKind::Index { target, index } => {
                self.resolve_types_deep(target);
                self.resolve_types_deep(index);
            },

            TypedExprKind::Slice { target, start, end } => {
                self.resolve_types_deep(target);
                if let Some(s) = start { self.resolve_types_deep(s); }
                if let Some(e) = end { self.resolve_types_deep(e); }
            },

            TypedExprKind::Range { start, end } => {
                self.resolve_types_deep(start);
                self.resolve_types_deep(end);
            },

            TypedExprKind::List(elems) => {
                for e in elems.iter_mut() { self.resolve_types_deep(e); }
            },

            TypedExprKind::Block(stmts) => {
                for s in stmts.iter_mut() { self.resolve_types_deep(s); }
            },

            TypedExprKind::ForLoop { iterable, cond, body, .. } => {
                self.resolve_types_deep(iterable);
                if let Some(c) = cond { self.resolve_types_deep(c); }
                self.resolve_types_deep(body);
            },

            TypedExprKind::Comprehension { iterable, cond, body, .. } => {
                self.resolve_types_deep(iterable);
                if let Some(c) = cond { self.resolve_types_deep(c); }
                self.resolve_types_deep(body);
            },

            TypedExprKind::StructInit { fields, .. } => {
                for (_, v) in fields.iter_mut() { self.resolve_types_deep(v); }
            },

            TypedExprKind::FieldAccess { target, .. } => self.resolve_types_deep(target),

            TypedExprKind::PlaceAssign { path, value, .. } => {
                for seg in path.iter_mut() {
                    if let PlaceSeg::Index { index, elem_ty } = seg {
                        self.resolve_types_deep(index);
                        *elem_ty = self.lookup(elem_ty);
                    }
                }
                self.resolve_types_deep(value);
            },

            TypedExprKind::VariantInit { fields, .. } => {
                for (_, v) in fields.iter_mut() { self.resolve_types_deep(v); }
            },

            TypedExprKind::IsVariant { target, .. } => self.resolve_types_deep(target),
            TypedExprKind::VariantField { target, .. } => self.resolve_types_deep(target),

            TypedExprKind::Return(value) => {
                if let Some(v) = value { self.resolve_types_deep(v); }
            },

            TypedExprKind::Widen { value, .. } => self.resolve_types_deep(value),
            TypedExprKind::Narrow { value, .. } => self.resolve_types_deep(value),
            TypedExprKind::TypeTag { target, .. } => self.resolve_types_deep(target),
            TypedExprKind::Truthy(value) => self.resolve_types_deep(value),
            TypedExprKind::Coerce(value) => self.resolve_types_deep(value),
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
        fn walk(ty: &Type, structs: &StructDefs, seen: &mut Vec<Vec<Type>>, span: Span) -> Result<(), Spanned<TypeError>> {
            if let Some(name) = ty.as_struct_name() {
                if let Some(fields) = structs.get(name) {
                    for (_, fty) in fields { walk(fty, structs, seen, span)?; }
                }
                return Ok(());
            }
            match ty {
                Type::Union(members) => {
                    if seen.iter().any(|s| s == members) {
                        return Err(Spanned::from(TypeError {
                            msg: format!(
                                "unsupported: printing a recursive union type ({}) isn't supported yet \
                                 — write a recursive function that formats it field-by-field instead.",
                                Type::Union(members.clone())
                            )
                        }, span));
                    }
                    seen.push(members.clone());
                    for m in members { walk(m, structs, seen, span)?; }
                    seen.pop();
                    Ok(())
                },
                _ => Ok(()),
            }
        }
        walk(ty, &self.struct_defs, &mut Vec::new(), span)
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
            Expression::Comprehension(inner) => self.lower_comprehension(inner, span),
            // Handled entirely by `hoist_data_decls` — never reaches codegen.
            Expression::DataDecl(_) => Ok(Spanned::from(TypedExpr { id: 0, ty: Type::None, kind: TypedExprKind::IntLit(0) }, span)),
            Expression::FieldAccess(fa)      => self.lower_field_access(fa, span),
            Expression::Match(m)             => self.lower_match(m.subject, m.arms, m.default, span),
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

    fn lower_literal(&mut self, lit: LiteralExpr, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let (kind, ty) = match lit.token {
            Token::Int(n)         => (TypedExprKind::IntLit(n),    Type::Int),
            Token::Float(f)       => (TypedExprKind::FloatLit(f),  Type::Float),
            Token::String(s)      => (TypedExprKind::StrLit(s),    Type::Str),
            Token::True           => (TypedExprKind::BoolLit(true),  Type::Bool),
            Token::False          => (TypedExprKind::BoolLit(false), Type::Bool),
            Token::None           => (TypedExprKind::NoneLit,      Type::None),
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
                    (TypedExprKind::Var(nm), ty)
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
            let resolved = self.lookup(&left.item.ty);
            if let Some(name) = resolved.as_struct_name() {
                self.desugar_struct_eq(b.op, left, right, name, span)
            } else {
                TypedExprKind::Binary { op: b.op, left: Box::new(left), right: Box::new(right) }
            }
        } else {
            TypedExprKind::Binary { op: b.op, left: Box::new(left), right: Box::new(right) }
        };
        Ok(Spanned::from(TypedExpr { id: 0, ty, kind }, span))
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
            return self.lower_match(ip.subject, arms, c.false_branch, span);
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
    fn flatten_place(target: Spanned<Expression>) -> (String, Vec<RawPlaceSeg>) {
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
                    let name = other.get_identifier()
                        .expect("Grammar::assign guarantees an identifier root")
                        .to_string();
                    segs.reverse();
                    return (name, segs);
                },
            }
        }
    }

    /// `root(.field | [index])* = value` — see `TypedExprKind::PlaceAssign`.
    /// `target` is a `FieldAccess` or `Index` (checked by the caller);
    /// `flatten_place` reduces it to `root` plus a root-to-leaf path,
    /// which this walks segment by segment, tracking the current type
    /// exactly as `lower_field_access`/`lower_index` do for a *read* of
    /// the same path, and enforcing that `root` is mutable before
    /// touching anything.
    fn lower_place_assign(
        &mut self,
        target: Spanned<Expression>,
        value: Spanned<Expression>,
        span: Span,
    ) -> Result<(TypedExprKind, Type), Spanned<TypeError>> {
        let (root, raw_path) = Self::flatten_place(target);
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
        // At most one `[index]` step — writing through a *second* one
        // (`xs[i][j] = v`, a list of lists) would need a heap store
        // nested inside another heap store, which codegen doesn't
        // implement yet. A v1 restriction, not a fundamental one — see
        // `TypedExprKind::PlaceAssign`'s doc comment.
        let mut seen_index = false;
        for seg in raw_path {
            match seg {
                RawPlaceSeg::Field(fname) => {
                    let resolved = self.lookup(&cur_ty);
                    let Some(struct_name) = resolved.as_struct_name() else {
                        return Err(Spanned::from(TypeError {
                            msg: format!("Can't assign field '{}' on {}, expected a struct", fname, resolved)
                        }, span));
                    };
                    let field_defs = self.struct_defs.get(struct_name).cloned().unwrap_or_default();
                    let field_ty = field_defs.iter().find(|(n, _)| n == &fname)
                        .map(|(_, t)| t.clone())
                        .ok_or_else(|| Spanned::from(TypeError {
                            msg: format!("Struct {} has no field '{}'", struct_name, fname)
                        }, span))?;
                    path.push(PlaceSeg::Field(fname));
                    cur_ty = field_ty;
                },
                RawPlaceSeg::Index(idx_expr) => {
                    if seen_index {
                        return Err(Spanned::from(TypeError {
                            msg: "assignment through more than one list index isn't supported yet".to_string()
                        }, span));
                    }
                    seen_index = true;
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
        let leaf_ty = self.lookup(&cur_ty);
        // The assigned leaf is exactly as much a union-typed slot as a
        // `StructInit` argument is, so it needs the same check-and-widen
        // — without it, `c.v = 9` would overwrite a boxed `Int | Bool`
        // field with the raw immediate `9`, and the next `TypeTag`/
        // `Narrow` would dereference it as a `FrogVariant*`.
        let value = self.lower_expected(value, &leaf_ty)?;
        // A place assignment is a statement: codegen rebinds the touched
        // leaf `Variable`(s) (a pure field path) or writes through the
        // indexed list (a path with one `Index`), and yields one dummy
        // value — `None` is both the documented result type and the only
        // single-slot type that can't disagree with that.
        Ok((TypedExprKind::PlaceAssign { root, path, value: Box::new(value) }, Type::None))
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
                            self.ctx.insert_mut(name.clone(), annotated_ty.clone(), mutable);
                            (TypedExprKind::Assign { name, value: Box::new(value) }, annotated_ty)
                        },
                        None => {
                            // Pre-bind fully-annotated functions so the body
                            // can reference the function by name (enabling
                            // recursion). `func_mut_params` is pre-bound
                            // alongside `func_ty` for the same reason: a
                            // recursive call with a `mut` argument
                            // (`lower_call`) needs it before this function's
                            // own body finishes checking, not after.
                            if let Expression::Function(func) = &a.value.item {
                                if func.return_type.is_some() && func.params.iter().all(|p| p.ty.is_some()) {
                                    let param_tys: Result<Vec<Type>, _> = func.params.iter()
                                        .map(|p| self.resolve_type_expr(p.ty.as_ref().expect("all params annotated — checked above")))
                                        .collect();
                                    let ret_ty = self.resolve_type_expr(func.return_type.as_ref().expect("return type present — checked above"))?;
                                    let func_ty = Type::Function { params: param_tys?, result: Box::new(ret_ty) };
                                    self.ctx.insert_mut(name.clone(), func_ty, mutable);
                                    self.func_mut_params.insert(name.clone(), func.params.iter().map(|p| p.mutable).collect());
                                }
                            }
                            let value = self.check_and_lower(*a.value)?;
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
                            if !mutable && is_fn_value {
                                let binders = self.generalize(&ty);
                                // Only a *genuinely* polymorphic binding
                                // (non-empty binders) needs the codegen
                                // gate to watch it — a monomorphic func
                                // declaration (e.g. every parameter and
                                // the return type annotated) is generalized
                                // trivially to zero binders and compiles
                                // exactly as before.
                                if !binders.is_empty() {
                                    self.generalized_names.insert(name.clone());
                                }
                                self.ctx.insert_generalized(name.clone(), ty.clone(), binders);
                            } else {
                                self.ctx.insert_mut(name.clone(), ty.clone(), mutable);
                            }
                            // Authoritative overwrite: covers the
                            // not-fully-annotated case the pre-bind above
                            // skips, and stays correct even when it ran.
                            if let TypedExprKind::Function { params, .. } = &value.item.kind {
                                self.func_mut_params.insert(name.clone(), params.iter().map(|(_, _, m)| *m).collect());
                            }
                            (TypedExprKind::Assign { name, value: Box::new(value) }, ty)
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
        // (`func f(): List(Str) = []`); without one it's
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

        // Struct construction: `Person(name="Alice", age=42)` looks
        // like an ordinary call syntactically (there's no dedicated
        // construction grammar — see `Grammar::data_decl`'s doc
        // comment), so it's disambiguated here, before the generic
        // function-call path, by checking whether the callee name is
        // a registered struct.
        let struct_name = c.callable.item.get_identifier()
            .filter(|n| self.struct_defs.contains_key(*n))
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
        // `push` is a builtin mutating operation on `List(T)`, polymorphic
        // over `T` — like `print`, it can't be a monomorphic
        // `default_context()` entry (there's no generics system; `List(T)`
        // is already a special-cased "builtin hack" per roadmap.md), so it
        // special-cases on the callee's literal name the same way `print`
        // does, rather than being a resolvable binding.
        let is_push = matches!(
            &c.callable.item,
            Expression::Literal(LiteralExpr { token: Token::Identifier(name) }) if name == "push"
        );
        // `len` is a builtin read-only query on `List(T)` or `Str`,
        // polymorphic over `T` the same way `push` is — same reason it
        // can't be a `default_context()` entry (a TypeVar there would get
        // permanently bound by the first call site, not re-instantiated
        // per call; there's no generalization/generics system yet).
        let is_len = matches!(
            &c.callable.item,
            Expression::Literal(LiteralExpr { token: Token::Identifier(name) }) if name == "len"
        );
        // `get` is a builtin bounds-checked read on `List(T)`, polymorphic
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

        let (kind, ty) = if let Some(name) = struct_name {
            let field_defs = self.struct_defs.get(&name).cloned().unwrap_or_default();
            let ordered = self.lower_record_args(&name, &field_defs, c.args, callee_span)?;
            let ty = Type::strukt(name.clone());
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
            (TypedExprKind::Call { callable: Box::new(callable), args: vec![arg], mut_args: vec![false] }, ty)
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
            // Same check the generic `mut`-argument path applies
            // (`self.ctx.is_mutable`) — `push`'s receiver is exactly a
            // `mut` argument, just to a builtin rather than a user `func`.
            let root = xs_inner.item.get_identifier().map(|s| s.to_string()).ok_or_else(|| Spanned::from(TypeError {
                msg: "'mut' argument must be a plain mutable binding, not an expression".to_string()
            }, xs_span))?;
            match self.ctx.is_mutable(&root) {
                None => return Err(Spanned::from(TypeError {
                    msg: format!("'{}' is not declared", root)
                }, xs_span)),
                Some(false) => return Err(Spanned::from(TypeError {
                    msg: format!("'{}' is not mutable — declare it with 'mut {} = ...' to pass it as a 'mut' argument", root, root)
                }, xs_span)),
                Some(true) => {},
            }

            let xs_lowered = self.check_and_lower(xs_inner)?;
            return self.finish_push(xs_lowered, xs_span, root, v_arg, callee_span, span);
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
        } else if matches!(&c.callable.item, Expression::FieldAccess(_)) {
            // `x.f(args)` where `f` isn't a struct/union field of
            // `typeof(x)` — resolved by `lower_ufcs_call` per
            // `TRAITS.md` Part 1 (steps 1 and 3; step 2, trait members,
            // doesn't exist yet).
            let Expression::FieldAccess(fa) = c.callable.item else { unreachable!("matched above") };
            return self.lower_ufcs_call(fa, c.args, callee_span, span);
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
    fn finish_push(&mut self, xs_lowered: Spanned<TypedExpr>, xs_span: Span, root: String, v_arg: Spanned<Expression>, callee_span: Span, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let xs_ty = self.lookup(&xs_lowered.item.ty);
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
        if !widens_to(&resolved_argt, &resolved_elem) && !self.unify(&v_lowered.item.ty, elem_ty) {
            return Err(Spanned::from(TypeError {
                msg: format!("Can't unify {:?} and {:?}", resolved_argt, resolved_elem)
            }, v_span));
        }
        let v_widened = self.lower_widen(v_lowered, elem_ty)?;

        // No `default_context()` entry backs "push" (see `is_push`'s own
        // comment), so the callable's `TypedExpr` is synthesized here
        // rather than resolved by `check_and_lower` — it's never
        // consulted for anything except `compile_call`'s dispatch on
        // the literal name `"push"`.
        let callable = Spanned::from(TypedExpr {
            id: 0,
            ty: Type::Function { params: vec![xs_ty.clone(), (*elem_ty).clone()], result: Box::new(Type::None) },
            kind: TypedExprKind::Var("push".to_string()),
        }, callee_span);

        Ok(Spanned::from(TypedExpr {
            id: 0,
            ty: Type::None,
            kind: TypedExprKind::Call {
                callable: Box::new(callable),
                args: vec![xs_lowered, v_widened],
                mut_args: vec![true, false],
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
            kind: TypedExprKind::Call { callable: Box::new(callable), args: vec![arg], mut_args: vec![false] },
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
        Ok(Spanned::from(TypedExpr { id: 0, ty, kind: TypedExprKind::Call { callable: Box::new(callable), args, mut_args } }, span))
    }

    /// One argument of a call: unwraps a `mut` marker, validates it
    /// against `declared` (the callee's declared mutability for this
    /// position, `None` meaning "no declaration reaches here"), then
    /// unifies/widens the argument's type against `param`. Shared by
    /// `finish_call` (every argument) and `lower_ufcs_call` (every
    /// argument after the exempt receiver).
    fn lower_call_arg(&mut self, i: usize, arg: Spanned<Expression>, param: &Type, declared: Option<bool>) -> Result<(Spanned<TypedExpr>, bool, Option<(String, Span)>), Spanned<TypeError>> {
        let arg_span = arg.span;
        let (is_mut, inner) = match arg.item {
            Expression::MutArg(inner) => (true, *inner),
            other => (false, Spanned::from(other, arg_span)),
        };
        let root = inner.item.get_identifier().map(|r| (r.to_string(), arg_span));
        if is_mut {
            let name = root.as_ref().map(|(n, _)| n.clone()).ok_or_else(|| Spanned::from(TypeError {
                msg: "'mut' argument must be a plain mutable binding, not an expression".to_string()
            }, arg_span))?;
            match self.ctx.is_mutable(&name) {
                None => return Err(Spanned::from(TypeError {
                    msg: format!("'{}' is not declared", name)
                }, arg_span)),
                Some(false) => return Err(Spanned::from(TypeError {
                    msg: format!("'{}' is not mutable — declare it with 'mut {} = ...' to pass it as a 'mut' argument", name, name)
                }, arg_span)),
                Some(true) => {},
            }
        }
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
        let lowered = self.check_and_lower(inner)?;
        let resolved_argt  = self.lookup(&lowered.item.ty);
        let resolved_param = self.lookup(param);
        // Allow implicit widening coercions at call sites (e.g. Int→Float).
        if !widens_to(&resolved_argt, &resolved_param) && !self.unify(&lowered.item.ty, param) {
            return Err(Spanned::from(TypeError {
                msg: format!("Can't unify {:?} and {:?}", resolved_argt, resolved_param)
            }, arg_span));
        }
        let widened = self.lower_widen(lowered, param)?;
        Ok((widened, is_mut, root))
    }

    /// A `mut`-marked argument's root may not also be any other
    /// argument's root in the same call (`swap(mut a, mut a)`,
    /// `merge(mut xs, xs)`).
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
    fn field_type_of(&self, resolved: &Type, field: &str) -> Option<Type> {
        if let Some(sname) = resolved.as_struct_name() {
            self.struct_defs.get(sname)
                .and_then(|fs| fs.iter().find(|(n, _)| n == field))
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
    fn lower_ufcs_call(&mut self, fa: FieldAccessExpr, rest_args: Vec<Spanned<Expression>>, callee_span: Span, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let target_span = fa.target.span;
        // Captured before the target is lowered — only a bare identifier
        // can be the root of a `mut` receiver, same restriction ordinary
        // `mut` arguments have (`lower_call_arg`).
        let root_name = fa.target.item.get_identifier().map(|s| s.to_string());
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

        // Step 3: no such field — a global `func` whose first parameter
        // accepts `typeof(target)`, rewritten to `f(target, ...rest_args)`.
        let field = fa.field;

        // `len` has no `ctx` entry either (`finish_len`'s comment) — same
        // special-casing as `push` below, minus any `mut` handling since
        // `len` doesn't mutate its receiver.
        if field == "len" {
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
        if field == "push" {
            if rest_args.len() != 1 {
                return Err(Spanned::from(TypeError {
                    msg: format!("Wrong number of arguments, expected 1, got {}", rest_args.len())
                }, callee_span));
            }
            let root = root_name.ok_or_else(|| Spanned::from(TypeError {
                msg: "the receiver of a mutating method must be a plain mutable binding, not an expression".to_string()
            }, target_span))?;
            match self.ctx.is_mutable(&root) {
                None => return Err(Spanned::from(TypeError {
                    msg: format!("'{}' is not declared", root)
                }, target_span)),
                Some(false) => return Err(Spanned::from(TypeError {
                    msg: format!("'{}' is not mutable — declare it with 'mut {} = ...' to call a mutating method on it", root, root)
                }, target_span)),
                Some(true) => {},
            }
            let v_arg = rest_args.into_iter().next().expect("arity checked just above");
            return self.finish_push(target, target_span, root, v_arg, callee_span, span);
        }

        // `get` has no `ctx` entry either, same reasoning as `len`/`push`
        // above — see `finish_get_ufcs`.
        if field == "get" {
            if rest_args.len() != 1 {
                return Err(Spanned::from(TypeError {
                    msg: format!("Wrong number of arguments, expected 1, got {}", rest_args.len())
                }, callee_span));
            }
            let i_arg = rest_args.into_iter().next().expect("arity checked just above");
            return self.finish_get_ufcs(target, i_arg, span);
        }

        let Some(func_ty) = self.ctx.get(&field).cloned() else {
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
        let binders = self.ctx.binders(&field).expect("just found by get").to_vec();
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

        let declared_mut = self.func_mut_params.get(&field).cloned();
        let mut_first = declared_mut.as_ref().and_then(|d| d.first()).copied().unwrap_or(false);

        // Receiver exemption (`TRAITS.md` Part 2, `MUTABILITY.md`'s
        // amendment): no `mut` marker is written or required at the dot
        // call site. The declaration-site guard the marker would
        // otherwise gate — "is this root actually a mutable binding" —
        // still applies directly to the receiver.
        if mut_first {
            let root = root_name.clone().ok_or_else(|| Spanned::from(TypeError {
                msg: "the receiver of a mutating method must be a plain mutable binding, not an expression".to_string()
            }, target_span))?;
            match self.ctx.is_mutable(&root) {
                None => return Err(Spanned::from(TypeError {
                    msg: format!("'{}' is not declared", root)
                }, target_span)),
                Some(false) => return Err(Spanned::from(TypeError {
                    msg: format!("'{}' is not mutable — declare it with 'mut {} = ...' to call a mutating method on it", root, root)
                }, target_span)),
                Some(true) => {},
            }
        }

        // No `check_and_lower` round-trip for the callee name: the
        // binding is already resolved (`func_ty` above), so the callable
        // is synthesized the same way `push`'s is.
        let callable = Spanned::from(TypedExpr { id: 0, ty: func_ty, kind: TypedExprKind::Var(field.clone()) }, callee_span);

        let mut args = Vec::with_capacity(rest_args.len() + 1);
        let mut mut_args = Vec::with_capacity(rest_args.len() + 1);
        let mut all_roots: Vec<Option<(String, Span)>> = Vec::with_capacity(rest_args.len() + 1);

        all_roots.push(root_name.map(|n| (n, target_span)));
        args.push(self.lower_widen(target, &params[0])?);
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
        Ok(Spanned::from(TypedExpr { id: 0, ty, kind: TypedExprKind::Call { callable: Box::new(callable), args, mut_args } }, span))
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
    fn lower_block(&mut self, stmts: Vec<Spanned<Expression>>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        self.hoist_data_decls(&stmts)?;
        let lowered = self.in_scope(|t| -> Result<Vec<Spanned<TypedExpr>>, Spanned<TypeError>> {
            let mut lowered = Vec::with_capacity(stmts.len());
            for s in stmts {
                if matches!(s.item, Expression::DataDecl(_)) { continue; }
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
        Ok(Spanned::from(TypedExpr { id: 0,
            ty: Type::list(Type::Int),
            kind: TypedExprKind::Range { start: Box::new(start), end: Box::new(end) },
        }, span))
    }

    fn lower_for_loop_expr(&mut self, fl: ForLoopExpr, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let (var, iterable, cond, body) = self.lower_for_loop(fl)?;
        // Each iteration discards the body's value exactly like a
        // non-tail Block statement does — same must-handle rule.
        self.check_must_handle(&body)?;
        Ok(Spanned::from(TypedExpr { id: 0, ty: Type::None, kind: TypedExprKind::ForLoop { var, iterable, cond, body } }, span))
    }

    fn lower_comprehension(&mut self, inner: Box<Spanned<Expression>>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let fl = match inner.item {
            Expression::ForLoop(fl) => fl,
            _ => unreachable!("Comprehension always wraps a ForLoop — see Grammar::tuple"),
        };
        // A comprehension collects the body's value into the
        // result list rather than discarding it, so must-handle
        // does not apply here — unlike a plain ForLoop.
        let (var, iterable, cond, body) = self.lower_for_loop(fl)?;
        let ty = Type::list(self.lookup(&body.item.ty));
        Ok(Spanned::from(TypedExpr { id: 0, ty, kind: TypedExprKind::Comprehension { var, iterable, cond, body } }, span))
    }

    fn lower_field_access(&mut self, fa: FieldAccessExpr, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let target_span = fa.target.span;
        let target = self.check_and_lower(*fa.target)?;
        let resolved = self.lookup(&target.item.ty);
        let (kind, ty) = if let Some(sname) = resolved.as_struct_name() {
            let field_ty = self.struct_defs.get(sname)
                .and_then(|fs| fs.iter().find(|(n, _)| *n == fa.field))
                .map(|(_, t)| t.clone())
                .ok_or_else(|| Spanned::from(TypeError {
                    msg: format!("Struct {} has no field '{}'", sname, fa.field)
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
        (String, Box<Spanned<TypedExpr>>, Option<Box<Spanned<TypedExpr>>>, Box<Spanned<TypedExpr>>),
        Spanned<TypeError>
    > {
        let iterable_span = fl.iterable.span;
        let iterable = self.check_and_lower(*fl.iterable)?;
        let iter_ty = iterable.item.ty.clone();
        let resolved_iter = self.lookup(&iter_ty);
        let elem_ty = match resolved_iter.as_list_elem() {
            Some(inner) => inner.clone(),
            None if matches!(&resolved_iter, Type::TypeVar { .. }) => {
                let elem = self.fresh_var();
                if !self.unify(&iter_ty, &Type::list(elem.clone())) {
                    return Err(Spanned::from(TypeError {
                        msg: format!("Can't iterate over {}", resolved_iter)
                    }, iterable_span));
                }
                elem
            },
            _ => return Err(Spanned::from(TypeError {
                msg: format!("Can't iterate over {}, expected a List", resolved_iter)
            }, iterable_span)),
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
        Ok((fl.var, Box::new(iterable), cond, Box::new(body)))
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
    fn lower_match(&mut self, subject: Box<Spanned<Expression>>, arms: Vec<MatchArm>, default: Option<Box<Spanned<Expression>>>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let (subject, narrow_target, subject_span) = self.lower_match_subject(*subject)?;
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
            .filter(|name| self.ctx.contains_key(*name))
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

            // A guard's binds must be extracted (the `prelude`) *before*
            // the guard itself runs — they can't be folded into a single
            // `base_cond and guard` boolean the way a bindless guard could,
            // since extraction is a statement, not an expression. So a
            // guarded arm nests one level deeper: the tag test's true
            // branch runs the prelude, then re-tests the guard, only
            // falling through to `tail` (cloned — it's the else of both
            // the tag test and, on guard failure, the inner check too) if
            // that also fails.
            // The arm's pattern binds are visible to its guard and body
            // only. The immediately-invoked closure that used to guarantee
            // the restore happened even on an early `?` is now `in_scope`'s
            // job.
            let (guard, body) = self.with_context_mut(
                bindings.iter().cloned(),
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

            let true_inner = match guard {
                None => body,
                Some(g) => {
                    let true_ty = body.item.ty.clone();
                    // `tail` is only ever `None` here once every remaining
                    // arm/default has been folded in already (the loop runs
                    // right-to-left) — and the exhaustiveness check
                    // already rejected a
                    // non-exhaustive match with no default before lowering
                    // ever starts, so a missing `tail` at this point means
                    // this guard's failure path is genuinely unreachable,
                    // not "produces None". `Never` (not `None`) is the
                    // correct placeholder: it vanishes from the union join
                    // below instead of forcing every guarded arm's type to
                    // widen to `T | None`.
                    let false_ty = tail.as_ref().map(|t| t.item.ty.clone()).unwrap_or(Type::Never);
                    let result_ty = self.join_types(&true_ty, &false_ty);
                    // Widen each branch to the joined type when it turned
                    // out to be a union — mirrors the matching fix in
                    // `check_and_lower`'s own `Conditional` arm; this
                    // hand-built `Conditional` needs the same treatment.
                    let body = self.lower_widen(body, &result_ty)?;
                    let false_branch = match tail.clone() {
                        Some(t) => Some(Box::new(self.lower_widen(t, &result_ty)?)),
                        None => None,
                    };
                    Spanned::from(
                        TypedExpr { id: 0, ty: result_ty, kind: TypedExprKind::Conditional {
                            cond: Box::new(g), true_branch: Box::new(body), false_branch,
                        } },
                        span,
                    )
                }
            };

            let true_branch = if prelude.is_empty() {
                true_inner
            } else {
                let inner_ty = true_inner.item.ty.clone();
                let mut stmts = prelude;
                stmts.push(true_inner);
                Spanned::from(TypedExpr { id: 0, ty: inner_ty, kind: TypedExprKind::Block(stmts) }, span)
            };

            let true_ty = true_branch.item.ty.clone();
            // See the matching comment above: a missing `tail` here means
            // this arm's tag-test-false path is unreachable (the match is
            // already known exhaustive), not that it produces `None`.
            let false_ty = tail.as_ref().map(|t| t.item.ty.clone()).unwrap_or(Type::Never);
            let result_ty = self.join_types(&true_ty, &false_ty);
            // See the matching comment in the guard case above.
            let true_branch = self.lower_widen(true_branch, &result_ty)?;
            let false_branch = match tail {
                Some(t) => Some(Box::new(self.lower_widen(t, &result_ty)?)),
                None => None,
            };

            tail = Some(Spanned::from(
                TypedExpr { id: 0, ty: result_ty, kind: TypedExprKind::Conditional {
                    cond: Box::new(base_cond), true_branch: Box::new(true_branch), false_branch,
                } },
                span,
            ));
        }

        let chain = tail.expect("an empty match with no arms and no default was already rejected as non-exhaustive");
        let chain_ty = chain.item.ty.clone();
        Ok(Spanned::from(TypedExpr { id: 0, ty: chain_ty, kind: TypedExprKind::Block(vec![subject_assign, chain]) }, span))
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

            // The arm's pattern binds are visible to its guard and body
            // only. The immediately-invoked closure that used to guarantee
            // the restore happened even on an early `?` is now `in_scope`'s
            // job.
            let (guard, body) = self.with_context_mut(
                bindings.iter().cloned(),
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

            let true_inner = match guard {
                None => body,
                Some(g) => {
                    let true_ty = body.item.ty.clone();
                    let false_ty = tail.as_ref().map(|t| t.item.ty.clone()).unwrap_or(Type::Never);
                    let result_ty = self.join_types(&true_ty, &false_ty);
                    // Widen each branch to the joined type when it turned
                    // out to be a union — mirrors the matching fix in
                    // `check_and_lower`'s own `Conditional` arm; this
                    // hand-built `Conditional` needs the same treatment.
                    let body = self.lower_widen(body, &result_ty)?;
                    let false_branch = match tail.clone() {
                        Some(t) => Some(Box::new(self.lower_widen(t, &result_ty)?)),
                        None => None,
                    };
                    Spanned::from(
                        TypedExpr { id: 0, ty: result_ty, kind: TypedExprKind::Conditional {
                            cond: Box::new(g), true_branch: Box::new(body), false_branch,
                        } },
                        span,
                    )
                }
            };

            let true_branch = if prelude.is_empty() {
                true_inner
            } else {
                let inner_ty = true_inner.item.ty.clone();
                let mut stmts = prelude;
                stmts.push(true_inner);
                Spanned::from(TypedExpr { id: 0, ty: inner_ty, kind: TypedExprKind::Block(stmts) }, span)
            };

            let true_ty = true_branch.item.ty.clone();
            let false_ty = tail.as_ref().map(|t| t.item.ty.clone()).unwrap_or(Type::Never);
            let result_ty = self.join_types(&true_ty, &false_ty);
            // See the matching comment in the guard case above.
            let true_branch = self.lower_widen(true_branch, &result_ty)?;
            let false_branch = match tail {
                Some(t) => Some(Box::new(self.lower_widen(t, &result_ty)?)),
                None => None,
            };

            tail = Some(Spanned::from(
                TypedExpr { id: 0, ty: result_ty, kind: TypedExprKind::Conditional {
                    cond: Box::new(base_cond), true_branch: Box::new(true_branch), false_branch,
                } },
                span,
            ));
        }

        let chain = tail.expect("an empty match with no arms and no default was already rejected as non-exhaustive");
        let chain_ty = chain.item.ty.clone();
        Ok(Spanned::from(TypedExpr { id: 0, ty: chain_ty, kind: TypedExprKind::Block(vec![subject_assign, chain]) }, span))
    }

    /// Desugar `left == right` / `left != right` (both already lowered,
    /// same `Type::Struct(name)`) into a per-field structural comparison.
    /// `left`/`right` are bound to fresh temporaries first so a
    /// side-effecting operand (e.g. a function call returning a struct)
    /// is only evaluated once, not once per field.
    fn desugar_struct_eq(&mut self, op: Token, left: Spanned<TypedExpr>, right: Spanned<TypedExpr>, name: &str, span: Span) -> TypedExprKind {
        let l_name = format!("__struct_eq_l{}", self.next_id); self.next_id += 1;
        let r_name = format!("__struct_eq_r{}", self.next_id); self.next_id += 1;
        let left_ty = left.item.ty.clone();
        let right_ty = right.item.ty.clone();

        let l_assign = Spanned::from(TypedExpr { id: 0, ty: left_ty.clone(), kind: TypedExprKind::Assign { name: l_name.clone(), value: Box::new(left) } }, span);
        let r_assign = Spanned::from(TypedExpr { id: 0, ty: right_ty.clone(), kind: TypedExprKind::Assign { name: r_name.clone(), value: Box::new(right) } }, span);
        let l_var = Spanned::from(TypedExpr { id: 0, ty: left_ty, kind: TypedExprKind::Var(l_name) }, span);
        let r_var = Spanned::from(TypedExpr { id: 0, ty: right_ty, kind: TypedExprKind::Var(r_name) }, span);

        let eq_expr = self.build_struct_eq(name, l_var, r_var, span);
        let result = if op == Token::NotEq {
            Spanned::from(TypedExpr { id: 0, ty: Type::Bool, kind: TypedExprKind::Unary { op: Token::Not, expr: Box::new(eq_expr) } }, span)
        } else {
            eq_expr
        };

        TypedExprKind::Block(vec![l_assign, r_assign, result])
    }

    /// Build `l.f1 == r.f1 and l.f2 == r.f2 and ...` for every field of
    /// struct `name`, recursing for nested-struct fields. `l`/`r` are
    /// assumed cheap to duplicate (a `Var` or `FieldAccess` chain — never
    /// something that could re-run a side effect).
    fn build_struct_eq(&self, name: &str, l: Spanned<TypedExpr>, r: Spanned<TypedExpr>, span: Span) -> Spanned<TypedExpr> {
        let fields = self.struct_defs.get(name).cloned().unwrap_or_default();
        let mut chain: Option<Spanned<TypedExpr>> = None;
        for (fname, fty) in &fields {
            let lf = Spanned::from(TypedExpr { id: 0, ty: fty.clone(), kind: TypedExprKind::FieldAccess { target: Box::new(l.clone()), field: fname.clone(), enum_name: None } }, span);
            let rf = Spanned::from(TypedExpr { id: 0, ty: fty.clone(), kind: TypedExprKind::FieldAccess { target: Box::new(r.clone()), field: fname.clone(), enum_name: None } }, span);
            let sub = match fty.as_struct_name() {
                Some(inner) => self.build_struct_eq(inner, lf, rf, span),
                None => Spanned::from(TypedExpr { id: 0, ty: Type::Bool, kind: TypedExprKind::Binary { op: Token::EqEq, left: Box::new(lf), right: Box::new(rf) } }, span),
            };
            chain = Some(match chain {
                None => sub,
                Some(prev) => Spanned::from(TypedExpr { id: 0, ty: Type::Bool, kind: TypedExprKind::Binary { op: Token::And, left: Box::new(prev), right: Box::new(sub) } }, span),
            });
        }
        chain.unwrap_or_else(|| Spanned::from(TypedExpr { id: 0, ty: Type::Bool, kind: TypedExprKind::BoolLit(true) }, span))
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
                // Declared arity of a named type constructor — `List` is 1,
                // every registered struct/union name is 0 for now (no
                // user-definable generics yet). This is the seam `TRAITS.md`
                // Stage 3 extends for `data Pair<A, B>`.
                if name == LIST_NAME {
                    if arg_types.len() == 1 {
                        return Ok(Type::list(arg_types.into_iter().next().expect("len checked")));
                    }
                    return Err(Spanned::from(
                        TypeError { msg: format!("List takes exactly 1 type argument, got {}", arg_types.len()) }, span));
                }
                if arg_types.is_empty() && (self.struct_defs.contains_key(name) || self.union_defs.contains_key(name)) {
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
        let nested = union(vec![Type::Int, union(vec![Type::Str, Type::Bool])]);
        assert_eq!(nested.normalize(), union(vec![Type::Bool, Type::Int, Type::Str]));
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
