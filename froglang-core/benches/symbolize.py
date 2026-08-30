#!/usr/bin/env python3
"""Attribute a sampling profile's JIT addresses to froglang functions.

A profiler sees JIT-compiled code as bare addresses in an anonymous
mapping, so profiling a froglang program lands essentially all of its time
in `???  (in <unknown binary>)`. Running the program with
`FROG_JIT_SYMBOLS=<file>` makes codegen write out where each compiled
function starts (`Codegen::dump_jit_symbols`); this joins the two.

Usage:
    FROG_JIT_SYMBOLS=syms.txt target/release/froglang-core run prog.frog &
    sample $! 3 1 -file prof.txt
    ./benches/symbolize.py syms.txt prof.txt

Addresses differ between runs (the JIT mmaps wherever it likes), so the
symbol file and the profile must come from the *same* process. Symbol
files are appended to, so delete a stale one first.

Reports self time — the leaf of each sampled stack — since that is what
says where the work is, and the frog-level call graph is inlined into
these frames anyway.
"""
import re
import sys
from bisect import bisect_right


def load_symbols(path):
    """[(start, name)] sorted by start. Each symbol runs to the next one."""
    syms = []
    for line in open(path):
        parts = line.split(None, 2)
        if len(parts) == 3:
            syms.append((int(parts[0], 16), parts[2].strip()))
    syms.sort()
    return syms


def resolve(syms, addr):
    i = bisect_right(syms, (addr, chr(0x10FFFF))) - 1
    if i < 0:
        return None
    start, name = syms[i]
    # A JIT page is 16KB-aligned per function group; anything much beyond
    # the last known symbol is some other mapping, not this function.
    if addr - start > (1 << 20):
        return None
    return f"{name}+{addr - start:#x}" if addr != start else name


# `sample` output: leading count, then the frame. Deeper frames are indented
# more, so the leaf of each stack is a line whose successor is not deeper.
LINE = re.compile(r"^(?P<indent>[\s+!:|]*)(?P<count>\d+) (?P<what>.*?)\s*\[(?P<addr>0x[0-9a-f]+)\]\s*$")


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    syms = load_symbols(sys.argv[1])
    if not syms:
        sys.exit(f"no symbols in {sys.argv[1]}")

    frames = []
    for line in open(sys.argv[2], errors="replace"):
        m = LINE.match(line.rstrip("\n"))
        if m:
            frames.append((len(m.group("indent")), int(m.group("count")),
                           m.group("what"), int(m.group("addr"), 16)))

    # A frame is a leaf when the next frame is not more deeply indented.
    self_time = {}
    total = 0
    for i, (depth, count, what, addr) in enumerate(frames):
        deeper = i + 1 < len(frames) and frames[i + 1][0] > depth
        if deeper:
            continue
        name = resolve(syms, addr) if "unknown binary" in what else what
        name = (name or what).split("+")[0].strip()
        self_time[name] = self_time.get(name, 0) + count
        total += count

    if not total:
        sys.exit("no leaf samples found — is this a `sample` text profile?")
    print(f"{'self%':>7}  {'samples':>7}  symbol")
    for name, count in sorted(self_time.items(), key=lambda kv: -kv[1])[:30]:
        print(f"{100 * count / total:6.1f}%  {count:7d}  {name}")
    print(f"\n{total} leaf samples total")


if __name__ == "__main__":
    main()
