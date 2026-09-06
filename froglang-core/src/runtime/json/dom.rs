//! The JSON DOM backend seam (`plans/DATA.md` stage 8).
//!
//! `runtime/json/mod.rs`'s accessors are written against exactly the dozen
//! functions below, so which library actually parses is a compile-time
//! choice and nothing else in the compiler knows the difference:
//!
//!   - default — [`serde_json::Value`]. Portable everywhere, and already in
//!     the workspace lockfile via `playground-server`, so it costs no new
//!     build.
//!   - `--features json_simd` — [`simd_json::OwnedValue`], SIMD-accelerated
//!     on x86_64 (SSE4.2/AVX2) and aarch64 (NEON).
//!
//! `tests/test_json.rs` is expected to pass identically under both; the
//! feature is a performance choice, never a semantic one.
//!
//! **Both backends own their parsed value.** `simd-json`'s faster
//! `BorrowedValue`/`Tape` types borrow their strings out of the input
//! buffer, which would make the thread-local in `mod.rs` self-referential
//! for no benefit this compiler can use — every string it reads is copied
//! into the GC heap on the way out anyway. `OwnedValue` keeps the SIMD
//! parse and drops the lifetime.

/// The parsed document. Node handles in `mod.rs` are raw pointers to
/// values *inside* one of these, so it must be a type whose children live
/// at stable addresses for as long as the root does — true of both
/// backends (`Vec`/`HashMap` of owned values).
#[cfg(not(feature = "json_simd"))]
pub type Json = serde_json::Value;
#[cfg(feature = "json_simd")]
pub type Json = simd_json::OwnedValue;

/// What a node is, for the anonymous-union and shape-mismatch paths.
/// Deliberately coarser than either backend's own type enum: froglang
/// distinguishes `Int` from `Float`, but has no unsigned or 128-bit types,
/// and treats everything a backend might invent beyond RFC 8259 as
/// `Other` rather than pretending it maps.
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum Kind { Int, Float, Bool, Str, Null, Array, Object, Other }

/// The `(message, byte offset)` pair `mod.rs`'s sticky-error state stores
/// — the same shape `runtime/read.rs`'s `ReadState::err` holds, so
/// `JsonError` and `ReadError` are populated by identical code paths.
pub type ParseError = (String, usize);

// ── serde_json ───────────────────────────────────────────────────────────────

#[cfg(not(feature = "json_simd"))]
mod backend {
    use super::{Json, Kind, ParseError};

    pub fn parse(src: &str) -> Result<Json, ParseError> {
        serde_json::from_str(src).map_err(|e| {
            // `serde_json` reports 1-based line/column, not a byte offset;
            // `simd-json` reports the offset directly. Normalizing here is
            // what lets `JsonError.offset` mean the same thing under both.
            (e.to_string(), line_col_to_offset(src, e.line(), e.column()))
        })
    }

    fn line_col_to_offset(src: &str, line: usize, col: usize) -> usize {
        if line == 0 { return 0; }
        let mut offset = 0usize;
        for (i, text) in src.split('\n').enumerate() {
            if i + 1 == line {
                return offset + col.saturating_sub(1);
            }
            offset += text.len() + 1;
        }
        src.len()
    }

    pub fn kind(v: &Json) -> Kind {
        match v {
            Json::Null => Kind::Null,
            Json::Bool(_) => Kind::Bool,
            // `is_i64` is false for a number written with a `.` or an
            // exponent even when it is integral (`1.0`), which is exactly
            // the distinction froglang's `Int`/`Float` split wants.
            Json::Number(n) if n.is_i64() => Kind::Int,
            Json::Number(_) => Kind::Float,
            Json::String(_) => Kind::Str,
            Json::Array(_) => Kind::Array,
            Json::Object(_) => Kind::Object,
        }
    }

    pub fn as_i64(v: &Json) -> Option<i64> { v.as_i64() }
    pub fn as_f64(v: &Json) -> Option<f64> { v.as_f64() }
    pub fn as_bool(v: &Json) -> Option<bool> { v.as_bool() }
    pub fn as_str(v: &Json) -> Option<&str> { v.as_str() }
    pub fn get<'a>(v: &'a Json, key: &str) -> Option<&'a Json> { v.get(key) }
    pub fn at(v: &Json, i: usize) -> Option<&Json> { v.as_array().and_then(|a| a.get(i)) }
    pub fn len(v: &Json) -> Option<usize> { v.as_array().map(|a| a.len()) }
}

// ── simd-json ────────────────────────────────────────────────────────────────

#[cfg(feature = "json_simd")]
mod backend {
    use super::{Json, Kind, ParseError};
    use simd_json::prelude::*;
    use simd_json::{StaticNode, ValueType};

    pub fn parse(src: &str) -> Result<Json, ParseError> {
        // `to_owned_value` scans in place and needs a mutable buffer, so
        // the input is copied once. That copy is the price of not handing
        // out borrowed values — see this module's own doc comment.
        let mut buf = src.as_bytes().to_vec();
        simd_json::to_owned_value(&mut buf).map_err(|e| (e.to_string(), e.index()))
    }

    pub fn kind(v: &Json) -> Kind {
        // `I64`/`U64` both mean "written without a `.` or exponent"; a
        // `u64` too large for `i64` still reads as an integer here and is
        // then rejected by `as_i64` returning `None`, which is the honest
        // outcome — froglang's `Int` is signed 64-bit.
        match v.value_type() {
            ValueType::Null => Kind::Null,
            ValueType::Bool => Kind::Bool,
            ValueType::I64 | ValueType::U64 | ValueType::I128 | ValueType::U128 => Kind::Int,
            ValueType::F64 => Kind::Float,
            ValueType::String => Kind::Str,
            ValueType::Array => Kind::Array,
            ValueType::Object => Kind::Object,
            _ => Kind::Other,
        }
    }

    pub fn as_i64(v: &Json) -> Option<i64> {
        // Not `ValueAsScalar::as_i64`, which happily converts a float:
        // `kind` above already promised `1.0` is a `Float`, and the two
        // must agree or `Int | Float` dispatch contradicts itself.
        match v {
            Json::Static(StaticNode::I64(n)) => Some(*n),
            Json::Static(StaticNode::U64(n)) => i64::try_from(*n).ok(),
            _ => None,
        }
    }
    // `cast_f64`, not `as_f64`: the latter is `None` for an integer node,
    // where `serde_json`'s `as_f64` converts. JSON has one number type, so
    // `1` is the only spelling a whole-valued float has on the wire and
    // both backends have to accept it — a difference the seam exists to
    // erase, and one `runtime/json/mod.rs`'s own test caught.
    pub fn as_f64(v: &Json) -> Option<f64> { v.cast_f64() }
    pub fn as_bool(v: &Json) -> Option<bool> { ValueAsScalar::as_bool(v) }
    pub fn as_str(v: &Json) -> Option<&str> { ValueAsScalar::as_str(v) }
    pub fn get<'a>(v: &'a Json, key: &str) -> Option<&'a Json> { ValueObjectAccess::get(v, key) }
    pub fn at(v: &Json, i: usize) -> Option<&Json> { v.as_array().and_then(|a| a.get(i)) }
    pub fn len(v: &Json) -> Option<usize> { v.as_array().map(|a| a.len()) }
}

pub use backend::{as_bool, as_f64, as_i64, as_str, at, get, kind, len, parse};

// ── writing ──────────────────────────────────────────────────────────────────

/// `s` as a JSON string literal — surrounding quotes included, escaped per
/// RFC 8259. Deliberately **not** `notation::escape_str`: frog notation and
/// JSON are separate tiers (`DATA.md`'s "the central decision"), and their
/// escape tables genuinely differ (`\'`, and frog's `\u{...}` vs JSON's
/// `\uXXXX`).
///
/// Always `serde_json`, under either feature — this is a few hundred bytes
/// of escaping on the *write* path, where the SIMD backend has nothing to
/// offer, and sharing it keeps one escape table rather than two.
pub fn escape(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

/// A `Float` as JSON. JSON has no spelling for infinities or NaN, and
/// `json.to_str` returns `Str` rather than `Str | JsonError`, so a
/// non-finite float serializes as `null` — the same choice `serde_json`
/// makes, and the only one that keeps the return type total.
pub fn float(f: f64) -> String {
    if f.is_finite() { serde_json::Number::from_f64(f).map_or_else(|| "null".to_string(), |n| n.to_string()) }
    else { "null".to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_classifies_every_kind() {
        let v = parse(r#"{"i":1,"f":1.5,"b":true,"s":"x","n":null,"a":[1,2]}"#).expect("valid");
        assert_eq!(kind(&v), Kind::Object);
        assert_eq!(kind(get(&v, "i").expect("key i")), Kind::Int);
        assert_eq!(kind(get(&v, "f").expect("key f")), Kind::Float);
        assert_eq!(kind(get(&v, "b").expect("key b")), Kind::Bool);
        assert_eq!(kind(get(&v, "s").expect("key s")), Kind::Str);
        assert_eq!(kind(get(&v, "n").expect("key n")), Kind::Null);
        assert_eq!(kind(get(&v, "a").expect("key a")), Kind::Array);
        assert!(get(&v, "missing").is_none());
    }

    #[test]
    fn an_integral_float_stays_a_float() {
        // The `Int`/`Float` distinction is syntactic, and both backends
        // must agree — otherwise `Int | Float` dispatch is backend-
        // dependent, which is exactly what the seam exists to prevent.
        let v = parse("1.0").expect("valid");
        assert_eq!(kind(&v), Kind::Float);
        assert_eq!(as_i64(&v), None);
        assert_eq!(as_f64(&v), Some(1.0));

        let v = parse("1").expect("valid");
        assert_eq!(kind(&v), Kind::Int);
        assert_eq!(as_i64(&v), Some(1));
    }

    #[test]
    fn reads_scalars_and_navigates() {
        let v = parse(r#"[10, "hi", false]"#).expect("valid");
        assert_eq!(len(&v), Some(3));
        assert_eq!(as_i64(at(&v, 0).expect("index 0")), Some(10));
        assert_eq!(as_str(at(&v, 1).expect("index 1")), Some("hi"));
        assert_eq!(as_bool(at(&v, 2).expect("index 2")), Some(false));
        assert!(at(&v, 3).is_none());
    }

    #[test]
    fn a_parse_error_carries_a_byte_offset() {
        // The `]` at byte 7 is where both backends notice.
        let (_, offset) = parse(r#"[1, 2, ]"#).expect_err("malformed");
        assert_eq!(offset, 7);

        // Across lines, so the serde_json line/column normalization is
        // exercised rather than trivially agreeing with the offset.
        let (_, offset) = parse("[\n  1,\n  }\n]").expect_err("malformed");
        assert_eq!(offset, 9);
    }

    #[test]
    fn escapes_and_formats_the_write_leaves() {
        assert_eq!(escape("a\"b\n"), r#""a\"b\n""#);
        assert_eq!(escape("é"), "\"é\"");
        assert_eq!(float(1.5), "1.5");
        assert_eq!(float(1.0), "1.0");
        assert_eq!(float(f64::NAN), "null");
        assert_eq!(float(f64::INFINITY), "null");
    }
}
