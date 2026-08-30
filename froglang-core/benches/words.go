// Word-pipeline benchmark — the Go sibling of benches/words.frog.
// Must print the same checksum.  Usage: words <words_per_doc> <rounds>
package main

import (
	"fmt"
	"os"
	"strconv"
	"strings"
	"time"
)

func modn(x, n int64) int64 { return x - (x/n)*n }

func hash(i int64) int64 { return modn(i*2654435761+1013904223, 2147483647) }

var vocab = [8]string{"frog", "frogs", "toad", "newt", "salamander", "axolotl", "tadpole", "pond"}

func wordFor(i int64) string { return vocab[modn(hash(i), 8)] }

func buildDoc(wordsPerDoc, round int64) string {
	base := round * wordsPerDoc
	ws := make([]string, wordsPerDoc)
	for i := int64(0); i < wordsPerDoc; i++ {
		ws[i] = wordFor(base + i)
	}
	return strings.Join(ws, " ")
}

func score(w string) int64 {
	base := int64(len(w))
	var bonus, exact int64
	if strings.HasPrefix(w, "frog") {
		bonus = 10
	}
	if w == "frog" {
		exact = 5
	}
	return base + bonus + exact
}

func main() {
	wordsPerDoc, rounds := int64(400), int64(800)
	if len(os.Args) > 1 {
		if v, err := strconv.ParseInt(os.Args[1], 10, 64); err == nil {
			wordsPerDoc = v
		}
	}
	if len(os.Args) > 2 {
		if v, err := strconv.ParseInt(os.Args[2], 10, 64); err == nil {
			rounds = v
		}
	}

	start := time.Now()
	var checksum int64
	for round := int64(0); round < rounds; round++ {
		doc := buildDoc(wordsPerDoc, round)
		var total int64
		for _, w := range strings.Split(doc, " ") {
			total += score(w)
		}
		shouted := strings.ToUpper(doc)
		var found int64
		if strings.Contains(shouted, "SALAMANDER") {
			found = 1
		}
		checksum += total + int64(len(doc)) + found
	}
	elapsed := time.Since(start)

	fmt.Println(checksum)
	fmt.Fprintln(os.Stderr, elapsed)
}
