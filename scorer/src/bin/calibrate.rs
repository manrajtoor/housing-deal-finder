//! calibrate: scores a D1 export the way the crawl job would and prints how
//! the discounts are distributed (DESIGN.md "Calibration").
//!
//!     cargo run --release --features calibrate --bin calibrate -- \
//!         d1-export.sql [--water mi-water.txt] [--sold crawled.json] [--market nyc|mi] [--top 25]
//!
//! The input is the SQL text of `wrangler d1 export` (or a SQLite file:
//! `.db`/`.sqlite`). `--water` fills Michigan water from lines
//! `id|waterType|waterBody` (the map classifier's output for the rows'
//! lat/lon) where the row has none. `--sold` merges crawled listings (the
//! crawler's `--dry-run` JSON `{"listings": [...]}`, a JSON array, or NDJSON)
//! into the dump, replacing rows with the same id: e.g. NYC sold rows from
//! `housedeals --dry-run --mode sold --market nyc`. NYC sold rows are placed
//! (building type, neighbourhood) as the crawl job places them
//! (`scorer::nyc_sold`), and the placement is printed. Nothing is written
//! anywhere.

use std::collections::BTreeMap;
use std::process::ExitCode;

use rusqlite::{types::ValueRef, Connection};
use serde_json::{Map, Value};

use scorer::listing::Facts;
use scorer::{Options, Refusal, Score, Scorer};

/// The listing columns of GET /api/score-input (no description: the scorer
/// sees what production sees), plus `last_seen` to date "now".
const COLUMNS: &[(&str, &str)] = &[
    ("id", "id"), ("market", "market"), ("status", "status"), ("url", "url"), ("address", "address"),
    ("lat", "lat"), ("lon", "lon"),
    ("unit", "unit"), ("city", "city"), ("price", "price"), ("sold_at", "soldAt"), ("beds", "beds"),
    ("baths", "baths"), ("sqft", "sqft"), ("lot_sqft", "lotSqft"), ("home_type", "homeType"),
    ("neighborhood", "neighborhood"), ("borough", "borough"), ("county", "county"), ("area", "area"),
    ("water_type", "waterType"), ("water_body", "waterBody"), ("water_source", "waterSource"),
    ("frontage_ft", "frontageFt"), ("removed_at", "removedAt"), ("comp_only", "compOnly"), ("last_seen", "lastSeen"),
];

fn load(path: &str) -> Result<Vec<Map<String, Value>>, String> {
    let db = if path.ends_with(".db") || path.ends_with(".sqlite") {
        Connection::open(path).map_err(|e| e.to_string())?
    } else {
        let sql = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        let db = Connection::open_in_memory().map_err(|e| e.to_string())?;
        db.execute_batch(&sql).map_err(|e| format!("{path}: {e}"))?;
        db
    };
    let cols: Vec<&str> = COLUMNS.iter().map(|c| c.0).collect();
    let mut st = db.prepare(&format!("SELECT {} FROM listings ORDER BY id", cols.join(", "))).map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| {
            let mut m = Map::new();
            for (i, (_, camel)) in COLUMNS.iter().enumerate() {
                let v = match r.get_ref(i)? {
                    ValueRef::Null => Value::Null,
                    ValueRef::Integer(n) => Value::from(n),
                    ValueRef::Real(x) => serde_json::Number::from_f64(x).map_or(Value::Null, Value::Number),
                    ValueRef::Text(t) => Value::from(String::from_utf8_lossy(t).into_owned()),
                    ValueRef::Blob(_) => Value::Null,
                };
                m.insert(camel.to_string(), v);
            }
            if let Some(n) = m.get("compOnly").and_then(Value::as_i64) {
                m.insert("compOnly".into(), Value::Bool(n != 0));
            }
            Ok(m)
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
}

/// Crawled listings: `{"listings": [...]}` (crawler --dry-run), a JSON array,
/// or one listing per line. Only score-input's fields are kept.
fn load_crawled(path: &str) -> Result<Vec<Map<String, Value>>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let items: Vec<Value> = match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(mut o)) => match o.remove("listings") {
            Some(Value::Array(a)) => a,
            _ => return Err(format!("{path}: an object without a \"listings\" array")),
        },
        Ok(Value::Array(a)) => a,
        _ => text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).map_err(|e| format!("{path}: {e}")))
            .collect::<Result<_, _>>()?,
    };
    Ok(items
        .into_iter()
        .filter_map(|v| match v {
            Value::Object(o) => Some(COLUMNS.iter().map(|(_, k)| (k.to_string(), o.get(*k).cloned().unwrap_or(Value::Null))).collect()),
            _ => None,
        })
        .collect())
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    scorer::stats::Sorted::new(sorted).percentile(p).unwrap_or(f64::NAN)
}

#[derive(Default)]
struct Seg {
    subjects: usize,
    discounts: Vec<f64>,
    alerts: usize,
}

impl Seg {
    fn add(&mut self, r: &Result<Score, Refusal>) {
        self.subjects += 1;
        if let Ok(s) = r {
            self.discounts.push(s.discount_pct);
            self.alerts += usize::from(s.alert);
        }
    }
}

fn header() {
    println!(
        "  {:<34} {:>6} {:>6} {:>6} {:>7} {:>7} {:>7} {:>7} {:>7} {:>5} {:>5} {:>5} {:>5}",
        "segment", "subj", "priced", "share", "p5", "p25", "median", "p75", "p95", ">=15", ">=25", ">=40", "alert"
    );
}

fn row(name: &str, s: &Seg) {
    let mut d = s.discounts.clone();
    d.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = d.len();
    let ge = |t: f64| d.iter().filter(|x| **x >= t).count();
    println!(
        "  {:<34} {:>6} {:>6} {:>5.0}% {:>7.1} {:>7.1} {:>7.1} {:>7.1} {:>7.1} {:>5} {:>5} {:>5} {:>5}",
        name.chars().take(34).collect::<String>(),
        s.subjects,
        n,
        100.0 * n as f64 / s.subjects.max(1) as f64,
        pct(&d, 5.0),
        pct(&d, 25.0),
        pct(&d, 50.0),
        pct(&d, 75.0),
        pct(&d, 95.0),
        ge(15.0),
        ge(25.0),
        ge(40.0),
        s.alerts
    );
}

fn refusal_key(r: &Refusal) -> String {
    match r {
        Refusal::Invalid => "invalid".into(),
        Refusal::NotActive => "not active".into(),
        Refusal::Excluded(why) if why.starts_with("its building") => "excluded: cheap building".into(),
        Refusal::Excluded(why) => format!("excluded: {why}"),
        Refusal::Implausible { .. } => "implausible (over the cap)".into(),
        Refusal::NoGroup(why) => {
            // The reason of the last level tried.
            let last = why.last().map(String::as_str).unwrap_or("no groups");
            let kind = ["no comps", "comps <", "ceiling", "spread"].iter().find(|k| last.contains(*k)).unwrap_or(&"no groups");
            format!("no group: {kind}")
        }
    }
}

/// NYC sold rows: how many got a building type and a neighbourhood, per borough.
fn print_placement(st: &scorer::nyc_sold::Stats, listings: &[Value], now: &str, days: i64) {
    let cutoff = scorer::dates::date_minus_days(now, days).unwrap_or_default();
    let recent = listings
        .iter()
        .filter(|l| l["status"] == "sold" && l["soldAt"].as_str().is_some_and(|s| s.len() >= 10 && s[..10] >= *cutoff.as_str()))
        .count();
    println!("\nNYC sold rows: {recent} sold since {cutoff}");
    println!(
        "  {:<14} {:>6} {:>9} {:>9} {:>9} {:>9} {:>9} {:>6} {:>7}",
        "borough", "sold", "type:SE", "type:zlCo", "type:none", "nbhd:bldg", "nbhd:knn", "none", "usable"
    );
    let line = |name: &str, b: &scorer::nyc_sold::BoroughStats| {
        let p = |n: usize| format!("{} {:>2.0}%", n, 100.0 * n as f64 / b.sold.max(1) as f64);
        println!(
            "  {:<14} {:>6} {:>9} {:>9} {:>9} {:>9} {:>9} {:>6} {:>7}",
            name,
            b.sold,
            p(b.type_from_streeteasy),
            p(b.type_from_zillow_coop),
            p(b.type_uncertain),
            p(b.nbhd_from_building),
            p(b.nbhd_from_neighbours),
            p(b.nbhd_none),
            p(b.usable)
        );
    };
    for (k, b) in &st.by_borough {
        line(k, b);
    }
    line("ALL", &st.total());
    let pairs: Vec<String> = st.zillow_vs_streeteasy.iter().map(|((z, s), n)| format!("{z}→{s} {n}")).collect();
    println!("  Zillow type → StreetEasy type (typed rows): {}", pairs.join(", "));
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut path = None;
    let mut water = None;
    let mut sold: Option<String> = None;
    let mut only: Option<String> = None;
    let mut top = 25usize;
    let mut opts = Options::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--water" => water = it.next().cloned(),
            "--sold" => sold = it.next().cloned(),
            "--market" => only = it.next().cloned(),
            "--top" => top = it.next().and_then(|t| t.parse().ok()).unwrap_or(25),
            // Experiments: --set name=value for the numeric Options.
            "--set" => {
                let kv = it.next().cloned().unwrap_or_default();
                let (k, v) = kv.split_once('=').unwrap_or((&kv, ""));
                let v: f64 = v.parse().unwrap_or(f64::NAN);
                match k {
                    "max_spread" => opts.max_spread = v,
                    "max_spread_price" => opts.max_spread_price = v,
                    "max_discount_pct" => opts.max_discount_pct = v,
                    "size_window_nyc" => opts.size_window_nyc = v,
                    "size_window_mi" => opts.size_window_mi = v,
                    "max_ceiling_share" => opts.max_ceiling_share = v,
                    "cheap_building_pct" => opts.cheap_building_pct = v,
                    "min_comps_nyc" => opts.min_comps_nyc = v as usize,
                    "min_comps_mi" => opts.min_comps_mi = v as usize,
                    _ => {
                        eprintln!("calibrate: unknown --set {k}");
                        return ExitCode::from(2);
                    }
                }
            }
            _ => path = Some(a.clone()),
        }
    }
    let Some(path) = path else {
        eprintln!("usage: calibrate d1-export.sql [--water id|type|body file] [--sold crawled.json] [--market nyc|mi] [--top N]");
        return ExitCode::from(2);
    };
    let mut rows = match load(&path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("calibrate: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(w) = water {
        let text = std::fs::read_to_string(&w).unwrap_or_default();
        let map: BTreeMap<&str, (&str, &str)> = text
            .lines()
            .filter_map(|l| {
                let mut p = l.splitn(3, '|');
                Some((p.next()?, (p.next()?, p.next().unwrap_or(""))))
            })
            .collect();
        let mut filled = 0;
        for r in rows.iter_mut() {
            let id = r["id"].as_str().unwrap_or("").to_string();
            if let (Some((t, b)), true) = (map.get(id.as_str()), r["waterType"].is_null()) {
                r.insert("waterType".into(), Value::from(*t));
                r.insert("waterBody".into(), if b.is_empty() { Value::Null } else { Value::from(*b) });
                r.insert("waterSource".into(), Value::from("map"));
                filled += 1;
            }
        }
        println!("water filled from {w}: {filled} rows");
    }
    if let Some(p) = &sold {
        let crawled = match load_crawled(p) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("calibrate: {e}");
                return ExitCode::FAILURE;
            }
        };
        let ids: std::collections::HashSet<String> = crawled.iter().filter_map(|c| c["id"].as_str().map(str::to_string)).collect();
        let before = rows.len();
        rows.retain(|r| !r["id"].as_str().is_some_and(|id| ids.contains(id)));
        println!("merged {} crawled rows from {p} ({} replaced dump rows)", crawled.len(), before - rows.len());
        rows.extend(crawled);
    }
    let now = rows.iter().filter_map(|r| r["lastSeen"].as_str()).max().unwrap_or("2026-10-05T12:00:00Z").to_string();
    println!("{path}: {} rows, now = {now}", rows.len());

    for market in ["nyc", "mi"] {
        if only.as_deref().is_some_and(|m| m != market) {
            continue;
        }
        let mut listings: Vec<Value> = rows.iter().filter(|r| r["market"] == market).cloned().map(Value::Object).collect();
        if listings.is_empty() {
            continue;
        }
        if market == "nyc" {
            let st = scorer::nyc_sold::enrich(&mut listings);
            if !st.by_borough.is_empty() {
                print_placement(&st, &listings, &now, opts.sold_comp_days);
            }
        }
        let scorer = Scorer::new(&listings, opts.clone(), &now);
        let results: Vec<(Facts, &Value, Result<Score, Refusal>)> = listings
            .iter()
            .filter_map(|l| {
                let f = Facts::from_json(l)?;
                (f.active && !f.removed).then(|| (f, l, scorer.score(l)))
            })
            .collect();

        println!("\n==================== {market}: {} rows, {} active subjects ====================", listings.len(), results.len());
        let mut classes: Vec<String> = Vec::new();
        for (f, _, _) in &results {
            let e = format!("{} {:.2}", if market == "mi" { "all" } else { f.home_type.as_str() }, scorer.elasticity_for(f));
            if f.priced_type() && !classes.contains(&e) {
                classes.push(e);
            }
        }
        println!("  size elasticity: {}", classes.join(", "));
        let mut segs: BTreeMap<String, Seg> = BTreeMap::new();
        let mut refusals: BTreeMap<String, usize> = BTreeMap::new();
        for (f, _, r) in &results {
            let mut keys = vec!["ALL".to_string(), format!("type {}", f.home_type)];
            if let Some(b) = &f.borough {
                keys.push(format!("borough {b}"));
            }
            if market == "mi" {
                keys.push(format!("water {}", f.water));
                if let Some(a) = &f.area {
                    keys.push(format!("area {a}"));
                }
            }
            keys.push(format!("sqft {}", if f.ppsf().is_some() { "yes" } else { "no" }));
            if let Ok(s) = r {
                keys.push(format!("level {:?}", s.level));
                keys.push(format!("basis {}", s.basis.as_str()));
            }
            for k in keys {
                segs.entry(k).or_default().add(r);
            }
            if let Err(e) = r {
                *refusals.entry(refusal_key(e)).or_default() += 1;
            }
        }
        header();
        for (k, s) in &segs {
            row(k, s);
        }
        println!("\n  refusals:");
        for (k, n) in &refusals {
            println!("    {n:>5}  {k}");
        }

        let mut priced: Vec<&(Facts, &Value, Result<Score, Refusal>)> = results.iter().filter(|r| r.2.is_ok()).collect();
        priced.sort_by(|a, b| {
            let (x, y) = (a.2.as_ref().unwrap().discount_pct, b.2.as_ref().unwrap().discount_pct);
            y.partial_cmp(&x).unwrap()
        });
        println!("\n  top {top} by discount:");
        println!(
            "  {:>5} {:>9} {:>9} {:<5} {:<38} {:<38} {:>4} {:<5} {:>5} {:>5} {:>6} {:>6} {:>4} {:>8} {:>8}",
            "disc", "price", "baseline", "alert", "address", "group", "n", "basis", "sqft", "ppsf", "medPS", "bd", "type", "p25", "p75"
        );
        for (f, l, r) in priced.iter().take(top) {
            let s = r.as_ref().unwrap();
            let addr = format!(
                "{} {}",
                l["address"].as_str().unwrap_or(""),
                l["unit"].as_str().or(l["waterBody"].as_str()).unwrap_or("")
            );
            println!(
                "  {:>5.1} {:>9} {:>9} {:<5} {:<38} {:<38} {:>4} {:<5} {:>5} {:>5} {:>6} {:>6} {:>4} {:>8} {:>8}",
                s.discount_pct,
                f.price,
                s.baseline,
                s.alert,
                addr.chars().take(38).collect::<String>(),
                s.group.chars().take(38).collect::<String>(),
                s.n,
                s.basis.as_str(),
                f.sqft.map_or("-".into(), |x| format!("{x:.0}")),
                f.ppsf().map_or("-".into(), |x| format!("{x:.0}")),
                s.median_ppsf.map_or("-".into(), |x| format!("{x:.0}")),
                f.beds.map_or("-".into(), |b| b.to_string()),
                f.home_type.chars().take(4).collect::<String>(),
                s.p25,
                s.p75
            );
        }
    }
    ExitCode::SUCCESS
}
