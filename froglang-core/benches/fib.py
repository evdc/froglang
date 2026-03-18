#!/usr/bin/env python3
"""Naive recursive fib(35) — CPython baseline."""
import sys
import time

def fib(n):
    if n <= 1:
        return n
    return fib(n - 1) + fib(n - 2)

# Read n at runtime so the interpreter can't short-circuit evaluation.
n = int(sys.argv[1]) if len(sys.argv) > 1 else 35

t0 = time.perf_counter()
result = fib(n)
elapsed_ms = (time.perf_counter() - t0) * 1000

print(result)
print(f"({elapsed_ms:.1f}ms)", file=sys.stderr)
