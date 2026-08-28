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
    const IS_PTR: bool = true;
    fn frog_type() -> Type { Type::strukt("ErrMsg") }
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
    let builder = str::install(builder);
    fs::install(builder)
}
