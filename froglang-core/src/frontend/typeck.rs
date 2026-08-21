use std::{collections::HashMap, fmt::Display, vec};

use crate::frontend::{
    expression::{
        AnnotatedExpr, AssignExpr, BinaryExpr, CallExpr, ConditionalExpr, Expression,
        FieldAccessExpr, ForLoopExpr, FunctionExpr, IndexExpr, IsPatternExpr, LiteralExpr,
        MatchArm, Pattern, RangeExpr, SliceExpr, UnaryExpr,
    },
    tokens::{Span, Spanned, Token},
};
use crate::frontend::type_expr::TypeExpr;
use crate::frontend::typed_ast::{TypedExpr, TypedExprKind, TypedExprRef};
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
    List(Box<Type>),
    /// Sum / union type: a value whose type is one of the variants.
    /// Produced by if-expressions whose branches have incompatible types.
    Union(Vec<Type>),
    /// A `data Name(...)` struct type. Nominal: only the name is compared
    /// (derived `PartialEq`/`unify`'s `t1 == t2` fast path already give
    /// this for free — two structs unify iff their names match). Field
    /// names/types live in `TypeChecker.struct_defs`, not here, so cloning
    /// a `Type::Struct` stays cheap regardless of field count.
    Struct(String),
    /// The bottom type: no value of this type is ever produced. `return`'s
    /// own type (see `TypeChecker::return_types`) — it unifies with
    /// anything and vanishes from any union it appears in (`normalize`,
    /// `is_subtype`), so `if c then return 1 else 2` types as plain `Int`,
    /// not `Never | Int`.
    Never,
}

impl Type {
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
            Type::List(inner) => write!(f, "[{}]", inner),
            Type::Union(variants) => {
                let strs: Vec<String> = variants.iter().map(|t| format!("{}", t)).collect();
                write!(f, "{}", strs.join(" | "))
            }
            Type::Struct(name) => write!(f, "{}", name),
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
/// nominal marker `Type::Struct("Name.Member")`, sorted the same way
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
struct ErrorArmEntry {
    pattern_variant: String,
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
#[derive(Debug, Clone, Default)]
pub struct ScopeStack {
    bindings: HashMap<String, Type>,
    log:      Vec<(String, Option<Type>)>,
    depth:    usize,
}

impl ScopeStack {
    fn new(bindings: HashMap<String, Type>) -> Self {
        ScopeStack { bindings, log: Vec::new(), depth: 0 }
    }

    fn get(&self, name: &str) -> Option<&Type> {
        self.bindings.get(name)
    }

    fn contains_key(&self, name: &str) -> bool {
        self.bindings.contains_key(name)
    }

    /// Bind `name` in the innermost open scope, shadowing (and, once that
    /// scope closes, restoring) whatever it held before.
    fn insert(&mut self, name: String, ty: Type) {
        let previous = self.bindings.insert(name.clone(), ty);
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
                Some(ty) => { self.bindings.insert(name, ty); },
                None     => { self.bindings.remove(&name); },
            }
        }
        self.depth -= 1;
    }

    fn into_bindings(self) -> HashMap<String, Type> {
        self.bindings
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
}

impl TypeChecker {
    pub fn empty() -> Self {
        TypeChecker { ctx: ScopeStack::new(HashMap::new()), substitutions: HashMap::new(), next_id: 0, struct_defs: HashMap::new(), union_defs: HashMap::new(), union_names: HashMap::new(), variant_owners: HashMap::new(), return_types: Vec::new(), provides: HashMap::new() }
    }

    pub fn new() -> Self {
        TypeChecker { ctx: ScopeStack::new(TypeChecker::default_context()), substitutions: HashMap::new(), next_id: 0, struct_defs: HashMap::new(), union_defs: HashMap::new(), union_names: HashMap::new(), variant_owners: HashMap::new(), return_types: Vec::new(), provides: HashMap::new() }
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
            Type::Struct(_) if *tr == Trait::Eq => true,
            // `Error` is granted, not structural — see `provides`.
            Type::Struct(name) if *tr == Trait::Error => {
                self.provides.get(name).map(|ts| ts.contains(tr)).unwrap_or(false)
            },
            _ => match tr {
                Trait::Num    => matches!(ty, Type::Int | Type::Float),
                Trait::Eq     => matches!(ty, Type::Int | Type::Float | Type::Bool | Type::Str),
                Trait::Ord    => matches!(ty, Type::Int | Type::Float | Type::Str),
                Trait::Error  => false,
                Trait::Truthy => matches!(ty, Type::Int | Type::Float | Type::Bool | Type::Str | Type::List(_) | Type::None),
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
        Spanned::from(TypedExpr { ty: Type::Bool, kind: TypedExprKind::Truthy(Box::new(e)) }, span)
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
        if matches!(stmt.item.kind, TypedExprKind::Assign { .. } | TypedExprKind::FieldAssign { .. }) {
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
    }

    pub fn add_ctx(mut self, ctx: impl Iterator<Item=(String, Type)>) -> Self {
        for (k, v) in ctx { self.ctx.insert(k, v); }
        self
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
            _ if self.struct_defs.contains_key(name) => Some(Type::Struct(name.to_string())),
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
    /// supported: scalars (`Int`/`Float`/`Bool`) and `None` ride unboxed
    /// (see `is_two_slot_union`); everything else — plain structs, and now
    /// `Str`/`List` too — is boxed into a `FrogVariant` exactly like a
    /// nominal union's non-nullary member already is (`box_into_variant`),
    /// trading one extra allocation and indirection (the `FrogStr`/
    /// `FrogList` is already its own heap object; boxing wraps a pointer to
    /// it) for reusing that machinery unchanged rather than teaching the
    /// GC to discriminate a union member by the pointee's own `ObjKind`.
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
                TypedExpr { ty: target.clone(), kind: TypedExprKind::Coerce(Box::new(lowered)) },
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
            TypedExpr { ty: target.clone(), kind: TypedExprKind::Widen { value: Box::new(lowered), tag: tag as u32 } },
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
            let bindings: Vec<(String, Type)> = func.params.iter().zip(params.iter())
                .map(|(p, pty)| (p.name.clone(), pty.clone()))
                .collect();
            let return_type = (**result).clone();
            self.return_types.push(return_type.clone());
            // Pop before propagating: an error inside the body must not
            // leave a stale frame on `return_types`, or a later entry on
            // this same checker would accept a top-level `return`.
            let body = self.with_context(
                bindings.iter().cloned(),
                |t| t.lower_expected(*func.body, &return_type),
            );
            self.return_types.pop();
            let body = body?;
            return Ok(Spanned::from(
                TypedExpr {
                    ty: expected.clone(),
                    kind: TypedExprKind::Function { params: bindings, return_type, body: Box::new(body) },
                },
                span,
            ));
        }

        match (expr.item, &expected) {
            (Expression::Tuple(elems), Type::List(elem_ty)) => {
                let elem_ty = (**elem_ty).clone();
                let mut items = Vec::with_capacity(elems.len());
                for e in elems {
                    items.push(self.lower_expected(e, &elem_ty)?);
                }
                Ok(Spanned::from(
                    TypedExpr { ty: Type::List(Box::new(elem_ty)), kind: TypedExprKind::List(items) },
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
                let ty = Type::Struct(format!("{}.{}", enum_name, vn));
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
                        ),
                        span,
                    ));
                    bind_names.push(bind);
                }
                let whole_value = Expression::call(callee, args);
                out.push(ErrorArmEntry { pattern_variant: vn.clone(), ty, is_error, bind_names, whole_value });
            }
        } else if let Type::Union(members) = &resolved {
            for m in members {
                let is_error = self.type_implements(m, &Trait::Error);
                let bind = "__whole".to_string();
                let whole_value = Expression::literal(Token::Identifier(bind.clone()));
                out.push(ErrorArmEntry { pattern_variant: m.to_string(), ty: m.clone(), is_error, bind_names: vec![bind], whole_value });
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
            let pattern = Pattern { path: None, variant: e.pattern_variant, binds: e.bind_names };
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
            let pattern = Pattern { path: None, variant: e.pattern_variant, binds: e.bind_names };
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
            let pattern = Pattern { path: None, variant: e.pattern_variant, binds: e.bind_names };
            let body = if e.is_error {
                match &handler_bind {
                    None => handler_body.clone(),
                    Some(bind) => {
                        let assign = Spanned::from(
                            Expression::assign(
                                Spanned::from(Expression::literal(Token::Identifier(bind.clone())), span),
                                None,
                                Spanned::from(e.whole_value, span),
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
                        .map(|v| Type::Struct(format!("{}.{}", d.name, v.name)))
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
                    // own — so `Type::Struct("X.Variant")` is a legitimate
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
                if let Type::Struct(inner) = fty {
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
                let ty = Type::Struct(format!("{}.{}", enum_name, vn));
                if self.type_implements(&ty, &Trait::Error) {
                    out.push(MatchArm {
                        pattern: Pattern { path: None, variant: vn.clone(), binds: Vec::new() },
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
            for m in members {
                if self.type_implements(m, &Trait::Error) {
                    out.push(MatchArm {
                        pattern: Pattern { path: None, variant: m.to_string(), binds: Vec::new() },
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
        let member_ty = self.resolve_type_name(&pattern.variant).ok_or_else(|| Spanned::from(TypeError {
            msg: format!("Unknown type '{}'", pattern.variant)
        }, span))?;
        let idx = members.iter().position(|m| *m == member_ty).ok_or_else(|| Spanned::from(TypeError {
            msg: format!("{} is not a member of {}", member_ty, Type::Union(members.to_vec()))
        }, span))?;
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
            Type::List(inner)      => Type::List(Box::new(self.lookup(inner))),
            Type::Union(variants)  => Type::Union(variants.iter().map(|t| self.lookup(t)).collect()),
            _ => ty.clone(),
        }
    }

    fn get(&self, name: &str, span: Span) -> TypeResult {
        let ty = self.ctx.get(name).ok_or(
            Spanned::from(TypeError { msg: format!("Unbound variable {}", name) }, span)
        )?;
        Ok(ty.clone())
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
        let result = closure(self);
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
                if bounds.iter().all(|b| variants.iter().all(|v| self.type_implements(v, b))) {
                    self.substitutions.insert(name.clone(), t1.clone());
                    true
                } else {
                    false
                }
            },
            (Type::TypeVar { name, bounds }, Type::Union(variants)) => {
                if bounds.iter().all(|b| variants.iter().all(|v| self.type_implements(v, b))) {
                    self.substitutions.insert(name.clone(), t2.clone());
                    true
                } else {
                    false
                }
            },
            // Bounded TypeVar on left, concrete type on right.
            (Type::TypeVar { name, bounds }, _) => {
                if !bounds.iter().all(|b| self.type_implements(&t2, b)) {
                    return false;
                }
                self.substitutions.insert(name.clone(), t2.clone());
                true
            },
            // Concrete type on left, bounded TypeVar on right.
            (_, Type::TypeVar { name, bounds }) => {
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
                Ok(Spanned::from(TypedExpr { ty, kind: TypedExprKind::Block(lowered) }, span))
            },
            other => self.check_and_lower(Spanned::from(other, span)),
        }
    }

    /// Post-lowering validation: reject constructs the type checker accepts
    /// but codegen can't yet compile, as a spanned `TypeError` rather than a
    /// codegen-time `panic!` recovered by `catch_unwind` (see
    /// `codegen::mod`'s `assert_no_two_slot_union_leaf` and `print_union`'s
    /// recursion guard, whose conditions this mirrors exactly). Both are
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

            TypedExprKind::Call { callable, args } => {
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
                if let Type::List(inner) = &expr.item.ty {
                    let mut leafs = Vec::new();
                    self.flatten_leaf_types(inner, &mut leafs);
                    self.check_scalar_union_consistency(&leafs, expr.span, "a list element type")?;
                }
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
                self.validate_codegen_constraints(body)?;
                let mut leafs = Vec::new();
                self.flatten_leaf_types(&body.item.ty, &mut leafs);
                self.check_scalar_union_consistency(&leafs, body.span, "a list element type")
            },

            TypedExprKind::StructInit { fields, .. } => {
                for (_, v) in fields { self.validate_codegen_constraints(v)?; }
                Ok(())
            },

            TypedExprKind::FieldAccess { target, .. } => self.validate_codegen_constraints(target),
            TypedExprKind::FieldAssign { value, .. } => self.validate_codegen_constraints(value),

            TypedExprKind::VariantInit { fields, .. } => {
                for (_, v) in fields { self.validate_codegen_constraints(v)?; }
                // Checked across *all* fields together, not one at a time:
                // `codegen::box_into_variant` boxes every field's flattened
                // leaves into one `FrogVariant`, sharing one `boxed_tags`
                // set for the whole thing (see `gc_masks`) — so it's the
                // combination that has to stay consistent, not each field
                // in isolation.
                let mut leafs = Vec::new();
                for (_, v) in fields { self.flatten_leaf_types(&v.item.ty, &mut leafs); }
                self.check_scalar_union_consistency(&leafs, expr.span, "a boxed union/struct field")
            },

            TypedExprKind::IsVariant { target, .. } => self.validate_codegen_constraints(target),
            TypedExprKind::VariantField { target, .. } => self.validate_codegen_constraints(target),

            TypedExprKind::Return(value) => {
                if let Some(v) = value { self.validate_codegen_constraints(v)?; }
                Ok(())
            },

            TypedExprKind::Widen { value, .. } => {
                self.validate_codegen_constraints(value)?;
                let mut leafs = Vec::new();
                self.flatten_leaf_types(&value.item.ty, &mut leafs);
                self.check_scalar_union_consistency(&leafs, value.span, "a boxed union/struct field")
            },

            TypedExprKind::Narrow { value, .. } => self.validate_codegen_constraints(value),
            TypedExprKind::TypeTag { target, .. } => self.validate_codegen_constraints(target),
            TypedExprKind::Truthy(value) => self.validate_codegen_constraints(value),
            TypedExprKind::Coerce(value) => self.validate_codegen_constraints(value),
        }
    }

    /// Flatten `ty` into its leaf types, recursing into struct fields the
    /// same way `codegen::struct_fields` does — leaf *types* only, no field
    /// names, no special-casing of a two-slot union leaf (unlike
    /// `struct_fields` itself), since `check_scalar_union_consistency` just
    /// needs to see every union type reachable, whatever slot count it
    /// ends up using.
    fn flatten_leaf_types(&self, ty: &Type, out: &mut Vec<Type>) {
        match ty {
            Type::Struct(name) => {
                if let Some(fields) = self.struct_defs.get(name) {
                    for (_, fty) in fields { self.flatten_leaf_types(fty, out); }
                }
            },
            _ => out.push(ty.clone()),
        }
    }

    /// Reject more than one *distinct* scalar-carrying union shape (a
    /// `Type::Union` with an `Int`/`Float`/`Bool` member — see
    /// `codegen::is_two_slot_union` — which rides as an unboxed `{tag,
    /// payload}` pair rather than allocating) among `leafs`. One such shape
    /// embedded in a GC-scanned aggregate (a list's elements, or a boxed
    /// union/struct's fields) is fine: codegen tracks a single
    /// `(cond_mask, boxed_tags)` pair per aggregate to make the payload
    /// slot's pointer-ness conditional on its sibling tag slot (see
    /// `codegen::gc_masks`, and `FrogList`/`FrogVariant`'s own doc comments
    /// in `runtime/gc.rs`) — but two *different* scalar-carrying unions
    /// can't share that one slot's bookkeeping.
    fn check_scalar_union_consistency(&self, leafs: &[Type], span: Span, context: &str) -> Result<(), Spanned<TypeError>> {
        let mut seen: Option<&Vec<Type>> = None;
        for t in leafs {
            let Type::Union(members) = t else { continue };
            if !members.iter().any(|m| matches!(m, Type::Int | Type::Float | Type::Bool)) { continue; }
            match seen {
                None => seen = Some(members),
                Some(prev) if prev == members => {},
                Some(_) => return Err(Spanned::from(TypeError {
                    msg: format!(
                        "unsupported: two different scalar-carrying union types can't yet appear together in {} \
                         — only one such union shape (e.g. `Int | Str`) is supported per list/struct/variant.",
                        context
                    )
                }, span)),
            }
        }
        Ok(())
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
            match ty {
                Type::Struct(name) => {
                    if let Some(fields) = structs.get(name) {
                        for (_, fty) in fields { walk(fty, structs, seen, span)?; }
                    }
                    Ok(())
                },
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
            Expression::DataDecl(_) => Ok(Spanned::from(TypedExpr { ty: Type::None, kind: TypedExprKind::IntLit(0) }, span)),
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
                    let ty = self.lookup(&bound);
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
        Ok(Spanned::from(TypedExpr { ty, kind }, span))
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
        Ok(Spanned::from(TypedExpr { ty, kind: TypedExprKind::Unary { op: u.op, expr: Box::new(inner) } }, span))
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
            if let Type::Struct(name) = self.lookup(&left.item.ty) {
                self.desugar_struct_eq(b.op, left, right, &name, span)
            } else {
                TypedExprKind::Binary { op: b.op, left: Box::new(left), right: Box::new(right) }
            }
        } else {
            TypedExprKind::Binary { op: b.op, left: Box::new(left), right: Box::new(right) }
        };
        Ok(Spanned::from(TypedExpr { ty, kind }, span))
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
        Ok(Spanned::from(TypedExpr {
            ty: result_ty,
            kind: TypedExprKind::Conditional {
                cond:         Box::new(cond),
                true_branch:  Box::new(true_branch),
                false_branch,
            },
        }, span))
    }

    fn lower_assign(&mut self, a: AssignExpr, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        // `alice.age = 43` — rebind-sugar for struct "mutation".
        // `Grammar::assign` only lets this parse when the
        // FieldAccess's own target is a bare identifier, so
        // `get_identifier` below is guaranteed to succeed.
        let (kind, ty) = if let Expression::FieldAccess(fa) = a.target.item {
            let base = fa.target.item.get_identifier()
                .expect("parser only allows a bare identifier as a field-assign base")
                .to_string();
            let base_ty = self.get(&base, span)?;
            let resolved_base = self.lookup(&base_ty);
            let Type::Struct(struct_name) = &resolved_base else {
                return Err(Spanned::from(TypeError {
                    msg: format!("Can't assign field '{}' on {}, expected a struct", fa.field, resolved_base)
                }, span));
            };
            let field_defs = self.struct_defs.get(struct_name).cloned().unwrap_or_default();
            let field_ty = field_defs.iter().find(|(n, _)| n == &fa.field)
                .map(|(_, t)| t.clone())
                .ok_or_else(|| Spanned::from(TypeError {
                    msg: format!("Struct {} has no field '{}'", struct_name, fa.field)
                }, span))?;
            // The assigned field is exactly as much a union-typed
            // slot as a `StructInit` argument is, so it needs the
            // same check-and-widen — without the widen, `c.v = 9`
            // would overwrite a boxed `Int | Bool` field with the
            // raw immediate `9` and the next `TypeTag`/`Narrow`
            // would dereference it as a `FrogVariant*`. The declared
            // field type comes from `struct_defs` (via the base's
            // own type), never from `a.typ` — a field assignment
            // carries no annotation of its own.
            let value = self.lower_expected(*a.value, &field_ty)?;
            // A field assignment is a statement: codegen rebinds the
            // touched leaf `Variable`s and yields one dummy value,
            // so `None` is both the documented result type and the
            // only single-slot type that can't disagree with that.
            (TypedExprKind::FieldAssign { base, field: fa.field, value: Box::new(value) }, Type::None)
        } else {
            let name = a.target.item.get_identifier()
                .expect("assignment target must be identifier").to_string();
            match &a.typ {
                Some(ann) => {
                    let annotated_ty = self.resolve_type_expr(ann)?;
                    // Validate *and* lower the value against the
                    // annotation, then bind the name at the
                    // annotation type (not the value's own). For
                    // function types that distinction matters: the
                    // body's type is not the variable's type.
                    let value = self.lower_expected(*a.value, &annotated_ty)?;
                    self.ctx.insert(name.clone(), annotated_ty.clone());
                    (TypedExprKind::Assign { name, value: Box::new(value) }, annotated_ty)
                },
                None => {
                    // Pre-bind fully-annotated functions so the body
                    // can reference the function by name (enabling
                    // recursion).
                    if let Expression::Function(func) = &a.value.item {
                        if func.return_type.is_some() && func.params.iter().all(|p| p.ty.is_some()) {
                            let param_tys: Result<Vec<Type>, _> = func.params.iter()
                                .map(|p| self.resolve_type_expr(p.ty.as_ref().expect("all params annotated — checked above")))
                                .collect();
                            let ret_ty = self.resolve_type_expr(func.return_type.as_ref().expect("return type present — checked above"))?;
                            let func_ty = Type::Function { params: param_tys?, result: Box::new(ret_ty) };
                            self.ctx.insert(name.clone(), func_ty);
                        }
                    }
                    let value = self.check_and_lower(*a.value)?;
                    let ty = self.lookup(&value.item.ty);
                    self.ctx.insert(name.clone(), ty.clone());
                    (TypedExprKind::Assign { name, value: Box::new(value) }, ty)
                },
            }
        };
        Ok(Spanned::from(TypedExpr { ty, kind }, span))
    }

    fn lower_function(&mut self, f: FunctionExpr, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let mut param_bindings: Vec<(String, Type)> = Vec::with_capacity(f.params.len());
        for p in &f.params {
            let param_ty = match &p.ty {
                Some(annotation) => self.resolve_type_expr(annotation)?,
                None => self.fresh_var(),
            };
            param_bindings.push((p.name.clone(), param_ty));
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
        let body = self.with_context(param_bindings.iter().cloned(), |t| {
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

        let params: Vec<(String, Type)> = param_bindings.into_iter()
            .map(|(n, t)| { let t = self.lookup(&t); (n, t) })
            .collect();
        let ty = Type::Function {
            params: params.iter().map(|(_, t)| t.clone()).collect(),
            result: Box::new(return_type.clone()),
        };
        Ok(Spanned::from(TypedExpr { ty, kind: TypedExprKind::Function { params, return_type, body: Box::new(body) } }, span))
    }

    fn lower_call(&mut self, c: CallExpr, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let callee_span = c.callable.span;

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

        let (kind, ty) = if let Some(name) = struct_name {
            let field_defs = self.struct_defs.get(&name).cloned().unwrap_or_default();
            let ordered = self.lower_record_args(&name, &field_defs, c.args, callee_span)?;
            let ty = Type::Struct(name.clone());
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
            (TypedExprKind::Call { callable: Box::new(callable), args: vec![arg] }, ty)
        } else {
            let callable = self.check_and_lower(*c.callable)?;
            let func_type = self.lookup(&callable.item.ty);

            // If the callee is an unbound TypeVar (e.g. a lambda
            // parameter used as a function), bind it to a fresh
            // function type whose arity matches this call site.
            let func_type = if let Type::TypeVar { name, .. } = &func_type {
                let param_types: Vec<Type> = c.args.iter().map(|_| self.fresh_var()).collect();
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
            if c.args.len() != params.len() {
                return Err(Spanned::from(TypeError {
                    msg: format!("Wrong number of arguments, expected {}, got {}", params.len(), c.args.len())
                }, callee_span));
            }
            let mut args = Vec::with_capacity(c.args.len());
            for (arg, param) in c.args.into_iter().zip(params.iter()) {
                let arg_span = arg.span;
                let lowered = self.check_and_lower(arg)?;
                let resolved_argt  = self.lookup(&lowered.item.ty);
                let resolved_param = self.lookup(param);
                // Allow implicit widening coercions at call sites (e.g. Int→Float).
                if !widens_to(&resolved_argt, &resolved_param) && !self.unify(&lowered.item.ty, param) {
                    return Err(Spanned::from(TypeError {
                        msg: format!("Can't unify {:?} and {:?}", resolved_argt, resolved_param)
                    }, arg_span));
                }
                args.push(self.lower_widen(lowered, param)?);
            }
            let ty = self.lookup(&result);
            (TypedExprKind::Call { callable: Box::new(callable), args }, ty)
        };
        Ok(Spanned::from(TypedExpr { ty, kind }, span))
    }

    fn lower_tuple(&mut self, elems: Vec<Spanned<Expression>>, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let (kind, ty) = if elems.is_empty() {
            (TypedExprKind::List(Vec::new()), Type::List(Box::new(self.fresh_var())))
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
            (TypedExprKind::List(items), Type::List(Box::new(elem_ty)))
        };
        Ok(Spanned::from(TypedExpr { ty, kind }, span))
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
        Ok(Spanned::from(TypedExpr { ty, kind: TypedExprKind::Block(lowered) }, span))
    }

    // The annotation is absorbed into this node's own type: lower
    // the inner expression *against* it (see `lower_expected`) and
    // reuse the result directly.
    fn lower_annotated(&mut self, a: AnnotatedExpr, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let annotated_ty = self.resolve_type_expr(&a.ty)?;
        let lowered = self.lower_expected(*a.expr, &annotated_ty)?;
        Ok(Spanned::from(TypedExpr { ty: lowered.item.ty, kind: lowered.item.kind }, span))
    }

    fn lower_index(&mut self, idx: IndexExpr, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let target_span = idx.target.span;
        let target = self.check_and_lower(*idx.target)?;
        let target_ty = target.item.ty.clone();
        let resolved_target = self.lookup(&target_ty);
        let elem_ty = match &resolved_target {
            Type::List(inner) => (**inner).clone(),
            Type::TypeVar { .. } => {
                let elem = self.fresh_var();
                if !self.unify(&target_ty, &Type::List(Box::new(elem.clone()))) {
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
        Ok(Spanned::from(TypedExpr { ty, kind: TypedExprKind::Index { target: Box::new(target), index: Box::new(index) } }, span))
    }

    fn lower_slice(&mut self, s: SliceExpr, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let target_span = s.target.span;
        let target = self.check_and_lower(*s.target)?;
        let target_ty = target.item.ty.clone();
        let resolved_target = self.lookup(&target_ty);
        let list_ty = match &resolved_target {
            Type::List(_) => resolved_target.clone(),
            Type::TypeVar { .. } => {
                let elem = self.fresh_var();
                let list_ty = Type::List(Box::new(elem));
                if !self.unify(&target_ty, &list_ty) {
                    return Err(Spanned::from(TypeError {
                        msg: format!("Can't slice {}", resolved_target)
                    }, target_span));
                }
                list_ty
            },
            _ => return Err(Spanned::from(TypeError {
                msg: format!("Can't slice {}, expected a List", resolved_target)
            }, target_span)),
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
        Ok(Spanned::from(TypedExpr { ty, kind: TypedExprKind::Slice { target: Box::new(target), start, end } }, span))
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
        Ok(Spanned::from(TypedExpr {
            ty: Type::List(Box::new(Type::Int)),
            kind: TypedExprKind::Range { start: Box::new(start), end: Box::new(end) },
        }, span))
    }

    fn lower_for_loop_expr(&mut self, fl: ForLoopExpr, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let (var, iterable, cond, body) = self.lower_for_loop(fl)?;
        // Each iteration discards the body's value exactly like a
        // non-tail Block statement does — same must-handle rule.
        self.check_must_handle(&body)?;
        Ok(Spanned::from(TypedExpr { ty: Type::None, kind: TypedExprKind::ForLoop { var, iterable, cond, body } }, span))
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
        let ty = Type::List(Box::new(self.lookup(&body.item.ty)));
        Ok(Spanned::from(TypedExpr { ty, kind: TypedExprKind::Comprehension { var, iterable, cond, body } }, span))
    }

    fn lower_field_access(&mut self, fa: FieldAccessExpr, span: Span) -> Result<Spanned<TypedExpr>, Spanned<TypeError>> {
        let target_span = fa.target.span;
        let target = self.check_and_lower(*fa.target)?;
        let resolved = self.lookup(&target.item.ty);
        let (kind, ty) = if let Type::Struct(sname) = &resolved {
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
        Ok(Spanned::from(TypedExpr { ty, kind }, span))
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
        Ok(Spanned::from(TypedExpr { ty: Type::Bool, kind }, span))
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
        Ok(Spanned::from(TypedExpr { ty: Type::Never, kind: TypedExprKind::Return(value) }, span))
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
        let elem_ty = match &resolved_iter {
            Type::List(inner) => (**inner).clone(),
            Type::TypeVar { .. } => {
                let elem = self.fresh_var();
                if !self.unify(&iter_ty, &Type::List(Box::new(elem.clone()))) {
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
            TypedExpr { ty: subject_ty.clone(), kind: TypedExprKind::Assign { name: subject_name.clone(), value: Box::new(subject) } },
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
                TypedExpr { ty: subject_ty.clone(), kind: TypedExprKind::Var(subject_name.clone()) },
                span,
            );
            let base_cond = Spanned::from(
                TypedExpr { ty: Type::Bool, kind: TypedExprKind::IsVariant {
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
                        TypedExpr { ty: fty.clone(), kind: TypedExprKind::VariantField {
                            target: Box::new(subject_var.clone()), enum_name: enum_name.clone(), variant: arm.pattern.variant.clone(), field: fname.clone(),
                        } },
                        span,
                    );
                    prelude.push(Spanned::from(
                        TypedExpr { ty: fty.clone(), kind: TypedExprKind::Assign { name: bind.clone(), value: Box::new(value) } },
                        span,
                    ));
                    bindings.push((bind.clone(), fty.clone()));
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
                        TypedExpr { ty: fty.clone(), kind: TypedExprKind::FieldAccess {
                            target: Box::new(subject_var.clone()), field: fname.clone(), enum_name: Some(enum_name.clone()),
                        } },
                        span,
                    )))
                });
                let own_fields = variant_fields.iter().map(|(fname, fty)| {
                    (fname.clone(), Box::new(Spanned::from(
                        TypedExpr { ty: fty.clone(), kind: TypedExprKind::VariantField {
                            target: Box::new(subject_var.clone()), enum_name: enum_name.clone(), variant: arm.pattern.variant.clone(), field: fname.clone(),
                        } },
                        span,
                    )))
                });
                let narrowed_fields: Vec<(String, TypedExprRef)> = common_fields.chain(own_fields).collect();
                let narrowed_ty = Type::Struct(qualified.clone());
                let value = Spanned::from(
                    TypedExpr { ty: narrowed_ty.clone(), kind: TypedExprKind::StructInit { name: qualified, fields: narrowed_fields } },
                    span,
                );
                prelude.push(Spanned::from(
                    TypedExpr { ty: narrowed_ty.clone(), kind: TypedExprKind::Assign { name: name.clone(), value: Box::new(value) } },
                    span,
                ));
                bindings.push((name.clone(), narrowed_ty));
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
            let (guard, body) = self.with_context(
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
                        TypedExpr { ty: result_ty, kind: TypedExprKind::Conditional {
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
                Spanned::from(TypedExpr { ty: inner_ty, kind: TypedExprKind::Block(stmts) }, span)
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
                TypedExpr { ty: result_ty, kind: TypedExprKind::Conditional {
                    cond: Box::new(base_cond), true_branch: Box::new(true_branch), false_branch,
                } },
                span,
            ));
        }

        let chain = tail.expect("an empty match with no arms and no default was already rejected as non-exhaustive");
        let chain_ty = chain.item.ty.clone();
        Ok(Spanned::from(TypedExpr { ty: chain_ty, kind: TypedExprKind::Block(vec![subject_assign, chain]) }, span))
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
            TypedExpr { ty: subject_ty.clone(), kind: TypedExprKind::Assign { name: subject_name.clone(), value: Box::new(subject) } },
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
                TypedExpr { ty: subject_ty.clone(), kind: TypedExprKind::Var(subject_name.clone()) },
                span,
            );
            let base_cond = Spanned::from(
                TypedExpr { ty: Type::Bool, kind: TypedExprKind::TypeTag {
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
            let bind_name = arm.pattern.binds.first().cloned().or_else(|| narrow_target.clone());
            if let Some(bind) = bind_name {
                if bind != "_" {
                    let value = Spanned::from(
                        TypedExpr { ty: member_ty.clone(), kind: TypedExprKind::Narrow {
                            value: Box::new(subject_var.clone()), tag: idx as u32,
                        } },
                        span,
                    );
                    prelude.push(Spanned::from(
                        TypedExpr { ty: member_ty.clone(), kind: TypedExprKind::Assign { name: bind.clone(), value: Box::new(value) } },
                        span,
                    ));
                    bindings.push((bind.clone(), member_ty.clone()));
                }
            }

            // The arm's pattern binds are visible to its guard and body
            // only. The immediately-invoked closure that used to guarantee
            // the restore happened even on an early `?` is now `in_scope`'s
            // job.
            let (guard, body) = self.with_context(
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
                        TypedExpr { ty: result_ty, kind: TypedExprKind::Conditional {
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
                Spanned::from(TypedExpr { ty: inner_ty, kind: TypedExprKind::Block(stmts) }, span)
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
                TypedExpr { ty: result_ty, kind: TypedExprKind::Conditional {
                    cond: Box::new(base_cond), true_branch: Box::new(true_branch), false_branch,
                } },
                span,
            ));
        }

        let chain = tail.expect("an empty match with no arms and no default was already rejected as non-exhaustive");
        let chain_ty = chain.item.ty.clone();
        Ok(Spanned::from(TypedExpr { ty: chain_ty, kind: TypedExprKind::Block(vec![subject_assign, chain]) }, span))
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

        let l_assign = Spanned::from(TypedExpr { ty: left_ty.clone(), kind: TypedExprKind::Assign { name: l_name.clone(), value: Box::new(left) } }, span);
        let r_assign = Spanned::from(TypedExpr { ty: right_ty.clone(), kind: TypedExprKind::Assign { name: r_name.clone(), value: Box::new(right) } }, span);
        let l_var = Spanned::from(TypedExpr { ty: left_ty, kind: TypedExprKind::Var(l_name) }, span);
        let r_var = Spanned::from(TypedExpr { ty: right_ty, kind: TypedExprKind::Var(r_name) }, span);

        let eq_expr = self.build_struct_eq(name, l_var, r_var, span);
        let result = if op == Token::NotEq {
            Spanned::from(TypedExpr { ty: Type::Bool, kind: TypedExprKind::Unary { op: Token::Not, expr: Box::new(eq_expr) } }, span)
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
            let lf = Spanned::from(TypedExpr { ty: fty.clone(), kind: TypedExprKind::FieldAccess { target: Box::new(l.clone()), field: fname.clone(), enum_name: None } }, span);
            let rf = Spanned::from(TypedExpr { ty: fty.clone(), kind: TypedExprKind::FieldAccess { target: Box::new(r.clone()), field: fname.clone(), enum_name: None } }, span);
            let sub = match fty {
                Type::Struct(inner) => self.build_struct_eq(inner, lf, rf, span),
                _ => Spanned::from(TypedExpr { ty: Type::Bool, kind: TypedExprKind::Binary { op: Token::EqEq, left: Box::new(lf), right: Box::new(rf) } }, span),
            };
            chain = Some(match chain {
                None => sub,
                Some(prev) => Spanned::from(TypedExpr { ty: Type::Bool, kind: TypedExprKind::Binary { op: Token::And, left: Box::new(prev), right: Box::new(sub) } }, span),
            });
        }
        chain.unwrap_or_else(|| Spanned::from(TypedExpr { ty: Type::Bool, kind: TypedExprKind::BoolLit(true) }, span))
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
                match (name.as_str(), arg_types.len()) {
                    ("List", 1) => Ok(Type::List(Box::new(arg_types.into_iter().next().expect("len checked")))),
                    ("List", n) => Err(Spanned::from(
                        TypeError { msg: format!("List takes exactly 1 type argument, got {}", n) }, span)),
                    _ => Err(Spanned::from(
                        TypeError { msg: format!("Type '{}' does not take type arguments", name) }, span)),
                }
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
            Type::List(Box::new(Type::Int)).normalize(),
            Type::List(Box::new(Type::Int)),
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
