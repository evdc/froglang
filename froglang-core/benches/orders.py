#!/usr/bin/env python3
"""Order-pipeline benchmark — CPython baseline.

Mirrors benches/orders.frog exactly; see that file for what it measures.
Items are __slots__ classes (the closest cheap analogue of a froglang struct)
and discounts are small tuples tagged by an int, which is what idiomatic
Python reaches for in the absence of sum types.
"""
import sys
import time

FOOD, BOOK, ELECTRONICS, TOY = 0, 1, 2, 3
NO_DISCOUNT, PERCENT, FLAT, BULK_OVER = 0, 1, 2, 3


class Item:
    __slots__ = ("sku", "category", "qty", "unit_price")

    def __init__(self, sku, category, qty, unit_price):
        self.sku = sku
        self.category = category
        self.qty = qty
        self.unit_price = unit_price


def hash_(i):
    return (i * 2654435761 + 1013904223) % 2147483647


def category_of(h):
    return h % 4


def discount_for(it):
    c = it.category
    if c == FOOD:
        return (BULK_OVER, 6, 5) if it.qty >= 6 else (NO_DISCOUNT, 0, 0)
    if c == BOOK:
        return (PERCENT, 10, 0)
    if c == ELECTRONICS:
        return (FLAT, 250, 0) if it.unit_price > 3000 else (PERCENT, 3, 0)
    return (BULK_OVER, 3, 15)


def apply(d, gross, qty):
    kind, a, pct = d
    if kind == NO_DISCOUNT:
        return gross
    if kind == PERCENT:
        return gross - gross * a // 100
    if kind == FLAT:
        return gross - a if gross > a else 0
    return gross - gross * pct // 100 if qty >= a else gross


def main():
    items_n = int(sys.argv[1]) if len(sys.argv) > 1 else 2000
    rounds = int(sys.argv[2]) if len(sys.argv) > 2 else 2000

    t0 = time.perf_counter()

    items = [
        Item(i, category_of(hash_(i)), 1 + hash_(i + 7) % 9, 100 + hash_(i + 13) % 5000)
        for i in range(items_n)
    ]

    total = 0
    for rnd in range(rounds):
        batch = [it for it in items if (it.sku + rnd) % 3 != 0]
        for it in batch:
            total += apply(discount_for(it), it.qty * it.unit_price, it.qty)

    elapsed_ms = (time.perf_counter() - t0) * 1000
    print(total)
    print(f"({elapsed_ms:.1f}ms)", file=sys.stderr)


main()
