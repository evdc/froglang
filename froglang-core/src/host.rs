//! The host-function embedding API — see `plans/EMBEDDING.md` for the design.
//!
//! Registers a Rust function as callable from frog source
//! (`FrogState::builder().func(...)`), over an ABI shared with a future
//! `cranelift-object` AOT backend: every host function is exposed to
//! Cranelift as `extern "C" fn(ctx: *mut FrogCtx, args: *const i64, out: *mut
//! i64)`, with argument/result marshalling handled by `FromFrog`/`ToFrog`.
//! `#[frog_fn]` (`froglang_macros`) generates that shim and a `HostFn`
//! descriptor from an ordinary-looking Rust function; this module is what it
//! expands into.

use std::collections::HashMap;

use crate::frontend::typeck::Type;
use crate::runtime::host::FrogCtx;

/// A registered host function, as `FrogStateBuilder::func` wants it.
/// Built by `#[frog_fn]`; can be built by hand for the rare escape-hatch
/// case of a signature `#[frog_fn]` can't express.
pub struct HostFn {
    /// The name frog source calls this by.
    pub name: &'static str,
    /// The JIT symbol name. Distinct from `name` only so a host function
    /// can share a Rust `extern "C"` symbol namespace with the runtime's
    /// own `frog_*` functions without colliding — ordinarily equal to
    /// `name` prefixed, see `#[frog_fn]`.
    pub symbol: &'static str,
    /// `extern "C" fn(*mut FrogCtx, *const i64, *mut i64)`, as a raw
    /// address — the same shape `builder.symbol` wants for every other
    /// runtime import (`Codegen::new`).
    pub shim: *const u8,
    pub params: Vec<Type>,
    pub ret: Type,
    /// Each parameter's flattened wire columns, as its `FromFrog::leaves()`
    /// reports them, and the result's as `ToFrog::leaves()` reports them.
    /// Carried on the descriptor purely so `FrogStateBuilder::build()` can
    /// audit them against the frog side's real flattening
    /// (`codegen::struct_fields`) once the prelude has run — the same
    /// check `.data::<T>()` gets, extended to signatures, since a
    /// signature-only type (a `Result<T, E>`, a `Vec<T>`, a
    /// `#[frog(declared)]` struct never passed to `.data()`) is otherwise
    /// never audited and desyncs at the first call instead. A hand-written
    /// `HostFn` fills these from the same `leaves()` calls `#[frog_fn]`
    /// emits.
    pub param_leaves: Vec<Vec<Type>>,
    pub ret_leaves: Vec<Type>,
}

// `HostFn` is built once at startup and handed to `Codegen::new_with_hosts`
// on the thread that constructs a `FrogState`; it never needs to cross a
// thread boundary itself; safe to make the concurrency check moot.
unsafe impl Send for HostFn {}

/// A Rust type that owns (or maps onto) a frog `data`/`error` declaration —
/// implemented by `#[derive(FrogData)]`/`#[derive(FrogUnion)]`
/// (`froglang_macros`) alongside `ToFrog`/`FromFrog`. `FrogStateBuilder::
/// data::<T>()` is the usual way to register one.
pub trait FrogDecl {
    /// The frog source text declaring this type (`"data Point(x: Int, y:
    /// Int)"`), or `None` for a `#[frog(declared)]` type that maps onto a
    /// declaration frog source already provides — in which case the
    /// declaration must be registered some other way (an explicit
    /// `.prelude(...)` call, or an earlier `.data(...)` call) before
    /// `FrogStateBuilder::build()`'s layout audit runs.
    fn frog_decl() -> Option<String>;
}

/// Converts a frog argument's flattened `i64` slots into a Rust value.
///
/// `leaves()` is this type's flattened wire columns, in slot order —
/// `struct_fields(Self::frog_type(), structs)`'s Rust-side twin, but
/// computed *compositionally* from the impl tree rather than by consulting
/// a runtime `StructDefs`. Every impl below composes its fields'/members'
/// `leaves()`; nothing calls into `codegen`'s type tables to do it. That is
/// what keeps this trait AOT-clean (no serialized layout table needed in a
/// linked binary) and is why a violation is only ever caught by
/// `FrogStateBuilder::build()`'s one-time audit against the real
/// `struct_fields` output, not by anything at call time — see
/// `plans/EMBEDDING.md`.
pub trait FromFrog: Sized {
    fn frog_type() -> Type;
    /// Default: a plain scalar/`Str`/`List`/`Dict`/pointer type is exactly
    /// one wire column, its own `frog_type()`. Only a struct or union
    /// derive needs to override this.
    fn leaves() -> Vec<Type> { vec![Self::frog_type()] }
    fn slots() -> usize { Self::leaves().len() }
    /// `slots` is exactly `Self::slots()` long.
    fn from_frog(ctx: &FrogCtx, slots: &[i64]) -> Self;
}

/// The mirror of `FromFrog`, converting a Rust value into a host function's
/// flattened result slots. See `FromFrog`'s doc comment for `leaves()`.
pub trait ToFrog {
    fn frog_type() -> Type;
    fn leaves() -> Vec<Type> { vec![Self::frog_type()] }
    fn slots() -> usize { Self::leaves().len() }
    /// `out` is exactly `Self::slots()` long; every slot must be written.
    fn to_frog(self, ctx: &mut FrogCtx, out: &mut [i64]);
}

impl FromFrog for i64 {
    fn frog_type() -> Type { Type::Int }
    fn from_frog(_ctx: &FrogCtx, slots: &[i64]) -> Self { slots[0] }
}
impl ToFrog for i64 {
    fn frog_type() -> Type { Type::Int }
    fn to_frog(self, _ctx: &mut FrogCtx, out: &mut [i64]) { out[0] = self; }
}

impl FromFrog for f64 {
    fn frog_type() -> Type { Type::Float }
    fn from_frog(_ctx: &FrogCtx, slots: &[i64]) -> Self { f64::from_bits(slots[0] as u64) }
}
impl ToFrog for f64 {
    fn frog_type() -> Type { Type::Float }
    fn to_frog(self, _ctx: &mut FrogCtx, out: &mut [i64]) { out[0] = self.to_bits() as i64; }
}

impl FromFrog for bool {
    fn frog_type() -> Type { Type::Bool }
    fn from_frog(_ctx: &FrogCtx, slots: &[i64]) -> Self { slots[0] != 0 }
}
impl ToFrog for bool {
    fn frog_type() -> Type { Type::Bool }
    fn to_frog(self, _ctx: &mut FrogCtx, out: &mut [i64]) { out[0] = self as i64; }
}

// One slot, not zero: `struct_fields(Type::None, ..)` (codegen/mod.rs) falls
// through to its default one-leaf arm, so both `compile_host_call`'s
// argument buffer and every stride computed from `slots()` below treat a
// `None`-typed value as occupying one slot. The default `leaves()` (`vec![
// Self::frog_type()]`) already gives this, so no override is needed here —
// unlike a union *member* of type `None`, which is a different case (a
// payload-less member contributes *zero* leaves, since it is its own tag;
// see `Option<T>`'s impl below, which never calls through this one).
impl FromFrog for () {
    fn frog_type() -> Type { Type::None }
    fn from_frog(_ctx: &FrogCtx, _slots: &[i64]) -> Self {}
}
impl ToFrog for () {
    fn frog_type() -> Type { Type::None }
    fn to_frog(self, _ctx: &mut FrogCtx, out: &mut [i64]) { out[0] = 0; }
}

impl FromFrog for String {
    fn frog_type() -> Type { Type::Str }
    fn from_frog(ctx: &FrogCtx, slots: &[i64]) -> Self {
        unsafe { ctx.str_of(slots[0]) }.to_owned()
    }
}
impl ToFrog for String {
    fn frog_type() -> Type { Type::Str }
    fn to_frog(self, ctx: &mut FrogCtx, out: &mut [i64]) { out[0] = ctx.alloc_str(&self); }
}
impl ToFrog for &str {
    fn frog_type() -> Type { Type::Str }
    fn to_frog(self, ctx: &mut FrogCtx, out: &mut [i64]) { out[0] = ctx.alloc_str(self); }
}

impl<T: FromFrog> FromFrog for Vec<T> {
    fn frog_type() -> Type { Type::list(T::frog_type()) }
    fn from_frog(ctx: &FrogCtx, slots: &[i64]) -> Self {
        let w = slots[0];
        let stride = T::slots().max(1);
        let len = ctx.list_len(w, stride);
        (0..len)
            .map(|i| {
                let elem_slots: Vec<i64> = (0..stride).map(|s| ctx.list_elem(w, i, stride, s)).collect();
                T::from_frog(ctx, &elem_slots)
            })
            .collect()
    }
}
impl<T: ToFrog> ToFrog for Vec<T> {
    fn frog_type() -> Type { Type::list(T::frog_type()) }
    fn to_frog(self, ctx: &mut FrogCtx, out: &mut [i64]) {
        let elem_leaves = T::leaves();
        let stride = elem_leaves.len().max(1);
        let mut flat: Vec<i64> = Vec::with_capacity(self.len() * stride);
        for elem in self {
            let mut buf = vec![0i64; T::slots()];
            elem.to_frog(ctx, &mut buf);
            flat.extend_from_slice(&buf);
        }
        // `List`'s pointer columns are per-element-slot (`FrogList`'s
        // `ptr_mask` doc comment in `gc.rs`) — one bit per leaf of `T`, in
        // order, which is exactly what `codegen::gc_mask` computes from a
        // leaf-type list. Correct for a multi-leaf (struct-shaped) `T` now,
        // unlike the single-bit shortcut this used to take.
        let ptr_mask = crate::codegen::gc_mask(&elem_leaves) as u64;
        out[0] = ctx.alloc_list(&flat, stride, ptr_mask);
    }
}

/// `HashMap<K, V>` ↔ `Dict<K, V>` — `Vec<T>`'s twin. **`K` must be one of
/// `i64`/`f64`/`bool`/`String` (`Trait::Hash`'s four members) and single-
/// slot** — unlike `Vec<T>`'s `T`, this can't be checked at compile time
/// (there is no marker trait yet distinguishing a `Hash`-able frog key
/// type from any other `ToFrog`/`FromFrog` impl), so passing e.g. a
/// `HashMap<MyStruct, V>` compiles but produces a `Dict` codegen and the
/// runtime can't actually index — same class of unchecked precondition
/// `Result<T, E>`'s impl documents below.
impl<K: FromFrog + std::hash::Hash + Eq, V: FromFrog> FromFrog for HashMap<K, V> {
    fn frog_type() -> Type { Type::dict(K::frog_type(), V::frog_type()) }
    fn from_frog(ctx: &FrogCtx, slots: &[i64]) -> Self {
        let w = slots[0];
        let kstride = K::slots().max(1);
        let vstride = V::slots();
        let len = ctx.dict_len(w);
        (0..len)
            .map(|i| {
                let k_slots: Vec<i64> = (0..kstride).map(|s| ctx.dict_entry_slot(w, i, kstride, vstride, s)).collect();
                let v_slots: Vec<i64> = (0..vstride).map(|s| ctx.dict_entry_slot(w, i, kstride, vstride, kstride + s)).collect();
                (K::from_frog(ctx, &k_slots), V::from_frog(ctx, &v_slots))
            })
            .collect()
    }
}
impl<K: ToFrog + std::hash::Hash + Eq, V: ToFrog> ToFrog for HashMap<K, V> {
    fn frog_type() -> Type { Type::dict(K::frog_type(), V::frog_type()) }
    fn to_frog(self, ctx: &mut FrogCtx, out: &mut [i64]) {
        let k_leaves = K::leaves();
        let v_leaves = V::leaves();
        let kstride = k_leaves.len().max(1);
        let vstride = v_leaves.len();
        let mut flat: Vec<i64> = Vec::with_capacity(self.len() * (kstride + vstride));
        for (k, v) in self {
            let mut kbuf = vec![0i64; K::slots()];
            k.to_frog(ctx, &mut kbuf);
            flat.extend_from_slice(&kbuf);
            let mut vbuf = vec![0i64; V::slots()];
            v.to_frog(ctx, &mut vbuf);
            flat.extend_from_slice(&vbuf);
        }
        // Key columns then value columns, each masked from its own leaf
        // list — the same per-leaf `gc_mask` `Vec<T>` uses above, just
        // placed at an offset for the value half.
        let ptr_mask: u64 = (crate::codegen::gc_mask(&k_leaves) as u64)
            | ((crate::codegen::gc_mask(&v_leaves) as u64) << kstride);
        let key_kind = crate::codegen::key_kind_of(&K::frog_type()) as u32;
        out[0] = ctx.alloc_dict(&flat, kstride, vstride, ptr_mask, key_kind);
    }
}

/// Build a two-member union's normalized member list plus each member's
/// flattened leaf list, keyed by member type — shared by `Result<T, E>`
/// and `Option<T>`'s marshalling below, both directions. Panics with a
/// clear message in the two cases where the pair doesn't survive
/// `Type::normalize` as an addressable two-member union:
///
/// 1. the members **collapse** to the same type (`X | X = X` — a
///    `Result<String, String>` or `Option<Option<T>>`-flavored ambiguity
///    has no way to be told apart once packed; pick a distinct type for
///    one side), or
/// 2. a member **expands**, because it is itself a union and normalize
///    flattens it (see the `members.len() != 2` check below).
fn two_member_union(
    ctx_label: &str,
    a_ty: Type, a_leaves: Vec<Type>,
    b_ty: Type, b_leaves: Vec<Type>,
) -> (Vec<Type>, Vec<Vec<Type>>) {
    let union_ty = Type::Union(vec![a_ty.clone(), b_ty.clone()]).normalize();
    let Type::Union(members) = union_ty else {
        panic!(
            "{}: the two members ({} and {}) map to the same frog type — `T | T` \
             normalizes to `T`, so they can't be told apart once packed. Use a distinct \
             type for one side.",
            ctx_label, a_ty, b_ty,
        );
    };
    // `Type::normalize` *flattens* nested unions, so a member type that is
    // itself a union (`Result<i64, MyErr>` where `MyErr` is a two-variant
    // union) comes back as three-plus members here — and this impl has no
    // way to split that side's packed columns back into per-variant leaf
    // lists. Silently handing the extra members the wrong leaf list would
    // desync the whole layout (wrong column offsets on both sides, plus a
    // `leaves()` longer than the slot buffer the caller allocated), so fail
    // loudly instead.
    if members.len() != 2 {
        panic!(
            "{}: its two members ({} and {}) normalize to {} frog members ({:?}) — a member \
             that is itself a union is flattened by `Type::normalize`, and this impl can't \
             split the flattened columns back into per-variant leaves. Wrap that side in a \
             single-field `data` type, or marshal the whole union with `#[derive(FrogUnion)]`.",
            ctx_label, a_ty, b_ty, members.len(), members,
        );
    }
    let member_leaves: Vec<Vec<Type>> = members.iter()
        .map(|m| {
            if *m == a_ty { a_leaves.clone() }
            else if *m == b_ty { b_leaves.clone() }
            else {
                panic!(
                    "{}: normalized member {} is neither {} nor {} — `Type::normalize` \
                     rewrote a member into a shape this impl can't map back to a leaf list.",
                    ctx_label, m, a_ty, b_ty,
                )
            }
        })
        .collect();
    (members, member_leaves)
}

/// `Option<T>` ↔ `T?` (`T | None`). The `None` variant is *not* marshalled
/// through `<() as ToFrog>`/`<() as FromFrog>` — a union member of type
/// `None` contributes **zero** leaves (a payload-less member is its own
/// tag; `codegen::member_leaf_types`'s rule), which is a different, more
/// specific case than `()` as a *standalone* type occupying one slot. That
/// is the whole reason this impl exists separately from a hypothetical
/// `Result<T, ()>`, which stays unsupported (see `Result`'s doc comment).
impl<T: ToFrog> ToFrog for Option<T> {
    fn frog_type() -> Type {
        Type::Union(vec![T::frog_type(), Type::None]).normalize()
    }
    fn leaves() -> Vec<Type> {
        let (members, member_leaves) = two_member_union(
            "Option<T>", T::frog_type(), T::leaves(), Type::None, Vec::new(),
        );
        let layout = crate::codegen::union_layout_of_leaves(&member_leaves);
        crate::codegen::union_columns(&layout, &Type::Union(members).normalize())
    }
    fn to_frog(self, ctx: &mut FrogCtx, out: &mut [i64]) {
        let (members, member_leaves) = two_member_union(
            "Option<T>", T::frog_type(), T::leaves(), Type::None, Vec::new(),
        );
        let (idx, leaf_vals) = match self {
            Some(v) => {
                let idx = members.iter().position(|m| *m == T::frog_type())
                    .expect("Option<T>::to_frog: T's type not found in its own normalized union");
                let mut buf = vec![0i64; T::slots()];
                v.to_frog(ctx, &mut buf);
                (idx, buf)
            }
            None => {
                let idx = members.iter().position(|m| *m == Type::None)
                    .expect("Option<T>::to_frog: None not found in its own normalized union");
                (idx, Vec::new())
            }
        };
        let tag = crate::codegen::member_tag(idx);
        let packed = crate::codegen::pack_union_member_runtime_of_leaves(&member_leaves, idx, tag, &leaf_vals);
        out[..packed.len()].copy_from_slice(&packed);
    }
}
impl<T: FromFrog> FromFrog for Option<T> {
    fn frog_type() -> Type {
        Type::Union(vec![T::frog_type(), Type::None]).normalize()
    }
    fn leaves() -> Vec<Type> {
        let (members, member_leaves) = two_member_union(
            "Option<T>", T::frog_type(), T::leaves(), Type::None, Vec::new(),
        );
        let layout = crate::codegen::union_layout_of_leaves(&member_leaves);
        crate::codegen::union_columns(&layout, &Type::Union(members).normalize())
    }
    fn from_frog(ctx: &FrogCtx, slots: &[i64]) -> Self {
        let (members, member_leaves) = two_member_union(
            "Option<T>", T::frog_type(), T::leaves(), Type::None, Vec::new(),
        );
        let idx = (slots[0] & crate::runtime::gc::TAG_MASK) as usize - 1;
        let leaf_vals = crate::codegen::unpack_union_member_runtime_of_leaves(&member_leaves, idx, slots);
        if members[idx] == T::frog_type() {
            Some(T::from_frog(ctx, &leaf_vals))
        } else {
            None
        }
    }
}

/// `Result<T, E>` ↔ `T | E`.
///
/// Two preconditions this impl cannot check at compile time, only assert or
/// panic on:
///
/// 1. **`T::frog_type()` must differ from `E::frog_type()`** — enforced by
///    `two_member_union`'s panic (`Type::normalize`'s `X | X = X` rule).
/// 2. **Neither `T` nor `E` may be `()`.** `()`'s `leaves()` is one leaf
///    (`Type::None`, matching `struct_fields`'s standalone-type default),
///    but a *union member* of type `None` must contribute zero leaves (a
///    payload-less member is its own tag). Packing `()` as an ordinary
///    member here would therefore claim a column real frog code never
///    allocates for it — `codegen`'s own `member_leaf_types` special-cases
///    `Type::None`/`Type::Never` for exactly this reason. `Result<T, ()>`
///    (the natural spelling of frog's `T?`) isn't supported by this impl;
///    use `Option<T>` above instead, or a distinct non-`()` sentinel.
impl<T: ToFrog, E: ToFrog> ToFrog for Result<T, E> {
    fn frog_type() -> Type {
        Type::Union(vec![T::frog_type(), E::frog_type()]).normalize()
    }
    fn leaves() -> Vec<Type> {
        let (members, member_leaves) = two_member_union(
            "Result<T, E>", T::frog_type(), T::leaves(), E::frog_type(), E::leaves(),
        );
        let layout = crate::codegen::union_layout_of_leaves(&member_leaves);
        crate::codegen::union_columns(&layout, &Type::Union(members).normalize())
    }
    fn to_frog(self, ctx: &mut FrogCtx, out: &mut [i64]) {
        let (members, member_leaves) = two_member_union(
            "Result<T, E>", T::frog_type(), T::leaves(), E::frog_type(), E::leaves(),
        );
        let (idx, leaf_vals) = match self {
            Ok(v) => {
                let idx = members.iter().position(|m| *m == T::frog_type())
                    .unwrap_or_else(|| panic!(
                        "Result<_, _>::to_frog: Ok's type {} not found in normalized union {:?} \
                         (nested Result/union payloads aren't supported by this impl)", T::frog_type(), members));
                let mut buf = vec![0i64; T::slots()];
                v.to_frog(ctx, &mut buf);
                (idx, buf)
            }
            Err(e) => {
                let idx = members.iter().position(|m| *m == E::frog_type())
                    .unwrap_or_else(|| panic!(
                        "Result<_, _>::to_frog: Err's type {} not found in normalized union {:?} \
                         (nested Result/union payloads aren't supported by this impl)", E::frog_type(), members));
                let mut buf = vec![0i64; E::slots()];
                e.to_frog(ctx, &mut buf);
                (idx, buf)
            }
        };
        let tag = crate::codegen::member_tag(idx);
        let packed = crate::codegen::pack_union_member_runtime_of_leaves(&member_leaves, idx, tag, &leaf_vals);
        out[..packed.len()].copy_from_slice(&packed);
    }
}
impl<T: FromFrog, E: FromFrog> FromFrog for Result<T, E> {
    fn frog_type() -> Type {
        Type::Union(vec![T::frog_type(), E::frog_type()]).normalize()
    }
    fn leaves() -> Vec<Type> {
        let (members, member_leaves) = two_member_union(
            "Result<T, E>", T::frog_type(), T::leaves(), E::frog_type(), E::leaves(),
        );
        let layout = crate::codegen::union_layout_of_leaves(&member_leaves);
        crate::codegen::union_columns(&layout, &Type::Union(members).normalize())
    }
    fn from_frog(ctx: &FrogCtx, slots: &[i64]) -> Self {
        let (members, member_leaves) = two_member_union(
            "Result<T, E>", T::frog_type(), T::leaves(), E::frog_type(), E::leaves(),
        );
        let idx = (slots[0] & crate::runtime::gc::TAG_MASK) as usize - 1;
        let leaf_vals = crate::codegen::unpack_union_member_runtime_of_leaves(&member_leaves, idx, slots);
        if members[idx] == T::frog_type() {
            Ok(T::from_frog(ctx, &leaf_vals))
        } else {
            Err(E::from_frog(ctx, &leaf_vals))
        }
    }
}
