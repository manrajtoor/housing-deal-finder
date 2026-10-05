//! POST /api/listings and POST /api/listings/detail, planned as statements.
//!
//! Water facts (`waterType`, `waterBody`, `waterSource`): a search card may
//! carry them from the map (`waterSource: "map"`, the crawler's offline
//! geography). They are stored only while the row has no `detail_read_at`
//! (checked in the UPDATE itself), so a description, once read, wins. A
//! detail read that names a water type overwrites all three
//! (`waterSource: "description"`); one that names none keeps what is there.
//!
//! Upsert rules:
//! - New id: insert, plus a price_history row.
//! - Known id, nothing new (same price, status and comp-only flag, not back
//!   after a removal, no newer detail page) and `last_seen` within
//!   [`TOUCH_EVERY_HOURS`] of `seenAt`: **no write at all**.
//! - Otherwise: overwrite the card fields, keep the sticky detail fields
//!   unless this push carries a detail read (`detailReadAt` set), bump
//!   `price_changes` and add price_history on a price change, clear
//!   `removed_at` (relisted).
//!
//! "Fresh" (alert-eligible) ids: not comp-only, active, and
//! - new, in a `quick` crawl, with `daysOnMarket` ≤ 3 or unknown (StreetEasy
//!   never sends it), and only once an earlier crawl run of that market
//!   exists (so a market's very first run never alerts); `full` and `sold`
//!   crawls never make a new listing fresh; or
//! - relisted, or cheaper than before (any mode).
//! A first load therefore stores listings without alerting on them.
//!
//! Nothing here scores: `fresh_at` is recorded, and the crawl job
//! (`housedeals-score`) alerts on listings whose `fresh_at` is under 72 h old.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use serde_json::{Map, Value};

use scorer::dates::{is_iso, iso_minus_hours};
use scorer::listing::WATER_TYPES;

use crate::sql::{id_queries, json_num, placeholders, Param, Stmt};

/// Most listings one request may carry (the crawler sends 25).
pub const MAX_LISTINGS_PER_REQUEST: usize = 200;
/// An unchanged listing's `last_seen` is refreshed at most this often.
pub const TOUCH_EVERY_HOURS: i64 = 20;
/// New listings this young (days on market) are fresh.
pub const FRESH_MAX_DAYS_ON_MARKET: f64 = 3.0;
pub const MAX_DESCRIPTION_CHARS: usize = 3000;
const MAX_TEXT_CHARS: usize = 500;
const MAX_URL_CHARS: usize = 1000;
const MAX_ID_CHARS: usize = 100;

pub const HOME_TYPES: [&str; 6] = ["condo", "coop", "townhouse", "single_family", "multi_family", "other"];
pub const MODES: [&str; 3] = ["quick", "full", "sold"];

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Text,
    Int,
    Real,
    Bool,
}

/// (column, contract field, kind, sticky). Sticky columns come from the
/// detail page: a push without a detail read never overwrites them.
pub const FIELDS: [(&str, &str, Kind, bool); 35] = [
    ("id", "id", Kind::Text, false),
    ("source", "source", Kind::Text, false),
    ("market", "market", Kind::Text, false),
    ("status", "status", Kind::Text, false),
    ("url", "url", Kind::Text, false),
    ("address", "address", Kind::Text, false),
    ("unit", "unit", Kind::Text, false),
    ("city", "city", Kind::Text, false),
    ("zip", "zip", Kind::Text, false),
    ("lat", "lat", Kind::Real, false),
    ("lon", "lon", Kind::Real, false),
    ("price", "price", Kind::Int, false),
    ("sold_at", "soldAt", Kind::Text, false),
    ("beds", "beds", Kind::Int, false),
    ("baths", "baths", Kind::Real, false),
    ("sqft", "sqft", Kind::Int, false),
    ("lot_sqft", "lotSqft", Kind::Int, false),
    ("year_built", "yearBuilt", Kind::Int, true),
    ("home_type", "homeType", Kind::Text, false),
    ("neighborhood", "neighborhood", Kind::Text, false),
    ("borough", "borough", Kind::Text, false),
    ("county", "county", Kind::Text, false),
    ("area", "area", Kind::Text, false),
    ("zestimate", "zestimate", Kind::Int, false),
    ("days_on_market", "daysOnMarket", Kind::Int, false),
    ("photo_url", "photoUrl", Kind::Text, false),
    ("description", "description", Kind::Text, true),
    ("water_type", "waterType", Kind::Text, true),
    ("water_body", "waterBody", Kind::Text, true),
    ("water_source", "waterSource", Kind::Text, true),
    ("frontage_ft", "frontageFt", Kind::Int, true),
    ("maintenance", "maintenance", Kind::Int, true),
    ("taxes", "taxes", Kind::Int, true),
    ("detail_read_at", "detailReadAt", Kind::Text, true),
    ("comp_only", "compOnly", Kind::Bool, false),
];

/// Fields only a detail page provides (`yearBuilt` may come from either;
/// the water fields also from the map, see [`normalize`]).
pub const DETAIL_ONLY: [&str; 8] =
    ["description", "waterType", "waterBody", "waterSource", "frontageFt", "maintenance", "taxes", "detailReadAt"];

/// The water columns, written by their own rules (module comment).
const WATER: [&str; 3] = ["waterType", "waterBody", "waterSource"];

pub const WATER_FROM_MAP: &str = "map";
pub const WATER_FROM_DESCRIPTION: &str = "description";

/// Which crawl sent the batch.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scope {
    pub source: Option<String>,
    pub market: Option<String>,
    pub mode: Option<String>,
    pub seen_at: String,
}

#[derive(Debug, Clone)]
pub struct Payload {
    /// Validated listings, contract shape.
    pub listings: Vec<Value>,
    pub scope: Scope,
    /// Listings refused by validation, with the reason.
    pub rejected: Vec<String>,
}

fn opt_str<'a>(v: Option<&'a Value>) -> Option<&'a str> {
    v.and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// `{"listings": [...], "scope": {source, market, mode, seenAt}}`.
pub fn parse_payload(body: &Value, now: &str) -> Result<Payload, String> {
    let rows = body
        .get("listings")
        .and_then(Value::as_array)
        .ok_or("body needs a \"listings\" array")?;
    if rows.len() > MAX_LISTINGS_PER_REQUEST {
        return Err(format!("too many listings in one request ({} > {MAX_LISTINGS_PER_REQUEST})", rows.len()));
    }
    let s = body.get("scope");
    let field = |k: &str| opt_str(s.and_then(|s| s.get(k))).map(str::to_string);
    let mode = field("mode");
    if let Some(m) = &mode {
        if !MODES.contains(&m.as_str()) {
            return Err(format!("scope.mode must be quick, full or sold, not {m:?}"));
        }
    }
    let market = field("market");
    if let Some(m) = &market {
        if m != "nyc" && m != "mi" {
            return Err(format!("scope.market must be nyc or mi, not {m:?}"));
        }
    }
    let seen_at = match field("seenAt") {
        Some(t) if is_iso(&t) => t,
        Some(t) => return Err(format!("scope.seenAt is not an ISO time: {t:?}")),
        None => now.to_string(),
    };
    let scope = Scope { source: field("source"), market, mode, seen_at };
    let mut listings = Vec::with_capacity(rows.len());
    let mut rejected = Vec::new();
    for (i, l) in rows.iter().enumerate() {
        match normalize(l, &scope) {
            Ok(v) => listings.push(v),
            Err(e) => rejected.push(format!("listings[{i}]: {e}")),
        }
    }
    Ok(Payload { listings, scope, rejected })
}

fn num_field(l: &Value, k: &str, lo: f64, hi: f64) -> Result<Option<f64>, String> {
    match l.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_f64() {
            Some(f) if f.is_finite() && (lo..=hi).contains(&f) => Ok(Some(f)),
            _ => Err(format!("{k} must be a number in {lo}..{hi}")),
        },
    }
}

/// A clean contract listing: known fields only, trimmed and length-capped
/// strings, whole numbers where the contract says int, enums checked. Detail
/// fields are dropped unless `detailReadAt` is set, so they can never blank
/// stored ones; the exception is map water (`waterSource: "map"` with a
/// known `waterType`) on a card. `waterSource` is "description" for a detail
/// read with a water type, "map" for map water, null otherwise.
pub fn normalize(l: &Value, scope: &Scope) -> Result<Value, String> {
    if !l.is_object() {
        return Err("not an object".into());
    }
    let id = opt_str(l.get("id")).ok_or("no id")?;
    if id.chars().count() > MAX_ID_CHARS {
        return Err("id too long".into());
    }
    let source = opt_str(l.get("source")).or(scope.source.as_deref()).ok_or("no source")?;
    if source != "streeteasy" && source != "zillow" {
        return Err(format!("unknown source {source:?}"));
    }
    let market = opt_str(l.get("market")).or(scope.market.as_deref()).ok_or("no market")?;
    if market != "nyc" && market != "mi" {
        return Err(format!("unknown market {market:?}"));
    }
    let status = opt_str(l.get("status")).unwrap_or("active");
    if status != "active" && status != "sold" {
        return Err(format!("unknown status {status:?}"));
    }
    let price = num_field(l, "price", 1.0, 1e9)?.ok_or("price must be a positive number")?;

    let detail_read_at = opt_str(l.get("detailReadAt")).filter(|t| is_iso(t));
    let mut out = Map::new();
    for (_, k, kind, _) in FIELDS {
        if DETAIL_ONLY.contains(&k) && detail_read_at.is_none() {
            out.insert(k.into(), Value::Null);
            continue;
        }
        let v = match (k, kind) {
            ("id", _) => Value::from(id),
            ("source", _) => Value::from(source),
            ("market", _) => Value::from(market),
            ("status", _) => Value::from(status),
            ("price", _) => Value::from(price.round() as i64),
            ("homeType", _) => {
                Value::from(opt_str(l.get(k)).filter(|t| HOME_TYPES.contains(t)).unwrap_or("other"))
            }
            ("waterType", _) => opt_str(l.get(k)).filter(|t| WATER_TYPES.contains(t)).map_or(Value::Null, Value::from),
            ("soldAt", _) | ("detailReadAt", _) => opt_str(l.get(k)).filter(|t| is_iso(t)).map_or(Value::Null, Value::from),
            ("compOnly", _) => Value::Bool(l.get(k) == Some(&Value::Bool(true)) || status == "sold"),
            (_, Kind::Text) => {
                let max = match k {
                    "description" => MAX_DESCRIPTION_CHARS,
                    "url" | "photoUrl" => MAX_URL_CHARS,
                    _ => MAX_TEXT_CHARS,
                };
                opt_str(l.get(k)).map_or(Value::Null, |s| Value::from(clip(s, max)))
            }
            (_, Kind::Int) => {
                let hi = if k == "zestimate" || k == "lotSqft" { 1e10 } else { 1e8 };
                // sqft is null when unknown, never 0.
                let lo = if k == "sqft" || k == "lotSqft" { 1.0 } else { 0.0 };
                num_field(l, k, lo, hi).unwrap_or(None).map_or(Value::Null, |f| Value::from(f.round() as i64))
            }
            (_, Kind::Real) => num_field(l, k, -1e4, 1e4).unwrap_or(None).map_or(Value::Null, json_num),
            (_, Kind::Bool) => Value::Bool(l.get(k) == Some(&Value::Bool(true))),
        };
        out.insert(k.into(), v);
    }
    let water_type = opt_str(l.get("waterType")).filter(|t| WATER_TYPES.contains(t));
    let source = match (detail_read_at, water_type) {
        (Some(_), Some(_)) => Some(WATER_FROM_DESCRIPTION),
        (None, Some(t)) if opt_str(l.get("waterSource")) == Some(WATER_FROM_MAP) => {
            out.insert("waterType".into(), Value::from(t));
            let body = opt_str(l.get("waterBody")).map_or(Value::Null, |b| Value::from(clip(b, MAX_TEXT_CHARS)));
            out.insert("waterBody".into(), body);
            Some(WATER_FROM_MAP)
        }
        _ => None,
    };
    out.insert("waterSource".into(), source.map_or(Value::Null, Value::from));
    Ok(Value::Object(out))
}

/// SET clauses for the water columns. `fields` are the bound fields in
/// parameter order (1-based), `detail` the number of the parameter that is 1
/// when the push carries a detail read. A detail read with a water type
/// writes all three; otherwise only a row never detail-read takes the card's
/// (map) values, nulls included, so a stale map value can be cleared.
fn water_sets(fields: &[&str], detail: Option<usize>) -> Vec<String> {
    let at = |k: &str| fields.iter().position(|f| *f == k).map(|i| i + 1).unwrap_or(0);
    let wt = at("waterType");
    WATER
        .iter()
        .map(|k| {
            let (c, i) = (FIELDS.iter().find(|f| f.1 == *k).map_or("", |f| f.0), at(k));
            let from_detail = format!("CASE WHEN ?{wt} IS NOT NULL THEN ?{i} ELSE {c} END");
            match detail {
                None => format!("{c} = {from_detail}"),
                Some(d) => format!(
                    "{c} = CASE WHEN ?{d} = 1 THEN {from_detail} WHEN detail_read_at IS NULL THEN ?{i} ELSE {c} END"
                ),
            }
        })
        .collect()
}

fn param(l: &Value, k: &str, kind: Kind) -> Param {
    let v = l.get(k);
    match kind {
        Kind::Text => Param::text(v.and_then(Value::as_str)),
        Kind::Int => Param::int(v.and_then(Value::as_f64).map(|f| f.round() as i64)),
        Kind::Real => Param::real(v.and_then(Value::as_f64)),
        Kind::Bool => Param::Int(i64::from(v == Some(&Value::Bool(true)))),
    }
}

/// What a batch changed (camelCase on the wire, CONTRACT.md).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveStats {
    pub seen: usize,
    pub added: usize,
    /// Known listings written (changed, or `last_seen` refreshed).
    pub updated: usize,
    /// Known, nothing new, seen recently: nothing written.
    pub unchanged: usize,
    pub price_drops: usize,
    pub price_rises: usize,
    pub relisted: usize,
    /// Listings refused by validation.
    pub skipped: usize,
}

/// The stored state of a known listing that the upsert rules need.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Existing {
    pub price: Option<f64>,
    pub removed: bool,
    pub last_seen: Option<String>,
    pub status: Option<String>,
    pub comp_only: bool,
    pub detail_read_at: Option<String>,
    /// Stored water, to notice a changed map classification.
    pub water_type: Option<String>,
    pub water_body: Option<String>,
}

pub fn existing_queries(ids: &[String]) -> Vec<Stmt> {
    id_queries(
        "SELECT id, price, removed_at, last_seen, status, comp_only, detail_read_at, water_type, water_body FROM listings WHERE id",
        "",
        ids,
    )
}

/// Any crawl run of `market` other than the current one (mode, seenAt):
///   SEARCH crawls USING COVERING INDEX sqlite_autoindex_crawls_1 (market=?)
pub fn prior_run_query(scope: &Scope) -> Option<Stmt> {
    Some(Stmt::new(
        "SELECT 1 AS found FROM crawls WHERE market = ?1 AND NOT (mode = ?2 AND seen_at = ?3) LIMIT 1",
        vec![
            Param::Text(scope.market.clone()?),
            Param::text(scope.mode.as_deref()),
            Param::Text(scope.seen_at.clone()),
        ],
    ))
}

pub fn existing_from_row(row: &Value) -> Option<(String, Existing)> {
    let s = |k: &str| row.get(k).and_then(Value::as_str).map(str::to_string);
    Some((
        s("id")?,
        Existing {
            price: row.get("price").and_then(Value::as_f64),
            removed: row.get("removed_at").is_some_and(|v| !v.is_null()),
            last_seen: s("last_seen"),
            status: s("status"),
            comp_only: row.get("comp_only").and_then(Value::as_f64) == Some(1.0),
            detail_read_at: s("detail_read_at"),
            water_type: s("water_type"),
            water_body: s("water_body"),
        },
    ))
}

const BOOKKEEPING: [&str; 6] = ["first_seen", "last_seen", "first_price", "price_changes", "removed_at", "fresh_at"];

fn insert_sql() -> String {
    let cols: Vec<&str> = FIELDS.iter().map(|f| f.0).chain(BOOKKEEPING).collect();
    // OR IGNORE: a concurrent push that inserted the same id first must not fail the batch.
    format!("INSERT OR IGNORE INTO listings ({}) VALUES ({})", cols.join(", "), placeholders(1, cols.len()))
}

/// The card fields are overwritten, the sticky ones only by a non-null value,
/// the water ones by [`water_sets`].
fn update_sql() -> String {
    let fields: Vec<_> = FIELDS.iter().filter(|f| f.0 != "id").collect();
    let names: Vec<&str> = fields.iter().map(|f| f.1).collect();
    let n = fields.len();
    let mut sets: Vec<String> = fields
        .iter()
        .enumerate()
        .filter(|(_, f)| !WATER.contains(&f.1))
        .map(|(i, (c, _, _, sticky))| if *sticky { format!("{c} = COALESCE(?{}, {c})", i + 1) } else { format!("{c} = ?{}", i + 1) })
        .collect();
    sets.extend(water_sets(&names, Some(n + 5)));
    sets.push(format!("last_seen = MAX(last_seen, ?{})", n + 1));
    sets.push(format!("price_changes = price_changes + ?{}", n + 2));
    sets.push("removed_at = NULL".into());
    sets.push(format!("fresh_at = COALESCE(?{}, fresh_at)", n + 3));
    format!("UPDATE listings SET {} WHERE id = ?{}", sets.join(", "), n + 4)
}

fn price_history(id: &str, seen_at: &str, price: i64) -> Stmt {
    Stmt::new(
        "INSERT OR REPLACE INTO price_history (listing_id, seen_at, price) VALUES (?1, ?2, ?3)",
        vec![Param::Text(id.into()), Param::Text(seen_at.into()), Param::Int(price)],
    )
}

/// Plans the writes for a batch. It remembers what it planned, so an id that
/// appears twice in one batch is treated as known the second time.
pub struct Planner {
    known: HashMap<String, Existing>,
    insert: String,
    update: String,
    pub stats: SaveStats,
    /// Ids whose scoring inputs changed (new, price, status, comp-only flag,
    /// relisted, detail read). In plan order.
    pub changed: Vec<String>,
    /// Alert-eligible ids (subset of `changed`).
    pub fresh: HashSet<String>,
    /// An earlier crawl run of this market exists: new listings may be fresh.
    pub prior_run: bool,
}

impl Planner {
    pub fn new(known: HashMap<String, Existing>) -> Planner {
        Planner {
            known,
            insert: insert_sql(),
            update: update_sql(),
            stats: SaveStats::default(),
            changed: Vec::new(),
            fresh: HashSet::new(),
            prior_run: false,
        }
    }

    /// Sets whether an earlier crawl run of the batch's market exists.
    pub fn with_prior_run(mut self, prior_run: bool) -> Planner {
        self.prior_run = prior_run;
        self
    }

    /// Statements for one normalized listing.
    pub fn plan(&mut self, l: &Value, scope: &Scope) -> Vec<Stmt> {
        let id = l["id"].as_str().unwrap_or_default().to_string();
        let price = l["price"].as_f64().unwrap_or(0.0);
        let status = l["status"].as_str().unwrap_or("active");
        let comp_only = l["compOnly"] == Value::Bool(true);
        let detail_at = l["detailReadAt"].as_str();
        let eligible = !comp_only && status == "active";
        let seen_at = scope.seen_at.as_str();
        self.stats.seen += 1;

        let Some(e) = self.known.get(&id).cloned() else {
            let dom = l["daysOnMarket"].as_f64();
            let young = dom.is_none_or(|d| d <= FRESH_MAX_DAYS_ON_MARKET);
            let fresh = eligible && young && self.prior_run && scope.mode.as_deref() == Some("quick");
            let mut params: Vec<Param> = FIELDS.iter().map(|(_, k, kind, _)| param(l, k, *kind)).collect();
            params.extend([
                Param::Text(seen_at.into()),
                Param::Text(seen_at.into()),
                Param::Int(price as i64),
                Param::Int(0),
                Param::Null,
                if fresh { Param::Text(seen_at.into()) } else { Param::Null },
            ]);
            self.stats.added += 1;
            self.mark(&id, fresh);
            self.known.insert(
                id.clone(),
                Existing {
                    price: Some(price),
                    last_seen: Some(seen_at.into()),
                    status: Some(status.into()),
                    comp_only,
                    detail_read_at: detail_at.map(str::to_string),
                    removed: false,
                    water_type: l["waterType"].as_str().map(str::to_string),
                    water_body: l["waterBody"].as_str().map(str::to_string),
                },
            );
            return vec![Stmt::new(self.insert.clone(), params), price_history(&id, seen_at, price as i64)];
        };

        let price_changed = e.price != Some(price);
        let status_changed = e.status.as_deref() != Some(status);
        let flag_changed = e.comp_only != comp_only;
        let brings_detail = detail_at.is_some_and(|d| e.detail_read_at.as_deref().is_none_or(|old| d > old));
        let recent = match (&e.last_seen, iso_minus_hours(seen_at, TOUCH_EVERY_HOURS)) {
            (Some(seen), Some(cutoff)) => *seen >= cutoff,
            _ => false,
        };
        // Map water lands only on rows never detail-read (as the UPDATE does).
        let (wt, wb) = (l["waterType"].as_str(), l["waterBody"].as_str());
        let map_water_changed = detail_at.is_none()
            && e.detail_read_at.is_none()
            && (e.water_type.as_deref(), e.water_body.as_deref()) != (wt, wb);
        let relevant =
            price_changed || status_changed || flag_changed || brings_detail || map_water_changed || e.removed;
        if !relevant && recent {
            self.stats.unchanged += 1;
            return Vec::new();
        }
        let dropped = price_changed && e.price.is_some_and(|old| price < old);
        let fresh = eligible && (e.removed || dropped);
        let mut params: Vec<Param> =
            FIELDS.iter().filter(|f| f.0 != "id").map(|(_, k, kind, _)| param(l, k, *kind)).collect();
        params.extend([
            Param::Text(seen_at.into()),
            Param::Int(i64::from(price_changed)),
            if fresh { Param::Text(seen_at.into()) } else { Param::Null },
            Param::Text(id.clone()),
            Param::Int(i64::from(detail_at.is_some())),
        ]);
        let mut out = vec![Stmt::new(self.update.clone(), params)];
        self.stats.updated += 1;
        if e.removed {
            self.stats.relisted += 1;
        }
        if price_changed {
            out.push(price_history(&id, seen_at, price as i64));
            if dropped {
                self.stats.price_drops += 1;
            } else {
                self.stats.price_rises += 1;
            }
        }
        if relevant {
            self.mark(&id, fresh);
        }
        self.known.insert(
            id,
            Existing {
                price: Some(price),
                last_seen: Some(seen_at.into()),
                status: Some(status.into()),
                comp_only,
                water_type: if map_water_changed || (brings_detail && wt.is_some()) { wt.map(str::to_string) } else { e.water_type },
                water_body: if map_water_changed || (brings_detail && wt.is_some()) { wb.map(str::to_string) } else { e.water_body },
                detail_read_at: if brings_detail { detail_at.map(str::to_string) } else { e.detail_read_at },
                removed: false,
            },
        );
        out
    }

    fn mark(&mut self, id: &str, fresh: bool) {
        if !self.changed.iter().any(|c| c == id) {
            self.changed.push(id.to_string());
        }
        if fresh {
            self.fresh.insert(id.to_string());
        }
    }
}

// ---------------------------------------------------------------------------
// POST /api/listings/detail

/// One detail-page result.
#[derive(Debug, Clone, PartialEq)]
pub struct Detail {
    pub id: String,
    /// Contract-shaped detail fields (absent or null = keep the stored value).
    pub fields: Value,
}

const DETAIL_FIELDS: [&str; 9] = [
    "detailReadAt",
    "description",
    "waterType",
    "waterBody",
    "waterSource",
    "frontageFt",
    "maintenance",
    "taxes",
    "yearBuilt",
];

/// `{"listings": [{id, detailReadAt, description?, waterType?, ...}]}`.
pub fn parse_details(body: &Value, now: &str) -> Result<Vec<Detail>, String> {
    let rows = body.get("listings").and_then(Value::as_array).ok_or("body needs a \"listings\" array")?;
    if rows.len() > MAX_LISTINGS_PER_REQUEST {
        return Err(format!("too many listings in one request ({} > {MAX_LISTINGS_PER_REQUEST})", rows.len()));
    }
    let mut out = Vec::new();
    for l in rows {
        let Some(id) = opt_str(l.get("id")).filter(|id| id.chars().count() <= MAX_ID_CHARS) else { continue };
        let mut probe = l.clone();
        // Reuse the listing normalizer for types, caps and enums.
        if opt_str(l.get("detailReadAt")).is_none_or(|t| !is_iso(t)) {
            probe["detailReadAt"] = Value::from(now);
        }
        probe["source"] = Value::from("zillow");
        probe["market"] = Value::from("mi");
        probe["price"] = Value::from(1);
        let clean = normalize(&probe, &Scope::default())?;
        let fields: Map<String, Value> = DETAIL_FIELDS.iter().map(|k| (k.to_string(), clean[*k].clone())).collect();
        out.push(Detail { id: id.to_string(), fields: Value::Object(fields) });
    }
    Ok(out)
}

/// The UPDATE for one detail result: only non-null values are written, and
/// a water type replaces the stored water (map or older description) whole.
pub fn detail_update(d: &Detail) -> Stmt {
    let cols: Vec<(&str, &str, Kind)> = FIELDS
        .iter()
        .filter(|f| DETAIL_FIELDS.contains(&f.1))
        .map(|f| (f.0, f.1, f.2))
        .collect();
    let names: Vec<&str> = cols.iter().map(|c| c.1).collect();
    let mut sets: Vec<String> = cols
        .iter()
        .enumerate()
        .filter(|(_, (_, k, _))| !WATER.contains(k))
        .map(|(i, (c, _, _))| format!("{c} = COALESCE(?{}, {c})", i + 1))
        .collect();
    sets.extend(water_sets(&names, None));
    let mut params: Vec<Param> = cols.iter().map(|(_, k, kind)| param(&d.fields, k, *kind)).collect();
    params.push(Param::Text(d.id.clone()));
    Stmt::new(format!("UPDATE listings SET {} WHERE id = ?{}", sets.join(", "), cols.len() + 1), params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const T: &str = "2026-10-05T12:00:00.000Z";

    fn scope(mode: &str) -> Scope {
        Scope { source: Some("streeteasy".into()), market: Some("nyc".into()), mode: Some(mode.into()), seen_at: T.into() }
    }

    fn nyc(id: &str, price: i64, dom: Option<i64>) -> Value {
        normalize(
            &json!({"id": id, "price": price, "beds": 2, "homeType": "condo", "neighborhood": "Astoria", "daysOnMarket": dom}),
            &scope("quick"),
        )
        .unwrap()
    }

    fn known(price: f64, last_seen: &str) -> Existing {
        Existing { price: Some(price), last_seen: Some(last_seen.into()), status: Some("active".into()), ..Default::default() }
    }

    #[test]
    fn payload_is_validated_and_capped() {
        let p = parse_payload(
            &json!({"listings": [{"id": "se:1", "price": 500000}, {"id": "se:2"}, {"price": 1}, 3],
                    "scope": {"source": "streeteasy", "market": "nyc", "mode": "quick", "seenAt": T}}),
            "NOW",
        )
        .unwrap();
        assert_eq!(p.listings.len(), 1);
        assert_eq!(p.rejected.len(), 3);
        assert_eq!(p.listings[0]["homeType"], "other");
        assert_eq!(p.listings[0]["compOnly"], false);
        assert!(parse_payload(&json!({"listings": [], "scope": {"mode": "slow"}}), T).is_err());
        assert!(parse_payload(&json!({"listings": [], "scope": {"seenAt": "today"}}), T).is_err());
        let many: Vec<Value> = (0..201).map(|i| json!({"id": i.to_string()})).collect();
        assert!(parse_payload(&json!({ "listings": many }), T).is_err());
        assert!(parse_payload(&json!([]), T).is_err());
    }

    #[test]
    fn normalize_caps_and_drops_unread_detail_fields() {
        let long = "x".repeat(5000);
        let l = normalize(
            &json!({"id": "zl:1", "source": "zillow", "market": "mi", "price": 499999.6, "sqft": 0,
                    "description": long, "waterType": "inland", "status": "sold", "homeType": "castle"}),
            &Scope::default(),
        )
        .unwrap();
        assert_eq!(l["price"], 500000);
        assert!(l["sqft"].is_null(), "0 sq ft is unknown");
        assert!(l["description"].is_null() && l["waterType"].is_null(), "no detailReadAt: detail fields dropped");
        assert_eq!(l["compOnly"], true, "sold rows are comps only");
        assert_eq!(l["homeType"], "other");
        let l = normalize(
            &json!({"id": "zl:1", "source": "zillow", "market": "mi", "price": 1, "description": long,
                    "waterType": "lagoon", "detailReadAt": T}),
            &Scope::default(),
        )
        .unwrap();
        assert_eq!(l["description"].as_str().unwrap().len(), MAX_DESCRIPTION_CHARS);
        assert!(l["waterType"].is_null(), "unknown water type");
        assert!(l["waterSource"].is_null(), "no water type, no source");
    }

    #[test]
    fn normalize_keeps_map_water_and_sets_the_source() {
        let mi = |extra: Value| {
            let mut l = json!({"id": "zl:1", "source": "zillow", "market": "mi", "price": 1});
            l.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            normalize(&l, &Scope::default()).unwrap()
        };
        let l = mi(json!({"waterType": "inland", "waterBody": "Torch Lake", "waterSource": "map", "frontageFt": 100}));
        assert_eq!((l["waterType"].as_str(), l["waterBody"].as_str(), l["waterSource"].as_str()), (Some("inland"), Some("Torch Lake"), Some("map")));
        assert!(l["frontageFt"].is_null(), "only water type and body come from the map");
        let l = mi(json!({"waterType": "lagoon", "waterBody": "X", "waterSource": "map"}));
        assert!(l["waterType"].is_null() && l["waterBody"].is_null() && l["waterSource"].is_null());
        let l = mi(json!({"waterType": "inland", "waterSource": "satellite"}));
        assert!(l["waterType"].is_null() && l["waterSource"].is_null());
        let l = mi(json!({"waterType": "access", "waterSource": "map", "detailReadAt": T}));
        assert_eq!(l["waterSource"], "description", "a detail read is always the description");
    }

    #[test]
    fn map_water_changes_are_relevant_only_before_a_detail_read() {
        let card = |t: Option<&str>| {
            let mut l = nyc("a", 500000, Some(9));
            if let Some(t) = t {
                l["waterType"] = json!(t);
                l["waterBody"] = json!("Torch Lake");
                l["waterSource"] = json!("map");
            }
            l
        };
        let mut p = Planner::new(HashMap::from([("a".to_string(), known(500000.0, T))]));
        assert_eq!(p.plan(&card(Some("inland")), &scope("quick")).len(), 1, "new map water is written");
        assert!(p.plan(&card(Some("inland")), &scope("quick")).is_empty(), "the same again is not");
        assert_eq!(p.plan(&card(None), &scope("quick")).len(), 1, "cleared map water is written");
        let mut e = known(500000.0, T);
        e.detail_read_at = Some(T.into());
        e.water_type = Some("access".into());
        let mut p = Planner::new(HashMap::from([("a".to_string(), e)]));
        assert!(p.plan(&card(Some("inland")), &scope("quick")).is_empty(), "a read description wins");
    }

    #[test]
    fn new_listings_are_fresh_only_when_young_quick_and_not_first() {
        let mut p = Planner::new(HashMap::new()).with_prior_run(true);
        let st = p.plan(&nyc("young", 500000, Some(2)), &scope("quick"));
        assert!(st[0].sql.starts_with("INSERT OR IGNORE INTO listings"));
        assert!(st[1].sql.contains("price_history"));
        p.plan(&nyc("old", 500000, Some(40)), &scope("quick"));
        p.plan(&nyc("young-full", 500000, Some(1)), &scope("full"));
        p.plan(&nyc("unknown-full", 500000, None), &scope("full"));
        p.plan(&nyc("unknown-quick", 500000, None), &scope("quick"));
        let mut c = nyc("comp", 500000, Some(1));
        c["compOnly"] = json!(true);
        p.plan(&c, &scope("quick"));
        let mut fresh: Vec<&str> = p.fresh.iter().map(String::as_str).collect();
        fresh.sort();
        assert_eq!(fresh, vec!["unknown-quick", "young"], "quick only; full never makes new listings fresh");
        assert_eq!(p.changed.len(), 6, "all are stored");
        assert_eq!(p.stats.added, 6);

        let mut first = Planner::new(HashMap::new());
        first.plan(&nyc("young", 500000, Some(0)), &scope("quick"));
        assert!(first.fresh.is_empty(), "a market's first crawl run never alerts");
        assert!(prior_run_query(&scope("quick")).unwrap().sql.contains("NOT (mode = ?2 AND seen_at = ?3)"));
    }

    #[test]
    fn unchanged_and_recent_writes_nothing() {
        let k = HashMap::from([("a".to_string(), known(500000.0, "2026-10-04T18:00:00.000Z"))]);
        let mut p = Planner::new(k);
        assert!(p.plan(&nyc("a", 500000, Some(9)), &scope("quick")).is_empty(), "seen 18 h ago");
        assert_eq!((p.stats.unchanged, p.stats.updated), (1, 0));
        assert!(p.changed.is_empty());

        let k = HashMap::from([("a".to_string(), known(500000.0, "2026-10-04T15:00:00.000Z"))]);
        let mut p = Planner::new(k);
        let st = p.plan(&nyc("a", 500000, Some(9)), &scope("quick"));
        assert_eq!(st.len(), 1, "seen 21 h ago: last_seen refreshed, no history");
        assert!(p.changed.is_empty(), "a touch is not a scoring change");
    }

    #[test]
    fn drops_rises_and_relists() {
        let mut gone = known(500000.0, "2026-09-01T00:00:00.000Z");
        gone.removed = true;
        let k = HashMap::from([
            ("drop".to_string(), known(500000.0, T)),
            ("rise".to_string(), known(500000.0, T)),
            ("back".to_string(), gone),
        ]);
        let mut p = Planner::new(k);
        let st = p.plan(&nyc("drop", 450000, Some(30)), &scope("full"));
        assert_eq!(st.len(), 2);
        assert!(st[0].sql.contains("price_changes = price_changes + ?"));
        assert!(st[0].sql.contains("description = COALESCE(?"));
        assert!(st[0].sql.contains("water_type = CASE WHEN ?"));
        assert!(st[0].sql.contains("removed_at = NULL"));
        p.plan(&nyc("rise", 550000, Some(30)), &scope("full"));
        p.plan(&nyc("back", 500000, Some(30)), &scope("full"));
        assert_eq!((p.stats.price_drops, p.stats.price_rises, p.stats.relisted, p.stats.updated), (1, 1, 1, 3));
        let mut fresh: Vec<&str> = p.fresh.iter().map(String::as_str).collect();
        fresh.sort();
        assert_eq!(fresh, vec!["back", "drop"]);
        assert_eq!(p.changed, vec!["drop", "rise", "back"]);
    }

    #[test]
    fn a_detail_read_is_a_change_but_the_same_one_twice_is_not() {
        let mut e = known(500000.0, T);
        e.detail_read_at = Some("2026-10-05T10:00:00.000Z".into());
        let mut p = Planner::new(HashMap::from([("a".to_string(), e)]));
        let mut l = nyc("a", 500000, Some(9));
        l["detailReadAt"] = json!("2026-10-05T10:00:00.000Z");
        assert!(p.plan(&l, &scope("quick")).is_empty());
        l["detailReadAt"] = json!("2026-10-05T11:00:00.000Z");
        assert_eq!(p.plan(&l, &scope("quick")).len(), 1);
        assert_eq!(p.changed, vec!["a"]);
    }

    #[test]
    fn existing_rows_parse() {
        let (id, e) = existing_from_row(&json!({"id": "x", "price": 5.0, "removed_at": "T", "comp_only": 1.0,
                                                "status": "active", "last_seen": "L"}))
        .unwrap();
        assert_eq!(id, "x");
        assert!(e.removed && e.comp_only);
        assert_eq!(existing_queries(&(0..91).map(|i| i.to_string()).collect::<Vec<_>>()).len(), 2);
    }

    #[test]
    fn details_parse_and_update() {
        let d = parse_details(
            &json!({"listings": [{"id": "zl:1", "detailReadAt": T, "waterType": "inland", "frontageFt": 100.4,
                                  "description": "On Torch Lake"}, {"nope": 1}]}),
            "NOW",
        )
        .unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].fields["frontageFt"], 100);
        let st = detail_update(&d[0]);
        assert!(st.sql.starts_with("UPDATE listings SET year_built = COALESCE(?1, year_built)"), "{}", st.sql);
        assert!(st.params.iter().filter(|p| p.is_null()).count() >= 3, "absent fields bind null and keep the stored value");
    }
}
