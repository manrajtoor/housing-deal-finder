//! Baselines from comps, and the deal score.
//!
//! For a subject listing, the groups of [`crate::group`] are tried in order.
//! A group answers when, without the subject itself:
//! - it has at least `min_comps` comps on the chosen basis, and
//! - its spread is sane: (p75 − p25) / median ≤ `max_spread` (0.75).
//!
//! The basis is price per sq ft (`ppsf`) when the subject has a usable sq ft
//! and the group has at least `min_comps` comps with one; the baseline is
//! then median ppsf × the subject's sq ft. Otherwise it is the group's median
//! price (`price`). A discount over `max_discount_pct` (50%) is refused as
//! bad data rather than reported.
//!
//! Comps are active listings (not removed) and sold listings whose `soldAt`
//! is within `sold_comp_days` (365) of `now`; sold ones use their sale price.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use crate::alert::AlertRule;
use crate::dates::date_minus_days;
use crate::group::{groups_of, label, GroupKey, Level};
use crate::listing::{deal_base, Facts, Market};
use crate::stats::{sorted, Sorted};

#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    /// Comp counts below this refuse a group (and the alert rule uses the same numbers).
    pub min_comps_nyc: usize,
    pub min_comps_mi: usize,
    pub max_spread: f64,
    pub max_discount_pct: f64,
    /// Sold comps older than this many days (by `soldAt`) are ignored.
    pub sold_comp_days: i64,
    pub alert: AlertRule,
}

impl Default for Options {
    fn default() -> Self {
        let alert = AlertRule::default();
        Options {
            min_comps_nyc: alert.min_comps_nyc,
            min_comps_mi: alert.min_comps_mi,
            max_spread: 0.75,
            max_discount_pct: 50.0,
            sold_comp_days: 365,
            alert,
        }
    }
}

impl Options {
    /// Options whose group minimums follow the alert rule's per-market minimums.
    pub fn with_rule(alert: AlertRule) -> Options {
        Options { min_comps_nyc: alert.min_comps_nyc, min_comps_mi: alert.min_comps_mi, alert, ..Options::default() }
    }

    pub fn min_comps(&self, m: Market) -> usize {
        match m {
            Market::Nyc => self.min_comps_nyc,
            Market::Mi => self.min_comps_mi,
        }
        .max(1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    Ppsf,
    Price,
}

impl Basis {
    pub fn as_str(self) -> &'static str {
        match self {
            Basis::Ppsf => "ppsf",
            Basis::Price => "price",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Score {
    pub baseline: f64,
    pub discount_pct: f64,
    pub basis: Basis,
    pub group: String,
    pub level: Level,
    pub n: usize,
    pub thin: bool,
    pub median_ppsf: Option<f64>,
    pub p25: f64,
    pub p75: f64,
    pub alert: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    /// Not a listing the scorer can read (no id, market or price).
    Invalid,
    /// Sold or removed: a comp at most, never a subject.
    NotActive,
    /// No group had enough comps with a sane spread; one reason per level tried.
    NoGroup(Vec<String>),
    /// Over `max_discount_pct` under the baseline: almost surely bad data.
    Implausible { discount_pct: f64, group: String, level: Level },
}

#[derive(Debug, Default)]
struct Group {
    /// Sorted.
    ppsf: Vec<f64>,
    /// Sorted.
    price: Vec<f64>,
}

pub struct Scorer {
    groups: HashMap<GroupKey, Group>,
    opts: Options,
    sold_cutoff: Option<String>,
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

impl Scorer {
    /// Builds the comp groups from `listings` (contract JSON; a repeated id
    /// counts once, the last copy wins). `now` (ISO) dates the sold-comp cutoff.
    pub fn new(listings: &[Value], opts: Options, now: &str) -> Scorer {
        let sold_cutoff = date_minus_days(now, opts.sold_comp_days);
        let mut seen = HashSet::new();
        let mut raw: HashMap<GroupKey, (Vec<f64>, Vec<f64>)> = HashMap::new();
        let mut s = Scorer { groups: HashMap::new(), opts, sold_cutoff };
        for f in listings.iter().rev().filter_map(Facts::from_json) {
            if !seen.insert(f.id.clone()) || !s.is_comp(&f) {
                continue;
            }
            let ppsf = f.ppsf();
            for g in groups_of(&f) {
                let e = raw.entry(g).or_default();
                if let Some(p) = ppsf {
                    e.0.push(p);
                }
                e.1.push(f.price);
            }
        }
        s.groups = raw.into_iter().map(|(k, (a, b))| (k, Group { ppsf: sorted(&a), price: sorted(&b) })).collect();
        s
    }

    pub fn options(&self) -> &Options {
        &self.opts
    }

    /// Active and not removed, or sold within the last `sold_comp_days`
    /// (a sold row without a usable `soldAt` is not trusted as a comp).
    pub fn is_comp(&self, f: &Facts) -> bool {
        if f.removed {
            return false;
        }
        if f.active {
            return true;
        }
        match (&f.sold_at, &self.sold_cutoff) {
            (Some(at), Some(cut)) if f.sold => at.len() >= 10 && at[..10] >= *cut.as_str(),
            _ => false,
        }
    }

    pub fn score(&self, l: &Value) -> Result<Score, Refusal> {
        let f = Facts::from_json(l).ok_or(Refusal::Invalid)?;
        self.score_facts(&f)
    }

    pub fn score_facts(&self, f: &Facts) -> Result<Score, Refusal> {
        if !f.active || f.removed {
            return Err(Refusal::NotActive);
        }
        let min = self.opts.min_comps(f.market);
        let in_own_groups = self.is_comp(f);
        let subject_ppsf = f.ppsf();
        let mut why = Vec::new();
        for g in groups_of(f) {
            let name = label(&g);
            let Some(group) = self.groups.get(&g) else {
                why.push(format!("{name}: no comps"));
                continue;
            };
            // The subject is never its own comp.
            let ppsf = match (in_own_groups, subject_ppsf) {
                (true, Some(p)) => Sorted::without(&group.ppsf, p),
                _ => Sorted::new(&group.ppsf),
            };
            let price = if in_own_groups { Sorted::without(&group.price, f.price) } else { Sorted::new(&group.price) };
            let (basis, values) = match subject_ppsf {
                Some(_) if ppsf.len() >= min => (Basis::Ppsf, ppsf),
                _ => (Basis::Price, price),
            };
            if values.len() < min {
                why.push(format!("{name}: {} comps < {min}", values.len()));
                continue;
            }
            let (Some(med), Some(p25), Some(p75)) = (values.median(), values.percentile(25.0), values.percentile(75.0)) else {
                continue;
            };
            let spread = (p75 - p25) / med;
            if !(med > 0.0) || spread > self.opts.max_spread {
                why.push(format!("{name}: spread {spread:.2} > {}", self.opts.max_spread));
                continue;
            }
            let baseline = match basis {
                Basis::Ppsf => med * f.sqft.unwrap_or(0.0),
                Basis::Price => med,
            };
            let discount_pct = round1((baseline - f.price) / baseline * 100.0);
            if discount_pct > self.opts.max_discount_pct {
                return Err(Refusal::Implausible { discount_pct, group: name, level: g.level });
            }
            let mut s = Score {
                baseline: baseline.round(),
                discount_pct,
                basis,
                group: name,
                level: g.level,
                n: values.len(),
                thin: g.level.thin(),
                median_ppsf: (basis == Basis::Ppsf).then_some(med.round()),
                p25: p25.round(),
                p75: p75.round(),
                alert: false,
            };
            s.alert = self.opts.alert.accepts(f, &s);
            return Ok(s);
        }
        Err(Refusal::NoGroup(why))
    }
}

/// The Deal (CONTRACT.md): the listing's dashboard fields plus the score.
pub fn deal_json(l: &Value, s: &Score) -> Value {
    let mut m: Map<String, Value> = deal_base(l);
    m.insert("baseline".into(), Value::from(s.baseline as i64));
    m.insert("discountPct".into(), serde_json::Number::from_f64(s.discount_pct).map_or(Value::Null, Value::Number));
    m.insert("basis".into(), Value::from(s.basis.as_str()));
    m.insert("group".into(), Value::from(s.group.clone()));
    m.insert("n".into(), Value::from(s.n));
    m.insert("thin".into(), Value::Bool(s.thin));
    m.insert("medianPpsf".into(), s.median_ppsf.map_or(Value::Null, |v| Value::from(v as i64)));
    m.insert("p25".into(), Value::from(s.p25 as i64));
    m.insert("p75".into(), Value::from(s.p75 as i64));
    m.insert("alert".into(), Value::Bool(s.alert));
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: &str = "2026-10-05T12:00:00.000Z";

    /// A NYC apartment at `ppsf` $/sq ft × 1 000 sq ft.
    fn apt(id: &str, nbhd: &str, beds: i64, ppsf: i64) -> Value {
        json!({"id": id, "market": "nyc", "status": "active", "price": ppsf * 1000, "sqft": 1000, "beds": beds,
               "homeType": "condo", "neighborhood": nbhd, "borough": "queens"})
    }

    fn lake(id: &str, area: &str, water: Option<&str>, price: i64) -> Value {
        json!({"id": id, "market": "mi", "status": "active", "price": price, "sqft": 2000,
               "homeType": "single_family", "area": area, "waterType": water})
    }

    /// `n` comps around `base` $/sq ft (±2%), so the spread is tight.
    fn comps(prefix: &str, nbhd: &str, beds: i64, n: usize, base: i64) -> Vec<Value> {
        (0..n).map(|i| apt(&format!("{prefix}{i}"), nbhd, beds, base + (i as i64 % 5 - 2) * base / 100)).collect()
    }

    fn score_of(all: &[Value], id: &str) -> Result<Score, Refusal> {
        let s = Scorer::new(all, Options::default(), NOW);
        s.score(all.iter().find(|l| l["id"] == id).unwrap())
    }

    #[test]
    fn neighbourhood_beds_group_first() {
        let mut all = comps("c", "Astoria", 2, 10, 800);
        all.push(apt("deal", "Astoria", 2, 600));
        let s = score_of(&all, "deal").unwrap();
        assert_eq!(s.group, "Astoria · condo · 2bd");
        assert_eq!((s.level, s.n, s.thin, s.basis), (Level::NycBeds, 10, false, Basis::Ppsf));
        assert_eq!(s.median_ppsf, Some(800.0));
        assert_eq!(s.baseline, 800_000.0);
        assert_eq!(s.discount_pct, 25.0);
        assert!(s.alert, "25% under, 10 comps, $600k");
        let d = deal_json(&all[10], &s);
        assert_eq!(d["group"], "Astoria · condo · 2bd");
        assert_eq!(d["discountPct"], 25.0);
        assert_eq!(d["basis"], "ppsf");
        assert_eq!(d["alert"], true);
        assert!(d.get("photoUrl").is_some_and(Value::is_null), "absent dashboard fields are null");
    }

    #[test]
    fn falls_back_to_neighbourhood_then_borough() {
        // 5 two-beds + 5 three-beds in Astoria: no beds group has 8, the
        // neighbourhood × type group has 10 (excluding the subject).
        let mut all = comps("two", "Astoria", 2, 5, 800);
        all.extend(comps("three", "Astoria", 3, 5, 800));
        all.push(apt("s", "Astoria", 2, 640));
        let s = score_of(&all, "s").unwrap();
        assert_eq!((s.group.as_str(), s.n, s.thin), ("Astoria · condo", 10, false));
        assert!(s.alert);

        // A thin neighbourhood: only the borough × type × beds group has enough.
        let mut all = comps("q", "Sunnyside", 2, 9, 800);
        all.push(apt("s", "Woodside", 2, 640));
        let s = score_of(&all, "s").unwrap();
        assert_eq!((s.group.as_str(), s.n, s.thin, s.level), ("Queens · condo · 2bd", 9, true, Level::NycBorough));
        assert_eq!(s.discount_pct, 20.0);
        assert!(!s.alert, "a thin group never alerts");

        // Nothing anywhere: refused with a reason per level.
        let all = vec![apt("s", "Woodside", 2, 640)];
        match score_of(&all, "s") {
            Err(Refusal::NoGroup(why)) => assert_eq!(why.len(), 3, "{why:?}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_subject_is_not_its_own_comp() {
        // 7 comps + the subject = 8 rows, but only 7 comps: refused at NYC's 8.
        let mut all = comps("c", "Astoria", 2, 7, 800);
        all.push(apt("s", "Astoria", 2, 600));
        assert!(matches!(score_of(&all, "s"), Err(Refusal::NoGroup(_))));
        // 8 comps: priced, n = 8, and the cheap subject does not drag the median.
        let mut all = comps("c", "Astoria", 2, 8, 800);
        all.push(apt("s", "Astoria", 2, 450));
        let s = score_of(&all, "s").unwrap();
        assert_eq!(s.n, 8);
        // 784 784 792 792 800 800 808 816: the median of the comps alone (796);
        // with the $450/sq ft subject counted it would be 792.
        assert_eq!(s.median_ppsf, Some(796.0));
        // Each comp is scored against the other 7 comps + the subject.
        let c = score_of(&all, "c0").unwrap();
        assert_eq!(c.n, 8);
    }

    #[test]
    fn a_wide_group_is_refused_and_the_next_one_used() {
        // Astoria 2bd: prices all over the place (spread > 0.75).
        let mut all: Vec<Value> = [300, 400, 500, 900, 1500, 2000, 2500, 3000, 3500, 4000]
            .iter()
            .enumerate()
            .map(|(i, p)| apt(&format!("w{i}"), "Astoria", 2, *p))
            .collect();
        all.push(apt("s", "Astoria", 2, 500));
        // Plus tight Queens comps elsewhere.
        all.extend(comps("q", "Sunnyside", 2, 12, 800));
        let s = score_of(&all, "s").unwrap();
        assert_eq!(s.level, Level::NycBorough, "Astoria groups are too dispersed: {}", s.group);
        // Without the borough comps: refused outright.
        let only: Vec<Value> = all.iter().filter(|l| !l["id"].as_str().unwrap().starts_with('q')).cloned().collect();
        match score_of(&only, "s") {
            Err(Refusal::NoGroup(why)) => assert!(why[0].contains("spread"), "{why:?}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn price_basis_without_usable_sqft() {
        let mut all = comps("c", "Astoria", 2, 10, 800);
        let mut s = apt("s", "Astoria", 2, 600);
        s["sqft"] = Value::Null;
        all.push(s);
        let sc = score_of(&all, "s").unwrap();
        assert_eq!((sc.basis, sc.median_ppsf), (Basis::Price, None));
        assert_eq!(sc.baseline, 800_000.0);
        assert_eq!(sc.p25, 792_000.0);

        // Comps mostly without sq ft (co-ops): too few ppsf comps, so price.
        let mut all: Vec<Value> = comps("c", "Astoria", 2, 10, 800)
            .into_iter()
            .enumerate()
            .map(|(i, mut l)| {
                if i < 4 {
                    l["sqft"] = Value::Null;
                } else if i < 6 {
                    l["sqft"] = json!(200); // bad data: ignored
                }
                l
            })
            .collect();
        all.push(apt("s", "Astoria", 2, 600));
        let sc = score_of(&all, "s").unwrap();
        assert_eq!((sc.basis, sc.n), (Basis::Price, 10));
    }

    #[test]
    fn over_fifty_percent_is_refused_as_bad_data() {
        let mut all = comps("c", "Astoria", 2, 10, 800);
        all.push(apt("s", "Astoria", 2, 300));
        assert!(matches!(score_of(&all, "s"), Err(Refusal::Implausible { .. })));
    }

    #[test]
    fn sold_comps_count_for_a_year_only() {
        let sold = |id: &str, at: Option<&str>| {
            let mut l = lake(id, "traverse", Some("inland"), 800_000);
            l["status"] = json!("sold");
            l["soldAt"] = at.map_or(Value::Null, |a| json!(a));
            l
        };
        let mut all: Vec<Value> = (0..5).map(|i| lake(&format!("a{i}"), "traverse", Some("inland"), 800_000)).collect();
        all.push(lake("s", "traverse", Some("inland"), 600_000));
        all.push(sold("old", Some("2025-09-01")));
        all.push(sold("undated", None));
        assert!(matches!(score_of(&all, "s"), Err(Refusal::NoGroup(_))), "5 active + stale or undated sold < 6");
        all.push(sold("recent", Some("2026-04-01T00:00:00Z")));
        let s = score_of(&all, "s").unwrap();
        assert_eq!((s.n, s.group.as_str()), (6, "Traverse · inland"));
        assert!(s.alert, "25% under with 6 comps: Michigan needs 6");
        assert!(matches!(score_of(&all, "recent"), Err(Refusal::NotActive)), "sold rows are comps only");
    }

    #[test]
    fn removed_listings_are_not_comps() {
        let mut all: Vec<Value> = (0..6).map(|i| lake(&format!("a{i}"), "traverse", Some("inland"), 800_000)).collect();
        all[0]["removedAt"] = json!("2026-10-01T00:00:00Z");
        all.push(lake("s", "traverse", Some("inland"), 600_000));
        assert!(score_of(&all, "s").is_err());
    }

    #[test]
    fn michigan_falls_back_to_all_counties_and_access_never_alerts() {
        let mut all: Vec<Value> = (0..3).map(|i| lake(&format!("t{i}"), "traverse", Some("great_lakes"), 1_000_000)).collect();
        all.extend((0..4).map(|i| lake(&format!("p{i}"), "petoskey", Some("great_lakes"), 1_000_000)));
        all.push(lake("s", "petoskey", Some("great_lakes"), 800_000));
        let s = score_of(&all, "s").unwrap();
        assert_eq!((s.group.as_str(), s.thin, s.n, s.alert), ("All six counties · great lakes", true, 7, false));

        let mut all: Vec<Value> = (0..8).map(|i| lake(&format!("a{i}"), "petoskey", Some("access"), 700_000)).collect();
        all.push(lake("s", "petoskey", Some("access"), 500_000));
        let s = score_of(&all, "s").unwrap();
        assert_eq!(s.group, "Petoskey · access");
        assert!(s.discount_pct >= 15.0);
        assert!(!s.alert, "access is scored but never alerts");
    }

    #[test]
    fn unread_water_type_is_scored_as_other() {
        let mut all: Vec<Value> = (0..6).map(|i| lake(&format!("o{i}"), "traverse", Some("other"), 800_000)).collect();
        all.push(lake("s", "traverse", None, 600_000));
        assert_eq!(score_of(&all, "s").unwrap().group, "Traverse · other");
    }

    #[test]
    fn michigan_alerts_only_on_read_lake_frontage() {
        for (water, alerts) in [(Some("inland"), true), (Some("great_lakes"), true), (Some("other"), false), (None, false)] {
            let mut all: Vec<Value> = (0..6).map(|i| lake(&format!("c{i}"), "traverse", water, 800_000)).collect();
            all.push(lake("s", "traverse", water, 600_000));
            let s = score_of(&all, "s").unwrap();
            assert!(s.discount_pct >= 15.0);
            assert_eq!(s.alert, alerts, "water {water:?}");
        }
    }

    #[test]
    fn alert_rule_price_cap_and_comp_only() {
        let mut all = comps("c", "Astoria", 2, 10, 1200);
        all.push(apt("big", "Astoria", 2, 950)); // $950k: 20.8% under but over the cap
        let mut old = apt("old", "Astoria", 2, 900);
        old["compOnly"] = json!(true);
        all.push(old);
        let s = score_of(&all, "big").unwrap();
        assert!(s.discount_pct > 15.0 && !s.alert);
        let s = score_of(&all, "old").unwrap();
        assert!(s.discount_pct > 15.0 && !s.alert, "comp-only listings never alert");
        let rule = AlertRule { max_price: 1_000_000.0, ..AlertRule::default() };
        let scorer = Scorer::new(&all, Options::with_rule(rule), NOW);
        assert!(scorer.score(&all[10]).unwrap().alert);
    }

    #[test]
    fn repeated_ids_count_once() {
        let mut all = comps("c", "Astoria", 2, 7, 800);
        all.push(all[0].clone());
        all.push(apt("s", "Astoria", 2, 600));
        assert!(score_of(&all, "s").is_err(), "7 distinct comps");
    }
}
