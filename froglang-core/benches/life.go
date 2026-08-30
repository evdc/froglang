// Conway's Game of Life — the Go sibling of benches/life.frog.
// Must print the same checksum.  Usage: life <size> <generations>
package main

import (
	"fmt"
	"os"
	"strconv"
	"time"
)

func modn(x, n int64) int64 { return x - (x/n)*n }

func hash(i int64) int64 { return modn(i*2654435761+1013904223, 2147483647) }

func seed(size int64) [][]int64 {
	rows := make([][]int64, size)
	for y := int64(0); y < size; y++ {
		rows[y] = make([]int64, size)
		for x := int64(0); x < size; x++ {
			if modn(hash(y*size+x), 3) == 0 {
				rows[y][x] = 1
			}
		}
	}
	return rows
}

func cellAt(rows [][]int64, y, x int64) int64 {
	size := int64(len(rows))
	if y >= 0 && y < size && x >= 0 && x < size {
		return rows[y][x]
	}
	return 0
}

func neighbours(rows [][]int64, y, x int64) int64 {
	var n int64
	for dy := int64(-1); dy < 2; dy++ {
		for dx := int64(-1); dx < 2; dx++ {
			if dx == 0 && dy == 0 {
				continue
			}
			n += cellAt(rows, y+dy, x+dx)
		}
	}
	return n
}

func nextCell(rows [][]int64, y, x int64) int64 {
	alive := rows[y][x]
	n := neighbours(rows, y, x)
	if alive == 1 {
		if n == 2 || n == 3 {
			return 1
		}
		return 0
	}
	if n == 3 {
		return 1
	}
	return 0
}

func step(rows [][]int64) [][]int64 {
	size := int64(len(rows))
	out := make([][]int64, size)
	for y := int64(0); y < size; y++ {
		out[y] = make([]int64, size)
		for x := int64(0); x < size; x++ {
			out[y][x] = nextCell(rows, y, x)
		}
	}
	return out
}

func population(rows [][]int64) int64 {
	var total int64
	for _, row := range rows {
		for _, cell := range row {
			total += cell
		}
	}
	return total
}

func main() {
	size, generations := int64(48), int64(40)
	if len(os.Args) > 1 {
		if v, err := strconv.ParseInt(os.Args[1], 10, 64); err == nil {
			size = v
		}
	}
	if len(os.Args) > 2 {
		if v, err := strconv.ParseInt(os.Args[2], 10, 64); err == nil {
			generations = v
		}
	}

	start := time.Now()
	grid := seed(size)
	var checksum int64
	for g := int64(0); g < generations; g++ {
		checksum += population(grid)
		grid = step(grid)
	}
	elapsed := time.Since(start)

	fmt.Println(checksum)
	fmt.Fprintln(os.Stderr, elapsed)
}
