//! housedeals-score: scores one market for the crawl job.
//!
//!     housedeals-score [--min-discount 15] [--min-comps-nyc 8] [--min-comps-mi 6]
//!                      [--max-price 900000] [--fresh-hours 72] < input.json > writes.json
//!
//! Reads `{"market", "now", "columns", "rows"}` (the pages of
//! GET /api/score-input) on stdin and writes `{"upserts", "deletes",
//! "alerts", "stats"}` (the body of POST /api/scores, before chunking) on
//! stdout. See `scorer::batch`. No network: the crawler does the HTTP.

use std::io::{Read, Write};
use std::process::ExitCode;

use scorer::batch::{run, Settings};

const USAGE: &str = "usage: housedeals-score [--min-discount PCT] [--min-comps-nyc N] [--min-comps-mi N] \
                     [--max-price USD] [--fresh-hours H] < input.json";

fn parse_args(args: &[String]) -> Result<Settings, String> {
    let mut s = Settings::default();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let (name, inline) = match flag.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (flag.as_str(), None),
        };
        if name == "-h" || name == "--help" {
            return Err(USAGE.into());
        }
        let value = match inline.or_else(|| it.next().cloned()) {
            Some(v) => v,
            None => return Err(format!("{name} needs a value\n{USAGE}")),
        };
        let num = |lo: f64, hi: f64| -> Result<f64, String> {
            value.trim().parse::<f64>().ok().filter(|n| n.is_finite() && (lo..=hi).contains(n)).ok_or(format!("{name}: {value:?} is not a number in {lo}..{hi}"))
        };
        match name.trim_start_matches('-') {
            "min-discount" => s.rule.min_discount_pct = num(0.1, 99.0)?,
            "min-comps-nyc" => s.rule.min_comps_nyc = num(1.0, 1000.0)? as usize,
            "min-comps-mi" => s.rule.min_comps_mi = num(1.0, 1000.0)? as usize,
            "max-price" => s.rule.max_price = num(1.0, 1e9)?,
            "fresh-hours" => s.fresh_hours = num(1.0, 24.0 * 365.0)? as i64,
            _ => return Err(format!("unknown flag {name}\n{USAGE}")),
        }
    }
    Ok(s)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let settings = match parse_args(&args) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("housedeals-score: {e}");
            return ExitCode::from(2);
        }
    };
    let mut raw = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut raw) {
        eprintln!("housedeals-score: reading stdin: {e}");
        return ExitCode::FAILURE;
    }
    let out = serde_json::from_str(&raw).map_err(|e| format!("stdin is not JSON: {e}")).and_then(|input| run(&input, &settings));
    match out {
        Ok(v) => {
            let mut stdout = std::io::stdout().lock();
            let written = serde_json::to_writer(&mut stdout, &v).map_err(std::io::Error::from).and_then(|_| stdout.write_all(b"\n"));
            if written.is_err() {
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("housedeals-score: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flags_override_the_defaults() {
        let s = parse_args(&args(&["--min-discount", "12", "--min-comps-mi=5", "--max-price", "800000"])).unwrap();
        assert_eq!((s.rule.min_discount_pct, s.rule.min_comps_mi, s.rule.min_comps_nyc, s.rule.max_price), (12.0, 5, 8, 800_000.0));
        assert_eq!(parse_args(&[]).unwrap(), Settings::default());
        assert!(parse_args(&args(&["--max-price"])).is_err());
        assert!(parse_args(&args(&["--min-discount", "abc"])).is_err());
        assert!(parse_args(&args(&["--nope", "1"])).is_err());
    }
}
