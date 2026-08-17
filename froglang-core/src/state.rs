use std::collections::HashMap;

use crate::codegen::Codegen;
use crate::frontend::parser::{Parser, ParseError};
use crate::frontend::typeck::{Type, TypeChecker};
use crate::frontend::tokens::Spanned;
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
            Type::None | Type::Function { .. } | Type::Union(_) | Type::TypeVar { .. } | Type::Struct(_) => {
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
                write!(f, "Parse error")?;
                for e in errs { write!(f, ": {:?}", e)?; }
                Ok(())
            },
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
    pub env:          HashMap<String, i64>,
    pub env_types:    HashMap<String, Type>,
    pub string_arena: Vec<Vec<u8>>,
    pub entry_count:  usize,
}

impl FrogState {
    pub fn new() -> Self {
        FrogState {
            heap:         GcHeap::new(),
            tc:           TypeChecker::new(),
            codegen:      Codegen::new(),
            env:          HashMap::new(),
            env_types:    HashMap::new(),
            string_arena: Vec::new(),
            entry_count:  0,
        }
    }

    /// Set `ACTIVE_HEAP` to this state's heap, call `func_ptr` with the
    /// out-buffer pointer, then clear it.
    fn call_jit(&mut self, func_ptr: fn(i64) -> i64, out_ptr: i64) -> i64 {
        ACTIVE_HEAP.with(|p| p.set(&mut self.heap as *mut GcHeap));
        let result = func_ptr(out_ptr);
        ACTIVE_HEAP.with(|p| p.set(std::ptr::null_mut()));
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
    pub fn eval(&mut self, src: &str) -> Result<(FrogValue, Type), FrogError> {
        let ast = Parser::parse(src).map_err(FrogError::Parse)?;

        let cp = self.tc.checkpoint();
        let typed = match self.tc.check_and_lower_entry(ast) {
            Ok(t) => t,
            Err(e) => {
                self.tc.restore(cp);
                return Err(FrogError::Type(e.to_string()));
            }
        };

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

        let mut out_buf: Vec<i64> = vec![0i64; bindings.len()];
        let out_ptr = out_buf.as_mut_ptr() as i64;
        let bits = self.call_jit(func_ptr, out_ptr);

        for ((name, ty), &val_bits) in bindings.iter().zip(out_buf.iter()) {
            self.env.insert(name.clone(), val_bits);
            self.env_types.insert(name.clone(), ty.clone());
        }

        // Rebuild the GC's explicit root set from scratch every time, from
        // exactly what's currently live: this entry's own result (which
        // needs to survive long enough for `FrogValue::from_bits` below)
        // plus every heap-typed binding still in `env`. The old code only
        // ever pushed roots and never popped them, so every string or list
        // any entry had ever produced — including ones since shadowed or
        // rebound — stayed alive for the process's lifetime.
        self.heap.clear_roots();
        if matches!(&result_ty, Type::Str | Type::List(_)) {
            self.heap.push_root(bits, true);
        }
        for (name, ty) in &self.env_types {
            if matches!(ty, Type::Str | Type::List(_)) {
                if let Some(&val_bits) = self.env.get(name) {
                    self.heap.push_root(val_bits, true);
                }
            }
        }

        self.heap.maybe_collect();

        let value = FrogValue::from_bits(bits, &result_ty, &self.heap);
        Ok((value, result_ty))
    }
}
