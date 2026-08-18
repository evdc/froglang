// Order-pipeline benchmark — Go.
// Mirrors benches/orders.frog exactly; see that file for what it measures.
//
//	go build -o /tmp/orders_go orders.go && /tmp/orders_go [items] [rounds]
package main

import (
	"fmt"
	"os"
	"strconv"
	"time"
)

type Category int

const (
	Food Category = iota
	Book
	Electronics
	Toy
)

type Item struct {
	sku       int64
	category  Category
	qty       int64
	unitPrice int64
}

// Go has no sum types, so a discount is a tag plus the fields the tag needs —
// the same information the froglang enum carries, minus the boxing.
type DiscountKind int

const (
	NoDiscount DiscountKind = iota
	Percent
	Flat
	BulkOver
)

type Discount struct {
	kind DiscountKind
	a    int64 // Percent: pct   Flat: amount   BulkOver: min_qty
	pct  int64 // BulkOver only
}

func hash(i int64) int64 { return (i*2654435761 + 1013904223) % 2147483647 }

func categoryOf(h int64) Category {
	switch h % 4 {
	case 0:
		return Food
	case 1:
		return Book
	case 2:
		return Electronics
	default:
		return Toy
	}
}

func discountFor(it Item) Discount {
	switch it.category {
	case Food:
		if it.qty >= 6 {
			return Discount{BulkOver, 6, 5}
		}
		return Discount{kind: NoDiscount}
	case Book:
		return Discount{kind: Percent, a: 10}
	case Electronics:
		if it.unitPrice > 3000 {
			return Discount{kind: Flat, a: 250}
		}
		return Discount{kind: Percent, a: 3}
	default:
		return Discount{BulkOver, 3, 15}
	}
}

func apply(d Discount, gross, qty int64) int64 {
	switch d.kind {
	case NoDiscount:
		return gross
	case Percent:
		return gross - gross*d.a/100
	case Flat:
		if gross > d.a {
			return gross - d.a
		}
		return 0
	default:
		if qty >= d.a {
			return gross - gross*d.pct/100
		}
		return gross
	}
}

func main() {
	var itemsN, rounds int64 = 2000, 2000
	if len(os.Args) > 1 {
		if v, err := strconv.ParseInt(os.Args[1], 10, 64); err == nil {
			itemsN = v
		}
	}
	if len(os.Args) > 2 {
		if v, err := strconv.ParseInt(os.Args[2], 10, 64); err == nil {
			rounds = v
		}
	}

	t0 := time.Now()

	items := make([]Item, 0, itemsN)
	for i := int64(0); i < itemsN; i++ {
		items = append(items, Item{
			sku:       i,
			category:  categoryOf(hash(i)),
			qty:       1 + hash(i+7)%9,
			unitPrice: 100 + hash(i+13)%5000,
		})
	}

	var total int64
	for round := int64(0); round < rounds; round++ {
		batch := make([]Item, 0, itemsN)
		for _, it := range items {
			if (it.sku+round)%3 != 0 {
				batch = append(batch, it)
			}
		}
		for _, it := range batch {
			total += apply(discountFor(it), it.qty*it.unitPrice, it.qty)
		}
	}

	elapsed := time.Since(t0)
	fmt.Println(total)
	fmt.Fprintf(os.Stderr, "(%s)\n", elapsed)
}
