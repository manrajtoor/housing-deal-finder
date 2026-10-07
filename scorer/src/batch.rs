//! One market's scoring pass, as the crawl job runs it (`housedeals-score`).
//!
//! Input: the rows of `GET /api/score-input` for one market (active listings
//! that are not removed, plus homes sold in the last 365 days), each with the
//! score stored for it, if any:
//!
//! ```json
//! {"market": "mi", "now": "2026-10-05T12:00:00Z",
//!  "columns": ["id", "url", ..., "storedDiscount", "storedAlert", "storedPrice", "scoredAt"],
//!  "rows": [["zl:1", "https://...", ...], ...]}
//! ```
//!
//! A row may also be an object keyed by those names. Output: the writes for
//! `POST /api/scores`:
//!
//! ```json
//! {"upserts": [{"id", "price", "discountPct", "alert", "deal"}],
//!  "deletes": ["id", ...],
//!  "alerts":  [{"id", "price", "deal"}],
//!  "stats":   {"rows", "subjects", "priced", "unchanged"}}
//! ```
//!
//! NYC sold rows (Zillow) first get a building type and a neighbourhood from
//! the StreetEasy rows (see [`crate::nyc_sold`]); they are comps only.
//! Every active row is scored against all rows. A stored score is rewritten
//! when it is missing, its discount moved by at least [`Settings::epsilon_pct`],
//! its alert flag flipped, the asking price changed, or the detail page was
//! read after it was scored (the Deal then shows the new detail fields). A
//! stored score is deleted when the listing no longer prices (or is sold).
//! An alert is emitted for a rewritten score the alert rule accepts when the
//! listing became fresh (new and young, relisted, cheaper) within
//! [`Settings::fresh_hours`]; the Worker stores alerts with INSERT OR IGNORE
//! keyed by (listing, price), so a repeat is harmless.

use serde_json::{json, Map, Value};

use crate::alert::AlertRule;
use crate::dates::iso_minus_hours;
use crate::listing::Market;
use crate::nyc_sold;
use crate::score::{deal_json, Options, Scorer};

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub rule: AlertRule,
    /// A listing alerts only when its `freshAt` is this recent.
    pub fresh_hours: i64,
    /// A stored score is rewritten when its discount moved at least this much.
    pub epsilon_pct: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { rule: AlertRule::default(), fresh_hours: 72, epsilon_pct: 0.5 }
    }
}

/// What is stored for a row before this pass.
#[derive(Debug, Clone, PartialEq)]
struct Stored {
    discount_pct: f64,
    alert: bool,
    price: Option<f64>,
    scored_at: Option<String>,
}

const STORED_KEYS: [&str; 4] = ["storedDiscount", "storedAlert", "storedPrice", "scoredAt"];

fn stored_of(row: &Map<String, Value>) -> Option<Stored> {
    Some(Stored {
        discount_pct: row.get("storedDiscount")?.as_f64()?,
        alert: matches!(row.get("storedAlert"), Some(Value::Bool(true))) || row.get("storedAlert").and_then(Value::as_f64) == Some(1.0),
        price: row.get("storedPrice").and_then(Value::as_f64),
        scored_at: row.get("scoredAt").and_then(Value::as_str).map(str::to_string),
    })
}

/// Rows as contract-shaped listing objects (camelCase keys), market filled in.
fn rows_of(input: &Value, market: Market) -> Result<Vec<Map<String, Value>>, String> {
    let rows = input.get("rows").and_then(Value::as_array).ok_or("input needs a \"rows\" array")?;
    let columns: Vec<&str> = match input.get("columns") {
        None | Some(Value::Null) => Vec::new(),
        Some(c) => c
            .as_array()
            .ok_or("\"columns\" must be an array of names")?
            .iter()
            .map(|v| v.as_str().ok_or("\"columns\" must be an array of names"))
            .collect::<Result<_, _>>()?,
    };
    rows.iter()
        .enumerate()
        .map(|(i, r)| {
            let mut m = match r {
                Value::Object(o) => o.clone(),
                Value::Array(a) => {
                    if a.len() != columns.len() {
                        return Err(format!("rows[{i}] has {} values for {} columns", a.len(), columns.len()));
                    }
                    columns.iter().zip(a).map(|(k, v)| (k.to_string(), v.clone())).collect()
                }
                _ => return Err(format!("rows[{i}] is neither an array nor an object")),
            };
            m.entry("market").or_insert_with(|| Value::from(market.as_str()));
            for k in ["compOnly", "storedAlert"] {
                if let Some(n) = m.get(k).and_then(Value::as_f64) {
                    m.insert(k.into(), Value::Bool(n != 0.0));
                }
            }
            Ok(m)
        })
        .collect()
}

/// Scores one market. `input` is described in the module comment.
pub fn run(input: &Value, s: &Settings) -> Result<Value, String> {
    let market = input
        .get("market")
        .and_then(Value::as_str)
        .and_then(Market::parse)
        .ok_or("input needs \"market\": \"nyc\" or \"mi\"")?;
    let now = input.get("now").and_then(Value::as_str).ok_or("input needs \"now\" (ISO time)")?;
    let fresh_cutoff = iso_minus_hours(now, s.fresh_hours).ok_or("\"now\" is not an ISO time")?;
    let rows = rows_of(input, market)?;
    let mut listings: Vec<Value> = rows
        .iter()
        .map(|r| Value::Object(r.iter().filter(|(k, _)| !STORED_KEYS.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect()))
        .collect();
    let sold = (market == Market::Nyc).then(|| nyc_sold::enrich(&mut listings).total());
    let scorer = Scorer::new(&listings, Options::with_rule(s.rule.clone()), now);

    let (mut upserts, mut deletes, mut alerts) = (Vec::new(), Vec::new(), Vec::new());
    let (mut subjects, mut priced, mut unchanged) = (0usize, 0usize, 0usize);
    for (row, l) in rows.iter().zip(&listings) {
        let Some(id) = l.get("id").and_then(Value::as_str) else { continue };
        let stored = stored_of(row);
        let active = l["status"] == "active" && l.get("removedAt").is_none_or(Value::is_null);
        if active {
            subjects += 1;
        }
        let score = match scorer.score(l) {
            Ok(sc) if active => sc,
            _ => {
                if stored.is_some() {
                    deletes.push(Value::from(id));
                }
                continue;
            }
        };
        priced += 1;
        let price = l["price"].as_f64().unwrap_or(0.0);
        let detail_after = |st: &Stored| match (l.get("detailReadAt").and_then(Value::as_str), &st.scored_at) {
            (Some(d), Some(at)) => d > at.as_str(),
            (Some(_), None) => true,
            _ => false,
        };
        let rewrite = stored.as_ref().is_none_or(|st| {
            (st.discount_pct - score.discount_pct).abs() >= s.epsilon_pct
                || st.alert != score.alert
                || st.price != Some(price)
                || detail_after(st)
        });
        if !rewrite {
            unchanged += 1;
            continue;
        }
        let deal = deal_json(l, &score);
        let fresh = l.get("freshAt").and_then(Value::as_str).is_some_and(|f| f >= fresh_cutoff.as_str());
        if score.alert && fresh {
            alerts.push(json!({"id": id, "price": price as i64, "deal": deal}));
        }
        upserts.push(json!({"id": id, "price": price as i64, "discountPct": score.discount_pct, "alert": score.alert, "deal": deal}));
    }
    Ok(json!({
        "market": market.as_str(),
        "upserts": upserts,
        "deletes": deletes,
        "alerts": alerts,
        "stats": {"rows": rows.len(), "subjects": subjects, "priced": priced, "unchanged": unchanged,
                  "nycSold": sold.map(|t| json!({"rows": t.sold, "typedByStreetEasy": t.type_from_streeteasy,
                                                  "placed": t.sold - t.nbhd_none, "usable": t.usable}))},
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: &str = "2026-10-05T12:00:00.000Z";
    const COLS: [&str; 14] = [
        "id", "price", "sqft", "beds", "homeType", "neighborhood", "borough", "status", "compOnly", "freshAt",
        "detailReadAt", "storedDiscount", "storedAlert", "storedPrice",
    ];

    fn apt(id: &str, price: i64, fresh: Option<&str>) -> Vec<Value> {
        vec![
            json!(id), json!(price), json!(1000), json!(2), json!("condo"), json!("Astoria"), json!("queens"),
            json!("active"), json!(0), json!(fresh), Value::Null, Value::Null, Value::Null, Value::Null,
        ]
    }

    fn with_stored(mut r: Vec<Value>, discount: f64, alert: bool, price: i64) -> Vec<Value> {
        r[11] = json!(discount);
        r[12] = json!(i64::from(alert));
        r[13] = json!(price);
        r
    }

    fn input(rows: Vec<Vec<Value>>) -> Value {
        let mut cols: Vec<Value> = COLS.iter().map(|c| json!(c)).collect();
        cols.push(json!("scoredAt"));
        let rows: Vec<Value> = rows
            .into_iter()
            .map(|mut r| {
                r.push(json!("2026-10-05T11:00:00.000Z"));
                Value::Array(r)
            })
            .collect();
        json!({"market": "nyc", "now": NOW, "columns": cols, "rows": rows})
    }

    fn comps() -> Vec<Vec<Value>> {
        (0..10).map(|i| apt(&format!("se:c{i}"), 790_000 + (i % 3) * 10_000, None)).collect()
    }

    fn ids(v: &Value, k: &str) -> Vec<String> {
        v[k].as_array().unwrap().iter().map(|x| x.get("id").unwrap_or(x).as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn scores_everything_new_and_alerts_only_fresh_deals() {
        let mut rows = comps();
        rows.push(apt("se:old", 600_000, Some("2026-10-01T12:00:00.000Z"))); // fresh 4 days ago
        rows.push(apt("se:new", 610_000, Some("2026-10-05T11:30:00.000Z")));
        let out = run(&input(rows), &Settings::default()).unwrap();
        assert_eq!(out["upserts"].as_array().unwrap().len(), 12);
        assert_eq!(ids(&out, "alerts"), vec!["se:new"]);
        let a = &out["alerts"][0];
        assert_eq!((a["price"].as_i64(), a["deal"]["group"].as_str()), (Some(610_000), Some("Astoria · condo · 2bd")));
        assert_eq!(a["deal"]["market"], "nyc");
        let old = out["upserts"].as_array().unwrap().iter().find(|u| u["id"] == "se:old").unwrap();
        assert_eq!(old["alert"], true, "a deal, just not fresh");
        assert!(old["discountPct"].as_f64().unwrap() >= 24.0);
        assert_eq!(out["stats"]["subjects"], 12);
    }

    #[test]
    fn unchanged_scores_are_not_rewritten_and_refused_ones_are_deleted() {
        let first = run(&input(comps()), &Settings::default()).unwrap();
        // Store what the first pass wrote, then run again.
        let rows: Vec<Vec<Value>> = comps()
            .into_iter()
            .zip(first["upserts"].as_array().unwrap())
            .map(|(r, u)| {
                let p = r[1].as_i64().unwrap();
                with_stored(r, u["discountPct"].as_f64().unwrap(), u["alert"].as_bool().unwrap(), p)
            })
            .collect();
        let out = run(&input(rows.clone()), &Settings::default()).unwrap();
        assert!(out["upserts"].as_array().unwrap().is_empty(), "{out}");
        assert_eq!(out["stats"]["unchanged"], 10);

        // A small price change (discount moves < 0.5) still rewrites: the Deal shows the price.
        let mut r2 = rows.clone();
        r2[0][1] = json!(rows[0][1].as_i64().unwrap() - 1000);
        let out = run(&input(r2), &Settings::default()).unwrap();
        assert_eq!(ids(&out, "upserts"), vec!["se:c0"]);

        // A detail page read after the score was stored rewrites it.
        let mut r3 = rows.clone();
        r3[1][10] = json!("2026-10-05T11:45:00.000Z");
        assert_eq!(ids(&run(&input(r3), &Settings::default()).unwrap(), "upserts"), vec!["se:c1"]);

        // Too few comps left: every stored score is deleted. A sold row with a score too.
        let mut few: Vec<Vec<Value>> = rows[..5].to_vec();
        few[4][7] = json!("sold");
        let out = run(&input(few), &Settings::default()).unwrap();
        assert!(out["upserts"].as_array().unwrap().is_empty());
        assert_eq!(out["deletes"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn a_moved_discount_or_flipped_alert_is_rewritten() {
        let mut rows = comps();
        rows.push(with_stored(apt("se:d", 600_000, None), 10.0, false, 600_000));
        let out = run(&input(rows), &Settings::default()).unwrap();
        assert!(ids(&out, "upserts").contains(&"se:d".to_string()));
        assert!(out["alerts"].as_array().unwrap().is_empty(), "not fresh");
    }

    #[test]
    fn rule_settings_apply_and_objects_are_accepted() {
        let rows: Vec<Value> = (0..7)
            .map(|i| json!({"id": format!("zl:{i}"), "price": 800_000, "sqft": 2000, "homeType": "single_family",
                            "area": "traverse", "waterType": "inland", "status": "active", "compOnly": false}))
            .chain([json!({"id": "zl:deal", "price": 600_000, "sqft": 2000, "homeType": "single_family", "area": "traverse",
                           "waterType": "inland", "status": "active", "compOnly": false, "freshAt": NOW})])
            .collect();
        let inp = json!({"market": "mi", "now": NOW, "rows": rows});
        assert_eq!(ids(&run(&inp, &Settings::default()).unwrap(), "alerts"), vec!["zl:deal"]);
        let strict = Settings { rule: AlertRule { min_discount_pct: 30.0, ..AlertRule::default() }, ..Settings::default() };
        assert!(run(&inp, &strict).unwrap()["alerts"].as_array().unwrap().is_empty());
        let cheap = Settings { rule: AlertRule { max_price: 500_000.0, ..AlertRule::default() }, ..Settings::default() };
        assert!(run(&inp, &cheap).unwrap()["alerts"].as_array().unwrap().is_empty());
    }

    #[test]
    fn nyc_sold_rows_are_placed_and_counted_as_comps_but_never_scored() {
        let se = |i: usize| {
            json!({"id": format!("se:{i}"), "price": 800_000, "sqft": 1000, "beds": 2, "homeType": "condo",
                   "neighborhood": "Astoria", "borough": "queens", "status": "active", "compOnly": false,
                   "address": format!("{} 30th Street", 10 + i), "lat": 40.7644, "lon": -73.9235})
        };
        let sold = |i: usize, addr: &str, stored: bool| {
            json!({"id": format!("zl:{i}"), "price": 790_000, "sqft": 1000, "beds": 2, "homeType": "condo",
                   "borough": "queens", "status": "sold", "soldAt": "2026-08-01T00:00:00Z", "compOnly": true,
                   "address": addr, "unit": "#2A", "lat": 40.7645, "lon": -73.9236,
                   "storedDiscount": if stored { json!(5.0) } else { Value::Null }})
        };
        // 4 StreetEasy condos (too few alone), 4 Zillow sales in their buildings, a subject.
        let mut rows: Vec<Value> = (0..4).map(se).collect();
        rows.extend((0..4).map(|i| sold(100 + i, &format!("{} 30th St APT 2A", 10 + i), i == 0)));
        // An uncertain Zillow CONDO nowhere near a StreetEasy building: not a comp.
        rows.push(sold(200, "1 Nowhere Ave", false));
        let mut subject = se(9);
        subject["price"] = json!(600_000);
        rows.push(subject);
        let inp = json!({"market": "nyc", "now": NOW, "rows": rows});
        let mut rule = AlertRule::default();
        rule.min_comps_nyc = 8;
        let out = run(&inp, &Settings { rule, ..Settings::default() }).unwrap();
        let ups = out["upserts"].as_array().unwrap();
        let s = ups.iter().find(|u| u["id"] == "se:9").expect("priced with the sold comps");
        assert_eq!(s["deal"]["n"], 8, "4 StreetEasy + 4 sold comps");
        assert!(ups.iter().all(|u| u["id"].as_str().unwrap().starts_with("se:")), "sold rows are never subjects");
        assert_eq!(ids(&out, "deletes"), vec!["zl:100"], "a sold row's stray score is deleted");
        assert_eq!(out["stats"]["subjects"], 5);
        assert_eq!(out["stats"]["nycSold"], json!({"rows": 5, "typedByStreetEasy": 4, "placed": 5, "usable": 4}));
    }

    #[test]
    fn bad_input_is_an_error() {
        assert!(run(&json!({"now": NOW, "rows": []}), &Settings::default()).is_err());
        assert!(run(&json!({"market": "mi", "rows": []}), &Settings::default()).is_err());
        assert!(run(&json!({"market": "mi", "now": NOW, "columns": ["id"], "rows": [["a", 1]]}), &Settings::default()).is_err());
        let empty = run(&json!({"market": "mi", "now": NOW, "columns": [], "rows": []}), &Settings::default()).unwrap();
        assert_eq!(empty["stats"]["rows"], 0);
    }
}
