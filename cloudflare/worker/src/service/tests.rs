//! Use-case tests on an in-memory SQLite loaded with the real D1 migrations.
//! Rows come back the way D1 hands them to the Worker (numbers as doubles).

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
}

impl Db {
    fn new() -> Db {
        let conn = Connection::open_in_memory().unwrap();
        for m in MIGRATIONS {
            conn.execute_batch(m).unwrap();
        }
        Db { conn, log: RefCell::new(Vec::new()), queries: Cell::new(0) }
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
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
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
    Config { rule: AlertRule::default() }
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
    block_on(ingest(db, &NoNotifier, &cfg(), &body, at)).unwrap()
}

/// Ten Astoria 2bd condos around $800/sq ft, an old full-sweep load.
fn astoria(db: &Db) {
    let comps: Vec<Value> = (0..10).map(|i| apt(&format!("se:c{i}"), "Astoria", 790_000 + (i % 3) * 10_000, 40)).collect();
    let r = push(db, comps, "nyc", "full", NOW);
    assert_eq!((r.stats.added, r.new_alerts), (10, 0));
}

#[test]
fn a_first_full_sweep_scores_but_never_alerts() {
    let db = Db::new();
    astoria(&db);
    // An old cheap listing in the same sweep: stored, scored, but not fresh.
    let r = push(&db, vec![apt("se:old", "Astoria", 600_000, 60)], "nyc", "full", NOW);
    assert_eq!((r.stats.added, r.new_alerts), (1, 0));
    assert!(r.scored >= 11);
    assert_eq!(db.count("SELECT COUNT(*) FROM listing_scores"), 11);
    assert_eq!(db.count("SELECT COUNT(*) FROM deal_alerts"), 0);
    let d = block_on(deals(&db, &DealsQuery::from_pairs([("market", "nyc")]).unwrap(), NOW)).unwrap();
    assert_eq!(d["deals"][0]["id"], "se:old", "best discount first");
    assert_eq!(d["deals"][0]["alert"], true, "the rule accepts it; it is just not fresh");
    assert_eq!(d["deals"][0]["group"], "Astoria · condo · 2bd");
}

#[test]
fn a_fresh_deal_alerts_once_and_an_unchanged_repush_writes_nothing() {
    let db = Db::new();
    astoria(&db);
    let deal = apt("se:deal", "Astoria", 600_000, 1);
    let r = push(&db, vec![deal.clone()], "nyc", "quick", NOW);
    assert_eq!((r.stats.added, r.new_alerts), (1, 1));
    let a = block_on(recent_alerts(&db, &AlertsQuery::from_pairs([]).unwrap(), NOW)).unwrap();
    assert_eq!(a["alerts"][0]["id"], "se:deal");
    assert_eq!(a["alerts"][0]["createdAt"], NOW);
    assert!(a["alerts"][0]["discountPct"].as_f64().unwrap() >= 24.0);

    // The same listing half an hour later: no listing write, no scoring.
    let before = db.writes().len();
    let q = db.queries.get();
    let r = push(&db, vec![deal.clone()], "nyc", "quick", LATER);
    assert_eq!((r.stats.unchanged, r.stats.updated, r.scored, r.new_alerts), (1, 0, 0, 0));
    let new_writes: Vec<String> = db.writes()[before..].to_vec();
    assert_eq!(new_writes, vec!["INSERT INTO crawls"], "only the crawl-run row");
    assert_eq!(db.queries.get() - q, 2, "prior-run check + id lookup, no group load");

    // A price drop: history row, rescored, a new alert key.
    let mut cheaper = deal.clone();
    cheaper["price"] = json!(580_000);
    let r = push(&db, vec![cheaper], "nyc", "quick", LATER);
    assert_eq!((r.stats.price_drops, r.new_alerts), (1, 1));
    assert_eq!(db.count("SELECT COUNT(*) FROM price_history WHERE listing_id = 'se:deal'"), 2);
    assert_eq!(db.count("SELECT price_changes FROM listings WHERE id = 'se:deal'"), 1);
    assert_eq!(db.count("SELECT COUNT(*) FROM deal_alerts"), 2);

    // Crawl runs are summed per (market, mode, seenAt).
    let s = block_on(stats(&db, NOW)).unwrap();
    assert!(s["crawls"].as_array().unwrap().iter().any(|c| c["mode"] == "quick" && c["at"] == LATER));
    assert!(s["counts"].as_array().unwrap().iter().any(|c| c["market"] == "nyc" && c["status"] == "active" && c["n"] == 11));
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
    let r = push(&db, batch, "nyc", "quick", NOW);
    assert_eq!((r.stats.added, r.new_alerts), (11, 0));
    // A second batch of the same run is still the first run.
    let r = push(&db, vec![apt("se:deal2", "Astoria", 600_000, 1)], "nyc", "quick", NOW);
    assert_eq!(r.new_alerts, 0);
    // The next run alerts on a new young deal (StreetEasy sends no daysOnMarket).
    let mut d = apt("se:deal3", "Astoria", 600_000, 0);
    d["daysOnMarket"] = Value::Null;
    let r = push(&db, vec![d], "nyc", "quick", LATER);
    assert_eq!(r.new_alerts, 1);
    // Full sweeps never make a new listing fresh, whatever its age.
    let r = push(&db, vec![apt("se:deal4", "Astoria", 600_000, 0)], "nyc", "full", LATER);
    assert_eq!((r.stats.added, r.new_alerts), (1, 0));
}

#[test]
fn a_thin_neighbourhood_falls_back_to_the_borough() {
    let db = Db::new();
    let comps: Vec<Value> = (0..9).map(|i| apt(&format!("se:s{i}"), "Sunnyside", 800_000, 40)).collect();
    push(&db, comps, "nyc", "full", NOW);
    let r = push(&db, vec![apt("se:w", "Woodside", 640_000, 1)], "nyc", "quick", NOW);
    assert_eq!(r.new_alerts, 0, "thin groups never alert");
    let d = db.one("SELECT deal FROM listing_scores WHERE listing_id = 'se:w'");
    let d: Value = serde_json::from_str(d["deal"].as_str().unwrap()).unwrap();
    assert_eq!((d["group"].as_str(), d["thin"].as_bool(), d["n"].as_f64()), (Some("Queens · condo · 2bd"), Some(true), Some(9.0)));
    assert_eq!(d["alert"], false);
}

#[test]
fn michigan_detail_reads_rescore_keep_sticky_fields_and_alert_fresh_listings() {
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
    let r = push(&db, vec![house("zl:new", "traverse", 600_000, 1)], "mi", "quick", NOW);
    assert_eq!((r.stats.added, r.new_alerts), (1, 0));
    assert_eq!(db.count("SELECT COUNT(*) FROM listing_scores WHERE listing_id = 'zl:new'"), 0);

    let nd = block_on(needs_detail(&db, &cfg(), &json!({"market": "mi", "limit": 5}), NOW)).unwrap();
    assert_eq!(nd["ids"], json!(["zl:new"]));
    assert_eq!(nd["urls"], json!(["https://www.zillow.com/homedetails/zl:new"]));

    let body = json!({"listings": [{"id": "zl:new", "detailReadAt": LATER, "waterType": "inland",
                                     "waterBody": "Elk Lake", "frontageFt": 90, "description": "On Elk Lake"},
                                    {"id": "zl:missing", "detailReadAt": LATER}]});
    let r = block_on(detail(&db, &NoNotifier, &cfg(), &body, LATER)).unwrap();
    assert_eq!((r.updated, r.unknown, r.new_alerts), (1, 1, 1));
    assert!(r.scored >= 7);
    let a = block_on(recent_alerts(&db, &AlertsQuery::from_pairs([("market", "mi")]).unwrap(), LATER)).unwrap();
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
    let body = json!({"listings": [{"id": "zl:old", "detailReadAt": LATER, "waterType": "inland"}]});
    let r = block_on(detail(&db, &NoNotifier, &cfg(), &body, LATER)).unwrap();
    assert_eq!((r.updated, r.new_alerts), (1, 0));
    assert_eq!(db.count("SELECT alert FROM listing_scores WHERE listing_id = 'zl:old'"), 1, "a deal, just not a fresh one");
}

#[test]
fn nyc_needs_detail_lists_cheap_scored_listings() {
    let db = Db::new();
    astoria(&db);
    push(&db, vec![apt("se:deal", "Astoria", 700_000, 1)], "nyc", "quick", NOW); // ~12% under
    push(&db, vec![apt("se:fair", "Astoria", 790_000, 1)], "nyc", "quick", NOW);
    let nd = block_on(needs_detail(&db, &cfg(), &json!({"market": "nyc"}), NOW)).unwrap();
    assert_eq!(nd["ids"], json!(["se:deal"]));
    let body = json!({"listings": [{"id": "se:deal", "detailReadAt": LATER, "maintenance": 950, "taxes": 400}]});
    let r = block_on(detail(&db, &NoNotifier, &cfg(), &body, LATER)).unwrap();
    assert_eq!(r.updated, 1);
    let d = db.one("SELECT deal FROM listing_scores WHERE listing_id = 'se:deal'");
    let d: Value = serde_json::from_str(d["deal"].as_str().unwrap()).unwrap();
    assert_eq!(d["maintenance"], 950, "the stored deal shows the detail fields");
    assert_eq!(block_on(needs_detail(&db, &cfg(), &json!({"market": "nyc"}), NOW)).unwrap()["ids"], json!([]));
}

#[test]
fn expiry_needs_a_recent_full_sweep_and_drops_scores() {
    let db = Db::new();
    // Seen 5 days ago in a quick crawl only.
    let comps: Vec<Value> = (0..10).map(|i| apt(&format!("se:c{i}"), "Astoria", 800_000, 40)).collect();
    push(&db, comps, "nyc", "quick", "2026-09-30T12:00:00.000Z");
    let cutoff = "2026-10-02T12:00:00.000Z";
    assert_eq!(block_on(expire(&db, NOW, cutoff)).unwrap(), 0, "no full sweep ran: nothing expires");
    // A full sweep today saw only one of them.
    push(&db, vec![apt("se:c0", "Astoria", 800_000, 45)], "nyc", "full", "2026-10-05T11:30:00.000Z");
    assert_eq!(block_on(expire(&db, NOW, cutoff)).unwrap(), 9);
    assert_eq!(db.count("SELECT COUNT(*) FROM listings WHERE removed_at IS NULL"), 1);
    assert_eq!(db.count("SELECT COUNT(*) FROM listing_scores WHERE listing_id != 'se:c0'"), 0);
    let s = block_on(stats(&db, NOW)).unwrap();
    assert!(s["counts"].as_array().unwrap().iter().any(|c| c["status"] == "expired" && c["n"] == 9));
    // One comes back: relisted, fresh.
    let r = push(&db, vec![apt("se:c5", "Astoria", 800_000, 50)], "nyc", "quick", LATER);
    assert_eq!(r.stats.relisted, 1);
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
    let r = push(&db, sold, "mi", "sold", NOW);
    assert_eq!(r.stats.added, 6);
    assert_eq!(r.scored, 0, "sold rows are comps, never subjects");
    let r = push(&db, vec![house("zl:a", "petoskey", 500_000, 1)], "mi", "quick", NOW);
    assert_eq!(r.new_alerts, 0, "only 5 recent sold comps (+ 1 too old) < 6");
    push(&db, vec![house("zl:b", "petoskey", 700_000, 30)], "mi", "full", NOW);
    let d = db.one("SELECT deal FROM listing_scores WHERE listing_id = 'zl:a'");
    let d: Value = serde_json::from_str(d["deal"].as_str().unwrap()).unwrap();
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

    let nd = block_on(needs_detail(&db, &cfg(), &json!({"market": "mi"}), NOW)).unwrap();
    assert_eq!(nd["ids"], json!(["zl:act", "zl:s2", "zl:s1"]), "active first, then sold newest first, none older than a year");
    let nd = block_on(needs_detail(&db, &cfg(), &json!({"market": "mi", "limit": 2}), NOW)).unwrap();
    assert_eq!(nd["ids"], json!(["zl:act", "zl:s2"]));

    // A sold row gets its water type: stored, its new group rescored, no alert,
    // never a score row of its own.
    let body = json!({"listings": [{"id": "zl:s2", "detailReadAt": LATER, "waterType": "inland", "waterBody": "Walloon Lake"}]});
    let r = block_on(detail(&db, &NoNotifier, &cfg(), &body, LATER)).unwrap();
    assert_eq!((r.updated, r.new_alerts), (1, 0));
    assert!(r.scored >= 6, "the inland group was rescored: {r:?}");
    assert_eq!(db.count("SELECT COUNT(*) FROM listings WHERE id = 'zl:s2' AND water_type = 'inland' AND detail_read_at IS NOT NULL"), 1);
    assert_eq!(db.count("SELECT COUNT(*) FROM listing_scores WHERE listing_id LIKE 'zl:s%'"), 0);
    assert_eq!(db.count("SELECT COUNT(*) FROM deal_alerts"), 0);
    // The sale (cheap) now counts as an inland comp: n went from 5 to 6 for each active.
    let d = db.one("SELECT deal FROM listing_scores WHERE listing_id = 'zl:i0'");
    let d: Value = serde_json::from_str(d["deal"].as_str().unwrap()).unwrap();
    assert_eq!((d["group"].as_str(), d["n"].as_f64()), (Some("Petoskey · inland"), Some(6.0)));
    let nd = block_on(needs_detail(&db, &cfg(), &json!({"market": "mi"}), NOW)).unwrap();
    assert_eq!(nd["ids"], json!(["zl:act", "zl:s1"]));
}

#[test]
fn bad_requests_are_400() {
    let db = Db::new();
    let e = block_on(ingest(&db, &NoNotifier, &cfg(), &json!({"nope": 1}), NOW)).unwrap_err();
    assert_eq!(e.status, 400);
    let many: Vec<Value> = (0..201).map(|i| apt(&format!("se:{i}"), "A", 1, 1)).collect();
    assert_eq!(block_on(ingest(&db, &NoNotifier, &cfg(), &json!({ "listings": many }), NOW)).unwrap_err().status, 400);
    assert_eq!(block_on(needs_detail(&db, &cfg(), &json!({"market": "sf"}), NOW)).unwrap_err().status, 400);
    let r = block_on(ingest(&db, &NoNotifier, &cfg(), &json!({"listings": [{"id": "x", "price": 5}]}), NOW)).unwrap();
    assert_eq!((r.stats.skipped, r.rejected.len()), (1, 1), "no source or market");
}

/// The hot queries are index-driven. This runs EXPLAIN QUERY PLAN on the real
/// statements against the real schema (D1 is SQLite, so the plans carry over).
#[test]
fn hot_queries_use_indexes() {
    let db = Db::new();
    let groups = scores::group_queries(
        &[
            Unit::NycNbhd("Astoria".into(), "condo".into()),
            Unit::NycBorough("queens".into(), "condo".into(), "4+".into()),
            Unit::NycBorough("queens".into(), "coop".into(), "2".into()),
            Unit::MiWater("inland".into()),
            Unit::MiWater("other".into()),
        ],
        "2025-10-05",
    );
    let plan = db.plan(&groups[0]);
    println!("group load:\n{plan}");
    assert!(plan.contains("MULTI-INDEX OR"), "{plan}");
    assert!(plan.contains("idx_listings_nyc_group (market=? AND neighborhood=? AND home_type=?)"), "{plan}");
    assert!(plan.contains("idx_listings_borough (market=? AND borough=? AND home_type=? AND beds>?)"), "{plan}");
    assert!(plan.contains("idx_listings_borough (market=? AND borough=? AND home_type=? AND beds=?)"), "{plan}");
    assert!(plan.contains("idx_listings_mi_group (market=? AND area=? AND water_type=?)"), "{plan}");
    assert!(!plan.contains("SCAN l\n") && !plan.ends_with("SCAN l"), "{plan}");

    for market in ["mi", "nyc"] {
        let plan = db.plan(&scores::needs_detail_query(market, 15, 900_000.0));
        println!("needs-detail {market}:\n{plan}");
        assert!(!plan.contains("SCAN"), "{plan}");
    }
    assert!(db.plan(&scores::needs_detail_query("mi", 15, 0.0)).contains("idx_listings_needs_detail"));
    let plan = db.plan(&scores::needs_detail_sold_query(15, "2025-10-05"));
    println!("needs-detail mi sold:\n{plan}");
    assert!(plan.contains("idx_listings_needs_detail_sold (market=? AND sold_at>?)") && !plan.contains("TEMP B-TREE"), "{plan}");

    let plan = db.plan(&scores::deals_query(&DealsQuery::from_pairs([("market", "nyc"), ("minDiscount", "15")]).unwrap()));
    assert!(plan.contains("idx_scores_market_discount"), "{plan}");
    let plan = db.plan(&scores::deals_query(&DealsQuery::from_pairs([]).unwrap()));
    assert!(plan.contains("idx_scores_discount") && !plan.contains("TEMP B-TREE"), "{plan}");

    let plan = db.plan(&crate::alerts::list_query(&AlertsQuery::from_pairs([("market", "mi")]).unwrap()));
    assert!(plan.contains("idx_alerts_market_created") && !plan.contains("TEMP B-TREE"), "{plan}");

    let exp = scores::expire_statements(NOW, NOW, NOW, NOW);
    let plan = db.plan(&exp[0]);
    println!("expire:\n{plan}");
    assert!(plan.contains("idx_listings_market_status (market=? AND status=? AND removed_at=?)"), "{plan}");
    let plan = db.plan(&exp[1]);
    assert!(plan.contains("idx_listings_market_status (market=? AND status=? AND removed_at=?)"), "{plan}");

    let plan = db.plan(&Stmt::new(scores::COUNTS_SQL, vec![]));
    assert!(plan.contains("COVERING INDEX idx_listings_market_status"), "{plan}");
    let scope = Scope { market: Some("nyc".into()), mode: Some("quick".into()), seen_at: NOW.into(), ..Default::default() };
    let plan = db.plan(&prior_run_query(&scope).unwrap());
    assert!(plan.contains("sqlite_autoindex_crawls_1 (market=?)"), "{plan}");
    let plan = db.plan(&existing_queries(&["a".to_string(), "b".to_string()])[0]);
    assert!(plan.contains("sqlite_autoindex_listings_1 (id=?)"), "{plan}");
}
