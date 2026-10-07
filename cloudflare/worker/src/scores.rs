//! Scores are computed by the crawl job (`housedeals-score`, scorer/), not
//! here: wasm plus converting D1 rows into Rust values cost far more than the
//! Free plan's ~10 ms of CPU per request. The Worker only moves data:
//!
//! - `GET /api/score-input`: a page of one market's scoring rows, built as
//!   one JSON string by SQLite and passed through untouched.
//! - `POST /api/scores`: the job's writes (score upserts, deletions, alerts),
//!   bound straight into statements; nothing is read back.
//! - `/api/deals`, `/api/alerts`, `/api/stats`: also one JSON string each,
//!   built by SQLite from the stored Deal JSON.
//!
//! Plus the needs-detail list and the daily expiry.

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::Value;

use crate::sql::{Param, Stmt};

// ---------------------------------------------------------------------------
// Query-string helpers

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

// ---------------------------------------------------------------------------
// GET /api/score-input?market=&after=&limit=

pub const DEFAULT_SCORE_INPUT_LIMIT: usize = 1000;
pub const MAX_SCORE_INPUT_LIMIT: usize = 1000;
/// Sold homes count as comps this long after the sale (scorer `sold_comp_days`).
pub const SOLD_COMP_DAYS: i64 = 365;

#[derive(Debug, Clone, PartialEq)]
pub struct ScoreInputQuery {
    pub market: String,
    /// Rows with an id greater than this (the previous page's `last`).
    pub after: String,
    pub limit: usize,
}

impl ScoreInputQuery {
    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Result<ScoreInputQuery, String> {
        let mut q = ScoreInputQuery { market: String::new(), after: String::new(), limit: DEFAULT_SCORE_INPUT_LIMIT };
        for (k, v) in pairs {
            match k {
                "market" => q.market = parse_market(v)?.unwrap_or_default(),
                "after" => q.after = v.to_string(),
                "limit" if !v.trim().is_empty() => q.limit = parse_limit(v, MAX_SCORE_INPUT_LIMIT)?,
                _ => {}
            }
        }
        if q.market.is_empty() {
            return Err("market is required (nyc or mi)".into());
        }
        Ok(q)
    }
}

/// (name in the output, column of the page subquery, expression in json_array).
/// Names are the contract's camelCase fields, plus the stored score.
/// `lat`/`lon` (with `address`) let the scorer place NYC sold rows, which
/// have no StreetEasy neighbourhood, among the StreetEasy rows (scorer
/// `nyc_sold`).
pub const INPUT_COLUMNS: [(&str, &str, &str); 36] = [
    ("id", "l.id", "id"),
    ("url", "l.url", "url"),
    ("address", "l.address", "address"),
    ("unit", "l.unit", "unit"),
    ("city", "l.city", "city"),
    ("lat", "l.lat", "lat"),
    ("lon", "l.lon", "lon"),
    ("neighborhood", "l.neighborhood", "neighborhood"),
    ("borough", "l.borough", "borough"),
    ("county", "l.county", "county"),
    ("area", "l.area", "area"),
    ("price", "l.price", "price"),
    ("beds", "l.beds", "beds"),
    ("baths", "l.baths", "baths"),
    ("sqft", "l.sqft", "sqft"),
    ("lotSqft", "l.lot_sqft", "lot_sqft"),
    ("homeType", "l.home_type", "home_type"),
    ("zestimate", "l.zestimate", "zestimate"),
    ("daysOnMarket", "l.days_on_market", "days_on_market"),
    ("photoUrl", "l.photo_url", "photo_url"),
    ("waterType", "l.water_type", "water_type"),
    ("waterBody", "l.water_body", "water_body"),
    ("waterSource", "l.water_source", "water_source"),
    ("frontageFt", "l.frontage_ft", "frontage_ft"),
    ("maintenance", "l.maintenance", "maintenance"),
    ("taxes", "l.taxes", "taxes"),
    ("status", "l.status", "status"),
    ("soldAt", "l.sold_at", "sold_at"),
    ("removedAt", "l.removed_at", "removed_at"),
    ("compOnly", "l.comp_only", "json(CASE WHEN comp_only THEN 'true' ELSE 'false' END)"),
    ("freshAt", "l.fresh_at", "fresh_at"),
    ("detailReadAt", "l.detail_read_at", "detail_read_at"),
    ("storedDiscount", "s.discount_pct", "stored_discount"),
    ("storedAlert", "s.alert", "json(CASE stored_alert WHEN 1 THEN 'true' WHEN 0 THEN 'false' END)"),
    ("storedPrice", "s.price", "stored_price"),
    ("scoredAt", "s.scored_at", "scored_at"),
];

fn input_alias(name: &str, col: &str) -> String {
    match name {
        "storedDiscount" => "stored_discount".into(),
        "storedAlert" => "stored_alert".into(),
        "storedPrice" => "stored_price".into(),
        _ => col.trim_start_matches("l.").trim_start_matches("s.").to_string(),
    }
}

/// One page of scoring rows, as a single JSON text column `body`:
/// `{"market", "columns": [...], "rows": [[...], ...], "last": id|null, "n"}`.
/// Rows: active listings not removed, and sold rows sold on or after
/// `sold_cutoff` (YYYY-MM-DD), by id, after `q.after`.
///   SEARCH l USING INDEX idx_listings_market_id (market=? AND id>?)
///   SEARCH s USING INDEX sqlite_autoindex_listing_scores_1 (listing_id=?) LEFT-JOIN
pub fn score_input_query(q: &ScoreInputQuery, sold_cutoff: &str) -> Stmt {
    let inner: Vec<String> = INPUT_COLUMNS.iter().map(|(n, c, _)| format!("{c} AS {}", input_alias(n, c))).collect();
    let outer: Vec<&str> = INPUT_COLUMNS.iter().map(|c| c.2).collect();
    let names = serde_json::to_string(&INPUT_COLUMNS.iter().map(|c| c.0).collect::<Vec<_>>()).unwrap_or_default();
    Stmt::new(
        format!(
            "SELECT json_object('market', ?1, 'columns', json('{names}'), 'rows', json_group_array(json_array({})), \
             'last', max(id), 'n', count(*)) AS body FROM (\
             SELECT {} FROM listings l LEFT JOIN listing_scores s ON s.listing_id = l.id \
             WHERE l.market = ?1 AND l.id > ?2 \
             AND ((l.status = 'active' AND l.removed_at IS NULL) OR (l.status = 'sold' AND l.sold_at >= ?3)) \
             ORDER BY l.id LIMIT ?4)",
            outer.join(", "),
            inner.join(", ")
        ),
        vec![
            Param::Text(q.market.clone()),
            Param::Text(q.after.clone()),
            Param::Text(sold_cutoff.to_string()),
            Param::Int(q.limit as i64),
        ],
    )
}

// ---------------------------------------------------------------------------
// POST /api/scores

/// Upserts + deletes + alerts in one request.
pub const MAX_SCORE_ITEMS: usize = 100;
const MAX_ID_CHARS: usize = 100;
const MAX_DEAL_BYTES: usize = 16_000;

#[derive(Debug, Deserialize)]
pub struct ScoresBody {
    pub market: String,
    #[serde(default)]
    pub upserts: Vec<ScoreUpsert>,
    #[serde(default)]
    pub deletes: Vec<String>,
    #[serde(default)]
    pub alerts: Vec<AlertIn>,
}

#[derive(Debug, Deserialize)]
pub struct ScoreUpsert {
    pub id: String,
    pub price: f64,
    #[serde(rename = "discountPct")]
    pub discount_pct: f64,
    pub alert: bool,
    /// Kept as text: validated as JSON, never turned into a tree.
    pub deal: Box<RawValue>,
}

#[derive(Debug, Deserialize)]
pub struct AlertIn {
    pub id: String,
    pub price: f64,
    pub deal: Box<RawValue>,
}

/// What `POST /api/scores` changed.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScoresOutcome {
    pub upserted: u64,
    pub deleted: u64,
    pub new_alerts: u64,
    /// Stored alerts whose Deal was refreshed (detail fields read later).
    pub patched_alerts: u64,
    pub rows_written: u64,
}

fn check_id(id: &str) -> Result<(), String> {
    if id.trim().is_empty() || id.chars().count() > MAX_ID_CHARS {
        return Err(format!("bad id {id:?}"));
    }
    Ok(())
}

fn check_price(id: &str, p: f64) -> Result<i64, String> {
    if p.is_finite() && (1.0..=1e9).contains(&p) {
        Ok(p.round() as i64)
    } else {
        Err(format!("{id}: price must be a number in 1..1e9"))
    }
}

fn check_deal<'a>(id: &str, d: &'a RawValue) -> Result<&'a str, String> {
    let s = d.get();
    if !s.starts_with('{') || s.len() > MAX_DEAL_BYTES {
        return Err(format!("{id}: deal must be a JSON object under {MAX_DEAL_BYTES} bytes"));
    }
    Ok(s)
}

/// The id is an active, unexpired listing of the market (a listing expired
/// between the job's read and its write gets no score or alert).
const ACTIVE_LISTING: &str = "EXISTS (SELECT 1 FROM listings WHERE id = ?1 AND market = ?2 AND status = 'active' AND removed_at IS NULL)";

/// Validated statements for a `POST /api/scores` body, alerts first, and how
/// many of each kind (alerts: insert + patch per alert).
pub struct ScorePlan {
    pub stmts: Vec<Stmt>,
    pub alerts: usize,
    pub upserts: usize,
    pub deletes: usize,
}

pub fn parse_scores(text: &str, now: &str) -> Result<ScorePlan, String> {
    let b: ScoresBody = serde_json::from_str(text).map_err(|e| format!("body is not a scores object: {e}"))?;
    let market = parse_market(&b.market)?.ok_or("market is required (nyc or mi)")?;
    let n = b.upserts.len() + b.deletes.len() + b.alerts.len();
    if n > MAX_SCORE_ITEMS {
        return Err(format!("too many items in one request ({n} > {MAX_SCORE_ITEMS})"));
    }
    let mut stmts = Vec::with_capacity(n + b.alerts.len());
    let m = || Param::Text(market.clone());
    for a in &b.alerts {
        check_id(&a.id)?;
        let price = check_price(&a.id, a.price)?;
        let deal = check_deal(&a.id, &a.deal)?;
        stmts.push(Stmt::new(
            format!(
                "INSERT OR IGNORE INTO deal_alerts (listing_id, price, market, created_at, deal) \
                 SELECT ?1, ?3, ?2, ?4, ?5 WHERE {ACTIVE_LISTING}"
            ),
            vec![Param::Text(a.id.clone()), m(), Param::Int(price), Param::Text(now.into()), Param::Text(deal.into())],
        ));
        stmts.push(Stmt::new(
            "UPDATE deal_alerts SET deal = ?3 WHERE listing_id = ?1 AND price = ?2 AND deal != ?3",
            vec![Param::Text(a.id.clone()), Param::Int(price), Param::Text(deal.into())],
        ));
    }
    for u in &b.upserts {
        check_id(&u.id)?;
        let price = check_price(&u.id, u.price)?;
        let deal = check_deal(&u.id, &u.deal)?;
        if !u.discount_pct.is_finite() || u.discount_pct.abs() > 1000.0 {
            return Err(format!("{}: discountPct must be a number", u.id));
        }
        stmts.push(Stmt::new(
            format!(
                "INSERT OR REPLACE INTO listing_scores (listing_id, market, price, discount_pct, alert, deal, scored_at) \
                 SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7 WHERE {ACTIVE_LISTING}"
            ),
            vec![
                Param::Text(u.id.clone()),
                m(),
                Param::Int(price),
                Param::Real(u.discount_pct),
                Param::Int(i64::from(u.alert)),
                Param::Text(deal.into()),
                Param::Text(now.into()),
            ],
        ));
    }
    for id in &b.deletes {
        check_id(id)?;
        stmts.push(Stmt::new("DELETE FROM listing_scores WHERE listing_id = ?1 AND market = ?2", vec![Param::Text(id.clone()), m()]));
    }
    Ok(ScorePlan { stmts, alerts: b.alerts.len(), upserts: b.upserts.len(), deletes: b.deletes.len() })
}

// ---------------------------------------------------------------------------
// GET /api/deals, /api/alerts, /api/stats: one JSON string from SQLite

pub const DEFAULT_DEALS_LIMIT: usize = 50;
pub const MAX_DEALS_LIMIT: usize = 500;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct DealsQuery {
    pub market: Option<String>,
    pub max_price: Option<f64>,
    pub min_discount: Option<f64>,
    pub limit: usize,
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

/// `{"generatedAt", "deals": [Deal...]}`, best discount first, as column `body`:
///   SEARCH listing_scores USING INDEX idx_scores_market_discount (market=? AND discount_pct>?)
pub fn deals_query(q: &DealsQuery, now: &str) -> Stmt {
    let mut params = vec![Param::Text(now.to_string())];
    let mut sql = String::from("SELECT deal, discount_pct FROM listing_scores WHERE 1 = 1");
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
    Stmt::new(
        format!("SELECT json_object('generatedAt', ?1, 'deals', json_group_array(json(deal))) AS body FROM ({sql})"),
        params,
    )
}

pub const DEFAULT_ALERTS_LIMIT: usize = 50;
pub const MAX_ALERTS_LIMIT: usize = 500;

#[derive(Debug, Clone, PartialEq)]
pub struct AlertsQuery {
    pub market: Option<String>,
    pub limit: usize,
}

impl AlertsQuery {
    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Result<AlertsQuery, String> {
        let mut q = AlertsQuery { market: None, limit: DEFAULT_ALERTS_LIMIT };
        for (k, v) in pairs {
            match k {
                "market" => q.market = parse_market(v)?,
                "limit" if !v.trim().is_empty() => q.limit = parse_limit(v, MAX_ALERTS_LIMIT)?,
                _ => {}
            }
        }
        Ok(q)
    }
}

/// `{"generatedAt", "alerts": [Deal & {createdAt}...]}`, newest first:
///   SEARCH deal_alerts USING INDEX idx_alerts_market_created (market=?)
///   SCAN deal_alerts USING INDEX idx_alerts_created (no market)
pub fn alerts_query(q: &AlertsQuery, now: &str) -> Stmt {
    let (inner, mut params) = match &q.market {
        Some(m) => (
            "SELECT deal, created_at FROM deal_alerts WHERE market = ?2 ORDER BY created_at DESC LIMIT ?3",
            vec![Param::Text(m.clone())],
        ),
        None => ("SELECT deal, created_at FROM deal_alerts ORDER BY created_at DESC LIMIT ?2", vec![]),
    };
    params.insert(0, Param::Text(now.to_string()));
    params.push(Param::Int(q.limit as i64));
    Stmt::new(
        format!(
            "SELECT json_object('generatedAt', ?1, 'alerts', \
             json_group_array(json_set(json(deal), '$.createdAt', created_at))) AS body FROM ({inner})"
        ),
        params,
    )
}

/// Counts per market and status (removed rows as "expired"):
///   SCAN listings USING COVERING INDEX idx_listings_market_status
pub const COUNTS_SQL: &str = "SELECT market, CASE WHEN removed_at IS NULL THEN status ELSE 'expired' END AS status, \
     COUNT(*) AS n FROM listings GROUP BY market, 2 ORDER BY market, 2";
pub const ALERT_COUNTS_SQL: &str = "SELECT market, COUNT(*) AS n FROM deal_alerts GROUP BY market ORDER BY market";
pub const SCORED_COUNTS_SQL: &str = "SELECT market, COUNT(*) AS n FROM listing_scores GROUP BY market ORDER BY market";
/// Last crawl per (market, mode), from the primary key.
pub const CRAWLS_SQL: &str = "SELECT market, mode, MAX(seen_at) AS at FROM crawls GROUP BY market, mode ORDER BY market, mode";

/// `GET /api/stats`: {generatedAt, counts:[{market,status,n}], crawls:[{market,mode,at}],
/// alerts:[{market,n}], scored:[{market,n}]}, as column `body`.
pub fn stats_query(now: &str) -> Stmt {
    let arr = |obj: &str, sql: &str| format!("json((SELECT json_group_array(json_object({obj})) FROM ({sql})))");
    Stmt::new(
        format!(
            "SELECT json_object('generatedAt', ?1, 'counts', {}, 'crawls', {}, 'alerts', {}, 'scored', {}) AS body",
            arr("'market', market, 'status', status, 'n', n", COUNTS_SQL),
            arr("'market', market, 'mode', mode, 'at', at", CRAWLS_SQL),
            arr("'market', market, 'n', n", ALERT_COUNTS_SQL),
            arr("'market', market, 'n', n", SCORED_COUNTS_SQL),
        ),
        vec![Param::Text(now.to_string())],
    )
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
/// NYC: active listings ≤ `max_price` whose stored score (written by the
/// crawl job) is ≥ 10% under, without a detail read.
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
// Expiry (daily cron)

/// A market is expired only when a full sweep of it ran since `full_since`,
/// so a crawler outage never retires (and later "relists" and alerts on)
/// every listing at once. Scores of every removed listing are deleted (the
/// crawl job only sees unremoved rows, so it cannot delete them itself).
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
             (SELECT id FROM listings WHERE market IN ('nyc', 'mi') AND status = 'active' AND removed_at IS NOT NULL)",
            vec![],
        ),
        Stmt::new("DELETE FROM crawls WHERE seen_at < ?1", vec![Param::Text(crawl_keep_since.into())]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_input_query_takes_a_market_and_pages() {
        let q = ScoreInputQuery::from_pairs([("market", "mi"), ("after", "zl:5"), ("limit", "5000")]).unwrap();
        assert_eq!((q.market.as_str(), q.after.as_str(), q.limit), ("mi", "zl:5", MAX_SCORE_INPUT_LIMIT));
        assert!(ScoreInputQuery::from_pairs([("after", "x")]).is_err());
        assert!(ScoreInputQuery::from_pairs([("market", "sf")]).is_err());
        let s = score_input_query(&q, "2025-10-05");
        assert_eq!(s.params.len(), 4);
        assert!(s.sql.contains("\"storedDiscount\""), "{}", s.sql);
        assert!(s.sql.contains("\"lat\",\"lon\"") && s.sql.contains("l.lat AS lat, l.lon AS lon"), "{}", s.sql);
    }

    #[test]
    fn scores_body_is_validated_and_planned_alerts_first() {
        let deal = r#"{"id":"zl:1","discountPct":20.0}"#;
        let body = format!(
            r#"{{"market":"mi","upserts":[{{"id":"zl:1","price":500000,"discountPct":20.0,"alert":true,"deal":{deal}}}],
                "deletes":["zl:2"],"alerts":[{{"id":"zl:1","price":500000,"deal":{deal}}}]}}"#
        );
        let p = parse_scores(&body, "T").unwrap();
        assert_eq!((p.alerts, p.upserts, p.deletes, p.stmts.len()), (1, 1, 1, 4));
        assert!(p.stmts[0].sql.starts_with("INSERT OR IGNORE INTO deal_alerts"));
        assert!(p.stmts[1].sql.starts_with("UPDATE deal_alerts"));
        assert!(p.stmts[2].sql.starts_with("INSERT OR REPLACE INTO listing_scores"));
        assert_eq!(p.stmts[2].params[5], Param::Text(deal.into()), "the deal is stored as sent");
        assert!(p.stmts[3].sql.starts_with("DELETE FROM listing_scores"));

        assert!(parse_scores(r#"{"market":"la"}"#, "T").is_err());
        assert!(parse_scores(r#"{"market":"mi","deletes":[""]}"#, "T").is_err());
        assert!(parse_scores(r#"{"market":"mi","alerts":[{"id":"a","price":0,"deal":{}}]}"#, "T").is_err());
        assert!(parse_scores(r#"{"market":"mi","alerts":[{"id":"a","price":5,"deal":[1]}]}"#, "T").is_err());
        let many: Vec<String> = (0..101).map(|i| format!("\"x{i}\"")).collect();
        assert!(parse_scores(&format!(r#"{{"market":"mi","deletes":[{}]}}"#, many.join(",")), "T").is_err());
    }

    #[test]
    fn deals_query_numbers_its_filters() {
        let q = DealsQuery::from_pairs([("market", "mi"), ("maxPrice", "900000"), ("minDiscount", "15"), ("limit", "9999")]).unwrap();
        assert_eq!(q.limit, MAX_DEALS_LIMIT);
        let s = deals_query(&q, "T");
        assert!(s.sql.contains("market = ?2 AND discount_pct >= ?3 AND price <= ?4 ORDER BY discount_pct DESC LIMIT ?5"), "{}", s.sql);
        assert!(DealsQuery::from_pairs([("market", "la")]).is_err());
        assert!(DealsQuery::from_pairs([("limit", "-1")]).is_err());
        assert_eq!(deals_query(&DealsQuery::from_pairs([]).unwrap(), "T").params, vec![Param::Text("T".into()), Param::Int(50)]);
    }

    #[test]
    fn alerts_query_params() {
        let q = AlertsQuery::from_pairs([("market", "mi"), ("limit", "5")]).unwrap();
        assert_eq!(alerts_query(&q, "T").params, vec![Param::Text("T".into()), Param::Text("mi".into()), Param::Int(5)]);
        assert!(AlertsQuery::from_pairs([("market", "x")]).is_err());
    }

    #[test]
    fn needs_detail_body() {
        assert_eq!(parse_needs_detail(&serde_json::json!({"market": "mi"})).unwrap(), ("mi".into(), 15));
        assert_eq!(parse_needs_detail(&serde_json::json!({"market": "nyc", "limit": 1000})).unwrap().1, MAX_NEEDS_DETAIL_LIMIT);
        assert!(parse_needs_detail(&serde_json::json!({})).is_err());
        assert!(parse_needs_detail(&serde_json::json!({"market": "mi", "limit": 0})).is_err());
    }
}
