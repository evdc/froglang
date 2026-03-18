// Naive recursive fib(35).
// Compile & run:  go run fib.go [n]
// Optimised:      go build -o /tmp/fib_go fib.go && /tmp/fib_go [n]
//
// n is read from os.Args so the compiler cannot evaluate fib(35) at
// compile time — Go's inliner handles small leaf functions, but it
// does not constant-fold recursive call trees at any depth.
package main

import (
	"fmt"
	"os"
	"strconv"
	"time"
)

func fib(n int64) int64 {
	if n <= 1 {
		return n
	}
	return fib(n-1) + fib(n-2)
}

func main() {
	n := int64(35)
	if len(os.Args) > 1 {
		if v, err := strconv.ParseInt(os.Args[1], 10, 64); err == nil {
			n = v
		}
	}

	t0 := time.Now()
	result := fib(n)
	elapsed := time.Since(t0)

	fmt.Println(result)
	fmt.Fprintf(os.Stderr, "(%s)\n", elapsed)
}
