//! Use-case tests on an in-memory SQLite loaded with the real D1 migrations.
//! Rows come back the way D1 hands them to the Worker (numbers as doubles).
//!
//! Scoring runs in the crawl job; [`job`] plays it here: it pages through
//! `score_input`, runs the scorer's batch pass (what `housedeals-score` runs)
//! and posts the writes to `post_scores` in chunks, as the crawler does.

use super::*;
use crate::alerts::NoNotifier;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use rusqlite::types::{ToSqlOutput, Value as Sv, ValueRef};
use rusqlite::Connection;

const MIGRATIONS: &[&str] = &[
    include_str!("../../../d1/migrations/0001_init.sql"),
    include_str!("../../../d1/migrations/0002_needs_detail_sold.sql"),
    include_str!("../../../d1/migrations/0003_score_input.sql"),
    include_str!("../../../d1/migrations/0004_water_source.sql"),
];

/// The store's futures never wait on anything, so one poll finishes them.
fn block_on<F: Future>(f: F) -> F::Output {
    let mut cx = Context::from_waker(Waker::noop());
    match pin!(f).as_mut().poll(&mut cx) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("store future did not complete"),
    }
}

struct Db {
    conn: Connection,
    /// Every statement run, for "no writes" assertions.
    log: RefCell<Vec<String>>,
    queries: Cell<usize>,
    rows_read: Cell<usize>,
}

impl Db {
    fn new() -> Db {
        let conn = Connection::open_in_memory().unwrap();
        for m in MIGRATIONS {
            conn.execute_batch(m).unwrap();
        }
        Db::with(conn)
    }

    fn with(conn: Connection) -> Db {
        Db { conn, log: RefCell::new(Vec::new()), queries: Cell::new(0), rows_read: Cell::new(0) }
    }

    fn one(&self, sql: &str) -> Value {
        block_on(self.query(&Stmt::new(sql, vec![]))).unwrap().into_iter().next().unwrap_or(Value::Null)
    }

    fn count(&self, sql: &str) -> i64 {
        self.conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn writes(&self) -> Vec<String> {
        self.log.borrow().clone()
    }

    fn plan(&self, s: &Stmt) -> String {
        let mut st = self.conn.prepare(&format!("EXPLAIN QUERY PLAN {}", s.sql)).unwrap();
        let rows = st
            .query_map(rusqlite::params_from_iter(s.params.iter().map(to_sql)), |r| r.get::<_, String>(3))
            .unwrap();
        rows.map(Result::unwrap).collect::<Vec<_>>().join("\n")
    }

    fn deal(&self, id: &str) -> Value {
        let d = self.one(&format!("SELECT deal FROM listing_scores WHERE listing_id = '{id}'"));
        serde_json::from_str(d["deal"].as_str().unwrap_or("null")).unwrap()
    }
}

fn to_sql(p: &Param) -> ToSqlOutput<'_> {
    match p {
        Param::Null => ToSqlOutput::Owned(Sv::Null),
        Param::Int(i) => ToSqlOutput::Owned(Sv::Integer(*i)),
        Param::Real(f) => ToSqlOutput::Owned(Sv::Real(*f)),
        Param::Text(t) => ToSqlOutput::Borrowed(ValueRef::Text(t.as_bytes())),
    }
}

impl Store for Db {
    async fn query(&self, s: &Stmt) -> Result<Vec<Value>, String> {
        self.queries.set(self.queries.get() + 1);
        let mut st = self.conn.prepare(&s.sql).map_err(|e| format!("{e}: {}", s.sql))?;
        let cols: Vec<String> = st.column_names().iter().map(|c| c.to_string()).collect();
        let rows = st
            .query_map(rusqlite::params_from_iter(s.params.iter().map(to_sql)), |r| {
                let mut m = serde_json::Map::new();
                for (i, c) in cols.iter().enumerate() {
                    let v = match r.get_ref(i)? {
                        ValueRef::Null => Value::Null,
                        ValueRef::Integer(n) => json!(n as f64),
                        ValueRef::Real(f) => json!(f),
                        ValueRef::Text(t) => Value::from(String::from_utf8_lossy(t).to_string()),
                        ValueRef::Blob(_) => Value::Null,
                    };
                    m.insert(c.clone(), v);
                }
                Ok(Value::Object(m))
            })
            .map_err(|e| e.to_string())?;
        let out = rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?;
        self.rows_read.set(self.rows_read.get() + out.len());
        Ok(out)
    }

    async fn body(&self, s: &Stmt) -> Result<Option<String>, String> {
        self.queries.set(self.queries.get() + 1);
        let mut st = self.conn.prepare(&s.sql).map_err(|e| format!("{e}: {}", s.sql))?;
        let mut rows = st.query(rusqlite::params_from_iter(s.params.iter().map(to_sql))).map_err(|e| e.to_string())?;
        match rows.next().map_err(|e| e.to_string())? {
            Some(r) => r.get::<_, Option<String>>("body").map_err(|e| e.to_string()),
            None => Ok(None),
        }
    }

    async fn batch(&self, stmts: Vec<Stmt>) -> Result<Vec<Written>, String> {
        let tx = self.conn.unchecked_transaction().map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for s in &stmts {
            let n = tx
                .execute(&s.sql, rusqlite::params_from_iter(s.params.iter().map(to_sql)))
                .map_err(|e| format!("{e}: {}", s.sql))?;
            self.log.borrow_mut().push(s.sql.split_whitespace().take(3).collect::<Vec<_>>().join(" "));
            out.push(Written { changes: n as u64, rows_written: n as u64 });
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(out)
    }
}

const NOW: &str = "2026-10-05T12:00:00.000Z";
const LATER: &str = "2026-10-05T12:30:00.000Z";

fn cfg() -> Config {
    Config { rule: scorer::AlertRule::default() }
}

fn apt(id: &str, nbhd: &str, price: i64, dom: i64) -> Value {
    json!({"id": id, "source": "streeteasy", "market": "nyc", "status": "active",
           "url": format!("https://streeteasy.com/sale/{id}"), "address": "1 Main St", "price": price,
           "sqft": 1000, "beds": 2, "homeType": "condo", "neighborhood": nbhd, "borough": "queens",
           "daysOnMarket": dom, "compOnly": false})
}

fn house(id: &str, area: &str, price: i64, dom: i64) -> Value {
    json!({"id": id, "source": "zillow", "market": "mi", "status": "active",
           "url": format!("https://www.zillow.com/homedetails/{id}"), "address": "1 Lake Rd", "price": price,
           "sqft": 2000, "beds": 3, "homeType": "single_family", "county": "antrim", "area": area,
           "daysOnMarket": dom, "compOnly": false})
}

fn push(db: &Db, listings: Vec<Value>, market: &str, mode: &str, at: &str) -> IngestOutcome {
    let source = if market == "nyc" { "streeteasy" } else { "zillow" };
    let body = json!({"listings": listings, "scope": {"source": source, "market": market, "mode": mode, "seenAt": at}});
    block_on(ingest(db, &body, at)).unwrap()
}

fn post_detail(db: &Db, listings: Value, at: &str) -> DetailOutcome {
    block_on(detail(db, &json!({ "listings": listings }), at)).unwrap()
}

/// What one scoring pass of the crawl job did.
#[derive(Debug, Default)]
struct Job {
    rows: usize,
    pages: usize,
    upserts: usize,
    deletes: usize,
    alerts: usize,
    stored: ScoresOutcome,
}

/// The crawl job for one market: score-input pages of `page` rows, the
/// scorer, then POST /api/scores in chunks of at most 50 items, alerts first.
fn job_paged(db: &Db, market: &str, now: &str, page: usize) -> Job {
    let mut out = Job::default();
    let (mut rows, mut after) = (Vec::new(), String::new());
    let columns = loop {
        let q = ScoreInputQuery { market: market.into(), after: after.clone(), limit: page };
        let body: Value = serde_json::from_str(&block_on(score_input(db, &q, now)).unwrap()).unwrap();
        out.pages += 1;
        rows.extend(body["rows"].as_array().unwrap().iter().cloned());
        match body["last"].as_str() {
            Some(last) if body["n"].as_u64() == Some(page as u64) => after = last.to_string(),
            _ => break body["columns"].clone(),
        }
    };
    out.rows = rows.len();
    let input = json!({"market": market, "now": now, "columns": columns, "rows": rows});
    let w = scorer::batch::run(&input, &scorer::batch::Settings::default()).unwrap();
    let items: Vec<(&str, &Value)> = w["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| ("alerts", v))
        .chain(w["upserts"].as_array().unwrap().iter().map(|v| ("upserts", v)))
        .chain(w["deletes"].as_array().unwrap().iter().map(|v| ("deletes", v)))
        .collect();
    out.alerts = w["alerts"].as_array().unwrap().len();
    out.upserts = w["upserts"].as_array().unwrap().len();
    out.deletes = w["deletes"].as_array().unwrap().len();
    for chunk in items.chunks(50) {
        let mut body = json!({"market": market, "upserts": [], "deletes": [], "alerts": []});
        for (k, v) in chunk {
            body[*k].as_array_mut().unwrap().push((*v).clone());
        }
        let r = block_on(post_scores(db, &NoNotifier, &body.to_string(), now)).unwrap();
        out.stored.upserted += r.upserted;
        out.stored.deleted += r.deleted;
        out.stored.new_alerts += r.new_alerts;
        out.stored.patched_alerts += r.patched_alerts;
    }
    out
}

fn job(db: &Db, market: &str, now: &str) -> Job {
    job_paged(db, market, now, 4)
}

fn deals_json(db: &Db, pairs: &[(&str, &str)]) -> Value {
    serde_json::from_str(&block_on(deals(db, &DealsQuery::from_pairs(pairs.iter().copied()).unwrap(), NOW)).unwrap()).unwrap()
}

fn alerts_json(db: &Db, pairs: &[(&str, &str)]) -> Value {
    serde_json::from_str(&block_on(recent_alerts(db, &AlertsQuery::from_pairs(pairs.iter().copied()).unwrap(), NOW)).unwrap())
        .unwrap()
}

fn stats_json(db: &Db) -> Value {
    serde_json::from_str(&block_on(stats(db, NOW)).unwrap()).unwrap()
}

/// Ten Astoria 2bd condos around $800/sq ft, an old full-sweep load.
fn astoria(db: &Db) {
    let comps: Vec<Value> = (0..10).map(|i| apt(&format!("se:c{i}"), "Astoria", 790_000 + (i % 3) * 10_000, 40)).collect();
    let r = push(db, comps, "nyc", "full", NOW);
    assert_eq!((r.stats.added, r.scored, r.new_alerts), (10, 0, 0));
}

#[test]
fn ingest_only_stores_and_the_job_scores_without_alerting_a_first_sweep() {
    let db = Db::new();
    astoria(&db);
    // An old cheap listing in the same sweep: stored, scored by the job, not fresh.
    let r = push(&db, vec![apt("se:old", "Astoria", 600_000, 60)], "nyc", "full", NOW);
    assert_eq!((r.stats.added, r.scored, r.new_alerts), (1, 0, 0));
    assert_eq!(db.count("SELECT COUNT(*) FROM listing_scores"), 0, "ingest never scores");
    assert!(db.writes().iter().all(|w| !w.contains("listing_scores") && !w.contains("deal_alerts")), "{:?}", db.writes());

    let j = job(&db, "nyc", NOW);
    assert_eq!((j.rows, j.pages, j.upserts, j.alerts), (11, 3, 11, 0));
    assert_eq!(j.stored.upserted, 11);
    assert_eq!(db.count("SELECT COUNT(*) FROM listing_scores"), 11);
    assert_eq!(db.count("SELECT COUNT(*) FROM deal_alerts"), 0);
    let d = deals_json(&db, &[("market", "nyc")]);
    assert_eq!(d["generatedAt"], NOW);
    assert_eq!(d["deals"][0]["id"], "se:old", "best discount first");
    assert_eq!(d["deals"][0]["alert"], true, "the rule accepts it; it is just not fresh");
    assert_eq!(d["deals"][0]["group"], "Astoria · condo · 2bd");
    let pcts: Vec<f64> = d["deals"].as_array().unwrap().iter().map(|x| x["discountPct"].as_f64().unwrap()).collect();
    assert!(pcts.windows(2).all(|w| w[0] >= w[1]), "{pcts:?}");

    // The same pass again: nothing moved, nothing written.
    let j = job(&db, "nyc", LATER);
    assert_eq!((j.upserts, j.deletes, j.alerts), (0, 0, 0));
}

#[test]
fn a_fresh_deal_alerts_once_and_an_unchanged_repush_writes_nothing() {
    let db = Db::new();
    astoria(&db);
    job(&db, "nyc", NOW);
    let deal = apt("se:deal", "Astoria", 600_000, 1);
    let r = push(&db, vec![deal.clone()], "nyc", "quick", NOW);
    assert_eq!(r.stats.added, 1);
    let j = job(&db, "nyc", NOW);
    assert_eq!((j.alerts, j.stored.new_alerts), (1, 1));
    assert!(j.upserts >= 1, "the deal, and comps whose baseline moved >= 0.5 pt");
    let a = alerts_json(&db, &[]);
    assert_eq!(a["alerts"][0]["id"], "se:deal");
    assert_eq!(a["alerts"][0]["createdAt"], NOW);
    assert!(a["alerts"][0]["discountPct"].as_f64().unwrap() >= 24.0);

    // The same listing half an hour later: no listing write.
    let before = db.writes().len();
    let q = db.queries.get();
    let r = push(&db, vec![deal.clone()], "nyc", "quick", LATER);
    assert_eq!((r.stats.unchanged, r.stats.updated), (1, 0));
    assert_eq!(db.writes()[before..].to_vec(), vec!["INSERT INTO crawls"], "only the crawl-run row");
    assert_eq!(db.queries.get() - q, 2, "prior-run check + id lookup");
    let j = job(&db, "nyc", LATER);
    assert_eq!((j.upserts, j.alerts), (0, 0), "nothing moved");

    // A price drop: history row, a new alert key.
    let mut cheaper = deal.clone();
    cheaper["price"] = json!(580_000);
    let r = push(&db, vec![cheaper], "nyc", "quick", LATER);
    assert_eq!(r.stats.price_drops, 1);
    let j = job(&db, "nyc", LATER);
    assert_eq!((j.upserts, j.stored.new_alerts), (1, 1));
    assert_eq!(db.count("SELECT COUNT(*) FROM price_history WHERE listing_id = 'se:deal'"), 2);
    assert_eq!(db.count("SELECT price_changes FROM listings WHERE id = 'se:deal'"), 1);
    assert_eq!(db.count("SELECT COUNT(*) FROM deal_alerts"), 2);

    // Crawl runs are summed per (market, mode, seenAt).
    let s = stats_json(&db);
    assert!(s["crawls"].as_array().unwrap().iter().any(|c| c["mode"] == "quick" && c["at"] == LATER));
    assert!(s["counts"].as_array().unwrap().iter().any(|c| c["market"] == "nyc" && c["status"] == "active" && c["n"] == 11));
    assert_eq!(s["alerts"], json!([{"market": "nyc", "n": 2}]));
    assert_eq!(s["scored"], json!([{"market": "nyc", "n": 11}]));
    let run = db.one("SELECT batches, stats FROM crawls WHERE mode = 'quick' AND seen_at = '2026-10-05T12:30:00.000Z'");
    assert_eq!(run["batches"], 2.0);
    let st: Value = serde_json::from_str(run["stats"].as_str().unwrap()).unwrap();
    assert_eq!((st["seen"].as_f64(), st["unchanged"].as_f64(), st["priceDrops"].as_f64()), (Some(2.0), Some(1.0), Some(1.0)));
}

#[test]
fn a_markets_first_quick_run_never_alerts() {
    let db = Db::new();
    // The very first crawl of NYC is a quick run: comps and a young cheap listing together.
    let mut batch: Vec<Value> = (0..10).map(|i| apt(&format!("se:c{i}"), "Astoria", 800_000, 0)).collect();
    batch.push(apt("se:deal", "Astoria", 600_000, 1));
    push(&db, batch, "nyc", "quick", NOW);
    // A second batch of the same run is still the first run.
    push(&db, vec![apt("se:deal2", "Astoria", 600_000, 1)], "nyc", "quick", NOW);
    assert_eq!(job(&db, "nyc", NOW).alerts, 0);
    // The next run alerts on a new young deal (StreetEasy sends no daysOnMarket).
    let mut d = apt("se:deal3", "Astoria", 600_000, 0);
    d["daysOnMarket"] = Value::Null;
    push(&db, vec![d], "nyc", "quick", LATER);
    // Full sweeps never make a new listing fresh, whatever its age.
    push(&db, vec![apt("se:deal4", "Astoria", 600_000, 0)], "nyc", "full", LATER);
    let j = job(&db, "nyc", LATER);
    assert_eq!(j.stored.new_alerts, 1);
    assert_eq!(alerts_json(&db, &[("market", "nyc")])["alerts"][0]["id"], "se:deal3");
}

#[test]
fn fresh_listings_alert_for_72_hours_only() {
    let db = Db::new();
    astoria(&db);
    push(&db, vec![apt("se:deal", "Astoria", 600_000, 1)], "nyc", "quick", LATER);
    // The job did not run for four days (or the listing had no score until then).
    let j = job(&db, "nyc", "2026-10-09T13:00:00.000Z");
    assert_eq!((j.upserts, j.alerts), (11, 0));
}

#[test]
fn a_thin_neighbourhood_is_refused_not_priced_borough_wide() {
    // The scorer has no borough fallback (DESIGN.md "Scorer"): nine Sunnyside
    // comps do not price a Woodside listing.
    let db = Db::new();
    let comps: Vec<Value> = (0..9).map(|i| apt(&format!("se:s{i}"), "Sunnyside", 800_000, 40)).collect();
    push(&db, comps, "nyc", "full", NOW);
    push(&db, vec![apt("se:w", "Woodside", 640_000, 1)], "nyc", "quick", LATER);
    let j = job(&db, "nyc", LATER);
    assert_eq!(j.alerts, 0);
    assert!(db.deal("se:w").is_null(), "refused, not stored");
    assert_eq!(db.count("SELECT COUNT(*) FROM listing_scores"), 9, "the Sunnyside comps price each other");
}

#[test]
fn michigan_detail_reads_are_stored_sticky_and_alert_fresh_listings() {
    let db = Db::new();
    // Six inland comps, already read (water type known).
    let comps: Vec<Value> = (0..6)
        .map(|i| {
            let mut h = house(&format!("zl:{i}"), "traverse", 800_000, 50);
            h["detailReadAt"] = json!(NOW);
            h["waterType"] = json!("inland");
            h["waterBody"] = json!("Torch Lake");
            h["description"] = json!("120 ft on Torch Lake");
            h
        })
        .collect();
    push(&db, comps, "mi", "full", NOW);
    // A new cheap listing: water type unknown, so it lands in "other" and does not price.
    push(&db, vec![house("zl:new", "traverse", 600_000, 1)], "mi", "quick", NOW);
    let j = job(&db, "mi", NOW);
    assert_eq!((j.rows, j.upserts, j.alerts), (7, 0, 0), "each inland comp has only 5 others");
    assert_eq!(db.count("SELECT COUNT(*) FROM listing_scores"), 0);

    let nd = block_on(needs_detail(&db, &cfg(), &json!({"market": "mi", "limit": 5}), NOW)).unwrap();
    assert_eq!(nd["ids"], json!(["zl:new"]));
    assert_eq!(nd["urls"], json!(["https://www.zillow.com/homedetails/zl:new"]));

    let before = db.writes().len();
    let r = post_detail(
        &db,
        json!([{"id": "zl:new", "detailReadAt": LATER, "waterType": "inland", "waterBody": "Elk Lake", "frontageFt": 90,
                "description": "On Elk Lake"}, {"id": "zl:missing", "detailReadAt": LATER}]),
        LATER,
    );
    assert_eq!((r.updated, r.unknown, r.scored, r.new_alerts), (1, 1, 0, 0));
    assert_eq!(db.writes()[before..].to_vec(), vec!["UPDATE listings SET", "UPDATE listings SET"], "updates only");
    let j = job(&db, "mi", LATER);
    assert_eq!((j.upserts, j.stored.new_alerts), (7, 1), "the new inland listing gives every inland comp a sixth comp");
    let a = alerts_json(&db, &[("market", "mi")]);
    assert_eq!((a["alerts"][0]["group"].as_str(), a["alerts"][0]["waterBody"].as_str()), (Some("Traverse · inland"), Some("Elk Lake")));
    let nd = block_on(needs_detail(&db, &cfg(), &json!({"market": "mi"}), LATER)).unwrap();
    assert_eq!(nd["ids"], json!([]));

    // A later card-only push (no detailReadAt, price unchanged, 25 h later) keeps the detail fields.
    let r = push(&db, vec![house("zl:new", "traverse", 600_000, 2)], "mi", "full", "2026-10-06T13:30:00.000Z");
    assert_eq!(r.stats.updated, 1, "last_seen refreshed");
    let row = db.one("SELECT water_type, water_body, frontage_ft, description, detail_read_at FROM listings WHERE id = 'zl:new'");
    assert_eq!(row["water_type"], "inland");
    assert_eq!(row["frontage_ft"], 90.0);
    assert_eq!(row["description"], "On Elk Lake");
    assert_eq!(row["detail_read_at"], LATER);
}

fn water(db: &Db, id: &str) -> (Value, Value, Value) {
    let r = db.one(&format!("SELECT water_type, water_body, water_source FROM listings WHERE id = '{id}'"));
    (r["water_type"].clone(), r["water_body"].clone(), r["water_source"].clone())
}

#[test]
fn map_water_from_cards_is_stored_until_a_description_is_read() {
    let db = Db::new();
    let map = |id: &str, t: &str, b: &str| {
        let mut h = house(id, "traverse", 700_000, 5);
        h["waterType"] = json!(t);
        h["waterBody"] = json!(b);
        h["waterSource"] = json!("map");
        h
    };
    let s = |v: &str| json!(v);
    // Six map-inland comps and a cheap fresh map-inland listing: map water prices and alerts.
    let comps: Vec<Value> = (0..6).map(|i| map(&format!("zl:{i}"), "inland", "Torch Lake")).collect();
    push(&db, comps, "mi", "full", NOW);
    let mut cheap = map("zl:cheap", "inland", "Elk Lake");
    cheap["price"] = json!(550_000);
    cheap["daysOnMarket"] = json!(1);
    push(&db, vec![cheap], "mi", "quick", LATER);
    assert_eq!(water(&db, "zl:cheap"), (s("inland"), s("Elk Lake"), s("map")));
    let j = job(&db, "mi", LATER);
    assert_eq!(j.stored.new_alerts, 1, "map-derived inland counts for the alert rule");
    let a = alerts_json(&db, &[("market", "mi")]);
    assert_eq!((a["alerts"][0]["waterBody"].as_str(), a["alerts"][0]["waterSource"].as_str()), (Some("Elk Lake"), Some("map")));
    // A card without the map's blessing ("waterSource" missing) carries no water.
    let mut bare = house("zl:bare", "traverse", 700_000, 5);
    bare["waterType"] = json!("inland");
    push(&db, vec![bare], "mi", "full", NOW);
    assert_eq!(water(&db, "zl:bare"), (Value::Null, Value::Null, Value::Null));

    // A changed map classification on a never-read row is written right away (not a 20 h touch)...
    let r = push(&db, vec![map("zl:0", "great_lakes", "West Grand Traverse Bay")], "mi", "full", LATER);
    assert_eq!(r.stats.updated, 1);
    assert_eq!(water(&db, "zl:0"), (s("great_lakes"), s("West Grand Traverse Bay"), s("map")));
    // ...and an unchanged one writes nothing.
    let before = db.writes().len();
    push(&db, vec![map("zl:0", "great_lakes", "West Grand Traverse Bay")], "mi", "full", LATER);
    assert_eq!(db.writes().len() - before, 1, "only the crawls row");
    // The map no longer placing it on water clears the map value.
    push(&db, vec![house("zl:1", "traverse", 700_000, 5)], "mi", "full", LATER);
    assert_eq!(water(&db, "zl:1"), (Value::Null, Value::Null, Value::Null));

    // A description wins: type, body and source replaced whole (body may be null)...
    post_detail(&db, json!([{"id": "zl:cheap", "detailReadAt": LATER, "waterType": "access", "description": "Deeded access"}]), LATER);
    assert_eq!(water(&db, "zl:cheap"), (s("access"), Value::Null, s("description")));
    // ...and later map cards never overwrite it.
    let mut again = map("zl:cheap", "inland", "Elk Lake");
    again["price"] = json!(550_000);
    push(&db, vec![again], "mi", "full", "2026-10-07T12:00:00.000Z");
    assert_eq!(water(&db, "zl:cheap"), (s("access"), Value::Null, s("description")));
    // A description that names no water keeps the map's value, and the map stops updating it.
    post_detail(&db, json!([{"id": "zl:2", "detailReadAt": LATER, "description": "Nice house"}]), LATER);
    assert_eq!(water(&db, "zl:2"), (s("inland"), s("Torch Lake"), s("map")));
    push(&db, vec![map("zl:2", "great_lakes", "Lake Michigan")], "mi", "full", "2026-10-07T12:00:00.000Z");
    assert_eq!(water(&db, "zl:2"), (s("inland"), s("Torch Lake"), s("map")));
    // A listing push that carries its own detail read behaves like a detail post.
    let mut d = map("zl:3", "great_lakes", "Lake Michigan");
    d["detailReadAt"] = json!(LATER);
    d["waterType"] = json!("other");
    d["waterBody"] = json!("Boardman River");
    push(&db, vec![d], "mi", "full", LATER);
    assert_eq!(water(&db, "zl:3"), (s("other"), s("Boardman River"), s("description")));
}

#[test]
fn old_listings_do_not_alert_after_their_detail_read() {
    let db = Db::new();
    let comps: Vec<Value> = (0..6)
        .map(|i| {
            let mut h = house(&format!("zl:{i}"), "traverse", 800_000, 50);
            h["detailReadAt"] = json!(NOW);
            h["waterType"] = json!("inland");
            h
        })
        .collect();
    push(&db, comps, "mi", "full", NOW);
    push(&db, vec![house("zl:old", "traverse", 600_000, 90)], "mi", "full", NOW);
    let r = post_detail(&db, json!([{"id": "zl:old", "detailReadAt": LATER, "waterType": "inland"}]), LATER);
    assert_eq!(r.updated, 1);
    assert_eq!(job(&db, "mi", LATER).alerts, 0);
    assert_eq!(db.count("SELECT alert FROM listing_scores WHERE listing_id = 'zl:old'"), 1, "a deal, just not a fresh one");
}

#[test]
fn nyc_needs_detail_lists_cheap_scored_listings_and_alerts_show_the_detail() {
    let db = Db::new();
    astoria(&db);
    job(&db, "nyc", NOW);
    push(&db, vec![apt("se:deal", "Astoria", 650_000, 1)], "nyc", "quick", NOW); // ~18% under, fresh
    push(&db, vec![apt("se:fair", "Astoria", 790_000, 1)], "nyc", "quick", NOW);
    let nd = block_on(needs_detail(&db, &cfg(), &json!({"market": "nyc"}), NOW)).unwrap();
    assert_eq!(nd["ids"], json!([]), "NYC needs-detail follows the stored scores, which the job writes");
    assert_eq!(job(&db, "nyc", NOW).stored.new_alerts, 1);
    let nd = block_on(needs_detail(&db, &cfg(), &json!({"market": "nyc"}), NOW)).unwrap();
    assert_eq!(nd["ids"], json!(["se:deal"]));

    let r = post_detail(&db, json!([{"id": "se:deal", "detailReadAt": LATER, "maintenance": 950, "taxes": 400}]), LATER);
    assert_eq!(r.updated, 1);
    let j = job(&db, "nyc", "2026-10-05T12:45:00.000Z");
    assert_eq!((j.upserts, j.stored.new_alerts, j.stored.patched_alerts), (1, 0, 1), "rewritten: read after it was scored");
    assert_eq!(db.deal("se:deal")["maintenance"], 950, "the stored deal shows the detail fields");
    assert_eq!(alerts_json(&db, &[])["alerts"][0]["maintenance"], 950, "and so does the alert");
    assert_eq!(block_on(needs_detail(&db, &cfg(), &json!({"market": "nyc"}), NOW)).unwrap()["ids"], json!([]));
    assert_eq!(job(&db, "nyc", "2026-10-05T13:00:00.000Z").upserts, 0);
}

#[test]
fn expiry_needs_a_recent_full_sweep_and_drops_scores() {
    let db = Db::new();
    // Seen 5 days ago in a quick crawl only.
    let comps: Vec<Value> = (0..10).map(|i| apt(&format!("se:c{i}"), "Astoria", 800_000, 40)).collect();
    push(&db, comps, "nyc", "quick", "2026-09-30T12:00:00.000Z");
    job(&db, "nyc", "2026-09-30T12:00:00.000Z");
    assert_eq!(db.count("SELECT COUNT(*) FROM listing_scores"), 10);
    let cutoff = "2026-10-02T12:00:00.000Z";
    assert_eq!(block_on(expire(&db, NOW, cutoff)).unwrap(), 0, "no full sweep ran: nothing expires");
    // A full sweep today saw only one of them.
    push(&db, vec![apt("se:c0", "Astoria", 800_000, 45)], "nyc", "full", "2026-10-05T11:30:00.000Z");
    assert_eq!(block_on(expire(&db, NOW, cutoff)).unwrap(), 9);
    assert_eq!(db.count("SELECT COUNT(*) FROM listings WHERE removed_at IS NULL"), 1);
    assert_eq!(db.count("SELECT COUNT(*) FROM listing_scores WHERE listing_id != 'se:c0'"), 0);
    let s = stats_json(&db);
    assert!(s["counts"].as_array().unwrap().iter().any(|c| c["status"] == "expired" && c["n"] == 9));
    // The job no longer sees them; c0 alone no longer prices, so its score goes too.
    let j = job(&db, "nyc", NOW);
    assert_eq!((j.rows, j.deletes, j.stored.deleted), (1, 1, 1));
    // One comes back: relisted, fresh.
    let r = push(&db, vec![apt("se:c5", "Astoria", 800_000, 50)], "nyc", "quick", LATER);
    assert_eq!(r.stats.relisted, 1);
    assert!(db.one("SELECT fresh_at FROM listings WHERE id = 'se:c5'")["fresh_at"].is_string());
}

#[test]
fn sold_comps_count_for_michigan() {
    let db = Db::new();
    let sold: Vec<Value> = (0..6)
        .map(|i| {
            let mut h = house(&format!("zl:s{i}"), "petoskey", 700_000, 0);
            h["status"] = json!("sold");
            h["soldAt"] = json!(if i == 0 { "2025-01-01" } else { "2026-06-01" });
            h
        })
        .collect();
    assert_eq!(push(&db, sold, "mi", "sold", NOW).stats.added, 6);
    let j = job(&db, "mi", NOW);
    assert_eq!((j.rows, j.upserts), (5, 0), "the old sale is not even read; sold rows are comps, never subjects");
    push(&db, vec![house("zl:a", "petoskey", 500_000, 1)], "mi", "quick", NOW);
    assert_eq!(job(&db, "mi", NOW).upserts, 0, "only 5 recent sold comps (+ 1 too old) < 6");
    push(&db, vec![house("zl:b", "petoskey", 700_000, 30)], "mi", "full", NOW);
    job(&db, "mi", NOW);
    let d = db.deal("zl:a");
    assert_eq!((d["n"].as_f64(), d["group"].as_str()), (Some(6.0), Some("Petoskey · other")));
}

#[test]
fn sold_comps_get_their_detail_read_and_move_groups_without_alerting() {
    let db = Db::new();
    // Six inland actives (detail read) and one cheap young active, inland too.
    let inland: Vec<Value> = (0..6)
        .map(|i| {
            let mut h = house(&format!("zl:i{i}"), "petoskey", 800_000, 50);
            h["detailReadAt"] = json!(NOW);
            h["waterType"] = json!("inland");
            h
        })
        .collect();
    push(&db, inland, "mi", "full", NOW);
    // Unread active listing, and sold rows: two recent, one too old. All unread, so "other".
    push(&db, vec![house("zl:act", "petoskey", 700_000, 1)], "mi", "quick", NOW);
    let sold = |id: &str, price: i64, at: &str| {
        let mut h = house(id, "petoskey", price, 0);
        h["status"] = json!("sold");
        h["soldAt"] = json!(at);
        h
    };
    push(&db, vec![sold("zl:s1", 500_000, "2026-03-01"), sold("zl:s2", 500_000, "2026-08-01"),
                   sold("zl:s3", 500_000, "2025-06-01")], "mi", "sold", NOW);
    assert_eq!(job(&db, "mi", NOW).upserts, 0, "five other inland actives are too few");

    let nd = block_on(needs_detail(&db, &cfg(), &json!({"market": "mi"}), NOW)).unwrap();
    assert_eq!(nd["ids"], json!(["zl:act", "zl:s2", "zl:s1"]), "active first, then sold newest first, none older than a year");
    let nd = block_on(needs_detail(&db, &cfg(), &json!({"market": "mi", "limit": 2}), NOW)).unwrap();
    assert_eq!(nd["ids"], json!(["zl:act", "zl:s2"]));

    // A sold row gets its water type: stored; the job moves it into the inland
    // group, never alerts on it and never gives it a score row.
    let r = post_detail(&db, json!([{"id": "zl:s2", "detailReadAt": LATER, "waterType": "inland", "waterBody": "Walloon Lake"}]), LATER);
    assert_eq!(r.updated, 1);
    let j = job(&db, "mi", LATER);
    assert_eq!(j.alerts, 0);
    assert!(j.upserts >= 6, "the inland group moved: {j:?}");
    assert_eq!(db.count("SELECT COUNT(*) FROM listings WHERE id = 'zl:s2' AND water_type = 'inland' AND detail_read_at IS NOT NULL"), 1);
    assert_eq!(db.count("SELECT COUNT(*) FROM listing_scores WHERE listing_id LIKE 'zl:s%'"), 0);
    assert_eq!(db.count("SELECT COUNT(*) FROM deal_alerts"), 0);
    // The sale (cheap) now counts as an inland comp: n went from 5 to 6 for each active.
    let d = db.deal("zl:i0");
    assert_eq!((d["group"].as_str(), d["n"].as_f64()), (Some("Petoskey · inland"), Some(6.0)));
    let nd = block_on(needs_detail(&db, &cfg(), &json!({"market": "mi"}), NOW)).unwrap();
    assert_eq!(nd["ids"], json!(["zl:act", "zl:s1"]));
}

#[test]
fn score_input_pages_by_id_with_the_stored_score() {
    let db = Db::new();
    let mut ls: Vec<Value> = (0..5).map(|i| house(&format!("zl:{i}"), "traverse", 800_000, 50)).collect();
    ls[0]["unit"] = json!("#1");
    ls[0]["baths"] = json!(2.5);
    push(&db, ls, "mi", "full", NOW);
    push(&db, vec![apt("se:x", "Astoria", 1, 1)], "nyc", "full", NOW);
    db.conn
        .execute_batch(
            "INSERT INTO listing_scores VALUES ('zl:1', 'mi', 800000, 12.5, 1, '{}', 'T');
             UPDATE listings SET removed_at = 'R' WHERE id = 'zl:4';
             UPDATE listings SET comp_only = 1 WHERE id = 'zl:2';",
        )
        .unwrap();
    let page = |after: &str, limit: usize| -> Value {
        let q = ScoreInputQuery { market: "mi".into(), after: after.into(), limit };
        serde_json::from_str(&block_on(score_input(&db, &q, NOW)).unwrap()).unwrap()
    };
    let p = page("", 2);
    assert_eq!((p["market"].as_str(), p["n"].as_u64(), p["last"].as_str()), (Some("mi"), Some(2), Some("zl:1")));
    let cols: Vec<&str> = p["columns"].as_array().unwrap().iter().map(|c| c.as_str().unwrap()).collect();
    let at = |row: &Value, k: &str| row[cols.iter().position(|c| *c == k).unwrap()].clone();
    let (r0, r1) = (&p["rows"][0], &p["rows"][1]);
    assert_eq!(cols.len(), r0.as_array().unwrap().len());
    assert_eq!((at(r0, "id"), at(r0, "unit"), at(r0, "baths"), at(r0, "price")), (json!("zl:0"), json!("#1"), json!(2.5), json!(800000)));
    assert_eq!((at(r0, "compOnly"), at(r0, "storedAlert"), at(r0, "storedDiscount")), (json!(false), Value::Null, Value::Null));
    assert_eq!((at(r1, "storedDiscount"), at(r1, "storedAlert"), at(r1, "storedPrice"), at(r1, "scoredAt")), (json!(12.5), json!(true), json!(800000), json!("T")));
    assert_eq!((at(r0, "homeType"), at(r0, "area"), at(r0, "status")), (json!("single_family"), json!("traverse"), json!("active")));
    let p = page("zl:1", 2);
    assert_eq!(p["rows"].as_array().unwrap().len(), 2, "zl:2 and zl:3; zl:4 is removed");
    assert_eq!(at(&p["rows"][0], "compOnly"), json!(true));
    let p = page("zl:3", 2);
    assert_eq!((p["n"].as_u64(), p["last"].clone(), p["rows"].clone()), (Some(0), Value::Null, json!([])));
}

#[test]
fn posted_scores_skip_unknown_or_removed_listings() {
    let db = Db::new();
    push(&db, vec![house("zl:1", "traverse", 500_000, 1), house("zl:2", "traverse", 500_000, 1)], "mi", "full", NOW);
    db.conn.execute_batch("UPDATE listings SET removed_at = 'R' WHERE id = 'zl:2'").unwrap();
    let deal = r#"{"id":"zl:1","price":500000}"#;
    let up = |id: &str| format!(r#"{{"id":"{id}","price":500000,"discountPct":20,"alert":true,"deal":{deal}}}"#);
    let al = |id: &str| format!(r#"{{"id":"{id}","price":500000,"deal":{deal}}}"#);
    let body = format!(
        r#"{{"market":"mi","upserts":[{},{},{}],"alerts":[{},{}],"deletes":["zl:9"]}}"#,
        up("zl:1"), up("zl:2"), up("zl:nope"), al("zl:1"), al("zl:2")
    );
    let r = block_on(post_scores(&db, &NoNotifier, &body, NOW)).unwrap();
    assert_eq!((r.upserted, r.new_alerts, r.deleted, r.patched_alerts), (1, 1, 0, 0));
    assert_eq!(db.deal("zl:1"), json!({"id": "zl:1", "price": 500000}));
    // The same post again: the alert exists, nothing new.
    let r = block_on(post_scores(&db, &NoNotifier, &body, NOW)).unwrap();
    assert_eq!((r.upserted, r.new_alerts), (1, 0));
    // The wrong market changes nothing.
    let r = block_on(post_scores(&db, &NoNotifier, &body.replace("\"mi\"", "\"nyc\""), NOW)).unwrap();
    assert_eq!((r.upserted, r.new_alerts), (0, 0));
    assert_eq!(block_on(post_scores(&db, &NoNotifier, "{\"market\":\"mi\",\"upserts\":[1]}", NOW)).unwrap_err().status, 400);
}

#[test]
fn reads_keep_the_dashboard_shapes() {
    let db = Db::new();
    let d = deals_json(&db, &[]);
    assert_eq!(d, json!({"generatedAt": NOW, "deals": []}));
    assert_eq!(alerts_json(&db, &[("market", "mi")]), json!({"generatedAt": NOW, "alerts": []}));
    assert_eq!(stats_json(&db), json!({"generatedAt": NOW, "counts": [], "crawls": [], "alerts": [], "scored": []}));
    astoria(&db);
    push(&db, vec![apt("se:deal", "Astoria", 640_000, 1)], "nyc", "quick", LATER);
    job(&db, "nyc", LATER);
    let d = deals_json(&db, &[("market", "nyc"), ("minDiscount", "15"), ("maxPrice", "700000")]);
    assert_eq!(d["deals"].as_array().unwrap().len(), 1);
    assert_eq!(d["deals"][0]["id"], "se:deal");
    assert_eq!(deals_json(&db, &[("limit", "3")])["deals"].as_array().unwrap().len(), 3);
    let a = alerts_json(&db, &[]);
    assert_eq!((a["alerts"][0]["id"].as_str(), a["alerts"][0]["createdAt"].as_str()), (Some("se:deal"), Some(LATER)));
    let s = stats_json(&db);
    assert_eq!(s["counts"], json!([{"market": "nyc", "status": "active", "n": 11}]));
    assert_eq!(s["crawls"], json!([{"market": "nyc", "mode": "full", "at": NOW}, {"market": "nyc", "mode": "quick", "at": LATER}]));
}

#[test]
fn bad_requests_are_400() {
    let db = Db::new();
    let e = block_on(ingest(&db, &json!({"nope": 1}), NOW)).unwrap_err();
    assert_eq!(e.status, 400);
    let many: Vec<Value> = (0..201).map(|i| apt(&format!("se:{i}"), "A", 1, 1)).collect();
    assert_eq!(block_on(ingest(&db, &json!({ "listings": many }), NOW)).unwrap_err().status, 400);
    assert_eq!(block_on(needs_detail(&db, &cfg(), &json!({"market": "sf"}), NOW)).unwrap_err().status, 400);
    let r = block_on(ingest(&db, &json!({"listings": [{"id": "x", "price": 5}]}), NOW)).unwrap();
    assert_eq!((r.stats.skipped, r.rejected.len()), (1, 1), "no source or market");
    assert_eq!(block_on(post_scores(&db, &NoNotifier, "nope", NOW)).unwrap_err().status, 400);
}

/// The hot queries are index-driven. This runs EXPLAIN QUERY PLAN on the real
/// statements against the real schema (D1 is SQLite, so the plans carry over).
#[test]
fn hot_queries_use_indexes() {
    let db = Db::new();
    for market in ["nyc", "mi"] {
        let q = ScoreInputQuery { market: market.into(), after: "x".into(), limit: 1000 };
        let plan = db.plan(&scores::score_input_query(&q, "2025-10-05"));
        println!("score-input {market}:\n{plan}");
        assert!(plan.contains("idx_listings_market_id (market=? AND id>?)"), "{plan}");
        assert!(plan.contains("sqlite_autoindex_listing_scores_1 (listing_id=?)"), "{plan}");
        assert!(!plan.contains("TEMP B-TREE") && !plan.contains("SCAN l"), "{plan}");
    }

    for market in ["mi", "nyc"] {
        let plan = db.plan(&scores::needs_detail_query(market, 15, 900_000.0));
        println!("needs-detail {market}:\n{plan}");
        assert!(!plan.contains("SCAN"), "{plan}");
    }
    assert!(db.plan(&scores::needs_detail_query("mi", 15, 0.0)).contains("idx_listings_needs_detail"));
    let plan = db.plan(&scores::needs_detail_sold_query(15, "2025-10-05"));
    assert!(plan.contains("idx_listings_needs_detail_sold (market=? AND sold_at>?)") && !plan.contains("TEMP B-TREE"), "{plan}");

    let plan = db.plan(&scores::deals_query(&DealsQuery::from_pairs([("market", "nyc"), ("minDiscount", "15")]).unwrap(), NOW));
    println!("deals:\n{plan}");
    assert!(plan.contains("idx_scores_market_discount") && !plan.contains("TEMP B-TREE"), "{plan}");
    let plan = db.plan(&scores::deals_query(&DealsQuery::from_pairs([]).unwrap(), NOW));
    assert!(plan.contains("idx_scores_discount") && !plan.contains("TEMP B-TREE"), "{plan}");

    let plan = db.plan(&scores::alerts_query(&AlertsQuery::from_pairs([("market", "mi")]).unwrap(), NOW));
    assert!(plan.contains("idx_alerts_market_created") && !plan.contains("TEMP B-TREE"), "{plan}");
    let plan = db.plan(&scores::alerts_query(&AlertsQuery::from_pairs([]).unwrap(), NOW));
    assert!(plan.contains("idx_alerts_created") && !plan.contains("TEMP B-TREE"), "{plan}");

    let plan = db.plan(&scores::stats_query(NOW));
    println!("stats:\n{plan}");
    assert!(plan.contains("COVERING INDEX idx_listings_market_status"), "{plan}");

    let exp = scores::expire_statements(NOW, NOW, NOW, NOW);
    let plan = db.plan(&exp[0]);
    assert!(plan.contains("idx_listings_market_status (market=? AND status=? AND removed_at=?)"), "{plan}");
    let plan = db.plan(&exp[1]);
    println!("expire scores:\n{plan}");
    assert!(plan.contains("idx_listings_market_status (market=? AND status=? AND removed_at>?)"), "{plan}");

    let plan = db.plan(&parse_scores_stmt(0));
    assert!(plan.contains("sqlite_autoindex_listings_1 (id=?)"), "{plan}");
    let scope = Scope { market: Some("nyc".into()), mode: Some("quick".into()), seen_at: NOW.into(), ..Default::default() };
    let plan = db.plan(&prior_run_query(&scope).unwrap());
    assert!(plan.contains("sqlite_autoindex_crawls_1 (market=?)"), "{plan}");
    let plan = db.plan(&existing_queries(&["a".to_string(), "b".to_string()])[0]);
    assert!(plan.contains("sqlite_autoindex_listings_1 (id=?)"), "{plan}");
}

fn parse_scores_stmt(i: usize) -> Stmt {
    let body = r#"{"market":"mi","upserts":[{"id":"a","price":1,"discountPct":1,"alert":false,"deal":{}}]}"#;
    scores::parse_scores(body, NOW).unwrap().stmts.remove(i)
}

/// Diagnostic: times the crawl job's pass (score-input pages, the scorer,
/// POST /api/scores) and one 25-listing ingest against a dump of the live D1.
/// `HD_DUMP=/path/d1.sql cargo test --release perf_live_dump -- --ignored --nocapture`
#[test]
#[ignore]
fn perf_live_dump() {
    let Ok(path) = std::env::var("HD_DUMP") else { return };
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&std::fs::read_to_string(path).unwrap()).unwrap();
    conn.execute_batch(MIGRATIONS[2]).unwrap();
    let db = Db::with(conn);
    let now = "2026-10-05T18:00:00.000Z";
    for market in ["nyc", "mi"] {
        let t = std::time::Instant::now();
        let j = job_paged(&db, market, now, 1000);
        println!("job {market}: {:?} rows={} pages={} upserts={} deletes={} alerts={}", t.elapsed(), j.rows, j.pages, j.upserts, j.deletes, j.alerts);
        let t = std::time::Instant::now();
        let q = ScoreInputQuery { market: market.into(), after: String::new(), limit: 1000 };
        let body = block_on(score_input(&db, &q, now)).unwrap();
        println!("  one score-input page: {:?}, {} bytes", t.elapsed(), body.len());
    }
    let rows = block_on(db.query(&Stmt::new(
        "SELECT id, source, market, status, url, address, price, beds, sqft, home_type AS homeType, neighborhood, borough \
         FROM listings WHERE market = 'nyc' ORDER BY id LIMIT 25",
        vec![],
    )))
    .unwrap();
    let batch: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut l = r.clone();
            l["price"] = json!((r["price"].as_f64().unwrap() * 0.99).round() as i64);
            l
        })
        .collect();
    db.queries.set(0);
    db.rows_read.set(0);
    let t = std::time::Instant::now();
    let out = push(&db, batch, "nyc", "full", now);
    println!("ingest 25: {:?} queries={} rows_read={} rows_written={}", t.elapsed(), db.queries.get(), db.rows_read.get(), out.rows_written);
    let t = std::time::Instant::now();
    let d = block_on(deals(&db, &DealsQuery::from_pairs([("market", "nyc"), ("limit", "500")]).unwrap(), now)).unwrap();
    println!("deals 500: {:?}, {} bytes", t.elapsed(), d.len());
}
