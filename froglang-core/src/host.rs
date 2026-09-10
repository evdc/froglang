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
/// case (`Slots`-based marshalling).
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
}

// `HostFn` is built once at startup and handed to `Codegen::new_with_hosts`
// on the thread that constructs a `FrogState`; it never needs to cross a
// thread boundary itself; safe to make the concurrency check moot.
unsafe impl Send for HostFn {}

/// Converts a frog argument's flattened `i64` slots into a Rust value.
/// `SLOTS` is `struct_fields(Self::frog_type(), structs).len()` for every
/// type this trait actually covers today (see the impls below) — all of
/// them are single-slot, so `SLOTS` is fixed at 1 rather than threaded
/// through `StructDefs` for now. A type whose slot count depends on
/// registered layout (a struct or union) needs the raw `Slots` escape
/// hatch until a derive macro closes that gap — see `plans/EMBEDDING.md`'s
/// follow-ups.
pub trait FromFrog: Sized {
    const SLOTS: usize = 1;
    fn frog_type() -> Type;
    /// `slots` is exactly `Self::SLOTS` long.
    fn from_frog(ctx: &FrogCtx, slots: &[i64]) -> Self;
}

/// The mirror of `FromFrog`, converting a Rust value into a host function's
/// flattened result slots.
pub trait ToFrog {
    const SLOTS: usize = 1;
    /// True iff this type's single wire slot is a GC-scannable pointer
    /// column (`codegen::is_heap_ty`) rather than a raw scalar — and, if
    /// so, that pointer is "overlay-safe" (`codegen::overlay_safe`: true
    /// for `Str`/`List`, false for anything union-shaped). Only meaningful
    /// when `SLOTS == 1`, which is why it lives here rather than being
    /// derived from `frog_type()` at the one call site that needs it
    /// (`Result<T, E>`'s blanket impl below) — `frog_type()` returns a
    /// runtime `Type` value, not something a `const` can pattern-match.
    const IS_PTR: bool = false;
    fn frog_type() -> Type;
    /// `out` is exactly `Self::SLOTS` long; every slot must be written.
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

// `SLOTS = 1`, not 0: `struct_fields(Type::None, ..)` (codegen/mod.rs) falls
// through to its default one-leaf arm, so both `compile_host_call`'s
// argument buffer and its `Vec<T>` stride math (`T::SLOTS.max(1)` above)
// treat a `None`-typed value as occupying one slot. Declaring `SLOTS = 0`
// here shrank the buffer this type consumes without shrinking codegen's,
// misaligning every argument after it.
impl FromFrog for () {
    const SLOTS: usize = 1;
    fn frog_type() -> Type { Type::None }
    fn from_frog(_ctx: &FrogCtx, _slots: &[i64]) -> Self {}
}
impl ToFrog for () {
    const SLOTS: usize = 1;
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
    const IS_PTR: bool = true;
    fn frog_type() -> Type { Type::Str }
    fn to_frog(self, ctx: &mut FrogCtx, out: &mut [i64]) { out[0] = ctx.alloc_str(&self); }
}
impl ToFrog for &str {
    const IS_PTR: bool = true;
    fn frog_type() -> Type { Type::Str }
    fn to_frog(self, ctx: &mut FrogCtx, out: &mut [i64]) { out[0] = ctx.alloc_str(self); }
}

impl<T: FromFrog> FromFrog for Vec<T> {
    fn frog_type() -> Type { Type::list(T::frog_type()) }
    fn from_frog(ctx: &FrogCtx, slots: &[i64]) -> Self {
        let w = slots[0];
        let stride = T::SLOTS.max(1);
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
    const IS_PTR: bool = true;
    fn frog_type() -> Type { Type::list(T::frog_type()) }
    fn to_frog(self, ctx: &mut FrogCtx, out: &mut [i64]) {
        let stride = T::SLOTS.max(1);
        let mut flat: Vec<i64> = Vec::with_capacity(self.len() * stride);
        for elem in self {
            let mut buf = vec![0i64; T::SLOTS];
            elem.to_frog(ctx, &mut buf);
            flat.extend_from_slice(&buf);
        }
        // `List`'s pointer columns are per-element-slot, not per-list — see
        // `FrogList`'s `ptr_mask` doc comment in `gc.rs`. Every type
        // `ToFrog`/`FromFrog` cover today is single-slot, so there is at
        // most one column and its scannability is `T`'s own.
        let ptr_mask: u64 = if crate::codegen::is_heap_ty(&T::frog_type()) { 1 } else { 0 };
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
        let kstride = K::SLOTS.max(1);
        let vstride = V::SLOTS;
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
    const IS_PTR: bool = true;
    fn frog_type() -> Type { Type::dict(K::frog_type(), V::frog_type()) }
    fn to_frog(self, ctx: &mut FrogCtx, out: &mut [i64]) {
        let kstride = K::SLOTS.max(1);
        let vstride = V::SLOTS;
        let mut flat: Vec<i64> = Vec::with_capacity(self.len() * (kstride + vstride));
        for (k, v) in self {
            let mut kbuf = vec![0i64; K::SLOTS];
            k.to_frog(ctx, &mut kbuf);
            flat.extend_from_slice(&kbuf);
            let mut vbuf = vec![0i64; V::SLOTS];
            v.to_frog(ctx, &mut vbuf);
            flat.extend_from_slice(&vbuf);
        }
        // One bit each for the key and value columns — sound only because
        // every `ToFrog` impl today is single-slot, exactly the
        // assumption `Vec<T>`'s identical `ptr_mask` computation above
        // already depends on.
        let ptr_mask: u64 = (K::IS_PTR as u64) | ((V::IS_PTR as u64) << kstride);
        let key_kind = crate::codegen::key_kind_of(&K::frog_type()) as u32;
        out[0] = ctx.alloc_dict(&flat, kstride, vstride, ptr_mask, key_kind);
    }
}

/// `Result<T, E>` ↔ `T | E` — the marshalling `plans/EMBEDDING.md` left as
/// a follow-up (return position only; there is no `FromFrog` here, host
/// functions don't need to *accept* unions for anything the stdlib plans
/// to ship). See `plans/STDLIB.md` Phase 2.
///
/// Two preconditions this impl cannot check at compile time, only assert:
///
/// 1. **`T::SLOTS == 1` and `E::SLOTS == 1`.** Every `ToFrog` impl today is
///    single-slot, so this always holds in practice; a future multi-slot
///    `ToFrog` type (a struct or nested union) would need real derive-macro
///    support anyway (`plans/EMBEDDING.md`'s follow-ups), not an extension
///    of this impl. Checked in `SLOTS`'s own `const` initializer, so a
///    violation is a compile error at the instantiation site, not a
///    surprise at runtime.
/// 2. **`T`'s and `E`'s pointer shapes, if either is `IS_PTR`, are
///    "overlay-safe"** (`codegen::overlay_safe` — true for `Str`/`List`,
///    which is everything `IS_PTR` is set on today). A hypothetical future
///    `IS_PTR` type that *isn't* overlay-safe would force
///    `codegen::union_layout` to give the tag a dedicated column — one slot
///    wider than `SLOTS`'s formula here assumes. Caught by the
///    `debug_assert_eq!` in `to_frog` (that one can't be a `const` check:
///    the real layout depends on `StructDefs`, a runtime value).
///
/// A fourth precondition isn't checked anywhere and will panic obscurely
/// (a `debug_assert_eq!` mismatch inside `pack_union_member_runtime`, not a
/// clear message) if violated: **neither `T` nor `E` may be `()`.**
/// `codegen::member_leaf_types` special-cases `Type::None` to contribute
/// *zero* leaves (a payload-less member is its tag), but this impl always
/// writes exactly one leaf value per member — so `Result<T, ()>`, which
/// would otherwise be the natural spelling of frog's `T?` (`T | None`), is
/// not actually supported today. Use a distinct non-`()` sentinel (e.g. an
/// error string) instead of a `None`-carrying variant until this is fixed.
///
/// A fifth precondition `to_frog` panics on directly rather than merely
/// asserting: **`T::frog_type()` must differ from `E::frog_type()`.**
/// `Type::normalize` collapses `X | X` to plain `X` (the same rule that
/// applies if a user wrote that union in frog source) — so `Result<String,
/// String>` has no way to distinguish Ok from Err once it crosses into
/// frog. Pick a distinct type for the error side (an error code `Int`, or
/// once struct marshalling exists, a nominal error type) rather than
/// reusing `T`'s own type.
impl<T: ToFrog, E: ToFrog> ToFrog for Result<T, E> {
    const SLOTS: usize = {
        assert!(T::SLOTS == 1, "Result<T, E>: ToFrog requires T::SLOTS == 1");
        assert!(E::SLOTS == 1, "Result<T, E>: ToFrog requires E::SLOTS == 1");
        // `codegen::union_layout`'s shape for two single-leaf members: one
        // pointer/tag column always (forced by `!dedicated_tag`, since the
        // only `IS_PTR` leaves `ToFrog` produces are overlay-safe), plus one
        // scalar column iff at least one member is scalar-shaped.
        1 + (!T::IS_PTR || !E::IS_PTR) as usize
    };

    fn frog_type() -> Type {
        Type::Union(vec![T::frog_type(), E::frog_type()]).normalize()
    }

    fn to_frog(self, ctx: &mut FrogCtx, out: &mut [i64]) {
        let ok_ty = T::frog_type();
        let err_ty = E::frog_type();
        let union_ty = Type::Union(vec![ok_ty.clone(), err_ty.clone()]).normalize();
        let Type::Union(members) = &union_ty else {
            panic!(
                "Result<_, _>::to_frog: Ok ({}) and Err ({}) map to the same frog type — \
                 `T | T` normalizes to `T`, so they can't be told apart once packed. Use a \
                 distinct type for the error side.",
                ok_ty, err_ty,
            );
        };

        let (member_ty, tag_index, leaf_vals) = match self {
            Ok(v) => {
                let idx = members.iter().position(|m| *m == ok_ty).unwrap_or_else(|| {
                    panic!("Result<_, _>::to_frog: Ok's type {} not found in normalized union {} \
                            (nested Result/union payloads aren't supported by this impl)", ok_ty, union_ty)
                });
                let mut buf = vec![0i64; T::SLOTS];
                v.to_frog(ctx, &mut buf);
                (ok_ty, idx, buf)
            }
            Err(e) => {
                let idx = members.iter().position(|m| *m == err_ty).unwrap_or_else(|| {
                    panic!("Result<_, _>::to_frog: Err's type {} not found in normalized union {} \
                            (nested Result/union payloads aren't supported by this impl)", err_ty, union_ty)
                });
                let mut buf = vec![0i64; E::SLOTS];
                e.to_frog(ctx, &mut buf);
                (err_ty, idx, buf)
            }
        };

        let tag = crate::codegen::member_tag(tag_index);
        let packed = crate::codegen::pack_union_member_runtime(members, &member_ty, tag, &leaf_vals, ctx.structs());
        debug_assert_eq!(
            packed.len(), Self::SLOTS,
            "Result<T, E>::SLOTS ({}) disagrees with the real union layout ({} slots) for {} — \
             see this impl's doc comment, precondition 2",
            Self::SLOTS, packed.len(), union_ty,
        );
        out[..packed.len()].copy_from_slice(&packed);
    }
}
