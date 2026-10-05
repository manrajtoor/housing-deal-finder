//! Use cases, written against a small storage port ([`Store`]). `d1.rs`
//! implements it on Cloudflare D1; the tests implement it on an in-memory
//! SQLite loaded with the real migrations, so the SQL itself is exercised.
//! `entry.rs` only translates HTTP and cron events into these calls.
//!
//! Nothing here scores (the crawl job does, see `scores.rs`). D1 round trips:
//! - POST /api/listings: a prior-run check (quick crawls), one id lookup, one
//!   write batch (listings, price history, the crawl row).
//! - POST /api/listings/detail: one write batch.
//! - POST /api/scores: one write batch.
//! - GET /api/score-input, /api/deals, /api/alerts, /api/stats: one query
//!   whose single text column is the response body.

use std::collections::HashSet;

use serde::Serialize;
use serde_json::{json, Value};

use scorer::dates::date_minus_days;

use crate::alerts::Notifier;
use crate::ingest::{detail_update, existing_from_row, existing_queries, parse_details, parse_payload, prior_run_query, Planner, SaveStats, Scope};
use crate::scores::{self, AlertsQuery, DealsQuery, ScoreInputQuery, ScoresOutcome};
use crate::sql::{Param, Stmt, Written};

/// An error with the HTTP status it maps to.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl ApiError {
    pub fn bad_request(m: impl Into<String>) -> Self {
        ApiError { status: 400, message: m.into() }
    }
    pub fn internal(m: impl Into<String>) -> Self {
        ApiError { status: 500, message: m.into() }
    }
}

/// The database.
#[allow(async_fn_in_trait)] // single-threaded wasm: no Send bound needed
pub trait Store {
    /// Rows of one SELECT, as JSON objects keyed by column (numbers as doubles, like D1).
    async fn query(&self, s: &Stmt) -> Result<Vec<Value>, String>;
    /// The text column `body` of the first row (`None` when there is no row
    /// or it is NULL). Used for JSON that SQLite builds: one value crosses
    /// from D1 to the Worker, whatever its size.
    async fn body(&self, s: &Stmt) -> Result<Option<String>, String>;
    /// Runs the statements as one transaction; what each changed.
    async fn batch(&self, stmts: Vec<Stmt>) -> Result<Vec<Written>, String>;
}

async fn query_all<S: Store>(store: &S, qs: Vec<Stmt>) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    for q in qs {
        out.extend(store.query(&q).await?);
    }
    Ok(out)
}

fn rows_written(w: &[Written]) -> u64 {
    w.iter().map(|w| w.rows_written).sum()
}

/// Settings from the Worker vars.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Config {
    /// Only `max_price` is used here (NYC needs-detail); the crawl job applies the rule.
    pub rule: scorer::AlertRule,
}

// ---------------------------------------------------------------------------
// POST /api/listings

#[derive(Debug, Clone, PartialEq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct IngestOutcome {
    #[serde(flatten)]
    pub stats: SaveStats,
    /// Always 0: scoring and alerts run in the crawl job (kept for older crawlers).
    pub scored: usize,
    pub new_alerts: usize,
    /// D1 rows written by this request (all tables and indexes).
    pub rows_written: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rejected: Vec<String>,
}

/// The run's row in `crawls`: one per (market, mode, seenAt), stats summed.
fn crawl_upsert(scope: &Scope, stats: &SaveStats) -> Option<Stmt> {
    let (market, mode) = (scope.market.as_ref()?, scope.mode.as_ref()?);
    let keys = ["seen", "added", "updated", "unchanged", "priceDrops", "priceRises", "relisted", "skipped"];
    let v = serde_json::to_value(stats).ok()?;
    let sums: Vec<String> = keys
        .iter()
        .map(|k| format!("'{k}', COALESCE(json_extract(stats, '$.{k}'), 0) + COALESCE(json_extract(excluded.stats, '$.{k}'), 0)"))
        .collect();
    Some(Stmt::new(
        format!(
            "INSERT INTO crawls (market, mode, seen_at, batches, stats) VALUES (?1, ?2, ?3, 1, ?4) \
             ON CONFLICT (market, mode, seen_at) DO UPDATE SET batches = batches + 1, stats = json_object({})",
            sums.join(", ")
        ),
        vec![Param::Text(market.clone()), Param::Text(mode.clone()), Param::Text(scope.seen_at.clone()), Param::Text(v.to_string())],
    ))
}

pub async fn ingest<S: Store>(store: &S, body: &Value, now: &str) -> Result<IngestOutcome, ApiError> {
    let payload = parse_payload(body, now).map_err(ApiError::bad_request)?;
    let mut ids: Vec<String> = payload.listings.iter().filter_map(|l| l["id"].as_str().map(str::to_string)).collect();
    ids.sort();
    ids.dedup();
    let known = query_all(store, existing_queries(&ids)).await.map_err(ApiError::internal)?;
    // Only a quick crawl can make new listings fresh, and only after a
    // first run of the market exists.
    let prior_run = match prior_run_query(&payload.scope).filter(|_| payload.scope.mode.as_deref() == Some("quick")) {
        Some(q) => !store.query(&q).await.map_err(ApiError::internal)?.is_empty(),
        None => false,
    };
    let mut planner = Planner::new(known.iter().filter_map(existing_from_row).collect()).with_prior_run(prior_run);
    let mut stmts: Vec<Stmt> = payload.listings.iter().flat_map(|l| planner.plan(l, &payload.scope)).collect();
    planner.stats.skipped = payload.rejected.len();
    stmts.extend(crawl_upsert(&payload.scope, &planner.stats));

    let mut out = IngestOutcome {
        stats: planner.stats.clone(),
        rejected: payload.rejected.iter().take(10).cloned().collect(),
        ..Default::default()
    };
    if !stmts.is_empty() {
        out.rows_written = rows_written(&store.batch(stmts).await.map_err(ApiError::internal)?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// POST /api/listings/detail

#[derive(Debug, Clone, PartialEq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DetailOutcome {
    pub updated: usize,
    /// Always 0 (scoring runs in the crawl job; kept for older crawlers).
    pub scored: usize,
    pub new_alerts: usize,
    /// Ids not in the store.
    pub unknown: usize,
    pub rows_written: u64,
}

/// Stores detail fields: one UPDATE per result, nothing read. An id the
/// store does not know changes no row and is counted as unknown.
pub async fn detail<S: Store>(store: &S, body: &Value, now: &str) -> Result<DetailOutcome, ApiError> {
    let details = parse_details(body, now).map_err(ApiError::bad_request)?;
    let mut out = DetailOutcome::default();
    if details.is_empty() {
        return Ok(out);
    }
    let w = store.batch(details.iter().map(detail_update).collect()).await.map_err(ApiError::internal)?;
    let mut updated = HashSet::new();
    let mut unknown = HashSet::new();
    for (d, w) in details.iter().zip(&w) {
        if w.changes > 0 {
            updated.insert(d.id.as_str());
        } else {
            unknown.insert(d.id.as_str());
        }
    }
    out.updated = updated.len();
    out.unknown = unknown.difference(&updated).count();
    out.rows_written = rows_written(&w);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Scoring, done by the crawl job

/// `GET /api/score-input`: the response body exactly as SQLite built it.
pub async fn score_input<S: Store>(store: &S, q: &ScoreInputQuery, now: &str) -> Result<String, ApiError> {
    let cutoff = date_minus_days(now, scores::SOLD_COMP_DAYS).unwrap_or_default();
    let body = store.body(&scores::score_input_query(q, &cutoff)).await.map_err(ApiError::internal)?;
    body.ok_or_else(|| ApiError::internal("score input: no row"))
}

/// `POST /api/scores`: alerts (insert, then refresh a stored alert's Deal),
/// score upserts and deletions in one batch. `text` is the raw body.
pub async fn post_scores<S: Store>(store: &S, notifier: &impl Notifier, text: &str, now: &str) -> Result<ScoresOutcome, ApiError> {
    let plan = scores::parse_scores(text, now).map_err(ApiError::bad_request)?;
    let mut out = ScoresOutcome::default();
    if plan.stmts.is_empty() {
        return Ok(out);
    }
    let w = store.batch(plan.stmts.clone()).await.map_err(ApiError::internal)?;
    let n_alert_stmts = plan.alerts * 2;
    let mut new_alerts = Vec::new();
    for (i, w) in w[..n_alert_stmts].iter().enumerate() {
        if i % 2 == 0 && w.changes > 0 {
            out.new_alerts += 1;
            new_alerts.push(&plan.stmts[i]);
        } else if i % 2 == 1 {
            out.patched_alerts += w.changes;
        }
    }
    out.upserted = w[n_alert_stmts..n_alert_stmts + plan.upserts].iter().map(|w| w.changes).sum();
    out.deleted = w[n_alert_stmts + plan.upserts..].iter().map(|w| w.changes).sum();
    out.rows_written = rows_written(&w);
    if !new_alerts.is_empty() {
        // Parsed only here, for the few new alerts: (id, price, deal) are params 1, 3, 5.
        let deals: Vec<Value> =
            new_alerts.iter().filter_map(|s| s.params[4].as_str().and_then(|d| serde_json::from_str(d).ok())).collect();
        if notifier.notify(&deals).await.map_err(ApiError::internal)? > 0 {
            let marks = new_alerts
                .iter()
                .map(|s| {
                    Stmt::new(
                        "UPDATE deal_alerts SET notified_at = ?1 WHERE listing_id = ?2 AND price = ?3",
                        vec![Param::Text(now.into()), s.params[0].clone(), s.params[2].clone()],
                    )
                })
                .collect();
            store.batch(marks).await.map_err(ApiError::internal)?;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Reads, needs-detail, expiry

/// Michigan: active listings first, then sold comps (last 365 days) whose
/// detail page was never read. NYC: cheap scored listings.
pub async fn needs_detail<S: Store>(store: &S, cfg: &Config, body: &Value, now: &str) -> Result<Value, ApiError> {
    let (market, limit) = scores::parse_needs_detail(body).map_err(ApiError::bad_request)?;
    let mut rows = store
        .query(&scores::needs_detail_query(&market, limit, cfg.rule.max_price))
        .await
        .map_err(ApiError::internal)?;
    if market == "mi" && rows.len() < limit {
        let cutoff = date_minus_days(now, scores::SOLD_COMP_DAYS).unwrap_or_default();
        let sold = store
            .query(&scores::needs_detail_sold_query(limit - rows.len(), &cutoff))
            .await
            .map_err(ApiError::internal)?;
        rows.extend(sold);
    }
    let col = |k: &str| rows.iter().map(|r| r.get(k).cloned().unwrap_or(Value::Null)).collect::<Vec<_>>();
    Ok(json!({ "ids": col("id"), "urls": col("url") }))
}

async fn body_of<S: Store>(store: &S, s: Stmt) -> Result<String, ApiError> {
    store.body(&s).await.map_err(ApiError::internal)?.ok_or_else(|| ApiError::internal("no row"))
}

/// `{"generatedAt", "deals": [...]}` as SQLite built it.
pub async fn deals<S: Store>(store: &S, q: &DealsQuery, now: &str) -> Result<String, ApiError> {
    body_of(store, scores::deals_query(q, now)).await
}

/// `{"generatedAt", "alerts": [...]}` as SQLite built it.
pub async fn recent_alerts<S: Store>(store: &S, q: &AlertsQuery, now: &str) -> Result<String, ApiError> {
    body_of(store, scores::alerts_query(q, now)).await
}

/// `{"generatedAt", "counts", "crawls", "alerts", "scored"}` as SQLite built it.
pub async fn stats<S: Store>(store: &S, now: &str) -> Result<String, ApiError> {
    body_of(store, scores::stats_query(now)).await
}

/// Hours a market's last full sweep may be old for its listings to expire.
pub const FULL_SWEEP_MAX_AGE_HOURS: i64 = 36;
/// `crawls` rows older than this are pruned by the daily cron.
pub const KEEP_CRAWLS_DAYS: i64 = 90;

/// Daily cron: retire active listings unseen since `cutoff`, drop the scores
/// of removed listings; returns how many listings were retired.
pub async fn expire<S: Store>(store: &S, now: &str, cutoff: &str) -> Result<u64, String> {
    let full_since = scorer::dates::iso_minus_hours(now, FULL_SWEEP_MAX_AGE_HOURS).unwrap_or_default();
    let keep = scorer::dates::iso_minus_hours(now, KEEP_CRAWLS_DAYS * 24).unwrap_or_default();
    let w = store.batch(scores::expire_statements(now, cutoff, &full_since, &keep)).await?;
    Ok(w.first().map_or(0, |w| w.changes))
}

#[cfg(test)]
mod tests;
