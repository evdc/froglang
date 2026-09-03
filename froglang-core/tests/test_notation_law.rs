//! The law `read(repr(x)) == x` (`plans/DATA.md` stage 5), property-tested
//! over generated programs — the actual deliverable of this stage, per the
//! doc's own framing: `repr`/`read` existing is necessary but not
//! sufficient, the law is what makes the round-trip promise checkable
//! rather than merely intended.
//!
//! **No `proptest`/`quickcheck`.** The "input" here is a generated
//! *program* (declarations plus a literal), and shrinking — the main thing
//! either crate buys — is close to useless on that shape: a shrunk program
//! is not obviously a smaller counterexample than the seed that produced
//! it. A failing case already prints its own seed and source (via `run`'s
//! own panic message), which reproduces exactly by re-running with
//! `FROG_LAW_SEED=<n>` set. Pulling in a dependency with `rand`,
//! `regex-syntax`, `bit-set` transitively for one test also cuts against
//! the project's own fast-compilation goal. So: a hand-rolled xorshift64
//! PRNG, seeded from a fixed range.
//!
//! **Coverage, not exhaustiveness.** Each seed generates one type *shape*
//! from the palette DATA.md's own Stage 5 status note names — `Int |
//! Float | Bool | Str | None | List<T> | T? | named struct | positional
//! struct | nominal union | Range<Int>` — with scalar-only nested fields/
//! elements (no unbounded recursion), a random literal of that shape, and
//! asserts the law plus a drift check (`repr` and `==` could both be wrong
//! the same way) in one program per seed.
//!
//! **Carve-outs**: `nan` is excluded (`nan != nan` is a failure of `Eq`,
//! not of notation — DATA.md's own stated carve-out); no seed generates a
//! bare `TypeVar` case (every generated `let` is annotated, matching every
//! other test in `test_repr.rs`/`test_read.rs`). The separate
//! `print(x) == repr(x)` drift table DATA.md also asks for lives in
//! `test_repr.rs::repr_and_print_agree_on_every_form` rather than being
//! duplicated here.

mod common;
use common::run;

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self { Rng(seed.max(1)) }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn range(&mut self, n: u64) -> u64 { self.next_u64() % n }

    fn int(&mut self, lo: i64, hi: i64) -> i64 { lo + (self.range((hi - lo) as u64) as i64) }

    fn bool(&mut self) -> bool { self.range(2) == 0 }
}

#[derive(Clone)]
enum Shape {
    Int,
    Float,
    Bool,
    Str,
    NoneT,
    List(Box<Shape>),
    Range,
    NamedStruct(Vec<(&'static str, Shape)>),
    PositionalStruct(Vec<Shape>),
    Union(Vec<(&'static str, &'static str, Shape)>),
    Optional(Box<Shape>),
}

fn gen_scalar(rng: &mut Rng) -> Shape {
    match rng.range(5) {
        0 => Shape::Int,
        1 => Shape::Float,
        2 => Shape::Bool,
        3 => Shape::Str,
        _ => Shape::NoneT,
    }
}

fn gen_top(rng: &mut Rng) -> Shape {
    match rng.range(10) {
        0 => Shape::Int,
        1 => Shape::Float,
        2 => Shape::Bool,
        3 => Shape::Str,
        4 => Shape::List(Box::new(gen_scalar(rng))),
        5 => Shape::Range,
        6 => Shape::NamedStruct(vec![("a", gen_scalar(rng)), ("b", gen_scalar(rng))]),
        7 => Shape::PositionalStruct(vec![gen_scalar(rng), gen_scalar(rng)]),
        8 => Shape::Union(vec![("A", "v", gen_scalar(rng)), ("B", "w", gen_scalar(rng))]),
        // `Bool`/`Float` deliberately excluded here — a pre-existing,
        // unrelated bug (`!` + `==` on a `(Bool|Float mixed with another
        // scalar) | <error type>` crashes Cranelift; reproduces with plain
        // hand-written source, no `read`/`repr` involved at all — filed
        // separately, not this stage's job to fix). `Int?`/`Str?` exercise
        // the same `build_read_anon_union`/`Widen` path without hitting it.
        _ => Shape::Optional(Box::new(if rng.bool() { Shape::Int } else { Shape::Str })),
    }
}

/// A safe ASCII string, occasionally with an escape-worthy character —
/// exercises `notation::escape_str` without needing UTF-8-boundary care.
fn gen_str_literal(rng: &mut Rng) -> String {
    let words = ["hi", "a", "frog", "notation", "x"];
    let mut s = words[rng.range(words.len() as u64) as usize].to_string();
    match rng.range(4) {
        0 => s.push('\n'),
        1 => s.push('"'),
        2 => s.push('\\'),
        _ => {}
    }
    s
}

fn frog_str_literal(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Emit `shape`'s frog type name and a random literal of that type,
/// pushing any `data` declaration it needs into `decls`. Scalar leaves
/// only ever recurse into more scalars (`gen_scalar`'s own restriction),
/// so this always terminates with no depth bound needed.
fn emit(shape: &Shape, rng: &mut Rng, decls: &mut Vec<String>) -> (String, String) {
    match shape {
        Shape::Int => ("Int".to_string(), rng.int(-1000, 1000).to_string()),
        Shape::Float => {
            let (whole, frac) = (rng.int(-1000, 1000), rng.range(1000));
            ("Float".to_string(), format!("{whole}.{frac}"))
        }
        Shape::Bool => ("Bool".to_string(), if rng.bool() { "true".to_string() } else { "false".to_string() }),
        Shape::Str => ("Str".to_string(), frog_str_literal(&gen_str_literal(rng))),
        Shape::NoneT => ("None".to_string(), "none".to_string()),
        Shape::List(elem) => {
            let n = rng.range(4);
            let mut elem_ty = String::new();
            let items: Vec<String> = (0..n).map(|_| {
                let (ty, lit) = emit(elem, rng, decls);
                elem_ty = ty;
                lit
            }).collect();
            if elem_ty.is_empty() {
                elem_ty = emit(elem, rng, decls).0;
            }
            (format!("List<{elem_ty}>"), format!("[{}]", items.join(", ")))
        }
        Shape::Range => {
            let lo = rng.int(0, 50);
            let hi = lo + rng.int(1, 50);
            ("Range<Int>".to_string(), format!("{lo}..{hi}"))
        }
        Shape::NamedStruct(fields) => {
            let mut field_decls = Vec::new();
            let mut ctor_args = Vec::new();
            for (fname, fshape) in fields {
                let (fty, flit) = emit(fshape, rng, decls);
                field_decls.push(format!("{fname}: {fty}"));
                ctor_args.push(format!("{fname}={flit}"));
            }
            decls.push(format!("data GenS({})", field_decls.join(", ")));
            ("GenS".to_string(), format!("GenS({})", ctor_args.join(", ")))
        }
        Shape::PositionalStruct(fields) => {
            let mut field_tys = Vec::new();
            let mut ctor_args = Vec::new();
            for fshape in fields {
                let (fty, flit) = emit(fshape, rng, decls);
                field_tys.push(fty);
                ctor_args.push(flit);
            }
            decls.push(format!("data GenP({})", field_tys.join(", ")));
            ("GenP".to_string(), format!("GenP({})", ctor_args.join(", ")))
        }
        Shape::Union(variants) => {
            let pick = rng.range(variants.len() as u64);
            let mut variant_decls = Vec::new();
            let mut constructed: Option<(String, String)> = None;
            for (i, (vname, fname, fshape)) in variants.iter().enumerate() {
                let (fty, flit) = emit(fshape, rng, decls);
                variant_decls.push(format!("{vname}({fname}: {fty})"));
                if i as u64 == pick {
                    constructed = Some((vname.to_string(), format!("{fname}={flit}")));
                }
            }
            decls.push(format!("data GenU is {}", variant_decls.join(" | ")));
            let (vname, args) = constructed.expect("pick is always in range");
            ("GenU".to_string(), format!("GenU.{vname}({args})"))
        }
        Shape::Optional(inner) => {
            let (ity, ilit) = emit(inner, rng, decls);
            let lit = if rng.bool() { "none".to_string() } else { ilit };
            (format!("{ity}?"), lit)
        }
    }
}

fn build_program(seed: u64) -> String {
    let mut rng = Rng::new(seed);
    let shape = gen_top(&mut rng);
    let mut decls = Vec::new();
    let (ty, lit) = emit(&shape, &mut rng, &mut decls);
    let decls_src = decls.join("\n");
    format!(
        "{decls_src}\nlet x: {ty} = {lit}\nlet back: {ty} | ReadError = read(repr(x))\nprint(back! == x)\nprint(repr(x) == repr(back!))\n"
    )
}

#[test]
fn the_law_holds_across_generated_programs() {
    let seeds: Vec<u64> = match std::env::var("FROG_LAW_SEED").ok().and_then(|s| s.parse().ok()) {
        Some(s) => vec![s],
        None => (1..=40).collect(),
    };
    for seed in seeds {
        let src = build_program(seed);
        let out = run(&src);
        assert_eq!(out, "true\ntrue\n", "law failed for seed {seed}:\n{src}");
    }
}
