// Conway's Game of Life — the Rust sibling of benches/life.frog.
// Must print the same checksum.  Usage: life <size> <generations>

fn modn(x: i64, n: i64) -> i64 {
    x - (x / n) * n
}

fn hash(i: i64) -> i64 {
    modn(i * 2654435761 + 1013904223, 2147483647)
}

fn seed(size: i64) -> Vec<Vec<i64>> {
    (0..size)
        .map(|y| {
            (0..size)
                .map(|x| if modn(hash(y * size + x), 3) == 0 { 1 } else { 0 })
                .collect()
        })
        .collect()
}

fn cell_at(rows: &[Vec<i64>], y: i64, x: i64) -> i64 {
    let size = rows.len() as i64;
    if y >= 0 && y < size && x >= 0 && x < size {
        rows[y as usize][x as usize]
    } else {
        0
    }
}

fn neighbours(rows: &[Vec<i64>], y: i64, x: i64) -> i64 {
    let mut n = 0;
    for dy in -1..2 {
        for dx in -1..2 {
            let skip = dx == 0 && dy == 0;
            n += if skip { 0 } else { cell_at(rows, y + dy, x + dx) };
        }
    }
    n
}

fn next_cell(rows: &[Vec<i64>], y: i64, x: i64) -> i64 {
    let alive = rows[y as usize][x as usize];
    let n = neighbours(rows, y, x);
    if alive == 1 {
        if n == 2 || n == 3 { 1 } else { 0 }
    } else if n == 3 { 1 } else { 0 }
}

fn step(rows: &[Vec<i64>]) -> Vec<Vec<i64>> {
    let size = rows.len() as i64;
    (0..size)
        .map(|y| (0..size).map(|x| next_cell(rows, y, x)).collect())
        .collect()
}

fn population(rows: &[Vec<i64>]) -> i64 {
    rows.iter().map(|row| row.iter().sum::<i64>()).sum()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let size: i64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(48);
    let generations: i64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(40);

    let start = std::time::Instant::now();
    let mut grid = seed(size);
    let mut checksum: i64 = 0;
    for _ in 0..generations {
        checksum += population(&grid);
        grid = step(&grid);
    }
    let elapsed = start.elapsed();

    println!("{}", checksum);
    eprintln!("{:?}", elapsed);
}
