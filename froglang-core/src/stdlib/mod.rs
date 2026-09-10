//! The minimal standard library — see `plans/STDLIB.md`.
//!
//! Built entirely on the embedding API (`plans/EMBEDDING.md`): every
//! function here is either a `#[frog_fn]` host function registered through
//! `FrogStateBuilder`, or (for the handful of things that need real
//! polymorphism over `List<T>`, e.g. `len`) a hardcoded builtin living
//! alongside `push` in `frontend/typeck.rs`/`codegen/mod.rs` rather than
//! here.

use crate::frontend::typeck::Type;
use crate::host::ToFrog;
use crate::runtime::host::FrogCtx;
use crate::state::FrogStateBuilder;

mod fs;
mod str;

/// The stdlib's shared error type: a nominal one-field struct wrapping a
/// message, declared into every `FrogState` this module installs onto
/// (`install`'s own prelude) before any function returning one can be
/// called.
///
/// Not a bare `String`: `ToFrog for Result<T, E>` (`host.rs`) panics if
/// `T::frog_type() == E::frog_type()` (`Type::normalize` collapses `Str |
/// Str` to plain `Str`, losing the tag) — which a `String`-shaped error
/// would hit on the very first stdlib function whose success value is
/// *also* a `Str` (`slice`, and file IO's eventual `read_file`). `ErrMsg`
/// is a distinct nominal type (`Type::strukt("ErrMsg")`), so it never
/// collides with any `Ok` type, string-shaped or not — the uniform error
/// convention for every fallible function in this module.
pub(crate) struct ErrMsg(pub String);

impl ToFrog for ErrMsg {
    fn frog_type() -> Type { Type::strukt("ErrMsg") }
    // `leaves()` cannot use the trait's default (`vec![Self::frog_type()]`):
    // that default is only correct for a type that *is* a leaf column in
    // frog's own flattening (`Int`/`Str`/`List`/... — anything
    // `codegen::is_heap_ty`/`overlay_safe` already knows how to classify on
    // sight). `ErrMsg`'s `frog_type()` is `Type::strukt("ErrMsg")`, a
    // compound (`Named`) type that is not itself a leaf — its *real* wire
    // column, per `codegen::struct_fields`, is its one field's type. Naming
    // that explicitly here is exactly what `#[derive(FrogData)]`
    // (`plans/EMBEDDING.md`) automates for an arbitrary struct by composing
    // each field's own `leaves()`; this hand-written impl states the same
    // fact by hand for frog's one built-in struct-shaped error type.
    fn leaves() -> Vec<Type> { vec![Type::Str] }
    fn to_frog(self, ctx: &mut FrogCtx, out: &mut [i64]) {
        out[0] = ctx.alloc_str(&self.0);
    }
}

/// Register every stdlib host function and prelude declaration onto a
/// builder. `FrogState::with_stdlib()` is the usual entry point; this is
/// exposed directly for embedders who want the stdlib alongside their own
/// `.func(...)`/`.prelude(...)` calls.
pub fn install(builder: FrogStateBuilder) -> FrogStateBuilder {
    // `error` (not plain `data`) grants `Trait::Error`, so `ErrMsg` values
    // are usable with `catch`/`?`/`!` in addition to plain `match`/`is`.
    let builder = builder.prelude("error ErrMsg(msg: Str)");
    // `get`'s error member — like `len`/`push`, `get` itself is a hardcoded
    // builtin in `typeck.rs`/`codegen/mod.rs` (it needs real polymorphism
    // over `List<T>`), but unlike them it returns a union, so it needs an
    // `Error`-providing type declared before it's usable — same reasoning
    // as `ErrMsg` above.
    let builder = builder.prelude("error IndexError(index: Int, len: Int)");
    // `Dict.get`'s error member — same reasoning as `IndexError` above.
    // `key` is `repr(k)`, not `k` itself, so this works for any `Trait::
    // Hash` key type rather than assuming `Str`.
    let builder = builder.prelude("error KeyError(key: Str)");
    let builder = str::install(builder);
    fs::install(builder)
}
