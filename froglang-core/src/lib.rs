// `#[frog_fn]` emits absolute `::froglang_core::…` paths (see
// `froglang_macros`), so code inside this crate — `src/stdlib` — needs this
// to resolve them too, not just downstream crates.
extern crate self as froglang_core;

pub mod frontend;
pub mod utils;
pub mod codegen;
pub mod runtime;
pub mod state;
pub mod notation;
pub mod diagnostics;
pub mod host;
pub mod stdlib;

/// Registers a Rust function as a froglang host function — see `host` and
/// `plans/EMBEDDING.md`. Re-exported here so `#[froglang_core::frog_fn]`
/// works without a separate `froglang_macros` dependency.
pub use froglang_macros::frog_fn;