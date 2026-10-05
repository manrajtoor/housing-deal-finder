//! Database-neutral statements: SQL text plus positional parameters.
//!
//! The planning code produces these without touching D1, so it is unit-tested
//! natively (against a real in-memory SQLite in `service.rs` tests). Only
//! `d1.rs` turns them into D1 calls.

use serde_json::Value;

/// D1 allows 100 bound parameters per statement; id lists are cut to this.
pub const IDS_PER_QUERY: usize = 90;

/// One bound value. D1 has no BigInt, so integers travel as JS numbers; every
/// value stored here fits in 53 bits.
#[derive(Debug, Clone, PartialEq)]
pub enum Param {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
}

impl Param {
    pub fn text(s: Option<&str>) -> Param {
        s.map_or(Param::Null, |s| Param::Text(s.to_string()))
    }

    pub fn int(i: Option<i64>) -> Param {
        i.map_or(Param::Null, Param::Int)
    }

    pub fn real(f: Option<f64>) -> Param {
        f.map_or(Param::Null, Param::Real)
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Param::Int(i) => Some(*i as f64),
            Param::Real(f) => Some(*f),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Param::Text(s) => Some(s),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Param::Null)
    }
}

/// A statement ready to run: `?1`, `?2`, ... refer to `params` in order.
#[derive(Debug, Clone, PartialEq)]
pub struct Stmt {
    pub sql: String,
    pub params: Vec<Param>,
}

impl Stmt {
    pub fn new(sql: impl Into<String>, params: Vec<Param>) -> Stmt {
        Stmt { sql: sql.into(), params }
    }
}

/// `?1, ?2, ... ?n` starting at `from`.
pub fn placeholders(from: usize, n: usize) -> String {
    (from..from + n).map(|i| format!("?{i}")).collect::<Vec<_>>().join(", ")
}

/// `<prefix> IN (?1, ...)<suffix>` per chunk of at most [`IDS_PER_QUERY`] ids.
pub fn id_queries(prefix: &str, suffix: &str, ids: &[String]) -> Vec<Stmt> {
    ids.chunks(IDS_PER_QUERY)
        .map(|c| {
            Stmt::new(
                format!("{prefix} IN ({}){suffix}", placeholders(1, c.len())),
                c.iter().map(|id| Param::Text(id.clone())).collect(),
            )
        })
        .collect()
}

/// A JSON number without a spurious `.0`: D1 hands every number back as a JS
/// double, so 2019 arrives as 2019.0.
pub fn json_num(f: f64) -> Value {
    if f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0 {
        Value::from(f as i64)
    } else {
        serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number)
    }
}

/// What one written statement changed (D1 `meta.changes` / `rows_written`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Written {
    pub changes: u64,
    pub rows_written: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_positional() {
        assert_eq!(placeholders(1, 3), "?1, ?2, ?3");
        assert_eq!(placeholders(4, 1), "?4");
    }

    #[test]
    fn id_lists_are_chunked_under_the_parameter_limit() {
        let ids: Vec<String> = (0..200).map(|i| i.to_string()).collect();
        let qs = id_queries("SELECT id FROM listings WHERE id", "", &ids);
        assert_eq!(qs.iter().map(|q| q.params.len()).collect::<Vec<_>>(), vec![90, 90, 20]);
        assert!(qs[2].sql.ends_with("?19, ?20)"));
    }

    #[test]
    fn json_num_drops_trailing_zero() {
        assert_eq!(json_num(2019.0).to_string(), "2019");
        assert_eq!(json_num(0.25).to_string(), "0.25");
    }
}
