use std::collections::HashMap;
use std::path::Path;

use crate::codegen::Codegen;
use crate::frontend::parser::ParseError;
use crate::frontend::modules;
use crate::frontend::typeck::{Type, TypeChecker};
use crate::frontend::tokens::Spanned;
use crate::frontend::expression::Expression;
use crate::runtime::gc::{GcHeap, ACTIVE_HEAP};
use crate::runtime;

// ── Public value type ─────────────────────────────────────────────────────────

/// A froglang value returned across the embedding boundary.
/// Strings and lists are deep-copied out of the GC heap on return.
#[derive(Debug, Clone)]
pub enum FrogValue {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    List(Vec<FrogValue>),
    None,
}

impl FrogValue {
    pub fn from_bits(bits: i64, ty: &Type, _heap: &GcHeap) -> Self {
        match ty {
            Type::Int  => FrogValue::Int(bits),
            Type::Float => FrogValue::Float(f64::from_bits(bits as u64)),
            Type::Bool  => FrogValue::Bool(bits != 0),
            Type::Str   => {
                let s = unsafe {
                    runtime::gc::frog_str_as_str(bits as *const runtime::gc::FrogStr)
                };
                FrogValue::Str(s.to_owned())
            },
            Type::List(inner) => {
                let len = runtime::ffi::frog_list_len(bits) as usize;
                let inner_ty = inner.as_ref();
                let elems = (0..len)
                    .map(|i| {
                        let elem = runtime::ffi::frog_list_get(bits, i as i64, 0);
                        FrogValue::from_bits(elem, inner_ty, _heap)
                    })
                    .collect();
                FrogValue::List(elems)
            },
            // Struct values aren't yet representable in the embedding API's
            // `FrogValue` (they're multi-slot in the JIT ABI — see
            // `struct_fields` in codegen/mod.rs — while every other
            // `FrogValue` variant round-trips through exactly one `i64`).
            // Not reachable from `compile_and_run`/test code as long as
            // struct values only ever appear as locals, not as a bare
            // top-level result or REPL binding — see the struct-support plan.
            // Struct/enum values aren't yet representable in the embedding
            // API's `FrogValue` — see the comment above for structs; an
            // enum value is a single `i64` (a `FrogVariant` pointer) but
            // decoding it generically would need the enum's field layout,
            // which isn't threaded through here.
            // `Never` is never a value's actual runtime type — nothing of
            // that type is ever produced — but the match must stay exhaustive.
            Type::None | Type::Function { .. } | Type::Union(_) | Type::TypeVar { .. } | Type::Struct(_) | Type::Never => {
                FrogValue::None
            },
        }
    }

    /// A display string for this value (without the type annotation).
    pub fn display_str(&self) -> String {
        match self {
            FrogValue::Int(n)    => format!("{}", n),
            FrogValue::Float(f)  => format!("{:?}", f),
            FrogValue::Bool(b)   => format!("{}", b),
            FrogValue::Str(s)    => format!("{:?}", s),
            FrogValue::List(v)   => format!(
                "[{}]",
                v.iter().map(|e| e.display_str()).collect::<Vec<_>>().join(", ")
            ),
            FrogValue::None      => String::new(),
        }
    }
}

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum FrogError {
    Parse(Vec<Spanned<ParseError>>),
    /// A module-resolution failure (bad import path, cycle, unknown
    /// export, etc) — see `frontend::modules::ModuleError`.
    Module(String),
    Type(String),
    /// Codegen hit an internal panic (e.g. an unsupported construct the type
    /// checker currently lets through — see `codegen::mod`'s `unimplemented!`
    /// and `panic!` sites). Recovered via `catch_unwind` in `eval`, which
    /// also rolls back the `Codegen`-internal state that panic would
    /// otherwise have corrupted, so this `FrogState` stays usable afterward.
    Codegen(String),
}

impl std::fmt::Display for FrogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrogError::Parse(errs) => {
                // A single lex error routinely cascades into several
                // downstream parse errors at the same (or nearby) span —
                // dedup identical (span, error) pairs so the user sees each
                // distinct problem once, not three copies of it.
                let mut seen: Vec<&Spanned<ParseError>> = Vec::new();
                for e in errs {
                    if !seen.iter().any(|s| s.span == e.span && s.item == e.item) {
                        seen.push(e);
                    }
                }
                for (i, e) in seen.iter().enumerate() {
                    if i > 0 { writeln!(f)?; }
                    write!(f, "{}: {}", e.span.start, e.item)?;
                }
                Ok(())
            },
            FrogError::Module(msg)  => write!(f, "Module error: {}", msg),
            FrogError::Type(msg)    => write!(f, "Type error: {}", msg),
            FrogError::Codegen(msg) => write!(f, "Codegen error: {}", msg),
        }
    }
}

/// Extract a human-readable message from a `catch_unwind` payload. Panics
/// via `panic!("{}", ...)` / `.expect(...)` carry a `&'static str` or
/// `String`; anything else (a custom payload type) falls back to a generic
/// message rather than failing to report the error at all.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "codegen panicked with a non-string payload".to_string()
    }
}

// ── FrogState ─────────────────────────────────────────────────────────────────

/// An independent froglang interpreter instance.
/// All state — heap, type-checker, JIT module, variable environment — is owned here.
/// Multiple `FrogState`s can coexist on different threads with no coordination.
pub struct FrogState {
    pub heap:         GcHeap,
    pub tc:           TypeChecker,
    pub codegen:      Codegen,
    /// One `i64` per flattened leaf field of the binding's type (see
    /// `struct_fields` in `codegen/mod.rs`) — a single element for every
    /// non-struct type, matching how it always worked before structs
    /// existed; more than one for a struct-typed binding.
    pub env:          HashMap<String, Vec<i64>>,
    pub env_types:    HashMap<String, Type>,
    pub string_arena: Vec<Vec<u8>>,
    pub entry_count:  usize,
}

impl FrogState {
    pub fn new() -> Self {
        FrogState::builder().build().expect("FrogState::builder().build() with no host functions cannot fail")
    }

    /// Start building a `FrogState` with host (Rust) functions registered —
    /// see `plans/EMBEDDING.md` and `crate::host`.
    pub fn builder() -> FrogStateBuilder {
        FrogStateBuilder { hosts: Vec::new(), prelude: Vec::new() }
    }

    /// `FrogState::builder()` with the stdlib (`crate::stdlib`) installed —
    /// see `plans/STDLIB.md`. `FrogState::new()` deliberately doesn't do
    /// this itself: it's what the existing test suite calls expecting
    /// today's minimal (`print`/`push`/`panic`/`gc_dump`-only) surface, so
    /// the stdlib is opt-in. The CLI (`main.rs`) opts in.
    pub fn with_stdlib() -> Result<Self, FrogError> {
        crate::stdlib::install(FrogState::builder()).build()
    }

    /// Set `ACTIVE_HEAP` to this state's heap, call `func_ptr` with the
    /// out-buffer pointer, then clear it.
    fn call_jit(&mut self, func_ptr: fn(i64) -> i64, out_ptr: i64, out_gc_slots: Vec<usize>) -> i64 {
        // `func_ptr` writes each top-level binding into `out_ptr` as soon as
        // it is created, and the roots below are only rebuilt after this
        // returns — so for the length of the call that buffer is the only
        // thing keeping some of those values reachable. See
        // `GcHeap::push_scanned_span`.
        self.heap.push_scanned_span(out_ptr as *const i64, out_gc_slots);
        ACTIVE_HEAP.with(|p| p.set(&mut self.heap as *mut GcHeap));
        // Publish a `FrogCtx` a host call can fetch via `frog_ctx_current`
        // (`runtime::host`) — built from raw pointers into `self.heap`/
        // `self.tc`'s tables, valid only for the duration of this call
        // (`FrogCtx::new`'s safety doc). Struct/union defs never move once
        // registered (`TypeChecker::struct_defs`/`union_defs`), so this is
        // sound even though `compile_entry` above may have just grown them.
        let mut host_ctx = unsafe {
            crate::runtime::host::FrogCtx::new(
                &mut self.heap as *mut GcHeap,
                self.tc.struct_defs() as *const _,
                self.tc.union_defs() as *const _,
            )
        };
        let result = crate::runtime::host::with_active_ctx(&mut host_ctx, || func_ptr(out_ptr));
        ACTIVE_HEAP.with(|p| p.set(std::ptr::null_mut()));
        self.heap.pop_scanned_span();
        result
    }

    /// Parse, type-check, compile, and run `src` in this state's context.
    /// Returns the result value and its type.
    ///
    /// On a type error, or a codegen panic (an unsupported construct the
    /// type checker currently lets through), the type-checker and codegen
    /// state are both rolled back to exactly how they were before this call,
    /// so a failed `eval` never leaves the `FrogState` unusable for the next
    /// one.
    ///
    /// `src`'s own `import` statements (if any) are resolved relative to
    /// the current working directory — this is the REPL/no-file entry
    /// point. See `eval_file` for a file-backed entry, where imports
    /// resolve relative to that file's own directory instead.
    pub fn eval(&mut self, src: &str) -> Result<(FrogValue, Type), FrogError> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        self.eval_with_base(src, &cwd.join("<repl>"))
    }

    /// Read, parse, type-check, compile, and run the file at `path`.
    /// `import` statements in it (and transitively, in every file it
    /// imports) resolve relative to each importing file's own directory.
    pub fn eval_file(&mut self, path: &Path) -> Result<(FrogValue, Type), FrogError> {
        let src = std::fs::read_to_string(path)
            .map_err(|e| FrogError::Module(format!("could not read '{}': {}", path.display(), e)))?;
        self.eval_with_base(&src, path)
    }

    fn eval_with_base(&mut self, src: &str, base_path: &Path) -> Result<(FrogValue, Type), FrogError> {
        let stmts = modules::resolve_source(src, base_path).map_err(|e| FrogError::Module(e.to_string()))?;
        let span = match (stmts.first(), stmts.last()) {
            (Some(first), Some(last)) => first.span.merge(last.span),
            _ => crate::frontend::tokens::Span::new((0, 0), (0, 0)),
        };
        let ast = Spanned::from(Expression::Block(stmts), span);

        let cp = self.tc.checkpoint();
        let mut typed = match self.tc.check_and_lower_entry(ast) {
            Ok(t) => t,
            Err(e) => {
                self.tc.restore(cp);
                return Err(FrogError::Type(e.to_string()));
            }
        };
        // Stamp every node with a fresh id before any post-lowering pass
        // touches the tree — see `liveness::number_nodes`'s doc comment for
        // why this must run once, after lowering, rather than during it.
        crate::frontend::liveness::number_nodes(&mut typed);
        if let Err(e) = self.tc.validate_codegen_constraints(&typed) {
            self.tc.restore(cp);
            return Err(FrogError::Type(e.to_string()));
        }

        let result_ty = typed.item.ty.clone();

        // `bindings` is every top-level `let`/assignment made in this entry
        // (in source order) — not just the last one, and not conflated with
        // this entry's own result value (see `build_main_body` in codegen
        // for how `out_ptr` is populated).
        //
        // Wrapped in `catch_unwind`: codegen still panics internally on
        // unsupported constructs (see codegen/mod.rs) rather than returning
        // a `Result`, so this is what stands between one bad `eval` call and
        // a permanently broken `FrogState` (see `Codegen::reset_builder_ctx`
        // for why the panic itself would otherwise corrupt reusable state).
        let func_ids_snap = self.codegen.checkpoint_func_ids();
        let entry_count = self.entry_count;
        let compile_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.codegen.compile_entry(
                typed,
                &mut self.string_arena,
                entry_count,
                &self.env,
                &self.env_types,
                self.tc.struct_defs(),
                self.tc.union_defs(),
            )
        }));

        let (main_id, bindings) = match compile_result {
            Ok(pair) => pair,
            Err(panic_payload) => {
                self.codegen.restore_func_ids(func_ids_snap);
                self.codegen.reset_builder_ctx();
                self.tc.restore(cp);
                return Err(FrogError::Codegen(panic_message(&*panic_payload)));
            }
        };
        self.entry_count += 1;

        let ptr = self.codegen.module.get_finalized_function(main_id);
        let func_ptr: fn(i64) -> i64 = unsafe { std::mem::transmute(ptr) };

        // Each binding occupies `struct_fields(ty, structs).len()` slots
        // (1 for every non-struct type) — see `build_main_body`'s doc
        // comment in codegen/mod.rs for the write side of this protocol.
        let total_slots: usize = bindings.iter()
            .map(|(_, ty)| crate::codegen::struct_fields(ty, self.tc.struct_defs()).len())
            .sum();
        let mut out_buf: Vec<i64> = vec![0i64; total_slots];
        let out_ptr = out_buf.as_mut_ptr() as i64;
        let out_gc_slots = crate::codegen::gc_slots_of_bindings(&bindings, self.tc.struct_defs());
        let bits = self.call_jit(func_ptr, out_ptr, out_gc_slots);

        let mut cursor = 0usize;
        for (name, ty) in &bindings {
            let width = crate::codegen::struct_fields(ty, self.tc.struct_defs()).len();
            self.env.insert(name.clone(), out_buf[cursor..cursor + width].to_vec());
            self.env_types.insert(name.clone(), ty.clone());
            cursor += width;
        }

        // Rebuild the GC's explicit root set from scratch every time, from
        // exactly what's currently live: this entry's own result (which
        // needs to survive long enough for `FrogValue::from_bits` below)
        // plus every heap-typed binding still in `env`. The old code only
        // ever pushed roots and never popped them, so every string or list
        // any entry had ever produced — including ones since shadowed or
        // rebound — stayed alive for the process's lifetime.
        self.heap.clear_roots();
        // `Type::Union` belongs here alongside `Str`/`List`: a union value
        // is a GC-visible word (`codegen::is_heap_ty`) exactly like the
        // other two. It used to be missing, so the result of an entry that
        // produced a `data`-union value was left unrooted for as long as
        // `from_bits` needed it below.
        //
        // Not a bare `matches!` on `result_ty`, though: `bits` is only ever
        // *leaf 0* of the result's flattened value (`build_main_body`'s
        // return is always a single i64), and whether that leaf is a
        // scannable column is a property of the column, not of the whole
        // type — route through `heap_roots_in_leaves` so it is decided the
        // same way the bindings loop below decides it.
        //
        // `get(..1)` rather than `[..1]`: a zero-leaf result type (`Type::None`
        // flattens to one leaf, but a struct with no fields flattens to none)
        // would otherwise panic on the slice.
        let result_leaf0 = crate::codegen::struct_fields(&result_ty, self.tc.struct_defs());
        for v in crate::codegen::heap_roots_in_leaves(&[bits], result_leaf0.get(..1).unwrap_or_default()) {
            self.heap.push_root(v, true);
        }
        for (name, ty) in &self.env_types {
            if let Some(vals) = self.env.get(name) {
                let leafs = crate::codegen::struct_fields(ty, self.tc.struct_defs());
                // Not a plain per-leaf `matches!` on the type: which
                // flattened leaves are GC-scannable columns is
                // `struct_fields`'s business, not something to re-derive
                // here. See `codegen::heap_roots_in_leaves`.
                for v in crate::codegen::heap_roots_in_leaves(vals, &leafs) {
                    self.heap.push_root(v, true);
                }
            }
        }

        self.heap.maybe_collect();

        let value = FrogValue::from_bits(bits, &result_ty, &self.heap);
        Ok((value, result_ty))
    }
}

// ── FrogStateBuilder ─────────────────────────────────────────────────────────

/// Names the frontend matches syntactically rather than by scope lookup
/// (`typeck.rs`'s `"print"`/`"push"` special cases, plus `panic` and its
/// reserved `!`-desugaring alias) — registering a host function under one
/// of these would silently never be called, since the special case wins
/// before the generic `func_ids` lookup ever runs. Rejected at `build()`
/// with a clear error instead.
const RESERVED_NAMES: &[&str] = &["print", "push", "len", "panic", "panic!builtin", "gc_dump"];

/// Builds a `FrogState` with host (Rust) functions registered before the
/// JIT module exists — required because `JITBuilder::symbol` only accepts
/// new symbols at construction, not afterward (see `plans/EMBEDDING.md`).
pub struct FrogStateBuilder {
    hosts:   Vec<crate::host::HostFn>,
    prelude: Vec<String>,
}

impl FrogStateBuilder {
    /// Register a host function, callable from frog source under
    /// `host.name`. See `#[frog_fn]` (`froglang_macros`) for the ordinary
    /// way to build a `HostFn`.
    pub fn func(mut self, host: crate::host::HostFn) -> Self {
        self.hosts.push(host);
        self
    }

    /// Evaluate `src` before any user code — the mechanism for declaring
    /// host `data` types (`error IoError(msg: Str)`) a host function's
    /// signature refers to, since it reuses the ordinary parser/typeck path
    /// rather than a separate Rust-side type-definition API.
    pub fn prelude(mut self, src: impl Into<String>) -> Self {
        self.prelude.push(src.into());
        self
    }

    /// Finish building. Fails if two host functions share a name, or a
    /// host function claims a name the frontend already treats specially
    /// (`RESERVED_NAMES`) — both are configuration errors in the embedder,
    /// not something `eval` should discover later as a confusing shadowing
    /// failure.
    pub fn build(self) -> Result<FrogState, FrogError> {
        let mut seen = std::collections::HashSet::new();
        for host in &self.hosts {
            if RESERVED_NAMES.contains(&host.name) {
                return Err(FrogError::Type(format!(
                    "'{}' is a reserved builtin name and can't be registered as a host function", host.name
                )));
            }
            if !seen.insert(host.name) {
                return Err(FrogError::Type(format!(
                    "host function '{}' is registered more than once", host.name
                )));
            }
        }

        // Symbols and their uniform (ctx, args, out) import signatures are
        // declared before any frog type exists — see `Codegen::new_with_hosts`,
        // which also rejects a host name that collides with a runtime
        // primitive's `func_ids` key.
        let codegen = Codegen::new_with_hosts(&self.hosts).map_err(FrogError::Type)?;

        let mut state = FrogState {
            heap:         GcHeap::new(),
            tc:           TypeChecker::new(),
            codegen,
            env:          HashMap::new(),
            env_types:    HashMap::new(),
            string_arena: Vec::new(),
            entry_count:  0,
        };

        // Host `data` types, if any, before anything references them.
        for src in &self.prelude {
            state.eval(src)?;
        }

        // Install each host function's frog-visible name and type into the
        // checker's global scope — after the prelude, so a signature built
        // against a prelude-declared struct/union name (once that's
        // supported — see `plans/EMBEDDING.md`'s follow-ups) would resolve.
        state.tc.add_ctx(self.hosts.iter().map(|h| {
            (h.name.to_string(), Type::Function { params: h.params.clone(), result: Box::new(h.ret.clone()) })
        }));

        Ok(state)
    }
}
