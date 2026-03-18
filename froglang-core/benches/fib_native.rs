// Naive recursive fib(35) — native Rust, optimised build.
//
// Compile & run:
//   rustc -C opt-level=3 fib_native.rs -o /tmp/fib_native && /tmp/fib_native [n]
//
// std::hint::black_box wraps the input so LLVM cannot see the concrete value
// at compile time and constant-fold the entire call tree away.  Without it,
// -C opt-level=3 would reduce fib(35) to a single `mov eax, 9227465`.

use std::hint::black_box;
use std::time::Instant;

fn fib(n: u64) -> u64 {
    if n <= 1 { n } else { fib(n - 1) + fib(n - 2) }
}

fn main() {
    let n: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(35);

    // black_box prevents LLVM from seeing the value of n at compile time.
    let n = black_box(n);

    let t0 = Instant::now();
    let result = fib(n);
    let elapsed = t0.elapsed();

    println!("{}", result);
    eprintln!("({:?})", elapsed);
}
