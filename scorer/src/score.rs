//! Baselines from comps, and the deal score.
//!
//! For a subject listing, the groups of [`crate::group`] are tried in order.
//! Inside a group the comps are picked for the subject (never the subject
//! itself):
//!
//! - **With a usable sq ft** (basis `ppsf`): comps with a usable sq ft
//!   within ±`size_window` (35%) of the subject's. Each comp's price is moved
//!   to the subject's size with the market's size elasticity `b`
//!   (price ∝ sqft^b, so $/sq ft falls with size when b < 1):
//!   `value = price × (subject sqft / comp sqft)^b`. The baseline is the
//!   median value. `b` is the pooled Theil–Sen slope of log price on log
//!   sq ft over pairs of comps in the same group (median of pairwise
//!   slopes), per NYC home type and for Michigan as a whole, clamped to
//!   0.3..1.0, with a default when there are too few pairs.
//! - **Without one** (basis `price`): the group's median asking price, over
//!   comps with the same number of baths in NYC (a proxy for size).
//!
//! A group answers when it has at least `min_comps` such comps, their spread
//! is sane ((p75 − p25) / median ≤ `max_spread`, or `max_spread_price` for
//! the price basis, which is noisier), and fewer than `max_ceiling_share`
//! of them ask within 10% of the crawl's price ceiling (the crawl stops at
//! $1.5M, so such a group is a truncated sample). A discount over
//! `max_discount_pct` (40%) is refused as bad data rather than reported.
//!
//! Some listings are refused outright ([`Refusal::Excluded`]) and are not
//! comps either: NYC homes that are not apartments, restricted or special
//! sales (Mitchell-Lama complexes, HDFC, income limits, auctions, land
//! leases, 55+), and NYC units whose building's other listings sit, at the
//! median, `cheap_building_pct` (10%) or more under their own baselines: a
//! whole building that cheap has a reason (restrictions, land lease, high
//! maintenance) that comps cannot see.
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
    /// Largest (p75 − p25) / median of the comps' values, per basis.
    pub max_spread: f64,
    pub max_spread_price: f64,
    pub max_discount_pct: f64,
    /// Comps' sq ft within subject × (1 ± window) (as a ratio: 1/(1+w)..1+w).
    pub size_window_nyc: f64,
    pub size_window_mi: f64,
    /// The crawl's search ceiling (both markets search up to $1.5M).
    pub price_ceiling: f64,
    /// A group with more than this share of comps at ≥ 90% of the ceiling is refused.
    pub max_ceiling_share: f64,
    /// NYC: refuse units of a building whose other listings are this far under, at the median.
    pub cheap_building_pct: f64,
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
            max_spread_price: 0.35,
            max_discount_pct: 40.0,
            size_window_nyc: 0.35,
            size_window_mi: 0.5,
            price_ceiling: 1_500_000.0,
            max_ceiling_share: 1.0 / 3.0,
            cheap_building_pct: 10.0,
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

    pub fn size_window(&self, m: Market) -> f64 {
        match m {
            Market::Nyc => self.size_window_nyc,
            Market::Mi => self.size_window_mi,
        }
    }
}

/// Size elasticity used when a class has fewer than [`MIN_SLOPE_PAIRS`]
/// pairs to estimate it from (measured on the 2026-10-05 data: NYC condos
/// 0.67, co-ops ≥ 1, Michigan ~0.6).
fn default_elasticity(m: Market, home_type: &str) -> f64 {
    match (m, home_type) {
        (Market::Nyc, "coop") => 0.9,
        (Market::Nyc, _) => 0.7,
        (Market::Mi, _) => 0.6,
    }
}
const MIN_SLOPE_PAIRS: usize = 200;
/// Pairs whose sq ft differ by less than this ratio say little about the slope.
const MIN_SLOPE_RATIO: f64 = 1.15;
const ELASTICITY_RANGE: (f64, f64) = (0.3, 1.0);
/// A comp asking at least this share of the ceiling counts as "at the ceiling".
const CEILING_BAND: f64 = 0.9;

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
    /// `ppsf` basis: the comps' median $/sq ft at the subject's size (baseline / sq ft).
    pub median_ppsf: Option<f64>,
    /// Of the comps' values: size-adjusted $/sq ft (`ppsf`) or price (`price`).
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
    /// Not the kind of home the market prices (e.g. a NYC house), or a
    /// special sale that comps cannot price (Mitchell-Lama, HDFC, auction, ...).
    Excluded(String),
    /// No group had enough comps with a sane spread; one reason per level tried.
    NoGroup(Vec<String>),
    /// Over `max_discount_pct` under the baseline: almost surely bad data.
    Implausible { discount_pct: f64, group: String, level: Level },
}

/// One comp as the scorer keeps it.
#[derive(Debug, Clone)]
struct Comp {
    id: String,
    price: f64,
    /// Usable sq ft (see [`Facts::ppsf`]).
    sqft: Option<f64>,
    baths: Option<f64>,
}

pub struct Scorer {
    groups: HashMap<GroupKey, Vec<Comp>>,
    /// Size elasticity per (market, class): NYC home type, or "all" in Michigan.
    elasticity: HashMap<(Market, String), f64>,
    /// NYC building → (listing id, discount against its group) of its priced units.
    buildings: HashMap<String, Vec<(String, f64)>>,
    opts: Options,
    sold_cutoff: Option<String>,
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn elasticity_class(f: &Facts) -> String {
    match f.market {
        Market::Nyc => f.home_type.clone(),
        Market::Mi => "all".to_string(),
    }
}

/// Baths as the price basis matches them: 1, 1.5, 2, 2.5+.
fn baths_bucket(b: Option<f64>) -> Option<i64> {
    b.map(|b| ((b * 2.0).round() as i64).clamp(2, 5))
}

impl Scorer {
    /// Builds the comp groups from `listings` (contract JSON; a repeated id
    /// counts once, the last copy wins). `now` (ISO) dates the sold-comp cutoff.
    pub fn new(listings: &[Value], opts: Options, now: &str) -> Scorer {
        let sold_cutoff = date_minus_days(now, opts.sold_comp_days);
        let mut seen = HashSet::new();
        let mut s = Scorer { groups: HashMap::new(), elasticity: HashMap::new(), buildings: HashMap::new(), opts, sold_cutoff };
        let mut facts = Vec::new();
        for f in listings.iter().rev().filter_map(Facts::from_json) {
            if !seen.insert(f.id.clone()) || !s.is_comp(&f) {
                continue;
            }
            let comp = Comp { id: f.id.clone(), price: f.price, sqft: f.ppsf().and(f.sqft), baths: f.baths };
            for g in groups_of(&f) {
                s.groups.entry(g).or_default().push(comp.clone());
            }
            facts.push(f);
        }
        s.elasticity = s.estimate_elasticity(&facts);
        // NYC buildings: each priced unit's discount against its own group.
        let mut buildings: HashMap<String, Vec<(String, f64)>> = HashMap::new();
        for f in facts.iter().filter(|f| f.market == Market::Nyc && f.active) {
            if let (Some(b), Ok(sc)) = (&f.building, s.score_in_groups(f)) {
                buildings.entry(b.clone()).or_default().push((f.id.clone(), sc.discount_pct));
            }
        }
        s.buildings = buildings;
        s
    }

    pub fn options(&self) -> &Options {
        &self.opts
    }

    /// The size elasticity the scorer uses for `f`'s class.
    pub fn elasticity_for(&self, f: &Facts) -> f64 {
        self.elasticity
            .get(&(f.market, elasticity_class(f)))
            .copied()
            .unwrap_or_else(|| default_elasticity(f.market, &f.home_type))
    }

    /// Pooled Theil–Sen: per class, the median over pairs of comps in the
    /// same first-level group of Δlog price / Δlog sq ft.
    fn estimate_elasticity(&self, facts: &[Facts]) -> HashMap<(Market, String), f64> {
        let class_of: HashMap<&str, (Market, String)> =
            facts.iter().map(|f| (f.id.as_str(), (f.market, elasticity_class(f)))).collect();
        let mut slopes: HashMap<(Market, String), Vec<f64>> = HashMap::new();
        for (g, comps) in &self.groups {
            if matches!(g.level, Level::MiAll) {
                continue;
            }
            let sized: Vec<(&Comp, f64, f64)> =
                comps.iter().filter_map(|c| c.sqft.map(|s| (c, s.ln(), c.price.ln()))).collect();
            for (i, a) in sized.iter().enumerate() {
                for b in &sized[i + 1..] {
                    let dx = b.1 - a.1;
                    if dx.abs() < MIN_SLOPE_RATIO.ln() {
                        continue;
                    }
                    if let Some(class) = class_of.get(a.0.id.as_str()) {
                        slopes.entry(class.clone()).or_default().push((b.2 - a.2) / dx);
                    }
                }
            }
        }
        slopes
            .into_iter()
            .filter(|(_, v)| v.len() >= MIN_SLOPE_PAIRS)
            .filter_map(|(k, v)| {
                let m = Sorted::new(&sorted(&v)).median()?;
                Some((k, m.clamp(ELASTICITY_RANGE.0, ELASTICITY_RANGE.1)))
            })
            .collect()
    }

    /// Active and not removed, or sold within the last `sold_comp_days`
    /// (a sold row without a usable `soldAt` is not trusted as a comp), and
    /// a home the market prices that is not a special sale.
    pub fn is_comp(&self, f: &Facts) -> bool {
        if f.removed || !f.priced_type() || f.special.is_some() {
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
        if !f.priced_type() {
            return Err(Refusal::Excluded(format!("not an apartment ({})", f.home_type.replace('_', " "))));
        }
        if let Some(why) = f.special {
            return Err(Refusal::Excluded(why.to_string()));
        }
        let score = self.score_in_groups(f);
        if matches!(score, Err(Refusal::NoGroup(_))) {
            return score;
        }
        if let Some(units) = f.building.as_ref().and_then(|b| self.buildings.get(b)) {
            let others: Vec<f64> = units.iter().filter(|(id, _)| *id != f.id).map(|(_, d)| *d).collect();
            if let Some(med) = Sorted::new(&sorted(&others)).median().filter(|m| *m >= self.opts.cheap_building_pct) {
                return Err(Refusal::Excluded(format!(
                    "its building's other {} listing{} ask {med:.0}% under their comps",
                    others.len(),
                    if others.len() == 1 { "" } else { "s" }
                )));
            }
        }
        score
    }

    /// The group search, without the exclusions and the building check.
    fn score_in_groups(&self, f: &Facts) -> Result<Score, Refusal> {
        let min = self.opts.min_comps(f.market);
        let subject_sqft = f.ppsf().and(f.sqft);
        let b = self.elasticity_for(f);
        let w = 1.0 + self.opts.size_window(f.market);
        let ceiling = self.opts.price_ceiling * CEILING_BAND;
        let mut why = Vec::new();
        for g in groups_of(f) {
            let name = label(&g);
            let Some(group) = self.groups.get(&g) else {
                why.push(format!("{name}: no comps"));
                continue;
            };
            // (value, comp price); the subject is never its own comp.
            let others = group.iter().filter(|c| c.id != f.id);
            let (basis, picked): (Basis, Vec<(f64, f64)>) = match subject_sqft {
                Some(s) => {
                    let v = others
                        .filter_map(|c| {
                            let r = s / c.sqft?;
                            (1.0 / w..=w).contains(&r).then(|| (c.price * r.powf(b), c.price))
                        })
                        .collect();
                    (Basis::Ppsf, v)
                }
                None => {
                    let baths = baths_bucket(f.baths).filter(|_| f.market == Market::Nyc);
                    let v = others
                        .filter(|c| baths.is_none_or(|bb| baths_bucket(c.baths) == Some(bb)))
                        .map(|c| (c.price, c.price))
                        .collect();
                    (Basis::Price, v)
                }
            };
            if picked.len() < min {
                why.push(format!("{name}: {} comps < {min}", picked.len()));
                continue;
            }
            let at_ceiling = picked.iter().filter(|(_, p)| *p >= ceiling).count();
            if at_ceiling as f64 > self.opts.max_ceiling_share * picked.len() as f64 {
                why.push(format!("{name}: {at_ceiling} of {} comps at the crawl ceiling", picked.len()));
                continue;
            }
            let values = sorted(&picked.iter().map(|(v, _)| *v).collect::<Vec<_>>());
            let values = Sorted::new(&values);
            let (Some(med), Some(p25), Some(p75)) = (values.median(), values.percentile(25.0), values.percentile(75.0)) else {
                continue;
            };
            let spread = (p75 - p25) / med;
            let max_spread = if basis == Basis::Price { self.opts.max_spread_price } else { self.opts.max_spread };
            if !(med > 0.0) || spread > max_spread {
                why.push(format!("{name}: spread {spread:.2} > {max_spread}"));
                continue;
            }
            let discount_pct = round1((med - f.price) / med * 100.0);
            if discount_pct > self.opts.max_discount_pct {
                return Err(Refusal::Implausible { discount_pct, group: name, level: g.level });
            }
            // The Deal shows $/sq ft for the ppsf basis: the values at the subject's size.
            let per = match (basis, subject_sqft) {
                (Basis::Ppsf, Some(s)) => s,
                _ => 1.0,
            };
            let mut sc = Score {
                baseline: med.round(),
                discount_pct,
                basis,
                group: name,
                level: g.level,
                n: values.len(),
                thin: g.level.thin(),
                median_ppsf: (basis == Basis::Ppsf).then_some((med / per).round()),
                p25: (p25 / per).round(),
                p75: (p75 / per).round(),
                alert: false,
            };
            sc.alert = self.opts.alert.accepts(f, &sc);
            return Ok(sc);
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
    fn neighbourhood_beds_group_with_similar_sizes() {
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
    fn comps_are_moved_to_the_subjects_size() {
        // Ten 800 sq ft condos at $640k ($800/sq ft) and a 1 000 sq ft subject.
        // A flat $/sq ft would say $800k; with the condo default elasticity
        // 0.7 (too few pairs to estimate one), $640k × 1.25^0.7 = $748.2k.
        let mut all: Vec<Value> = (0..10)
            .map(|i| {
                let mut l = apt(&format!("c{i}"), "Astoria", 2, 800);
                l["sqft"] = json!(800);
                l["price"] = json!(640_000);
                l
            })
            .collect();
        all.push(apt("s", "Astoria", 2, 600));
        let sc = Scorer::new(&all, Options::default(), NOW);
        let f = Facts::from_json(&all[10]).unwrap();
        assert_eq!(sc.elasticity_for(&f), 0.7);
        let s = sc.score(&all[10]).unwrap();
        assert_eq!(s.baseline, (640_000.0 * 1.25f64.powf(0.7)).round());
        assert_eq!(s.median_ppsf, Some(748.0), "$/sq ft at the subject's size");
        assert!((s.discount_pct - 19.8).abs() < 0.05, "{}", s.discount_pct);

        // Comps outside ±35% of the subject's size are not comps at all.
        for l in all.iter_mut().take(10) {
            l["sqft"] = json!(700); // 1000 / 700 = 1.43
        }
        match score_of(&all, "s") {
            Err(Refusal::NoGroup(why)) => assert!(why[0].contains("0 comps < 8"), "{why:?}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn elasticity_is_estimated_from_pairs_in_a_group() {
        // 40 condos from 600 to 1 770 sq ft priced exactly ∝ sqft^0.6
        // ($800/sq ft at 1 000 sq ft).
        let all: Vec<Value> = (0..40)
            .map(|i| {
                let sqft = 600.0 + 30.0 * i as f64;
                json!({"id": format!("c{i}"), "market": "nyc", "status": "active", "beds": 2, "homeType": "condo",
                       "neighborhood": "Astoria", "sqft": sqft, "price": (800_000.0 * (sqft / 1000.0).powf(0.6)).round()})
            })
            .collect();
        let sc = Scorer::new(&all, Options::default(), NOW);
        let e = sc.elasticity_for(&Facts::from_json(&all[0]).unwrap());
        assert!((e - 0.6).abs() < 0.01, "{e}");
        // Co-ops have no pairs here: their default.
        let coop = Facts::from_json(&json!({"id": "x", "market": "nyc", "price": 1, "homeType": "coop"})).unwrap();
        assert_eq!(sc.elasticity_for(&coop), 0.9);
    }

    #[test]
    fn beds_always_match_and_there_is_no_wider_nyc_group() {
        // 5 two-beds + 5 three-beds in Astoria: no beds group has 8. Refused,
        // where the old neighbourhood × type level would have mixed them.
        let mut all = comps("two", "Astoria", 2, 5, 800);
        all.extend(comps("three", "Astoria", 3, 5, 800));
        all.push(apt("s", "Astoria", 2, 640));
        match score_of(&all, "s") {
            Err(Refusal::NoGroup(why)) => assert_eq!(why.len(), 1, "{why:?}"),
            other => panic!("{other:?}"),
        }
        // A thin neighbourhood is not rescued by the rest of the borough.
        let mut all = comps("q", "Sunnyside", 2, 9, 800);
        all.push(apt("s", "Woodside", 2, 640));
        assert!(matches!(score_of(&all, "s"), Err(Refusal::NoGroup(_))));
        // Condos and co-ops never share a group.
        let mut all = comps("c", "Astoria", 2, 10, 800);
        let mut s = apt("s", "Astoria", 2, 600);
        s["homeType"] = json!("coop");
        all.push(s);
        assert!(matches!(score_of(&all, "s"), Err(Refusal::NoGroup(_))));
    }

    #[test]
    fn the_subject_is_not_its_own_comp() {
        // 7 comps + the subject = 8 rows, but only 7 comps: refused at NYC's 8.
        let mut all = comps("c", "Astoria", 2, 7, 800);
        all.push(apt("s", "Astoria", 2, 600));
        assert!(matches!(score_of(&all, "s"), Err(Refusal::NoGroup(_))));
        // 8 comps: priced, n = 8, and the cheap subject does not drag the median.
        let mut all = comps("c", "Astoria", 2, 8, 800);
        all.push(apt("s", "Astoria", 2, 640));
        let s = score_of(&all, "s").unwrap();
        assert_eq!(s.n, 8);
        // 784 784 792 792 800 800 808 816: the median of the comps alone (796).
        assert_eq!(s.median_ppsf, Some(796.0));
        // Each comp is scored against the other 7 comps + the subject.
        let c = score_of(&all, "c0").unwrap();
        assert_eq!(c.n, 8);
    }

    #[test]
    fn a_wide_group_is_refused() {
        // $/sq ft p25 225, median 600, p75 975: spread 1.25.
        let mut all: Vec<Value> = [100, 150, 200, 300, 500, 700, 900, 1000, 1100, 1200]
            .iter()
            .enumerate()
            .map(|(i, p)| apt(&format!("w{i}"), "Astoria", 2, *p))
            .collect();
        all.push(apt("s", "Astoria", 2, 400));
        match score_of(&all, "s") {
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
        assert_eq!((sc.group.as_str(), sc.n), ("Astoria · condo · 2bd", 10), "no baths: no baths filter");
        assert_eq!(sc.baseline, 800_000.0);
        assert_eq!(sc.p25, 792_000.0);

        // With baths, only comps with the same baths count (1, 1.5, 2, 2.5+).
        for (i, l) in all.iter_mut().enumerate() {
            l["baths"] = json!(if i < 9 { 1.0 } else { 2.0 });
        }
        all[10]["baths"] = json!(2.0);
        assert!(matches!(score_of(&all, "s"), Err(Refusal::NoGroup(_))), "one 2-bath comp");
        all[10]["baths"] = json!(1.0);
        let sc = score_of(&all, "s").unwrap();
        assert_eq!(sc.n, 9, "the 2-bath comp is left out");

        // The price basis needs a tight group (0.35), unlike $/sq ft (0.75).
        let mut all: Vec<Value> = [500, 600, 700, 800, 900, 1000, 1100, 1200, 1300]
            .iter()
            .enumerate()
            .map(|(i, p)| apt(&format!("w{i}"), "Astoria", 2, *p))
            .collect();
        all.push(apt("s", "Astoria", 2, 700));
        assert!(score_of(&all, "s").is_ok(), "spread 0.5 is fine for $/sq ft");
        all[9]["sqft"] = Value::Null;
        match score_of(&all, "s") {
            Err(Refusal::NoGroup(why)) => assert!(why[0].contains("spread"), "{why:?}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_subject_with_sqft_is_not_priced_by_comps_without() {
        // Comps mostly without sq ft (co-ops): too few sized comps, refused
        // rather than falling back to the median price of all sizes.
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
        assert!(matches!(score_of(&all, "s"), Err(Refusal::NoGroup(_))));
    }

    #[test]
    fn over_forty_percent_is_refused_as_bad_data() {
        let mut all = comps("c", "Astoria", 2, 10, 800);
        all.push(apt("s", "Astoria", 2, 460)); // 42.5% under
        assert!(matches!(score_of(&all, "s"), Err(Refusal::Implausible { .. })));
        all[10] = apt("s", "Astoria", 2, 500); // 37.5% under
        assert!(score_of(&all, "s").is_ok());
    }

    #[test]
    fn groups_at_the_crawl_ceiling_are_refused() {
        // Comps at $1.4M-$1.5M: the crawl stops at $1.5M, so this sample is
        // the bottom of a pricier group. 4 of 10 at ≥ $1.35M is over a third.
        let mut all: Vec<Value> = (0..10).map(|i| apt(&format!("c{i}"), "Tribeca", 2, if i < 4 { 1400 } else { 1200 })).collect();
        all.push(apt("s", "Tribeca", 2, 900));
        match score_of(&all, "s") {
            Err(Refusal::NoGroup(why)) => assert!(why[0].contains("4 of 10 comps at the crawl ceiling"), "{why:?}"),
            other => panic!("{other:?}"),
        }
        all[0] = apt("c0", "Tribeca", 2, 1200); // 3 of 10
        assert!(score_of(&all, "s").is_ok());
    }

    #[test]
    fn nyc_houses_and_special_sales_are_excluded() {
        let mut all = comps("c", "Astoria", 2, 10, 800);
        let mut house = apt("h", "Astoria", 2, 500);
        house["homeType"] = json!("single_family");
        all.push(house);
        match score_of(&all, "h") {
            Err(Refusal::Excluded(why)) => assert_eq!(why, "not an apartment (single family)"),
            other => panic!("{other:?}"),
        }
        let mut hdfc = apt("x", "Astoria", 2, 500);
        hdfc["address"] = json!("12 Main St (HDFC)");
        all.push(hdfc);
        assert_eq!(score_of(&all, "x"), Err(Refusal::Excluded("HDFC co-op (income-restricted)".into())));
        let mut ml = apt("m", "Starrett City", 2, 500);
        ml["unit"] = json!("#2");
        all.push(ml);
        assert!(matches!(score_of(&all, "m"), Err(Refusal::Excluded(_))));

        // Excluded listings are not comps either: 7 condos + 3 houses = 7 comps.
        let mut all = comps("c", "Astoria", 2, 7, 800);
        for i in 0..3 {
            let mut h = apt(&format!("h{i}"), "Astoria", 2, 800);
            h["homeType"] = json!("multi_family");
            all.push(h);
        }
        all.push(apt("s", "Astoria", 2, 600));
        assert!(matches!(score_of(&all, "s"), Err(Refusal::NoGroup(_))));
    }

    #[test]
    fn units_of_a_building_that_lists_cheap_are_excluded() {
        let mut all = comps("c", "Astoria", 2, 10, 800);
        for (id, ppsf) in [("a", 560), ("b", 580)] {
            let mut l = apt(id, "Astoria", 2, ppsf);
            l["address"] = json!("246 East 51st  Street");
            all.push(l);
        }
        // a is 30% under, b 27.5%: each one's building mate is under too.
        match score_of(&all, "a") {
            Err(Refusal::Excluded(why)) => assert!(why.starts_with("its building's other 1 listing ask 2"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(score_of(&all, "b"), Err(Refusal::Excluded(_))));
        // Alone in its building, the same unit is a deal.
        all[11]["address"] = json!("1 Other Street");
        assert_eq!(score_of(&all, "a").unwrap().discount_pct, 30.0);
        // A building whose other units ask their comps' price is no warning.
        let mut all = comps("c", "Astoria", 2, 10, 800);
        for l in all.iter_mut().take(3) {
            l["address"] = json!("1 Main St");
        }
        let mut s = apt("s", "Astoria", 2, 600);
        s["address"] = json!("1 MAIN ST");
        all.push(s);
        assert_eq!(score_of(&all, "s").unwrap().discount_pct, 25.0, "address case and spaces do not matter");
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
    fn michigan_houses_of_any_type_are_priced() {
        let mut all: Vec<Value> = (0..6).map(|i| lake(&format!("c{i}"), "traverse", Some("inland"), 800_000)).collect();
        let mut s = lake("s", "traverse", Some("inland"), 600_000);
        s["homeType"] = json!("multi_family");
        all.push(s);
        assert!(score_of(&all, "s").is_ok(), "the apartments-only rule is NYC's");
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
