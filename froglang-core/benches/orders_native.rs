// Order-pipeline benchmark — native Rust, optimised build.
// Mirrors benches/orders.frog exactly; see that file for what it measures.
//
//   rustc -C opt-level=3 orders_native.rs -o /tmp/orders && /tmp/orders [items] [rounds]

use std::hint::black_box;
use std::time::Instant;

#[derive(Clone, Copy)]
enum Category { Food, Book, Electronics, Toy }

#[derive(Clone, Copy)]
struct Item { sku: i64, category: Category, qty: i64, unit_price: i64 }

enum Discount { NoDiscount, Percent(i64), Flat(i64), BulkOver(i64, i64) }

fn hash(i: i64) -> i64 { (i * 2654435761 + 1013904223) % 2147483647 }

fn category_of(h: i64) -> Category {
    match h % 4 {
        0 => Category::Food,
        1 => Category::Book,
        2 => Category::Electronics,
        _ => Category::Toy,
    }
}

fn discount_for(it: &Item) -> Discount {
    match it.category {
        Category::Food => if it.qty >= 6 { Discount::BulkOver(6, 5) } else { Discount::NoDiscount },
        Category::Book => Discount::Percent(10),
        Category::Electronics =>
            if it.unit_price > 3000 { Discount::Flat(250) } else { Discount::Percent(3) },
        Category::Toy => Discount::BulkOver(3, 15),
    }
}

fn apply(d: Discount, gross: i64, qty: i64) -> i64 {
    match d {
        Discount::NoDiscount => gross,
        Discount::Percent(pct) => gross - gross * pct / 100,
        Discount::Flat(amount) => if gross > amount { gross - amount } else { 0 },
        Discount::BulkOver(min_qty, pct) =>
            if qty >= min_qty { gross - gross * pct / 100 } else { gross },
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let items_n: i64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(2000);
    let rounds: i64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(2000);
    // black_box keeps LLVM from specialising the loop bounds it can see.
    let (items_n, rounds) = (black_box(items_n), black_box(rounds));

    let t0 = Instant::now();

    let items: Vec<Item> = (0..items_n)
        .map(|i| Item {
            sku: i,
            category: category_of(hash(i)),
            qty: 1 + hash(i + 7) % 9,
            unit_price: 100 + hash(i + 13) % 5000,
        })
        .collect();

    let mut total: i64 = 0;
    for round in 0..rounds {
        let batch: Vec<Item> = items.iter().filter(|it| (it.sku + round) % 3 != 0).copied().collect();
        for it in &batch {
            total += apply(discount_for(it), it.qty * it.unit_price, it.qty);
        }
    }

    let elapsed = t0.elapsed();
    println!("{}", total);
    eprintln!("({:?})", elapsed);
}
