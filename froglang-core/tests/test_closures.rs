//! Tier 1 function values: functions as arguments, lambdas, captures.
//!
//! froglang has no closure object, no function pointer and no indirect
//! call. `TypeChecker::lower_function_values` is what lets it have
//! first-class functions anyway — it hoists every lambda to a top-level
//! declaration, turns each free name into a by-value parameter, and clones
//! a higher-order callee per function it is passed, leaving only direct
//! calls for codegen.
//!
//! Three things worth knowing before reading:
//!
//!  - **Capture is by value and immutable.** A lambda may only capture
//!    `let` bindings; capturing a `mut` is a type error naming the fix.
//!    That restriction is what makes lifting a capture into a parameter
//!    semantics-preserving without any escape analysis, so it is load-
//!    bearing rather than merely tidy — see `capturing_a_mut_binding_is_rejected`.
//!  - **What is rejected is as much the deliverable as what works.** A
//!    function value whose callee the compiler cannot name has escaped,
//!    and escaping closures are tier 2, deliberately unimplemented. The
//!    rejection tests below pin that boundary so it moves on purpose.
//!  - **Several of these used to crash rather than fail.** A nested `func`
//!    panicked in codegen with `no entry found for key`, and a captured
//!    top-level `let` with `unbound variable in codegen` — both on code
//!    the type checker had already accepted.

mod common;
use common::{run, run_gc_stress, run_raw};
use froglang_core::state::{FrogState, FrogValue};

fn int(v: &FrogValue) -> i64 {
    match v {
        FrogValue::Int(n) => *n,
        other => panic!("expected Int, got {:?}", other),
    }
}

/// The message from a program expected to be rejected. Not `type_error`'s
/// in-process shape (as in `test_json.rs`): these rejections come from a
/// pass that runs *after* `check_and_lower`, so only the driver reaches
/// them — and `main.rs` renders a diagnostic to *stdout*, so both streams
/// are joined rather than assuming which one carries it.
fn rejection(src: &str) -> String {
    let out = run_raw(src);
    assert_ne!(out.status, Some(0), "program was expected to fail:\n{}\n--- stdout ---\n{}", src, out.stdout);
    format!("{}{}", out.stdout, out.stderr)
}

// ── functions as arguments ───────────────────────────────────────────────────

#[test]
fn a_named_function_can_be_passed_as_an_argument() {
    assert_eq!(
        run("func apply(f: (Int -> Int), x: Int): Int = f(x)\n\
             func inc(n: Int): Int = n + 1\n\
             print(apply(inc, 2))"),
        "3\n",
    );
}

#[test]
fn a_lambda_literal_can_be_passed_as_an_argument() {
    assert_eq!(
        run("func apply(f: (Int -> Int), x: Int): Int = f(x)\n\
             print(apply(y -> y * 10, 2))"),
        "20\n",
    );
}

#[test]
fn two_call_sites_with_different_lambdas_get_separate_specializations() {
    // The test that catches a specialization keyed too coarsely: if both
    // call sites collapsed onto one clone, one of these two lines would be
    // wrong — and keying on the callee alone, or on its *type*, does
    // exactly that, since both lambdas are `(Int -> Int)`.
    assert_eq!(
        run("func apply(f: (Int -> Int), x: Int): Int = f(x)\n\
             print(apply(y -> y + 1, 10))\n\
             print(apply(y -> y * 2, 10))"),
        "11\n20\n",
    );
}

#[test]
fn a_function_parameter_can_be_called_more_than_once() {
    assert_eq!(
        run("func twice(f: (Int -> Int), x: Int): Int = f(f(x))\n\
             print(twice(y -> y * 3, 2))"),
        "18\n",
    );
}

#[test]
fn a_function_parameter_can_be_forwarded_to_another_higher_order_function() {
    // Forwarding is what makes specialization a fixed point rather than a
    // single pass: `outer`'s clone is the first place `inner`'s call site
    // knows which function it is passing.
    assert_eq!(
        run("func inner(f: (Int -> Int), x: Int): Int = f(x)\n\
             func outer(g: (Int -> Int), x: Int): Int = inner(g, x) + 1\n\
             print(outer(y -> y * 5, 3))"),
        "16\n",
    );
}

#[test]
fn a_higher_order_function_can_take_two_function_parameters() {
    assert_eq!(
        run("func combine(f: (Int -> Int), g: (Int -> Int), x: Int): Int = f(x) + g(x)\n\
             print(combine(a -> a + 1, b -> b * 100, 3))"),
        "304\n",
    );
}

#[test]
fn a_predicate_parameter_works_in_a_comprehension() {
    // The shape a stdlib `filter` would have, and the reason the pass
    // walks comprehension bodies with the loop variable bound.
    assert_eq!(
        run("func keep(xs: List<Int>, p: (Int -> Bool)): List<Int> = [for x in xs if p(x) do x]\n\
             print(keep([1, 2, 3, 4, 5, 6], n -> n > 3))"),
        "[4, 5, 6]\n",
    );
}

#[test]
fn a_function_value_can_be_given_a_second_name_and_then_passed() {
    // `let g = f` is a compile-time alias (`lower_assign`), which resolves
    // to the same symbol — so it is "statically known" and composes with
    // specialization rather than defeating it.
    assert_eq!(
        run("func apply(f: (Int -> Int), x: Int): Int = f(x)\n\
             func inc(n: Int): Int = n + 1\n\
             let same = inc\n\
             print(apply(same, 41))"),
        "42\n",
    );
}

// ── lambdas and capture ──────────────────────────────────────────────────────

#[test]
fn a_lambda_captures_a_let_binding_by_value() {
    assert_eq!(
        run("let n = 10\nlet f = x -> x + n\nprint(f(1))"),
        "11\n",
    );
}

#[test]
fn a_lambda_passed_as_an_argument_carries_its_captures() {
    // Two transforms compose here: the lambda is hoisted and specialized
    // into `apply`, and the capture is then threaded through the clone —
    // which is why specialization runs *before* capture resolution.
    assert_eq!(
        run("func apply(f: (Int -> Int), x: Int): Int = f(x)\n\
             let n = 100\n\
             print(apply(y -> y + n, 2))"),
        "102\n",
    );
}

#[test]
fn a_lambda_captures_more_than_one_binding() {
    assert_eq!(
        run("let a = 3\nlet b = 4\nlet f = x -> x * a + b\nprint(f(2))"),
        "10\n",
    );
}

#[test]
fn a_capture_is_threaded_through_two_levels_of_call() {
    // The propagation fixed point: `b` never mentions `n`, but it calls
    // `a`, which does, so `b` must receive it too.
    assert_eq!(
        run("let n = 7\n\
             func a(x: Int): Int = x + n\n\
             func b(x: Int): Int = a(x) * 2\n\
             print(b(1))"),
        "16\n",
    );
}

#[test]
fn a_capture_is_threaded_through_three_levels_of_call() {
    assert_eq!(
        run("let n = 1\n\
             func a(x: Int): Int = x + n\n\
             func b(x: Int): Int = a(x)\n\
             func c(x: Int): Int = b(x)\n\
             print(c(10))"),
        "11\n",
    );
}

#[test]
fn a_captured_name_shadowed_inside_the_lambda_is_not_captured() {
    // The rename must touch exactly the *free* occurrences. A scope-blind
    // rewrite would turn the inner `n` into the capture parameter too and
    // print 10 instead of 3.
    assert_eq!(
        run("let n = 10\n\
             let f = x -> {\n  let n = 2\n  x + n\n}\n\
             print(f(1))"),
        "3\n",
    );
}

#[test]
fn a_lambda_parameter_shadowing_an_outer_binding_is_not_a_capture() {
    assert_eq!(
        run("let x = 99\nlet f = x -> x + 1\nprint(f(1))"),
        "2\n",
    );
}

#[test]
fn a_capture_is_read_where_the_lambda_is_created_not_where_it_is_called() {
    // The by-value guarantee, stated as a program. `n` is immutable, so
    // the only way to observe a difference is to shadow it between the
    // declaration and the call — the lambda must still see the first one.
    assert_eq!(
        run("let n = 1\n\
             let f = x -> x + n\n\
             let g = {\n  let n = 1000\n  f(0)\n}\n\
             print(g)"),
        "1\n",
    );
}

// ── nested declarations ──────────────────────────────────────────────────────

#[test]
fn a_nested_func_declaration_compiles() {
    // Used to panic in codegen: `compile_entry`'s Pass 1 only ever
    // declared *top-level* functions, so the call found no `FuncId`.
    assert_eq!(
        run("func outer(n: Int): Int = {\n\
               func inner(x: Int): Int = x + 1\n\
               inner(n)\n\
             }\n\
             print(outer(1))"),
        "2\n",
    );
}

#[test]
fn a_nested_func_captures_its_enclosing_parameter() {
    assert_eq!(
        run("func outer(n: Int): Int = {\n\
               func inner(x: Int): Int = x + n\n\
               inner(10)\n\
             }\n\
             print(outer(5))"),
        "15\n",
    );
}

#[test]
fn two_functions_may_each_nest_a_declaration_of_the_same_name() {
    // Hoisting gives each one a fresh top-level symbol, so the second does
    // not silently replace the first — which is what would happen if the
    // source name were reused.
    assert_eq!(
        run("func a(): Int = {\n  func helper(): Int = 1\n  helper()\n}\n\
             func b(): Int = {\n  func helper(): Int = 2\n  helper()\n}\n\
             print(a())\nprint(b())"),
        "1\n2\n",
    );
}

#[test]
fn a_nested_func_may_recurse() {
    // The hoisted name is registered before the body is walked, so the
    // recursive reference resolves to the hoisted declaration.
    assert_eq!(
        run("func outer(n: Int): Int = {\n\
               func fact(x: Int): Int = if x <= 1 then 1 else x * fact(x - 1)\n\
               fact(n)\n\
             }\n\
             print(outer(5))"),
        "120\n",
    );
}

#[test]
fn a_top_level_func_can_read_a_top_level_let() {
    // Previously a codegen panic (`unbound variable in codegen: n`) — a
    // function body's variable environment is seeded only from its
    // parameters, so the fix is to make the capture one.
    assert_eq!(
        run("let base = 100\nfunc f(x: Int): Int = x + base\nprint(f(1))"),
        "101\n",
    );
}

#[test]
fn a_nested_func_captures_through_two_levels_of_nesting() {
    assert_eq!(
        run("func outer(a: Int): Int = {\n\
               func mid(b: Int): Int = {\n\
                 func inner(c: Int): Int = a + b + c\n\
                 inner(1)\n\
               }\n\
               mid(10)\n\
             }\n\
             print(outer(100))"),
        "111\n",
    );
}

#[test]
fn a_lambda_declared_inside_a_loop_body_is_hoisted_once() {
    assert_eq!(
        run("let n = 3\n\
             for i in 0..3 do {\n\
               let f = x -> x * n\n\
               print(f(i))\n\
             }"),
        "0\n3\n6\n",
    );
}

#[test]
fn a_capture_may_be_a_struct() {
    assert_eq!(
        run("data P(x: Int, y: Int)\n\
             let p = P(x=3, y=4)\n\
             let f = k -> p.x * k + p.y\n\
             print(f(10))"),
        "34\n",
    );
}

#[test]
fn a_capture_survives_recursion() {
    assert_eq!(
        run("let step = 2\n\
             func count(n: Int): Int = if n <= 0 then 0 else step + count(n - step)\n\
             print(count(6))"),
        "6\n",
    );
}

#[test]
fn a_function_parameter_composes_with_a_mut_parameter() {
    // `mut` parameters are copy-in/copy-out, which means the callee's
    // extra return values are positional — so appending capture and
    // dropping function parameters must not disturb them.
    assert_eq!(
        run("func bump(mut xs: List<Int>, f: (Int -> Int)) = { xs[0] = f(xs[0]) }\n\
             mut ys = [1, 2]\n\
             bump(mut ys, x -> x * 7)\n\
             print(ys)"),
        "[7, 2]\n",
    );
}

#[test]
fn a_fallible_function_can_be_passed_as_an_argument() {
    assert_eq!(
        run("error E(msg: Str)\n\
             func tryit(f: (Int -> Int | E), x: Int): Int = f(x) catch 0\n\
             print(tryit(n -> if n > 0 then n else E(msg=\"neg\"), 5))\n\
             print(tryit(n -> if n > 0 then n else E(msg=\"neg\"), -5))"),
        "5\n0\n",
    );
}

#[test]
fn a_lambda_captures_inside_a_match_arm() {
    // Match arms lower to `Conditional` plus `Assign` binds, so the arm's
    // bindings are ordinary block scope as far as the capture walk is
    // concerned — this is the test that says so.
    assert_eq!(
        run("data S is A(v: Int) | B(v: Int)\n\
             let bonus = 100\n\
             func score(s: S): Int = match s {\n\
               is A then { let f = x -> x + bonus\n\
                           f(s.v) }\n\
               is B then s.v\n\
             }\n\
             print(score(S.A(v=1)))"),
        "101\n",
    );
}

#[test]
fn two_textually_identical_lambdas_are_still_two_lambdas() {
    // Hoisting keys on the occurrence, not the text, so nothing here
    // depends on structural equality of function bodies.
    assert_eq!(
        run("func ap(f: (Int -> Int), x: Int): Int = f(x)\n\
             print(ap(x -> x + 1, 10))\n\
             print(ap(x -> x + 1, 20))"),
        "11\n21\n",
    );
}

// ── captured heap values ─────────────────────────────────────────────────────

#[test]
fn a_captured_list_is_rooted() {
    // A capture becomes an ordinary parameter, and parameters are already
    // GC roots — so this should need nothing new. Under stress it either
    // proves that or exposes the assumption.
    assert_eq!(
        run_gc_stress("let xs = [1, 2, 3]\n\
                       let f = i -> xs[i] * 10\n\
                       print(f(0) + f(1) + f(2))"),
        "60\n",
    );
}

#[test]
fn a_captured_str_survives_collection_in_a_loop() {
    assert_eq!(
        run_gc_stress("let prefix = \"item-\"\n\
                       func label(n: Int): Str = prefix + int_to_str(n)\n\
                       for i in 0..3 do print(label(i))"),
        "item-0\nitem-1\nitem-2\n",
    );
}

#[test]
fn a_captured_list_keeps_value_semantics() {
    // Capture is a read of the binding like any other, so the existing
    // copy-on-write share-mark applies and the original is untouched.
    assert_eq!(
        run("let xs = [1, 2, 3]\n\
             func total(): Int = {\n\
               mut ys = xs\n\
               ys[0] = 99\n\
               ys[0]\n\
             }\n\
             print(total())\nprint(xs[0])"),
        "99\n1\n",
    );
}

// ── generics and higher-order together ───────────────────────────────────────

#[test]
fn a_generic_higher_order_function_works_at_two_element_types() {
    // Monomorphization runs first and splits on the *type*; specialization
    // then splits on the *function*. Both lambdas below are `(Int -> Int)`
    // after the first split, which is exactly the case the second exists
    // to separate.
    assert_eq!(
        run("func mapped<A, B>(xs: List<A>, f: (A -> B)): List<B> = [for x in xs do f(x)]\n\
             print(mapped([1, 2, 3], n -> n * 2))\n\
             print(mapped([\"a\", \"b\"], s -> s.to_upper()))"),
        "[2, 4, 6]\n[\"A\", \"B\"]\n",
    );
}

#[test]
fn a_generic_higher_order_function_is_specialized_per_lambda() {
    assert_eq!(
        run("func mapped<A, B>(xs: List<A>, f: (A -> B)): List<B> = [for x in xs do f(x)]\n\
             print(mapped([1, 2], n -> n + 1))\n\
             print(mapped([1, 2], n -> n * 100))"),
        "[2, 3]\n[100, 200]\n",
    );
}

// ── the by-value capture rule ────────────────────────────────────────────────

#[test]
fn capturing_a_mut_binding_is_rejected() {
    let err = rejection("mut total = 0\nlet f = x -> x + total\nprint(f(1))");
    assert!(
        err.contains("closures capture by value") && err.contains("copy it into a `let` first"),
        "expected the by-value capture rule, got:\n{}",
        err,
    );
}

#[test]
fn a_mut_binding_copied_into_a_let_can_be_captured() {
    // The fix the error message names, as a working program — a rejection
    // whose suggested remedy doesn't compile is worse than no suggestion.
    assert_eq!(
        run("mut total = 5\ntotal = total + 1\n\
             let snapshot = total\n\
             let f = x -> x + snapshot\n\
             print(f(1))"),
        "7\n",
    );
}

#[test]
fn a_let_capture_is_not_poisoned_by_an_unrelated_mut_of_the_same_name() {
    // `mut_names` records every name ever declared `mut`, name-keyed and
    // unscoped — so without a scope check, `bump`'s function-local `mut
    // total` would make the top-level `let total` (captured by `f` below)
    // look mutable too, and reject a legal capture with a message naming a
    // binding that already is a `let`.
    assert_eq!(
        run("func bump(): Int = { mut total = 0\n\
             total = total + 1\n\
             total }\n\
             let total = 5\n\
             let f = x -> x + total\n\
             print(f(1))"),
        "6\n",
    );
}

// ── the tier-2 boundary ──────────────────────────────────────────────────────

#[test]
fn returning_a_function_is_rejected() {
    let err = rejection("func mk(): (Int -> Int) = x -> x + 1\nprint(mk()(2))");
    assert!(
        err.contains("can see which one is called"),
        "expected the escaping-function rejection, got:\n{}",
        err,
    );
}

#[test]
fn storing_a_function_in_a_list_is_rejected() {
    let err = rejection("let fs = [x -> x + 1]\nprint(fs[0](2))");
    assert!(
        err.contains("can see which one is called"),
        "expected the escaping-function rejection, got:\n{}",
        err,
    );
}

#[test]
fn storing_a_function_in_a_struct_field_is_rejected() {
    let err = rejection(
        "data Holder(f: (Int -> Int))\n\
         let h = Holder(f=x -> x + 1)\n\
         print(1)",
    );
    assert!(
        err.contains("can see which one is called"),
        "expected the escaping-function rejection, got:\n{}",
        err,
    );
}

#[test]
fn a_mut_binding_holding_a_function_is_rejected() {
    // Deliberately excluded from `lower_assign`'s alias path: reassigning
    // it would have to change what a call site resolves to at runtime,
    // which is the indirect call froglang does not have.
    let err = rejection("func inc(n: Int): Int = n + 1\nmut f = inc\nprint(f(1))");
    assert!(!err.is_empty(), "expected a rejection, got empty stderr");
}

#[test]
fn a_higher_order_function_that_is_never_called_is_rejected_not_miscompiled() {
    // Nothing specializes it, so its function-typed parameter would reach
    // `make_sig`, which has no Cranelift type for one. The declaration is
    // stripped and the *use* is what reports — here there is no use, so
    // the program is simply the print.
    assert_eq!(
        run("func apply(f: (Int -> Int), x: Int): Int = f(x)\nprint(1)"),
        "1\n",
    );
}

// ── across REPL entries ──────────────────────────────────────────────────────

#[test]
fn a_capturing_function_declared_in_one_entry_is_callable_from_a_later_one() {
    // The lifted arity has to survive the entry boundary: the entry that
    // calls `f` never sees the tree in which `f` gained its capture
    // parameter, so `lifted_captures` is what tells it to pass `base`.
    let mut state = FrogState::with_stdlib().expect("stdlib");
    state.eval("let base = 40").expect("entry 1");
    state.eval("func f(x: Int): Int = x + base").expect("entry 2");
    assert_eq!(int(&state.eval("f(2)").expect("entry 3").0), 42);
}

#[test]
fn a_higher_order_function_declared_in_one_entry_is_specialized_in_a_later_one() {
    let mut state = FrogState::with_stdlib().expect("stdlib");
    state.eval("func apply(f: (Int -> Int), x: Int): Int = f(x)").expect("entry 1");
    state.eval("func inc(n: Int): Int = n + 1").expect("entry 2");
    assert_eq!(int(&state.eval("apply(inc, 41)").expect("entry 3").0), 42);
}

#[test]
fn the_same_specialization_reached_from_two_entries_reuses_the_first_ones_body() {
    // The second entry finds `apply$$inc` already compiled and builds
    // nothing — but its own call site still has to be pointed at it. A
    // fixed point that only rewrote when it had just emitted something
    // left this call naming a template that had been stripped, and codegen
    // died looking for it.
    let mut state = FrogState::with_stdlib().expect("stdlib");
    state.eval("func apply(f: (Int -> Int), x: Int): Int = f(x)\nfunc inc(n: Int): Int = n + 1").expect("entry 1");
    assert_eq!(int(&state.eval("apply(inc, 1)").expect("entry 2").0), 2);
    assert_eq!(int(&state.eval("apply(inc, 10)").expect("entry 3").0), 11);
}

#[test]
fn redeclaring_a_substituted_function_rebuilds_its_specialization() {
    // `emitted_specializations` is keyed by the mangled name
    // (`apply$$inc`), which is built from `inc`'s source name alone — so
    // without eviction on redeclaration, this would keep answering with
    // the first `inc`'s clone forever.
    let mut state = FrogState::with_stdlib().expect("stdlib");
    state.eval("func apply(f: (Int -> Int), x: Int): Int = f(x)\nfunc inc(n: Int): Int = n + 1").expect("entry 1");
    assert_eq!(int(&state.eval("apply(inc, 1)").expect("entry 2").0), 2);
    state.eval("func inc(n: Int): Int = n + 100").expect("entry 3");
    assert_eq!(int(&state.eval("apply(inc, 1)").expect("entry 4").0), 101);
}

#[test]
fn redeclaring_the_callee_rebuilds_its_specialization() {
    // Same hazard, the other side: `apply` itself is redeclared with a
    // different body, so `apply$$inc` must be rebuilt against the new
    // template rather than reusing the first `apply`'s clone.
    let mut state = FrogState::with_stdlib().expect("stdlib");
    state.eval("func apply(f: (Int -> Int), x: Int): Int = f(x)\nfunc inc(n: Int): Int = n + 1").expect("entry 1");
    assert_eq!(int(&state.eval("apply(inc, 1)").expect("entry 2").0), 2);
    state.eval("func apply(f: (Int -> Int), x: Int): Int = f(f(x))").expect("entry 3");
    assert_eq!(int(&state.eval("apply(inc, 1)").expect("entry 4").0), 3);
}

#[test]
fn redeclaring_a_higher_order_function_without_function_parameters_works() {
    // `fn_templates` is keyed by name and persists, and an
    // un-substituted higher-order declaration is stripped before codegen
    // — so a stale template would strip this perfectly ordinary second
    // `apply` too, and the call would find nothing.
    let mut state = FrogState::with_stdlib().expect("stdlib");
    state.eval("func apply(f: (Int -> Int), x: Int): Int = f(x)").expect("entry 1");
    state.eval("func apply(x: Int): Int = x * 3").expect("entry 2");
    assert_eq!(int(&state.eval("apply(4)").expect("entry 3").0), 12);
}

#[test]
fn redeclaring_a_capturing_function_without_captures_works() {
    // The same hazard on `lifted_captures`: the second `f` takes no
    // capture parameter, so a stale entry would have the call site pass a
    // phantom argument.
    let mut state = FrogState::with_stdlib().expect("stdlib");
    state.eval("let n = 100").expect("entry 1");
    state.eval("func f(x: Int): Int = x + n").expect("entry 2");
    assert_eq!(int(&state.eval("f(1)").expect("entry 3").0), 101);
    state.eval("func f(x: Int): Int = x * 2").expect("entry 4");
    assert_eq!(int(&state.eval("f(21)").expect("entry 5").0), 42);
}

#[test]
fn a_recursive_higher_order_function_specializes() {
    // The clone's own body calls the template recursively, so the
    // rewrite has to reach inside what the same round just emitted.
    assert_eq!(
        run("func sum_upto(f: (Int -> Int), n: Int): Int =\n\
               if n <= 0 then 0 else f(n) + sum_upto(f, n - 1)\n\
             print(sum_upto(x -> x * 2, 4))"),
        "20\n",
    );
}
