//! Comp groups, in the order DESIGN.md tries them.
//!
//! NYC (condos and co-ops only; beds always match):
//!   1. neighbourhood × home type × beds bucket (2, 3, 4+)
//!   There is no wider NYC level: a whole neighbourhood mixes unit sizes
//!   and a borough mixes markets, so a listing whose neighbourhood × type ×
//!   beds group cannot price it is refused.
//! Michigan:
//!   1. area × water type
//!   2. all six counties × water type                 (thin)
//!
//! Inside a group the scorer still picks the comps per listing (similar
//! size, same baths when pricing without sq ft): see `score`.
//! A listing belongs to every group whose key parts it has; a listing missing
//! a part (no neighbourhood, no beds, ...) simply skips that level.

use crate::listing::{Facts, Market};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Level {
    NycBeds,
    MiArea,
    MiAll,
}

impl Level {
    /// A fallback group: its baseline is shown but never alerts.
    pub fn thin(self) -> bool {
        matches!(self, Level::MiAll)
    }
}

/// One concrete group, e.g. (NycBeds, "queens|Astoria|condo|2").
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GroupKey {
    pub level: Level,
    pub key: String,
}

/// 2, 3 and 4+ bedrooms; 0-1 bedroom units (rare in a 2+ bed search) get their own bucket.
pub fn beds_bucket(beds: Option<i64>) -> Option<&'static str> {
    Some(match beds? {
        i64::MIN..=1 => "1",
        2 => "2",
        3 => "3",
        _ => "4+",
    })
}

/// The groups `f` belongs to, in fallback order. NYC homes that are not
/// apartments belong to none.
pub fn groups_of(f: &Facts) -> Vec<GroupKey> {
    let mut out = Vec::with_capacity(2);
    let mut push = |level: Level, parts: &[Option<&str>]| {
        if parts.iter().all(Option::is_some) {
            let key = parts.iter().map(|p| p.unwrap()).collect::<Vec<_>>().join("|");
            out.push(GroupKey { level, key });
        }
    };
    let ht = Some(f.home_type.as_str());
    match f.market {
        Market::Nyc if f.priced_type() => {
            // The borough is part of the key: StreetEasy reuses area names
            // (Murray Hill is in Manhattan and Queens, Bay Terrace and
            // Sunnyside in Queens and Staten Island).
            let borough = Some(f.borough.as_deref().unwrap_or(""));
            push(Level::NycBeds, &[borough, f.neighborhood.as_deref(), ht, beds_bucket(f.beds)]);
        }
        Market::Nyc => {}
        Market::Mi => {
            push(Level::MiArea, &[f.area.as_deref(), Some(f.water.as_str())]);
            push(Level::MiAll, &[Some(f.water.as_str())]);
        }
    }
    out
}

fn title(s: &str) -> String {
    s.split(['_', ' '])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            c.next().map_or(String::new(), |f| f.to_uppercase().collect::<String>() + c.as_str())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn home_type_label(t: &str) -> String {
    match t {
        "coop" => "co-op".to_string(),
        t => t.replace('_', " "),
    }
}

/// Human label, e.g. `Astoria · condo · 2bd`, `Astoria · co-op · 4+bd`,
/// `Traverse · inland`, `All six counties · great lakes`.
pub fn label(g: &GroupKey) -> String {
    let p: Vec<&str> = g.key.split('|').collect();
    let beds = |b: &str| if b == "1" { "0-1bd".to_string() } else { format!("{b}bd") };
    match g.level {
        Level::NycBeds => format!("{} · {} · {}", p[1], home_type_label(p[2]), beds(p[3])),
        Level::MiArea => format!("{} · {}", title(p[0]), p[1].replace('_', " ")),
        Level::MiAll => format!("All six counties · {}", p[0].replace('_', " ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn facts(v: serde_json::Value) -> Facts {
        Facts::from_json(&v).unwrap()
    }

    #[test]
    fn nyc_groups_and_labels() {
        let f = facts(json!({"id": "a", "market": "nyc", "price": 1, "beds": 5, "homeType": "coop",
                             "neighborhood": "Astoria", "borough": "queens"}));
        let g = groups_of(&f);
        let labels: Vec<String> = g.iter().map(label).collect();
        assert_eq!(labels, vec!["Astoria · co-op · 4+bd"], "beds always match, no borough fallback");
        assert!(!g[0].level.thin());
        assert_eq!(g[0].key, "queens|Astoria|coop|4+");
        let mut other = f.clone();
        other.borough = Some("staten_island".into());
        assert_ne!(groups_of(&other), g, "same area name in another borough: another group");

        let no_beds = facts(json!({"id": "a", "market": "nyc", "price": 1, "homeType": "condo",
                                   "neighborhood": "Astoria", "borough": "staten_island"}));
        assert!(groups_of(&no_beds).is_empty(), "no beds: no group");

        for t in ["single_family", "multi_family", "townhouse", "other"] {
            let house = facts(json!({"id": "a", "market": "nyc", "price": 1, "beds": 3, "homeType": t,
                                     "neighborhood": "Astoria", "borough": "queens"}));
            assert!(groups_of(&house).is_empty(), "{t}: NYC prices apartments only");
        }
    }

    #[test]
    fn michigan_groups_and_labels() {
        let f = facts(json!({"id": "a", "market": "mi", "price": 1, "area": "traverse", "waterType": "great_lakes"}));
        let labels: Vec<String> = groups_of(&f).iter().map(label).collect();
        assert_eq!(labels, vec!["Traverse · great lakes", "All six counties · great lakes"]);
        let f = facts(json!({"id": "a", "market": "mi", "price": 1}));
        assert_eq!(groups_of(&f).iter().map(label).collect::<Vec<_>>(), vec!["All six counties · other"]);
    }
}
