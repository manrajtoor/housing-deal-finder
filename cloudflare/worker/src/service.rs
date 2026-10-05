//! Use cases, written against a two-method storage port ([`Store`]). `d1.rs`
//! implements it on Cloudflare D1; the tests implement it on an in-memory
//! SQLite loaded with the real migrations, so the SQL itself is exercised.
//! `entry.rs` only translates HTTP and cron events into these calls.
//!
//! D1 round trips per POST /api/listings: one id lookup, one write batch
//! (listings + the crawl row), and only when something changed: one or two
//! group loads and one score/alert batch.

use std::collections::{BTreeSet, HashMap, HashSet};

use serde::Serialize;
use serde_json::{json, Value};

use scorer::dates::date_minus_days;
use scorer::{deal_json, AlertRule, Options, Scorer};

use crate::alerts::{self, AlertsQuery, NewAlert, Notifier};
use crate::ingest::{
    detail_load_queries, detail_update, existing_from_row, existing_queries, prior_run_query, fresh_after_detail, listing_from_row,
    merge_detail, parse_details, parse_payload, Planner, SaveStats, Scope,
};
use crate::scores::{self, borough_unit_of, needs_borough, unit_of, DealsQuery, Stored, Unit};
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
    pub rule: AlertRule,
}

impl Config {
    fn options(&self) -> Options {
        Options::with_rule(self.rule.clone())
    }
}

// ---------------------------------------------------------------------------
// Rescoring

#[derive(Debug, Default)]
pub struct Rescore {
    /// Score writes and deletions.
    pub stmts: Vec<Stmt>,
    /// Fresh subjects the alert rule accepts.
    pub alerts: Vec<NewAlert>,
    /// Subjects priced.
    pub scored: usize,
    /// id -> (price, Deal) of every subject priced.
    pub deals: HashMap<String, (i64, Value)>,
}

/// Scores the units of `changed` (contract-shaped listings already stored)
/// and plans the writes. Subjects: every active listing of a neighbourhood
/// or water-type unit, plus the changed listings themselves.
pub async fn rescore<S: Store>(
    store: &S,
    cfg: &Config,
    changed: &[Value],
    fresh: &HashSet<String>,
    now: &str,
) -> Result<Rescore, String> {
    let units: BTreeSet<Unit> = changed.iter().filter_map(unit_of).collect();
    if units.is_empty() {
        return Ok(Rescore::default());
    }
    let changed_ids: BTreeSet<String> = changed.iter().filter_map(|l| l["id"].as_str().map(str::to_string)).collect();
    let sold_cutoff = date_minus_days(now, cfg.options().sold_comp_days).unwrap_or_default();
    let units_v: Vec<Unit> = units.iter().cloned().collect();

    let mut stored: HashMap<String, Stored> = HashMap::new();
    let mut rows: Vec<Value> = Vec::new();
    let mut have: HashSet<String> = HashSet::new();
    let mut take = |raw: Vec<Value>, rows: &mut Vec<Value>, stored: &mut HashMap<String, Stored>| {
        for r in raw {
            let l = listing_from_row(&r);
            let Some(id) = l["id"].as_str().map(str::to_string) else { continue };
            if !have.insert(id.clone()) {
                continue;
            }
            if let Some(s) = scores::stored_from_row(&r) {
                stored.insert(id, s);
            }
            rows.push(l);
        }
    };
    take(query_all(store, scores::group_queries(&units_v, &sold_cutoff)).await?, &mut rows, &mut stored);

    let subject = |l: &Value| {
        l["id"].as_str().is_some_and(|id| changed_ids.contains(id))
            || unit_of(l).is_some_and(|u| units.contains(&u) && !matches!(u, Unit::NycBorough(..)))
    };
    let subjects: Vec<Value> = rows.iter().filter(|l| subject(l)).cloned().collect();
    let scorer = Scorer::new(&rows, cfg.options(), now);
    let mut scored = scores::score_all(&scorer, &subjects);

    // NYC subjects that fell through to the borough group: load that group
    // in full and score them again.
    let more: BTreeSet<Unit> = scored
        .iter()
        .filter(|s| needs_borough(s))
        .filter_map(|s| borough_unit_of(&s.listing))
        .filter(|u| !units.contains(u))
        .collect();
    if !more.is_empty() {
        let more_v: Vec<Unit> = more.into_iter().collect();
        take(query_all(store, scores::group_queries(&more_v, &sold_cutoff)).await?, &mut rows, &mut stored);
        let scorer = Scorer::new(&rows, cfg.options(), now);
        for s in scored.iter_mut().filter(|s| needs_borough(s)) {
            s.result = scorer.score(&s.listing);
        }
    }

    let mut out = Rescore { stmts: scores::upsert_statements(&scored, &changed_ids, &stored, now), ..Default::default() };
    for s in &scored {
        let Ok(score) = &s.result else { continue };
        out.scored += 1;
        let price = s.listing["price"].as_f64().unwrap_or(0.0) as i64;
        let deal = deal_json(&s.listing, score);
        if score.alert && fresh.contains(s.id()) {
            out.alerts.push(NewAlert {
                listing_id: s.id().to_string(),
                price,
                market: s.listing["market"].as_str().unwrap_or_default().to_string(),
                deal: deal.clone(),
            });
        }
        out.deals.insert(s.id().to_string(), (price, deal));
    }
    Ok(out)
}

/// Runs the rescore writes plus `extra`, alert inserts first; returns the
/// alerts that were new (INSERT OR IGNORE changed a row) and rows written.
async fn save_rescore<S: Store>(
    store: &S,
    r: &Rescore,
    extra: Vec<Stmt>,
    now: &str,
) -> Result<(Vec<NewAlert>, u64, usize), String> {
    let n_alerts = r.alerts.len();
    let mut stmts: Vec<Stmt> = r.alerts.iter().map(|a| alerts::insert_statement(a, now)).collect();
    let score_writes = r.stmts.len();
    stmts.extend(r.stmts.iter().cloned());
    stmts.extend(extra);
    if stmts.is_empty() {
        return Ok((Vec::new(), 0, 0));
    }
    let w = store.batch(stmts).await?;
    let new = r.alerts.iter().zip(&w[..n_alerts]).filter(|(_, w)| w.changes > 0).map(|(a, _)| a.clone()).collect();
    Ok((new, rows_written(&w), score_writes))
}

async fn notify<S: Store>(store: &S, notifier: &impl Notifier, new: &[NewAlert], now: &str) -> Result<(), String> {
    if new.is_empty() {
        return Ok(());
    }
    let deals: Vec<Value> = new.iter().map(|a| a.deal.clone()).collect();
    if notifier.notify(&deals).await? > 0 {
        store.batch(new.iter().map(|a| alerts::notified_statement(a, now)).collect()).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// POST /api/listings

#[derive(Debug, Clone, PartialEq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct IngestOutcome {
    #[serde(flatten)]
    pub stats: SaveStats,
    pub scored: usize,
    pub new_alerts: usize,
    /// listing_scores rows written or deleted.
    pub score_writes: usize,
    /// D1 rows written by this request (all tables and indexes).
    pub rows_written: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rejected: Vec<String>,
    /// Scoring or alerting failed; the listings are stored either way.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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

pub async fn ingest<S: Store>(
    store: &S,
    notifier: &impl Notifier,
    cfg: &Config,
    body: &Value,
    now: &str,
) -> Result<IngestOutcome, ApiError> {
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
        out.rows_written += rows_written(&store.batch(stmts).await.map_err(ApiError::internal)?);
    }
    if planner.changed.is_empty() {
        return Ok(out);
    }
    // The last copy of each changed listing, as stored.
    let mut last: HashMap<&str, &Value> = HashMap::new();
    for l in &payload.listings {
        last.insert(l["id"].as_str().unwrap_or_default(), l);
    }
    let changed: Vec<Value> = planner.changed.iter().filter_map(|id| last.get(id.as_str()).map(|l| (*l).clone())).collect();
    let result = async {
        let r = rescore(store, cfg, &changed, &planner.fresh, now).await?;
        let (new, written, score_writes) = save_rescore(store, &r, Vec::new(), now).await?;
        out.scored = r.scored;
        out.new_alerts = new.len();
        out.score_writes = score_writes;
        out.rows_written += written;
        notify(store, notifier, &new, now).await
    }
    .await;
    if let Err(e) = result {
        out.error = Some(format!("scoring or alerts failed (listings are stored): {e}"));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// POST /api/listings/detail

#[derive(Debug, Clone, PartialEq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DetailOutcome {
    pub updated: usize,
    pub scored: usize,
    pub new_alerts: usize,
    /// Ids not in the store.
    pub unknown: usize,
    pub rows_written: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub async fn detail<S: Store>(
    store: &S,
    notifier: &impl Notifier,
    cfg: &Config,
    body: &Value,
    now: &str,
) -> Result<DetailOutcome, ApiError> {
    let details = parse_details(body, now).map_err(ApiError::bad_request)?;
    let mut ids: Vec<String> = details.iter().map(|d| d.id.clone()).collect();
    ids.sort();
    ids.dedup();
    let rows = query_all(store, detail_load_queries(&ids)).await.map_err(ApiError::internal)?;
    let stored: HashMap<String, Value> = rows
        .iter()
        .map(listing_from_row)
        .filter_map(|l| Some((l["id"].as_str()?.to_string(), l)))
        .collect();
    let mut out = DetailOutcome::default();
    let mut stmts = Vec::new();
    let mut merged: HashMap<String, Value> = HashMap::new();
    // The stored versions too: a new water type moves a listing to another
    // group, and the group it left must be rescored without it.
    let mut before: Vec<Value> = Vec::new();
    for d in &details {
        let Some(base) = merged.get(&d.id).or_else(|| stored.get(&d.id)).cloned() else {
            out.unknown += 1;
            continue;
        };
        stmts.push(detail_update(d));
        if !merged.contains_key(&d.id) {
            before.push(base.clone());
        }
        merged.insert(d.id.clone(), merge_detail(&base, d));
    }
    out.updated = merged.len();
    if stmts.is_empty() {
        return Ok(out);
    }
    out.rows_written += rows_written(&store.batch(stmts).await.map_err(ApiError::internal)?);

    let changed: Vec<Value> = merged.into_values().collect();
    // Sold rows are comps: rescoring their groups never makes them fresh
    // (fresh_after_detail requires an active listing) and the scorer refuses
    // them as subjects, so they never alert.
    let mut touched = changed.clone();
    touched.extend(before.into_iter().filter(|b| {
        let unit = unit_of(b);
        unit.is_some() && changed.iter().all(|c| c["id"] != b["id"] || unit_of(c) != unit)
    }));
    let fresh: HashSet<String> = changed
        .iter()
        .filter(|l| fresh_after_detail(l, now))
        .filter_map(|l| l["id"].as_str().map(str::to_string))
        .collect();
    let result = async {
        let r = rescore(store, cfg, &touched, &fresh, now).await?;
        // Alerts already stored for these listings show the new detail fields.
        let patches: Vec<Stmt> = changed
            .iter()
            .filter_map(|l| r.deals.get(l["id"].as_str()?))
            .map(|(price, deal)| alerts::patch_statement(deal["id"].as_str().unwrap_or_default(), *price, deal))
            .collect();
        let (new, written, _) = save_rescore(store, &r, patches, now).await?;
        out.scored = r.scored;
        out.new_alerts = new.len();
        out.rows_written += written;
        notify(store, notifier, &new, now).await
    }
    .await;
    if let Err(e) = result {
        out.error = Some(format!("scoring or alerts failed (details are stored): {e}"));
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
        let cutoff = date_minus_days(now, cfg.options().sold_comp_days).unwrap_or_default();
        let sold = store
            .query(&scores::needs_detail_sold_query(limit - rows.len(), &cutoff))
            .await
            .map_err(ApiError::internal)?;
        rows.extend(sold);
    }
    let col = |k: &str| rows.iter().map(|r| r.get(k).cloned().unwrap_or(Value::Null)).collect::<Vec<_>>();
    Ok(json!({ "ids": col("id"), "urls": col("url") }))
}

pub async fn deals<S: Store>(store: &S, q: &DealsQuery, now: &str) -> Result<Value, ApiError> {
    let rows = store.query(&scores::deals_query(q)).await.map_err(ApiError::internal)?;
    Ok(json!({ "generatedAt": now, "deals": scores::parse_deal_rows(&rows, &[]) }))
}

pub async fn recent_alerts<S: Store>(store: &S, q: &AlertsQuery, now: &str) -> Result<Value, ApiError> {
    let rows = store.query(&alerts::list_query(q)).await.map_err(ApiError::internal)?;
    Ok(json!({ "generatedAt": now, "alerts": scores::parse_deal_rows(&rows, &[("created_at", "createdAt")]) }))
}

pub async fn stats<S: Store>(store: &S, now: &str) -> Result<Value, ApiError> {
    let q = |sql: &'static str| Stmt::new(sql, vec![]);
    let e = ApiError::internal;
    let counts = store.query(&q(scores::COUNTS_SQL)).await.map_err(e)?;
    let crawls = store.query(&q(scores::CRAWLS_SQL)).await.map_err(e)?;
    let alerts = store.query(&q(scores::ALERT_COUNTS_SQL)).await.map_err(e)?;
    let scored = store.query(&q(scores::SCORED_COUNTS_SQL)).await.map_err(e)?;
    Ok(scores::stats_json(now, &counts, &crawls, &alerts, &scored))
}

/// Hours a market's last full sweep may be old for its listings to expire.
pub const FULL_SWEEP_MAX_AGE_HOURS: i64 = 36;
/// `crawls` rows older than this are pruned by the daily cron.
pub const KEEP_CRAWLS_DAYS: i64 = 90;

/// Daily cron: retire active listings unseen since `cutoff`; returns how many.
pub async fn expire<S: Store>(store: &S, now: &str, cutoff: &str) -> Result<u64, String> {
    let full_since = scorer::dates::iso_minus_hours(now, FULL_SWEEP_MAX_AGE_HOURS).unwrap_or_default();
    let keep = scorer::dates::iso_minus_hours(now, KEEP_CRAWLS_DAYS * 24).unwrap_or_default();
    let w = store.batch(scores::expire_statements(now, cutoff, &full_since, &keep)).await?;
    Ok(w.first().map_or(0, |w| w.changes))
}

#[cfg(test)]
mod tests;
