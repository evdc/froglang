#!/usr/bin/env python3
"""Conway's Game of Life — the Python sibling of benches/life.frog.

Must print the same checksum.  Usage: life.py <size> <generations>
"""
import sys
import time


def modn(x, n):
    return x - (x // n) * n


def hash_(i):
    return modn(i * 2654435761 + 1013904223, 2147483647)


def seed(size):
    return [[1 if modn(hash_(y * size + x), 3) == 0 else 0
             for x in range(size)] for y in range(size)]


def cell_at(rows, y, x):
    size = len(rows)
    if 0 <= y < size and 0 <= x < size:
        return rows[y][x]
    return 0


def neighbours(rows, y, x):
    n = 0
    for dy in (-1, 0, 1):
        for dx in (-1, 0, 1):
            if dx == 0 and dy == 0:
                continue
            n += cell_at(rows, y + dy, x + dx)
    return n


def next_cell(rows, y, x):
    alive = rows[y][x]
    n = neighbours(rows, y, x)
    if alive == 1:
        return 1 if n == 2 or n == 3 else 0
    return 1 if n == 3 else 0


def step(rows):
    size = len(rows)
    return [[next_cell(rows, y, x) for x in range(size)] for y in range(size)]


def population(rows):
    return sum(sum(row) for row in rows)


def main():
    size = int(sys.argv[1]) if len(sys.argv) > 1 else 48
    generations = int(sys.argv[2]) if len(sys.argv) > 2 else 40

    start = time.perf_counter()
    grid = seed(size)
    checksum = 0
    for _ in range(generations):
        checksum += population(grid)
        grid = step(grid)
    elapsed = (time.perf_counter() - start) * 1000

    print(checksum)
    print(f"{elapsed:.1f}ms", file=sys.stderr)


main()
