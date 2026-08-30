#!/usr/bin/env python3
"""Word-pipeline benchmark — the Python sibling of benches/words.frog.

Must print the same checksum.  Usage: words.py <words_per_doc> <rounds>
"""
import sys
import time

VOCAB = ["frog", "frogs", "toad", "newt", "salamander", "axolotl", "tadpole", "pond"]


def modn(x, n):
    return x - (x // n) * n


def hash_(i):
    return modn(i * 2654435761 + 1013904223, 2147483647)


def word_for(i):
    return VOCAB[modn(hash_(i), 8)]


def build_doc(words_per_doc, round_):
    base = round_ * words_per_doc
    return " ".join(word_for(base + i) for i in range(words_per_doc))


def score(w):
    base = len(w)
    bonus = 10 if w.startswith("frog") else 0
    exact = 5 if w == "frog" else 0
    return base + bonus + exact


def main():
    words_per_doc = int(sys.argv[1]) if len(sys.argv) > 1 else 400
    rounds = int(sys.argv[2]) if len(sys.argv) > 2 else 800

    start = time.perf_counter()
    checksum = 0
    for round_ in range(rounds):
        doc = build_doc(words_per_doc, round_)
        total = sum(score(w) for w in doc.split(" "))
        shouted = doc.upper()
        found = 1 if "SALAMANDER" in shouted else 0
        checksum += total + len(doc) + found
    elapsed = (time.perf_counter() - start) * 1000

    print(checksum)
    print(f"{elapsed:.1f}ms", file=sys.stderr)


main()
