//! Alerts: a fresh listing (new and young, relisted, or cheaper) whose score
//! passes the alert rule (`scorer::AlertRule`, set in the Deal's `alert`) is
//! stored once in `deal_alerts`, keyed by (listing id, asking price), and
//! handed to a [`Notifier`]. There is no notifier yet (dashboard only); the
//! trait is the seam for one.

use serde_json::Value;

use scorer::AlertRule;

use crate::scores::{parse_limit, parse_market};
use crate::sql::{Param, Stmt};

pub const DEFAULT_LIST_LIMIT: usize = 50;
pub const MAX_LIST_LIMIT: usize = 500;

/// Delivers new alerts somewhere (e-mail, Telegram, ...). Returns how many
/// it delivered; delivered alerts get `notified_at`.
#[allow(async_fn_in_trait)] // single-threaded wasm: no Send bound needed
pub trait Notifier {
    async fn notify(&self, deals: &[Value]) -> Result<usize, String>;
}

/// Alerts stay on the dashboard.
pub struct NoNotifier;

impl Notifier for NoNotifier {
    async fn notify(&self, _deals: &[Value]) -> Result<usize, String> {
        Ok(0)
    }
}

/// The rule from the Worker vars; a bad value is reported and the default kept.
pub fn rule_from_vars(get: impl Fn(&str) -> Option<String>) -> (AlertRule, Vec<String>) {
    let mut r = AlertRule::default();
    let mut errors = Vec::new();
    let mut num = |k: &str, min: f64, max: f64| -> Option<f64> {
        let v = get(k)?.trim().to_string();
        if v.is_empty() {
            return None;
        }
        match v.parse::<f64>().ok().filter(|n| n.is_finite() && (min..=max).contains(n)) {
            Some(n) => Some(n),
            None => {
                errors.push(format!("{k}: {v:?} is not a number in {min}..{max}"));
                None
            }
        }
    };
    if let Some(n) = num("ALERT_MIN_DISCOUNT_PCT", 0.1, 99.0) {
        r.min_discount_pct = n;
    }
    if let Some(n) = num("ALERT_MAX_PRICE", 1.0, 1e9) {
        r.max_price = n;
    }
    if let Some(n) = num("ALERT_MIN_COMPS_NYC", 1.0, 1000.0) {
        r.min_comps_nyc = n as usize;
    }
    if let Some(n) = num("ALERT_MIN_COMPS_MI", 1.0, 1000.0) {
        r.min_comps_mi = n as usize;
    }
    (r, errors)
}

/// One alert to store.
#[derive(Debug, Clone, PartialEq)]
pub struct NewAlert {
    pub listing_id: String,
    pub price: i64,
    pub market: String,
    pub deal: Value,
}

/// INSERT OR IGNORE: the same (listing, price) is never stored twice; the
/// statement's `changes` (1 or 0) tells whether this one is new.
pub fn insert_statement(a: &NewAlert, now: &str) -> Stmt {
    Stmt::new(
        "INSERT OR IGNORE INTO deal_alerts (listing_id, price, market, created_at, deal) VALUES (?1, ?2, ?3, ?4, ?5)",
        vec![
            Param::Text(a.listing_id.clone()),
            Param::Int(a.price),
            Param::Text(a.market.clone()),
            Param::Text(now.to_string()),
            Param::Text(a.deal.to_string()),
        ],
    )
}

/// After a detail read: the stored alert for this (listing, price), if any,
/// shows the newly read fields (maintenance, water body, ...).
pub fn patch_statement(listing_id: &str, price: i64, deal: &Value) -> Stmt {
    Stmt::new(
        "UPDATE deal_alerts SET deal = ?1 WHERE listing_id = ?2 AND price = ?3",
        vec![Param::Text(deal.to_string()), Param::Text(listing_id.to_string()), Param::Int(price)],
    )
}

pub fn notified_statement(a: &NewAlert, now: &str) -> Stmt {
    Stmt::new(
        "UPDATE deal_alerts SET notified_at = ?1 WHERE listing_id = ?2 AND price = ?3",
        vec![Param::Text(now.to_string()), Param::Text(a.listing_id.clone()), Param::Int(a.price)],
    )
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlertsQuery {
    pub market: Option<String>,
    pub limit: usize,
}

impl AlertsQuery {
    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Result<AlertsQuery, String> {
        let mut q = AlertsQuery { market: None, limit: DEFAULT_LIST_LIMIT };
        for (k, v) in pairs {
            match k {
                "market" => q.market = parse_market(v)?,
                "limit" if !v.trim().is_empty() => q.limit = parse_limit(v, MAX_LIST_LIMIT)?,
                _ => {}
            }
        }
        Ok(q)
    }
}

/// Newest first:
///   SEARCH deal_alerts USING INDEX idx_alerts_market_created (market=?)
///   SCAN deal_alerts USING INDEX idx_alerts_created (no market)
pub fn list_query(q: &AlertsQuery) -> Stmt {
    match &q.market {
        Some(m) => Stmt::new(
            "SELECT deal, created_at FROM deal_alerts WHERE market = ?1 ORDER BY created_at DESC LIMIT ?2",
            vec![Param::Text(m.clone()), Param::Int(q.limit as i64)],
        ),
        None => Stmt::new(
            "SELECT deal, created_at FROM deal_alerts ORDER BY created_at DESC LIMIT ?1",
            vec![Param::Int(q.limit as i64)],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rule_reads_the_vars_and_reports_bad_ones() {
        let (r, e) = rule_from_vars(|k| match k {
            "ALERT_MAX_PRICE" => Some("900000".into()),
            "ALERT_MIN_DISCOUNT_PCT" => Some("12".into()),
            "ALERT_MIN_COMPS_NYC" => Some("ten".into()),
            "ALERT_MIN_COMPS_MI" => Some("5".into()),
            _ => None,
        });
        assert_eq!((r.max_price, r.min_discount_pct, r.min_comps_nyc, r.min_comps_mi), (900000.0, 12.0, 8, 5));
        assert_eq!(e.len(), 1);
    }

    #[test]
    fn statements_and_queries() {
        let a = NewAlert { listing_id: "se:1".into(), price: 500000, market: "nyc".into(), deal: json!({"id": "se:1"}) };
        let s = insert_statement(&a, "T");
        assert!(s.sql.starts_with("INSERT OR IGNORE INTO deal_alerts"));
        assert_eq!(s.params[1], Param::Int(500000));
        let q = AlertsQuery::from_pairs([("market", "mi"), ("limit", "5")]).unwrap();
        assert_eq!(list_query(&q).params, vec![Param::Text("mi".into()), Param::Int(5)]);
        assert!(AlertsQuery::from_pairs([("market", "x")]).is_err());
    }
}
