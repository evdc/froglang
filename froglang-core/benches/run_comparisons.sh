#!/usr/bin/env bash
# Run froglang's benchmarks against the same program written in other
# languages, and report wall-clock time.  Run from the froglang-core/ directory:
#
#   ./benches/run_comparisons.sh            # every benchmark
#   ./benches/run_comparisons.sh fib        # naive recursive fib(35)
#   ./benches/run_comparisons.sh orders     # struct/enum/list order pipeline
#
# Languages checked: froglang (Cranelift JIT), Rust (-O3), Go (gc), Python 3,
# LuaJIT, Lua.  Missing runtimes/compilers are skipped with a note.
#
# Two timings are printed per row.  `wall` is the whole process, measured by
# this script, and is the only number available for froglang; the parenthesised
# number after it is the program's own measurement of just the benchmarked
# region, where the program reports one.  Compare wall to wall for the fairest
# picture — froglang's wall time includes JIT compilation, which is the honest
# cost of running a froglang program.

BENCH_DIR="$(cd "$(dirname "$0")" && pwd)"
FIB_N=35
TMPDIR_LOCAL=$(mktemp -d)
trap 'rm -rf "$TMPDIR_LOCAL"' EXIT

have() { command -v "$1" &>/dev/null; }

# Current time in fractional seconds.  bash 5 has $EPOCHREALTIME built in;
# macOS still ships bash 3.2, and its `date` has no %N, so fall back to perl.
if [[ -n "${EPOCHREALTIME:-}" ]]; then
    now() { echo "$EPOCHREALTIME"; }
elif have perl; then
    now() { perl -MTime::HiRes=time -e 'printf "%.6f", time'; }
else
    now() { python3 -c 'import time; print(time.time())'; }
fi

# The two loop bounds live in orders.frog and are read back out of it here, so
# every implementation runs the same problem size from a single source of truth.
ORDERS_FROG="$BENCH_DIR/orders.frog"
ORDERS_ITEMS=$(sed -n 's/.*for i in 0\.\.\([0-9]*\) do Item.*/\1/p' "$ORDERS_FROG")
ORDERS_ROUNDS=$(sed -n 's/.*for round in 0\.\.\([0-9]*\) do.*/\1/p' "$ORDERS_FROG")

# Run one command, printing result, wall time, and whatever the program wrote
# to stderr (its own timing, for those that report one).
run_timed() {
    local label="$1"; shift
    local stderr_file="$TMPDIR_LOCAL/stderr.txt"
    local start end wall result

    # Discard one run first.  A freshly built, unsigned binary pays a
    # one-off ~250ms on macOS for signature verification, which would
    # otherwise swamp the 20ms benchmarks; this also warms the page cache
    # so every language is measured in the same (warm) state.
    "$@" >/dev/null 2>&1

    start=$(now)
    if result=$("$@" 2>"$stderr_file"); then
        end=$(now)
        wall=$(awk -v a="$start" -v b="$end" 'BEGIN { printf "%.0fms", (b - a) * 1000 }')
        printf "  %-26s  result=%-14s  wall=%-9s %s\n" \
            "$label" "$result" "$wall" "$(tr -d '\n' <"$stderr_file")"
    else
        printf "  %-26s  FAILED (exit %d)\n" "$label" "$?"
    fi
}

# ── build phase ────────────────────────────────────────────────────────────────
echo "=== Building compiled benchmarks ==="

# build_rust <source> -> echoes binary path, or nothing if unavailable
build_rust() {
    local src="$1" out="$TMPDIR_LOCAL/$(basename "$1" .rs)"
    have rustc || return
    rustc -C opt-level=3 "$src" -o "$out" 2>/dev/null && echo "$out"
}

build_go() {
    local src="$1" out="$TMPDIR_LOCAL/$(basename "$1" .go)_go"
    have go || return
    go build -o "$out" "$src" 2>/dev/null && echo "$out"
}

if ! have rustc; then echo "  rustc not found — skipping Rust"; else echo "  rustc -C opt-level=3 ..."; fi
if ! have go;    then echo "  go not found — skipping Go";       else echo "  go build ...";            fi

FIB_RUST=$(build_rust "$BENCH_DIR/fib_native.rs")
FIB_GO=$(build_go "$BENCH_DIR/fib.go")
ORDERS_RUST=$(build_rust "$BENCH_DIR/orders_native.rs")
ORDERS_GO=$(build_go "$BENCH_DIR/orders.go")

echo "  cargo build --release ..."
FROG_BIN=""
# Cargo workspaces place the binary in the workspace root's target/, not the
# crate's own directory.  `cargo build --message-format json` would give the
# exact path, but parsing that is overkill; just probe both locations.
#
# Anchored on $BENCH_DIR (this script's own directory) rather than $(pwd):
# probing relative to the working directory only found the binary when the
# script happened to be invoked from the crate or workspace root, and
# printed "could not locate froglang-core binary" — silently dropping the
# one row the table exists for — when it was run from benches/ itself.
if cargo build --release --quiet 2>/dev/null; then
    for candidate in \
        "$BENCH_DIR/../target/release/froglang-core" \
        "$BENCH_DIR/../../target/release/froglang-core"
    do
        if [[ -x "$candidate" ]]; then
            FROG_BIN="$candidate"
            break
        fi
    done
    [[ -z "$FROG_BIN" ]] && echo "  warning: could not locate froglang-core binary"
else
    echo "  cargo build failed"
fi

# ── fib ────────────────────────────────────────────────────────────────────────
bench_fib() {
    # Write the froglang source to a temp file (avoids shell quoting issues
    # with embedded newlines when passing source inline on the command line).
    local fib_frog="$TMPDIR_LOCAL/fib.frog"
    cat > "$fib_frog" <<EOF
func fib(n: Int): Int = if n <= 1 then n else fib(n - 1) + fib(n - 2)
fib($FIB_N)
EOF

    echo ""
    echo "=== fib($FIB_N) — naive recursion, no allocation ==="
    echo ""

    if [[ -n "$FROG_BIN" ]]; then
        run_timed "froglang (Cranelift JIT)" "$FROG_BIN" run "$fib_frog"
    else
        printf "  %-26s  (build failed)\n" "froglang (Cranelift JIT)"
    fi
    [[ -n "$FIB_RUST" ]] && run_timed "Rust -O3"         "$FIB_RUST" "$FIB_N"
    [[ -n "$FIB_GO"   ]] && run_timed "Go (gc, default)" "$FIB_GO"   "$FIB_N"
    have python3 && run_timed "Python 3"    python3 "$BENCH_DIR/fib.py"  "$FIB_N"
    have luajit  && run_timed "LuaJIT"      luajit  "$BENCH_DIR/fib.lua" "$FIB_N"
    have lua     && run_timed "Lua (plain)" lua     "$BENCH_DIR/fib.lua" "$FIB_N"
}

# ── orders ─────────────────────────────────────────────────────────────────────
bench_orders() {
    local n="$ORDERS_ITEMS" r="$ORDERS_ROUNDS"

    echo ""
    echo "=== orders — $n structs x $r rounds: structs, enums + match, lists ==="
    echo ""

    if [[ -n "$FROG_BIN" ]]; then
        run_timed "froglang (Cranelift JIT)" "$FROG_BIN" run "$ORDERS_FROG"
    else
        printf "  %-26s  (build failed)\n" "froglang (Cranelift JIT)"
    fi
    [[ -n "$ORDERS_RUST" ]] && run_timed "Rust -O3"         "$ORDERS_RUST" "$n" "$r"
    [[ -n "$ORDERS_GO"   ]] && run_timed "Go (gc, default)" "$ORDERS_GO"   "$n" "$r"
    have python3 && run_timed "Python 3"    python3 "$BENCH_DIR/orders.py"  "$n" "$r"
    have luajit  && run_timed "LuaJIT"      luajit  "$BENCH_DIR/orders.lua" "$n" "$r"
    have lua     && run_timed "Lua (plain)" lua     "$BENCH_DIR/orders.lua" "$n" "$r"

    echo ""
    echo "  All implementations must print the same result; a differing row is a bug."
}

case "${1:-all}" in
    fib)    bench_fib ;;
    orders) bench_orders ;;
    all)    bench_fib; bench_orders ;;
    *)      echo "usage: $0 [fib|orders|all]" >&2; exit 2 ;;
esac

echo ""
echo "Note: froglang's wall time includes JIT compilation of the whole program."
