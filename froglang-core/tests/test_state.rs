//! Regression tests for `FrogState`'s multi-entry REPL/embedding semantics.
//!
//! `tests/test_run.rs` exercises `codegen::compile_and_run`, a single-shot
//! path that never touches `FrogState::eval`'s environment-threading logic.
//! These tests instead go through `FrogState` directly, which is what the
//! REPL and the embedding API (`README.md`'s `FrogState` example) actually
//! use — the bugs below shipped for months precisely because nothing here
//! existed to catch them.

use froglang_core::state::{FrogState, FrogValue};

fn int(v: &FrogValue) -> i64 {
    match v {
        FrogValue::Int(n) => *n,
        other => panic!("expected Int, got {:?}", other),
    }
}

fn float(v: &FrogValue) -> f64 {
    match v {
        FrogValue::Float(n) => *n,
        other => panic!("expected Float, got {:?}", other),
    }
}

/// A `let` followed by a bare expression in the *same* entry must bind the
/// `let`'s own value, not the entry's trailing expression value.
#[test]
fn test_let_then_bare_expr_binds_correct_value() {
    let mut s = FrogState::new();
    let (result, _) = s.eval("let a = 1\n99").unwrap();
    assert_eq!(int(&result), 99, "entry's own result should be the trailing expression");

    let (a, _) = s.eval("a").unwrap();
    assert_eq!(int(&a), 1, "`a` should be bound to 1, not to the entry's trailing value 99");
}

/// Multiple `let` bindings in a single entry must *all* survive into later
/// entries, not just the last one found.
#[test]
fn test_multiple_bindings_in_one_entry_all_survive() {
    let mut s = FrogState::new();
    s.eval("let a = 1\nlet b = 2").unwrap();

    let (a, _) = s.eval("a").unwrap();
    assert_eq!(int(&a), 1);
    let (b, _) = s.eval("b").unwrap();
    assert_eq!(int(&b), 2);
}

/// Three bindings, to make sure ordering/offsets into the out-buffer are
/// correct, not just "first vs last" as with two.
#[test]
fn test_three_bindings_in_one_entry_all_survive_in_order() {
    let mut s = FrogState::new();
    s.eval("let a = 10\nlet b = 20\nlet c = 30").unwrap();

    assert_eq!(int(&s.eval("a").unwrap().0), 10);
    assert_eq!(int(&s.eval("b").unwrap().0), 20);
    assert_eq!(int(&s.eval("c").unwrap().0), 30);
}

/// Redefining a function in a later entry must not panic, and later calls
/// must dispatch to the new definition.
#[test]
fn test_function_redefinition_uses_new_body() {
    let mut s = FrogState::new();
    s.eval("func f(n: Int): Int = n + 1").unwrap();
    s.eval("func f(n: Int): Int = n + 2").unwrap();

    let (result, _) = s.eval("f(10)").unwrap();
    assert_eq!(int(&result), 12, "f(10) should use the second definition (n + 2)");
}

/// A string bound early must remain readable — and correct — after many
/// subsequent entries that themselves allocate strings (and thus may
/// trigger GC collections between JIT calls).
#[test]
fn test_str_binding_survives_across_many_entries() {
    let mut s = FrogState::new();
    s.eval(r#"let keep = "SENTINEL""#).unwrap();

    for i in 0..40 {
        s.eval(&format!(
            r#"let junk{} = "padding padding padding" + " more more more more""#,
            i
        )).unwrap();
    }

    let (keep, _) = s.eval("keep").unwrap();
    match keep {
        FrogValue::Str(s) => assert_eq!(s, "SENTINEL"),
        other => panic!("expected Str, got {:?}", other),
    }
}

/// A failed `eval` due to a *type* error (as opposed to a codegen panic,
/// which is a separate, still-open issue — see DESIGN.md / project notes on
/// `builder_ctx` poisoning) must not poison the state: a subsequent valid
/// `eval` on the same `FrogState` should still succeed. The type checker
/// already rolls back via `checkpoint`/`restore`; this just guards it.
#[test]
fn test_eval_recovers_after_type_error() {
    let mut s = FrogState::new();
    assert!(s.eval(r#"1 + "oops""#).is_err());

    let (result, _) = s.eval("1 + 2").unwrap();
    assert_eq!(int(&result), 3);
}

/// A `let` made inside an `if` branch must not be visible after the
/// conditional. Before this was fixed at the type-checker level, this
/// program type-checked successfully and then crashed the Cranelift
/// verifier in codegen (the branch-local SSA value doesn't dominate the use
/// site) — now it's rejected with an ordinary type error, across both the
/// braced and bare-branch spellings.
#[test]
fn test_let_in_conditional_branch_is_a_type_error_not_a_crash() {
    let mut s = FrogState::new();
    assert!(s.eval("let c = true\nif c then { let y = 5 } else 0\ny").is_err());

    let mut s2 = FrogState::new();
    assert!(s2.eval("let c = true\nif c then let y = 5 else 0\ny").is_err());
}

/// Rebinding the same name must let the *old* value become garbage. Before
/// this was fixed, `GcHeap`'s explicit root set only ever grew (every value
/// any entry had ever produced was pushed once and never popped), so a
/// shadowed/rebound value stayed live — and rooted — for the rest of the
/// process, even though nothing could reach it anymore.
#[test]
fn test_rebinding_same_name_does_not_leak_old_value() {
    let mut s = FrogState::new();
    let big = "x".repeat(4000);
    for _ in 0..500 {
        s.eval(&format!(r#"let s = "{}""#, big)).unwrap();
    }
    // gc_threshold grows to 2x the live set on every collection, so an
    // automatic collection can legitimately skip several hundred KB of
    // *real* garbage before the next one fires — force one final sweep for
    // a deterministic check, rather than assert on however far the
    // self-growing threshold happened to get in 500 iterations.
    s.heap.force_collect();
    // Only the *latest* `s` (~4000 bytes plus its GC header) should still be
    // live. Under the old accumulate-forever roots, this would be on the
    // order of 500 * 4000 = 2,000,000 bytes instead.
    assert!(
        s.heap.bytes_allocated < 100_000,
        "expected old rebindings of `s` to be collected, but {} bytes are still live",
        s.heap.bytes_allocated
    );
}

/// Codegen still panics internally on constructs the type checker allows
/// but doesn't implement — here printing a `List(Never)`, which
/// `print_value` has no arm for. `eval` must convert that panic into a
/// clean `Err`, not let it escape — and, critically, the `FrogState` must
/// stay fully usable afterward: defining and calling new functions, and
/// referencing bindings made before the panic.
///
/// This used to use `(x -> x + 1)(5)`, which now fails earlier and better,
/// as a spanned type error — see `test_typeck.rs`'s function-value
/// diagnostics; and then `print(none)`, which `plans/DATA.md` stage 1 made
/// print `none` as it always should have.
///
/// This prints a panic message to stderr (Rust's default panic hook runs
/// before `catch_unwind` recovers) — that's expected, not a test failure.
#[test]
fn test_codegen_panic_becomes_clean_error_and_state_survives() {
    use froglang_core::state::FrogError;

    let mut s = FrogState::new();
    s.eval("let kept = 41").unwrap();

    match s.eval("print([panic(\"boom\")])") {
        Err(FrogError::Codegen(_)) => {},
        other => panic!("expected a Codegen error, got {:?}", other),
    }

    // Old bindings survived, and the state can still compile and run.
    let (kept, _) = s.eval("kept").unwrap();
    assert_eq!(int(&kept), 41);

    s.eval("func double(n: Int): Int = n * 2").unwrap();
    let (doubled, _) = s.eval("double(21)").unwrap();
    assert_eq!(int(&doubled), 42);
}

/// A binding is still visible to the rest of *its own* branch, and the
/// `FrogState` recovers cleanly and keeps working after the type error above.
#[test]
fn test_let_in_conditional_branch_visible_within_branch_and_state_recovers() {
    let mut s = FrogState::new();
    let (ok, _) = s.eval("if true then { let y = 5; y + 1 } else 0").unwrap();
    assert_eq!(int(&ok), 6);

    assert!(s.eval("if true then { let z = 1 } else 0\nz").is_err());

    let (recovered, _) = s.eval("1 + 1").unwrap();
    assert_eq!(int(&recovered), 2);
}

/// End-to-end stress for the two GC mechanisms a program exercises on every
/// heap-touching call: Cranelift's stack-map roots (RUNTIME.md Part 2,
/// `gc.rs`'s "Precise roots"), and the size-class free lists `GcHeap::sweep`
/// recycles blocks onto.
///
/// Both replaced simpler-but-slower designs (a hand-written shadow stack,
/// and a straight `alloc`/`dealloc` per object). Their failure mode is not
/// a wrong answer in the small — every existing test still passes against a
/// that drops a frame, or a free list that hands out a block still reachable
/// from somewhere — but corruption that only appears once collections
/// actually fire *while* a deep call stack holds heap values in registers.
/// That needs three things at once, which is what this builds:
///
///   * a large structure that stays reachable across many collections
///     (`spine`), so the mark phase has to find it through the shadow chain
///     rather than trivially through the entry frame;
///   * enough garbage churn to drive many collections and so recycle
///     thousands of blocks through the free lists;
///   * recursion deep enough that the shadow chain is many frames long when
///     a collection fires mid-call.
///
/// The assertion is that the retained structure still sums correctly at the
/// end. A dropped frame or a prematurely recycled block shows up here as a
/// wrong total or a crash, not as a subtle slowdown.
#[test]
fn test_gc_survives_churn_against_a_deep_retained_structure() {
    let mut s = FrogState::new();
    s.eval(
        r#"
data Chain is Nil | Link(v: Int, rest: Chain)
func build(n: Int, acc: Chain): Chain = if n == 0 then acc else build(n - 1, Link(v=n, rest=acc))
func total(c: Chain): Int = match c {
    is Nil then 0
    is Link(v, rest) then v + total(rest)
}
"#,
    )
    .unwrap();

    // 1..=800 == 320400. Retained for the whole test.
    s.eval("let spine = build(800, Nil)").unwrap();

    // Churn: each round builds and discards a chain of its own, and also
    // re-walks `spine` — so `spine`'s cells are live, in registers, and
    // deep in the shadow chain at the moment a collection fires.
    let (v, _) = s
        .eval(
            r#"
mut checksum = 0
for round in 0..300 do {
    let garbage = build(120, Nil)
    checksum = checksum + total(garbage) + total(spine)
}
checksum
"#,
        )
        .unwrap();
    // Per round: 1..=120 (7260) + 1..=800 (320400) = 327660, times 300.
    assert_eq!(int(&v), 327660 * 300);

    // `spine` must have survived every one of those collections intact.
    let (after, _) = s.eval("total(spine)").unwrap();
    assert_eq!(int(&after), 320400);

    // And a forced sweep must reclaim all the garbage, leaving only the
    // spine — the free lists recycle blocks rather than returning them to
    // the system allocator, but `bytes_allocated` still counts live bytes
    // only, so this is the same bound it always was.
    s.heap.force_collect();
    assert!(
        s.heap.bytes_allocated < 200_000,
        "expected the churn to be collected, but {} bytes are still live",
        s.heap.bytes_allocated
    );
}

// ── Type schemes across entries (TRAITS.md Stage 2/3b) ───────────────────────

/// A generic that establishes its concrete type by being *called* in its
/// own entry (which is what makes `monomorphize_generics` able to compile
/// that instantiation before that entry's own codegen runs) keeps working
/// at that same type from a later entry — the ordinary "define in one
/// REPL entry, use it in a later one" pattern the REPL exists for. The
/// second call reuses the same compiled instantiation
/// (`emitted_instantiations`) rather than recompiling it.
#[test]
fn test_generic_established_in_one_entry_is_callable_from_a_later_entry() {
    let mut s = FrogState::new();
    s.eval("let f = x -> x + x\nf(1)").unwrap();

    let (result, _) = s.eval("f(3)").unwrap();
    assert_eq!(int(&result), 6);
}

/// `TRAITS.md` Stage 3b's real monomorphization closes the Stage 2 gap: a
/// generic instantiated at one type in an early entry and at a genuinely
/// different type in a later entry now compiles and runs *both*
/// instantiations correctly, rather than being rejected.
/// `monomorphize_generics`'s per-entry walk only sees each entry's own
/// typed AST, but that's enough — each entry independently notices its
/// own new instantiation and compiles it, keyed by mangled name so the two
/// instantiations' `FuncId`s never collide.
#[test]
fn test_generic_instantiated_at_a_different_type_in_a_later_entry_both_work() {
    let mut s = FrogState::new();
    s.eval("let f = x -> x + x\nf(1)").unwrap();

    let (result, _) = s.eval("f(1.5)").unwrap();
    assert_eq!(float(&result), 3.0);

    // The original entry's instantiation must still work unchanged.
    let (result, _) = s.eval("f(10)").unwrap();
    assert_eq!(int(&result), 20);
}

/// `TRAITS.md` Stage 2's documented "known remaining boundary": a generic
/// declared in one entry and never called there must still monomorphize
/// correctly the first time it's called — in a later entry — at each of
/// several distinct concrete types, closing the boundary Stage 2 left
/// open (a declaration's un-substituted binder `TypeVar`s used to only
/// ever get resolved by the entry that first called it).
#[test]
fn test_generic_declared_without_being_called_monomorphizes_on_first_call_in_a_later_entry() {
    let mut s = FrogState::new();
    s.eval("let f = x -> x + x").unwrap();

    let (result, _) = s.eval("f(4)").unwrap();
    assert_eq!(int(&result), 8);

    let (result, _) = s.eval("f(2.5)").unwrap();
    assert_eq!(float(&result), 5.0);
}

/// Redefining a generic in a later entry must compile the *new* body, not
/// silently reuse the old one. `emitted_instantiations` is keyed by the
/// mangled name, which used to be built from the bare source name — so the
/// redefinition's `f$Int` was already marked emitted and its body was
/// skipped, with call sites rewritten to the stale symbol. Each generalized
/// declaration now carries its own template symbol, so the two `f`s mangle
/// to different names and never collide.
#[test]
fn test_redefining_a_generic_in_a_later_entry_uses_the_new_body() {
    let mut s = FrogState::new();
    s.eval("let f = x -> x").unwrap();
    assert_eq!(int(&s.eval("f(3)").unwrap().0), 3);

    s.eval("let f = x -> x + x").unwrap();
    assert_eq!(int(&s.eval("f(3)").unwrap().0), 6);
}

/// An entry whose last statement is a generic declaration has no
/// representable result value — the declaration is stripped from codegen
/// like any other, and `None` stands in for it. It used to fall through to
/// the value of the statement *before* it.
#[test]
fn test_an_entry_ending_in_a_generic_declaration_evaluates_to_none() {
    let mut s = FrogState::new();
    let (result, ty) = s.eval("let x = 5\nlet f = y -> y").unwrap();
    assert!(matches!(result, FrogValue::None), "expected None, got {:?}", result);
    assert_eq!(format!("{}", ty), "None");
}

/// `let f = g` where `g` names a function is an *alias*: froglang has no
/// runtime function value to copy, so a second name for a function is a
/// compile-time rebinding and the declaration itself compiles to nothing.
/// It used to reach codegen as `Assign { f, Var(g) }` and panic with
/// "unbound variable in codegen: g". The alias must survive into later
/// entries like any other binding.
#[test]
fn test_a_function_alias_is_callable_in_a_later_entry() {
    let mut s = FrogState::new();
    s.eval("func g(x: Int): Int = x + 1\nlet f = g").unwrap();

    assert_eq!(int(&s.eval("f(3)").unwrap().0), 4);
    // Aliasing an alias resolves to the same underlying declaration.
    s.eval("let h = f").unwrap();
    assert_eq!(int(&s.eval("h(10)").unwrap().0), 11);
}

/// An alias of a *generic* stays generic — it carries the target's binders
/// and its template symbol, so calling it instantiates through the same
/// template (and shares already-emitted instantiations with the original
/// name rather than compiling duplicates).
#[test]
fn test_an_alias_of_a_generic_instantiates_through_the_same_template() {
    let mut s = FrogState::new();
    s.eval("func id(x) = x\nlet f = id").unwrap();

    assert_eq!(int(&s.eval("f(3)").unwrap().0), 3);
    assert_eq!(float(&s.eval("f(1.5)").unwrap().0), 1.5);
    // The original name still works, at an instantiation the alias made.
    assert_eq!(int(&s.eval("id(7)").unwrap().0), 7);
}

/// Every other use of a function in value position — there is no closure
/// object, function pointer, or indirect call to compile it to — is a
/// spanned type error now, not a codegen panic. `mut f = g` is included
/// deliberately: reassigning it would have to change what a call site
/// resolves to at runtime, which is exactly the indirect call that
/// doesn't exist.
#[test]
fn test_using_a_function_as_a_value_is_a_type_error() {
    use froglang_core::state::FrogError;

    for src in ["func g(x: Int): Int = x\nprint(g)",
                "func g(x: Int): Int = x\nmut f = g",
                "func g(x: Int): Int = x\nlet xs = [g]"] {
        let mut s = FrogState::new();
        match s.eval(src) {
            Err(FrogError::Type(msg)) => assert!(
                msg.contains("is a function"), "unexpected error for {:?}: {}", src, msg),
            other => panic!("expected a Type error for {:?}, got {:?}", src, other),
        }
    }
}

/// The same for a function *literal* outside a declaration — an argument,
/// a list element, or the callee of an immediately-invoked lambda.
#[test]
fn test_using_a_function_literal_as_a_value_is_a_type_error() {
    use froglang_core::state::FrogError;

    for src in ["(x -> x + 1)(5)",
                "func apply2(h, v) = h(v)\napply2(x -> x + 1, 4)",
                "let xs = [x -> x]"] {
        let mut s = FrogState::new();
        match s.eval(src) {
            Err(FrogError::Type(msg)) => assert!(
                msg.contains("function literal"), "unexpected error for {:?}: {}", src, msg),
            other => panic!("expected a Type error for {:?}, got {:?}", src, other),
        }
    }
}

// ── Traits across entries (`plans/TRAITS.md` Stage 5) ────────────────────

/// A trait, its impl, and a call to a member may each land in a different
/// entry. The registries backing them (`traits`, `impls`, `member_index`)
/// persist across entries the way `struct_defs` and `ctx` already do.
#[test]
fn test_trait_declared_implemented_and_called_in_three_separate_entries() {
    let mut s = FrogState::new();
    s.eval("trait Shape { func area(s: Self): Int }").unwrap();
    s.eval("data Circle(r: Int) provides Shape { func area(c: Circle): Int = c.r * c.r }").unwrap();
    let (v, _) = s.eval("Circle(r=6).area()").unwrap();
    assert_eq!(int(&v), 36);
}

/// A generic bounded by a trait is lowered once, at its declaration, with
/// its member calls left pending — so the impl it eventually resolves to may
/// be declared at a *later* prompt than the generic itself, and each call
/// resolves against whatever the registry says when that call is compiled.
#[test]
fn test_a_bounded_generic_resolves_members_against_impls_declared_after_it() {
    let mut s = FrogState::new();
    s.eval("trait Shape { func area(s: Self): Int }").unwrap();
    s.eval("func report<T: Shape>(x: T): Int = x.area()").unwrap();
    // Declared after the generic that calls it.
    s.eval("data Circle(r: Int) provides Shape { func area(c: Circle): Int = c.r * c.r }").unwrap();
    let (v, _) = s.eval("report(Circle(r=6))").unwrap();
    assert_eq!(int(&v), 36);
    // A second implementing type, later still: the same declaration is
    // instantiated again, against an impl that didn't exist at either the
    // declaration or the first call.
    s.eval("provides Shape for Int { func area(n: Int): Int = n + 1 }").unwrap();
    let (v, _) = s.eval("report(7)").unwrap();
    assert_eq!(int(&v), 8);
}

/// A failed entry must leave the trait registries exactly as they were —
/// `TypeCheckerCheckpoint` covers them alongside `provides` and `struct_defs`,
/// so a rejected impl can be corrected and retried at the next prompt.
#[test]
fn test_a_rejected_impl_leaves_the_registries_clean_and_can_be_retried() {
    let mut s = FrogState::new();
    s.eval("trait Shape { func area(s: Self): Int }").unwrap();
    // Wrong return type: rejected during `expand_impls`, after the data
    // declaration in the same entry was already hoisted.
    assert!(s.eval("data Circle(r: Int) provides Shape { func area(c: Circle): Str = \"x\" }").is_err());
    // The same declaration, corrected, must now be accepted — nothing from
    // the failed attempt may still be registered.
    s.eval("data Circle(r: Int) provides Shape { func area(c: Circle): Int = c.r }").unwrap();
    let (v, _) = s.eval("Circle(r=4).area()").unwrap();
    assert_eq!(int(&v), 4);
}
