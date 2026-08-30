// Word-pipeline benchmark — the Rust sibling of benches/words.frog.
// Must print the same checksum.  Usage: words <words_per_doc> <rounds>

fn modn(x: i64, n: i64) -> i64 { x - (x / n) * n }

fn hash(i: i64) -> i64 { modn(i * 2654435761 + 1013904223, 2147483647) }

const VOCAB: [&str; 8] = ["frog", "frogs", "toad", "newt", "salamander", "axolotl", "tadpole", "pond"];

fn word_for(i: i64) -> &'static str { VOCAB[modn(hash(i), 8) as usize] }

fn build_doc(words_per_doc: i64, round: i64) -> String {
    let base = round * words_per_doc;
    let ws: Vec<&str> = (0..words_per_doc).map(|i| word_for(base + i)).collect();
    ws.join(" ")
}

fn score(w: &str) -> i64 {
    let base = w.len() as i64;
    let bonus = if w.starts_with("frog") { 10 } else { 0 };
    let exact = if w == "frog" { 5 } else { 0 };
    base + bonus + exact
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let words_per_doc: i64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(400);
    let rounds: i64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(800);

    let start = std::time::Instant::now();
    let mut checksum: i64 = 0;
    for round in 0..rounds {
        let doc = build_doc(words_per_doc, round);
        let total: i64 = doc.split(' ').map(score).sum();
        let shouted = doc.to_uppercase();
        let found = if shouted.contains("SALAMANDER") { 1 } else { 0 };
        checksum += total + doc.len() as i64 + found;
    }
    let elapsed = start.elapsed();

    println!("{}", checksum);
    eprintln!("{:?}", elapsed);
}
