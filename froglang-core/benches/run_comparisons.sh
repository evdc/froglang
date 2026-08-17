#!/usr/bin/env bash
# Run fib(35) across multiple languages and report wall-clock time.
# Must be run from the froglang-core/ directory:
#
#   ./benches/run_comparisons.sh
#
# Languages checked: froglang (Cranelift JIT), Python 3, Rust (-O3),
#                    Go (gc), Lua, LuaJIT
# Missing runtimes/compilers are skipped with a note.

BENCH_DIR="$(cd "$(dirname "$0")" && pwd)"
N=35
TMPDIR_LOCAL=$(mktemp -d)
trap 'rm -rf "$TMPDIR_LOCAL"' EXIT

have() { command -v "$1" &>/dev/null; }

# Print a summary row: label | result | timing.
# The timing string is expected on stderr from the command.
run_timed() {
    local label="$1"; shift
    local stderr_file="$TMPDIR_LOCAL/stderr_$$.txt"
    local result timing

    if result=$("$@" 2>"$stderr_file"); then
        timing=$(cat "$stderr_file")
        printf "  %-26s  result=%-12s  %s\n" "$label" "$result" "$timing"
    else
        printf "  %-26s  FAILED (exit %d)\n" "$label" "$?"
    fi
}

# ── build phase ────────────────────────────────────────────────────────────────
echo "=== Building compiled benchmarks ==="

RUST_BIN=""
if have rustc; then
    echo "  rustc -C opt-level=3 ..."
    if rustc -C opt-level=3 "$BENCH_DIR/fib_native.rs" \
             -o "$TMPDIR_LOCAL/fib_native" 2>/dev/null; then
        RUST_BIN="$TMPDIR_LOCAL/fib_native"
    else
        echo "  rustc build failed"
    fi
else
    echo "  rustc not found — skipping"
fi

GO_BIN=""
if have go; then
    echo "  go build ..."
    if go build -o "$TMPDIR_LOCAL/fib_go" "$BENCH_DIR/fib.go" 2>/dev/null; then
        GO_BIN="$TMPDIR_LOCAL/fib_go"
    else
        echo "  go build failed"
    fi
else
    echo "  go not found — skipping"
fi

echo "  cargo build --release ..."
FROG_BIN=""
# Cargo workspaces place the binary in the workspace root's target/, not the
# crate's own directory.  `cargo build --message-format json` would give the
# exact path, but parsing that is overkill; just probe both locations.
if cargo build --release --quiet 2>/dev/null; then
    for candidate in \
        "$(pwd)/target/release/froglang-core" \
        "$(pwd)/../target/release/froglang-core"
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

# Write the froglang source to a temp file (avoids shell quoting issues
# with embedded newlines when passing source inline on the command line).
FIB_FROG="$TMPDIR_LOCAL/fib.frog"
cat > "$FIB_FROG" <<'EOF'
func fib(n: Int): Int = if n <= 1 then n else fib(n - 1) + fib(n - 2)
fib(35)
EOF

echo ""
echo "=== fib($N) wall-clock time ==="
echo ""

# ── froglang JIT ───────────────────────────────────────────────────────────────
if [[ -n "$FROG_BIN" ]]; then
    run_timed "froglang (Cranelift JIT)" "$FROG_BIN" run "$FIB_FROG"
else
    printf "  %-26s  (build failed)\n" "froglang (Cranelift JIT)"
fi

# ── Rust ───────────────────────────────────────────────────────────────────────
if [[ -n "$RUST_BIN" ]]; then
    run_timed "Rust -O3"           "$RUST_BIN"     "$N"
fi

# ── Go ─────────────────────────────────────────────────────────────────────────
if [[ -n "$GO_BIN" ]]; then
    run_timed "Go (gc, default)"   "$GO_BIN"       "$N"
fi

# ── Python ─────────────────────────────────────────────────────────────────────
if have python3; then
    run_timed "Python 3"           python3 "$BENCH_DIR/fib.py"  "$N"
fi

# ── LuaJIT ─────────────────────────────────────────────────────────────────────
if have luajit; then
    run_timed "LuaJIT"             luajit  "$BENCH_DIR/fib.lua" "$N"
fi

# ── Lua (plain) ────────────────────────────────────────────────────────────────
if have lua; then
    run_timed "Lua (plain)"        lua     "$BENCH_DIR/fib.lua" "$N"
fi

# ── note if almost nothing ran ─────────────────────────────────────────────────
echo ""
echo "Note: froglang timing includes JIT compile time (~1ms) and execution."
