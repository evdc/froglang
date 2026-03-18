use std::hint::black_box;
use criterion::{criterion_group, criterion_main, Criterion};
use froglang_core::codegen::Codegen;
use froglang_core::frontend::parser::Parser;
use froglang_core::frontend::typeck::TypeChecker;

const FIB_SRC: &str =
    "func fib(n: Int): Int = if n <= 1 then n else fib(n - 1) + fib(n - 2)\nfib(35)";

fn compile_fib() -> fn() -> i64 {
    let ast    = Parser::parse(FIB_SRC).expect("parse error");
    let mut tc = TypeChecker::new();
    let typed  = tc.check_and_lower(ast).expect("type error");
    let mut cg = Codegen::new();
    let main_id = cg.compile(typed);
    let ptr = cg.module.get_finalized_function(main_id);
    // Leak cg so the JIT memory stays alive for the lifetime of the bench.
    std::mem::forget(cg);
    unsafe { std::mem::transmute(ptr) }
}

fn bench_fib(c: &mut Criterion) {
    let fib_fn = compile_fib();
    c.bench_function("fib(35) jit", |b| b.iter(|| black_box(fib_fn())));
}

criterion_group!(benches, bench_fib);
criterion_main!(benches);
