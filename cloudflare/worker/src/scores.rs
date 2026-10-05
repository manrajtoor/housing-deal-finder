//! Stored scores (`listing_scores`).
//!
//! Scoring every listing does not fit the Free plan's ~10 ms of CPU per
//! request, so an ingest rescores only the units its changed listings belong
//! to, and stores one row per listing. `/api/deals` reads those rows back and
//! never runs the scorer.
//!
//! Units (what is loaded and rescored):
//! - NYC: neighbourhood × home type (covers comp groups 1 and 2). Subjects
//!   that fall through to group 3 (borough × home type × beds) get a second,
//!   targeted load of that borough group before they are scored for good.
//! - Michigan: water type, all areas (covers both comp groups; ~500 rows in all).
//!
//! Writes are limited to rows whose stored score is missing or moved: the
//! changed listings themselves, rows without a stored score, rows whose
//! discount moved by ≥ [`RESCORE_EPSILON_PCT`] or whose alert flag flipped,
//! and deletions for rows that no longer price.

use std::collections::{BTreeSet, HashMap};

use serde_json::{json, Value};

use scorer::group::{beds_bucket, Level};
use scorer::listing::{Facts, Market};
use scorer::{deal_json, Refusal, Score, Scorer};

use crate::ingest::row_select;
use crate::sql::{Param, Stmt};

/// A stored score is rewritten when its discount moved at least this much.
pub const RESCORE_EPSILON_PCT: f64 = 0.5;
/// Units per load query (≤ 3 parameters each + 1, under D1's 100).
const UNITS_PER_QUERY: usize = 25;
/// Michigan areas (CONTRACT.md `area`); the group query names them so SQLite
/// can use idx_listings_mi_group for "all areas × water type".
pub const MI_AREAS: [&str; 2] = ["petoskey", "traverse"];

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Unit {
    /// (neighbourhood, home type)
    NycNbhd(String, String),
    /// (borough, home type, beds bucket)
    NycBorough(String, String, String),
    /// water type (null stored as `other`)
    MiWater(String),
}

/// The unit whose rows must be loaded to score `l` (contract shape).
pub fn unit_of(l: &Value) -> Option<Unit> {
    let f = Facts::from_json(l)?;
    match f.market {
        Market::Nyc => match (&f.neighborhood, &f.borough, beds_bucket(f.beds)) {
            (Some(n), _, _) => Some(Unit::NycNbhd(n.clone(), f.home_type)),
            (None, Some(b), Some(bk)) => Some(Unit::NycBorough(b.clone(), f.home_type, bk.to_string())),
            _ => None,
        },
        Market::Mi => Some(Unit::MiWater(f.water)),
    }
}

/// The borough unit a NYC listing falls back to, if it has one.
pub fn borough_unit_of(l: &Value) -> Option<Unit> {
    let f = Facts::from_json(l)?;
    (f.market == Market::Nyc).then_some(())?;
    Some(Unit::NycBorough(f.borough?, f.home_type, beds_bucket(f.beds)?.to_string()))
}

/// True when `l` belongs to `u` as a subject (its scores are rewritten).
pub fn in_unit(l: &Value, u: &Unit) -> bool {
    unit_of(l).as_ref() == Some(u)
}

fn unit_term(u: &Unit, params: &mut Vec<Param>) -> String {
    let mut p = |v: &str| {
        params.push(Param::Text(v.to_string()));
        format!("?{}", params.len())
    };
    match u {
        Unit::NycNbhd(n, t) => format!("(l.market = 'nyc' AND l.neighborhood = {} AND l.home_type = {})", p(n), p(t)),
        Unit::NycBorough(b, t, bucket) => {
            let beds = match bucket.as_str() {
                "1" => "l.beds <= 1".to_string(),
                "4+" => "l.beds >= 4".to_string(),
                n => format!("l.beds = {}", p(n)),
            };
            format!("(l.market = 'nyc' AND l.borough = {} AND l.home_type = {} AND {beds})", p(b), p(t))
        }
        Unit::MiWater(w) => {
            let areas = MI_AREAS.map(|a| format!("'{a}'")).join(", ");
            if w == "other" {
                // Unread water type counts as "other": two indexable terms.
                format!(
                    "(l.market = 'mi' AND l.area IN ({areas}) AND l.water_type = 'other') OR \
                     (l.market = 'mi' AND l.area IN ({areas}) AND l.water_type IS NULL)"
                )
            } else {
                format!("(l.market = 'mi' AND l.area IN ({areas}) AND l.water_type = {})", p(w))
            }
        }
    }
}

/// Comps and subjects of `units`: active unexpired rows, and sold rows sold
/// on or after `sold_cutoff` (YYYY-MM-DD), each with its stored score.
///
/// Each unit is an OR term that SQLite answers from its own index (MULTI-INDEX
/// OR); the status filter is applied to those rows only. `+l.removed_at`
/// stops the planner from preferring idx_listings_market_status instead.
pub fn group_queries(units: &[Unit], sold_cutoff: &str) -> Vec<Stmt> {
    units
        .chunks(UNITS_PER_QUERY)
        .map(|chunk| {
            let mut params = vec![Param::Text(sold_cutoff.to_string())];
            let terms: Vec<String> = chunk.iter().map(|u| unit_term(u, &mut params)).collect();
            Stmt::new(
                format!(
                    "SELECT {}, s.discount_pct AS stored_discount, s.alert AS stored_alert \
                     FROM listings l LEFT JOIN listing_scores s ON s.listing_id = l.id \
                     WHERE ({}) AND ((l.status = 'active' AND +l.removed_at IS NULL) OR (l.status = 'sold' AND l.sold_at >= ?1))",
                    row_select("l."),
                    terms.join(" OR ")
                ),
                params,
            )
        })
        .collect()
}

/// What is stored for a row before rescoring.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stored {
    pub discount_pct: f64,
    pub alert: bool,
}

pub fn stored_from_row(row: &Value) -> Option<Stored> {
    Some(Stored {
        discount_pct: row.get("stored_discount")?.as_f64()?,
        alert: row.get("stored_alert").and_then(Value::as_f64) == Some(1.0),
    })
}

/// The outcome for one subject.
#[derive(Debug, Clone)]
pub struct Scored {
    pub listing: Value,
    pub result: Result<Score, Refusal>,
}

impl Scored {
    pub fn id(&self) -> &str {
        self.listing["id"].as_str().unwrap_or_default()
    }
}

/// True when the result is final with only the neighbourhood rows loaded:
/// NYC group 1/2 answered (or refused as implausible there). Michigan loads
/// are always complete.
pub fn needs_borough(s: &Scored) -> bool {
    let nyc = s.listing["market"] == "nyc";
    let level = match &s.result {
        Ok(sc) => Some(sc.level),
        Err(Refusal::Implausible { level, .. }) => Some(*level),
        Err(Refusal::NoGroup(_)) => None,
        Err(_) => return false,
    };
    nyc && !matches!(level, Some(Level::NycBeds | Level::NycType)) && borough_unit_of(&s.listing).is_some()
}

/// Scores `subjects` against `rows` (all comps of their groups).
pub fn score_all(scorer: &Scorer, subjects: &[Value]) -> Vec<Scored> {
    subjects.iter().map(|l| Scored { listing: l.clone(), result: scorer.score(l) }).collect()
}

/// Score writes for rescored subjects (see the module comment for which).
pub fn upsert_statements(
    scored: &[Scored],
    changed: &BTreeSet<String>,
    stored: &HashMap<String, Stored>,
    now: &str,
) -> Vec<Stmt> {
    scored
        .iter()
        .filter_map(|s| {
            let id = s.id().to_string();
            let before = stored.get(&id);
            match &s.result {
                Err(_) => before.map(|_| Stmt::new("DELETE FROM listing_scores WHERE listing_id = ?1", vec![Param::Text(id)])),
                Ok(score) => {
                    let moved = before.is_none_or(|b| {
                        (b.discount_pct - score.discount_pct).abs() >= RESCORE_EPSILON_PCT || b.alert != score.alert
                    });
                    if !moved && !changed.contains(&id) {
                        return None;
                    }
                    Some(score_upsert(&s.listing, score, now))
                }
            }
        })
        .collect()
}

pub fn score_upsert(l: &Value, score: &Score, now: &str) -> Stmt {
    Stmt::new(
        "INSERT OR REPLACE INTO listing_scores (listing_id, market, price, discount_pct, alert, deal, scored_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        vec![
            Param::text(l["id"].as_str()),
            Param::text(l["market"].as_str()),
            Param::Int(l["price"].as_f64().unwrap_or(0.0) as i64),
            Param::Real(score.discount_pct),
            Param::Int(i64::from(score.alert)),
            Param::Text(deal_json(l, score).to_string()),
            Param::Text(now.to_string()),
        ],
    )
}

// ---------------------------------------------------------------------------
// GET /api/deals

pub const DEFAULT_DEALS_LIMIT: usize = 50;
pub const MAX_DEALS_LIMIT: usize = 500;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct DealsQuery {
    pub market: Option<String>,
    pub max_price: Option<f64>,
    pub min_discount: Option<f64>,
    pub limit: usize,
}

fn parse_num(k: &str, v: &str) -> Result<f64, String> {
    v.trim().parse::<f64>().ok().filter(|f| f.is_finite()).ok_or(format!("{k} must be a number"))
}

pub fn parse_market(v: &str) -> Result<Option<String>, String> {
    match v.trim() {
        "" => Ok(None),
        m @ ("nyc" | "mi") => Ok(Some(m.to_string())),
        m => Err(format!("market must be nyc or mi, not {m:?}")),
    }
}

pub fn parse_limit(v: &str, max: usize) -> Result<usize, String> {
    v.trim().parse::<usize>().ok().filter(|n| *n >= 1).map(|n| n.min(max)).ok_or("limit must be a positive integer".into())
}

impl DealsQuery {
    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Result<DealsQuery, String> {
        let mut q = DealsQuery { limit: DEFAULT_DEALS_LIMIT, ..Default::default() };
        for (k, v) in pairs {
            if v.trim().is_empty() {
                continue;
            }
            match k {
                "market" => q.market = parse_market(v)?,
                "maxPrice" => q.max_price = Some(parse_num(k, v)?),
                "minDiscount" => q.min_discount = Some(parse_num(k, v)?),
                "limit" => q.limit = parse_limit(v, MAX_DEALS_LIMIT)?,
                _ => {}
            }
        }
        Ok(q)
    }
}

/// Best discount first, straight from listing_scores:
///   SEARCH listing_scores USING INDEX idx_scores_market_discount (market=? AND discount_pct>?)
pub fn deals_query(q: &DealsQuery) -> Stmt {
    let mut sql = String::from("SELECT deal FROM listing_scores WHERE 1 = 1");
    let mut params = Vec::new();
    if let Some(m) = &q.market {
        params.push(Param::Text(m.clone()));
        sql.push_str(&format!(" AND market = ?{}", params.len()));
    }
    if let Some(d) = q.min_discount {
        params.push(Param::Real(d));
        sql.push_str(&format!(" AND discount_pct >= ?{}", params.len()));
    }
    if let Some(p) = q.max_price {
        params.push(Param::Real(p));
        sql.push_str(&format!(" AND price <= ?{}", params.len()));
    }
    params.push(Param::Int(q.limit as i64));
    sql.push_str(&format!(" ORDER BY discount_pct DESC LIMIT ?{}", params.len()));
    Stmt::new(sql, params)
}

pub fn parse_deal_rows(rows: &[Value], extra: &[(&str, &str)]) -> Vec<Value> {
    rows.iter()
        .filter_map(|r| {
            let mut d: Value = serde_json::from_str(r.get("deal")?.as_str()?).ok()?;
            for (col, key) in extra {
                d[*key] = r.get(*col).cloned().unwrap_or(Value::Null);
            }
            Some(d)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// POST /api/listings/needs-detail

pub const DEFAULT_NEEDS_DETAIL_LIMIT: usize = 15;
pub const MAX_NEEDS_DETAIL_LIMIT: usize = 200;
/// NYC detail pages (maintenance, taxes) are read for listings this far under.
pub const NYC_DETAIL_MIN_DISCOUNT: f64 = 10.0;

pub fn parse_needs_detail(body: &Value) -> Result<(String, usize), String> {
    let market = body.get("market").and_then(Value::as_str).unwrap_or_default();
    let market = parse_market(market)?.ok_or("market is required (nyc or mi)")?;
    let limit = match body.get("limit") {
        None | Some(Value::Null) => DEFAULT_NEEDS_DETAIL_LIMIT,
        Some(v) => v
            .as_f64()
            .filter(|n| *n >= 1.0)
            .map(|n| (n as usize).min(MAX_NEEDS_DETAIL_LIMIT))
            .ok_or("limit must be a positive number")?,
    };
    Ok((market, limit))
}

/// Michigan sold comps without a detail read, sold on or after `sold_cutoff`
/// (YYYY-MM-DD), newest sale first. Listed after the active ones.
///   SEARCH listings USING INDEX idx_listings_needs_detail_sold (market=? AND sold_at>?)
pub fn needs_detail_sold_query(limit: usize, sold_cutoff: &str) -> Stmt {
    Stmt::new(
        "SELECT id, url FROM listings \
         WHERE market = 'mi' AND detail_read_at IS NULL AND status = 'sold' AND sold_at >= ?1 \
         ORDER BY sold_at DESC LIMIT ?2",
        vec![Param::Text(sold_cutoff.to_string()), Param::Int(limit as i64)],
    )
}

/// Michigan: active listings without a detail read, newest first.
///   SEARCH listings USING INDEX idx_listings_needs_detail (market=?)
/// NYC: active listings ≤ `max_price` scoring ≥ 10% under, without a detail read.
///   SEARCH s USING INDEX idx_scores_market_discount (market=? AND discount_pct>?)
///   SEARCH l USING INDEX sqlite_autoindex_listings_1 (id=?)
pub fn needs_detail_query(market: &str, limit: usize, max_price: f64) -> Stmt {
    if market == "mi" {
        Stmt::new(
            "SELECT id, url FROM listings \
             WHERE market = 'mi' AND detail_read_at IS NULL AND status = 'active' AND removed_at IS NULL \
             ORDER BY first_seen DESC LIMIT ?1",
            vec![Param::Int(limit as i64)],
        )
    } else {
        Stmt::new(
            "SELECT l.id, l.url FROM listing_scores s JOIN listings l ON l.id = s.listing_id \
             WHERE s.market = 'nyc' AND s.discount_pct >= ?1 AND s.price <= ?2 \
             AND l.detail_read_at IS NULL AND l.status = 'active' AND l.removed_at IS NULL \
             ORDER BY s.discount_pct DESC LIMIT ?3",
            vec![Param::Real(NYC_DETAIL_MIN_DISCOUNT), Param::Real(max_price), Param::Int(limit as i64)],
        )
    }
}

// ---------------------------------------------------------------------------
// Expiry (daily cron) and /api/stats

/// A market is expired only when a full sweep of it ran since `full_since`,
/// so a crawler outage never retires (and later "relists" and alerts on)
/// every listing at once.
pub fn expire_statements(now: &str, cutoff: &str, full_since: &str, crawl_keep_since: &str) -> Vec<Stmt> {
    vec![
        Stmt::new(
            "UPDATE listings SET removed_at = ?1 \
             WHERE market IN (SELECT DISTINCT market FROM crawls WHERE mode = 'full' AND seen_at >= ?3) \
             AND status = 'active' AND removed_at IS NULL AND last_seen < ?2",
            vec![Param::Text(now.into()), Param::Text(cutoff.into()), Param::Text(full_since.into())],
        ),
        Stmt::new(
            "DELETE FROM listing_scores WHERE listing_id IN \
             (SELECT id FROM listings WHERE market IN ('nyc', 'mi') AND status = 'active' AND removed_at = ?1)",
            vec![Param::Text(now.into())],
        ),
        Stmt::new("DELETE FROM crawls WHERE seen_at < ?1", vec![Param::Text(crawl_keep_since.into())]),
    ]
}

/// Counts per market and status (removed rows as "expired"):
///   SCAN listings USING COVERING INDEX idx_listings_market_status
pub const COUNTS_SQL: &str = "SELECT market, CASE WHEN removed_at IS NULL THEN status ELSE 'expired' END AS status, \
     COUNT(*) AS n FROM listings GROUP BY market, 2 ORDER BY market, 2";
pub const ALERT_COUNTS_SQL: &str = "SELECT market, COUNT(*) AS n FROM deal_alerts GROUP BY market ORDER BY market";
pub const SCORED_COUNTS_SQL: &str = "SELECT market, COUNT(*) AS n FROM listing_scores GROUP BY market ORDER BY market";
/// Last crawl per (market, mode), from the primary key.
pub const CRAWLS_SQL: &str = "SELECT market, mode, MAX(seen_at) AS at FROM crawls GROUP BY market, mode ORDER BY market, mode";

/// `GET /api/stats`: {generatedAt, counts:[{market,status,n}], crawls:[{market,mode,at}], alerts, scored}.
pub fn stats_json(now: &str, counts: &[Value], crawls: &[Value], alerts: &[Value], scored: &[Value]) -> Value {
    let n = |r: &Value| r.get("n").and_then(Value::as_f64).unwrap_or(0.0) as i64;
    let s = |r: &Value, k: &str| r.get(k).cloned().unwrap_or(Value::Null);
    json!({
        "generatedAt": now,
        "counts": counts.iter().map(|r| json!({"market": s(r, "market"), "status": s(r, "status"), "n": n(r)})).collect::<Vec<_>>(),
        "crawls": crawls.iter().map(|r| json!({"market": s(r, "market"), "mode": s(r, "mode"), "at": s(r, "at")})).collect::<Vec<_>>(),
        "alerts": alerts.iter().map(|r| json!({"market": s(r, "market"), "n": n(r)})).collect::<Vec<_>>(),
        "scored": scored.iter().map(|r| json!({"market": s(r, "market"), "n": n(r)})).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_follow_the_market() {
        let nyc = json!({"id": "a", "market": "nyc", "price": 1, "homeType": "condo", "neighborhood": "Astoria",
                         "borough": "queens", "beds": 5});
        assert_eq!(unit_of(&nyc), Some(Unit::NycNbhd("Astoria".into(), "condo".into())));
        assert_eq!(borough_unit_of(&nyc), Some(Unit::NycBorough("queens".into(), "condo".into(), "4+".into())));
        let nyc2 = json!({"id": "a", "market": "nyc", "price": 1, "homeType": "coop", "borough": "bronx", "beds": 2});
        assert_eq!(unit_of(&nyc2), Some(Unit::NycBorough("bronx".into(), "coop".into(), "2".into())));
        let mi = json!({"id": "b", "market": "mi", "price": 1, "waterType": null});
        assert_eq!(unit_of(&mi), Some(Unit::MiWater("other".into())));
    }

    #[test]
    fn group_queries_are_chunked_and_numbered() {
        let units: Vec<Unit> = (0..30).map(|i| Unit::NycNbhd(format!("n{i}"), "condo".into())).collect();
        let qs = group_queries(&units, "2025-10-05");
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].params.len(), 1 + 2 * 25);
        assert!(qs[0].sql.contains("(l.market = 'nyc' AND l.neighborhood = ?2 AND l.home_type = ?3) OR"));
        assert!(qs[0].sql.contains("l.sold_at >= ?1"));
        let q = &group_queries(&[Unit::NycBorough("queens".into(), "condo".into(), "4+".into()), Unit::MiWater("other".into())], "D")[0];
        assert!(q.sql.contains("l.beds >= 4"));
        assert!(q.sql.contains("l.water_type IS NULL"));
        assert_eq!(q.params.len(), 3);
    }

    #[test]
    fn deals_query_numbers_its_filters() {
        let q = DealsQuery::from_pairs([("market", "mi"), ("maxPrice", "900000"), ("minDiscount", "15"), ("limit", "9999")]).unwrap();
        assert_eq!(q.limit, MAX_DEALS_LIMIT);
        let s = deals_query(&q);
        assert!(s.sql.ends_with("market = ?1 AND discount_pct >= ?2 AND price <= ?3 ORDER BY discount_pct DESC LIMIT ?4"), "{}", s.sql);
        assert!(DealsQuery::from_pairs([("market", "la")]).is_err());
        assert!(DealsQuery::from_pairs([("limit", "-1")]).is_err());
        assert_eq!(deals_query(&DealsQuery::from_pairs([]).unwrap()).params, vec![Param::Int(50)]);
    }

    #[test]
    fn needs_detail_body() {
        assert_eq!(parse_needs_detail(&json!({"market": "mi"})).unwrap(), ("mi".into(), 15));
        assert_eq!(parse_needs_detail(&json!({"market": "nyc", "limit": 1000})).unwrap().1, MAX_NEEDS_DETAIL_LIMIT);
        assert!(parse_needs_detail(&json!({})).is_err());
        assert!(parse_needs_detail(&json!({"market": "mi", "limit": 0})).is_err());
    }
}
