//! Comp groups, in the order DESIGN.md tries them.
//!
//! NYC:
//!   1. neighbourhood × home type × beds bucket (2, 3, 4+)
//!   2. neighbourhood × home type
//!   3. borough × home type × beds bucket            (thin)
//! Michigan:
//!   1. area × water type
//!   2. all six counties × water type                 (thin)
//!
//! A listing belongs to every group whose key parts it has; a listing missing
//! a part (no neighbourhood, no beds, ...) simply skips that level.

use crate::listing::{Facts, Market};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Level {
    NycBeds,
    NycType,
    NycBorough,
    MiArea,
    MiAll,
}

impl Level {
    /// A fallback group: its baseline is shown but never alerts.
    pub fn thin(self) -> bool {
        matches!(self, Level::NycBorough | Level::MiAll)
    }
}

/// One concrete group, e.g. (NycBeds, "Astoria|condo|2").
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

/// The groups `f` belongs to, in fallback order.
pub fn groups_of(f: &Facts) -> Vec<GroupKey> {
    let mut out = Vec::with_capacity(3);
    let mut push = |level: Level, parts: &[Option<&str>]| {
        if parts.iter().all(Option::is_some) {
            let key = parts.iter().map(|p| p.unwrap()).collect::<Vec<_>>().join("|");
            out.push(GroupKey { level, key });
        }
    };
    let ht = Some(f.home_type.as_str());
    match f.market {
        Market::Nyc => {
            let bucket = beds_bucket(f.beds);
            push(Level::NycBeds, &[f.neighborhood.as_deref(), ht, bucket]);
            push(Level::NycType, &[f.neighborhood.as_deref(), ht]);
            push(Level::NycBorough, &[f.borough.as_deref(), ht, bucket]);
        }
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

/// Human label, e.g. `Astoria · condo · 2bd`, `Queens · co-op · 4+bd`,
/// `Traverse · inland`, `All six counties · great lakes`.
pub fn label(g: &GroupKey) -> String {
    let p: Vec<&str> = g.key.split('|').collect();
    let beds = |b: &str| if b == "1" { "0-1bd".to_string() } else { format!("{b}bd") };
    match g.level {
        Level::NycBeds | Level::NycBorough => {
            let place = if g.level == Level::NycBorough { title(p[0]) } else { p[0].to_string() };
            format!("{place} · {} · {}", home_type_label(p[1]), beds(p[2]))
        }
        Level::NycType => format!("{} · {}", p[0], home_type_label(p[1])),
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
        assert_eq!(labels, vec!["Astoria · co-op · 4+bd", "Astoria · co-op", "Queens · co-op · 4+bd"]);
        assert_eq!(g.iter().map(|g| g.level.thin()).collect::<Vec<_>>(), vec![false, false, true]);

        let no_beds = facts(json!({"id": "a", "market": "nyc", "price": 1, "homeType": "condo",
                                   "neighborhood": "Astoria", "borough": "staten_island"}));
        assert_eq!(groups_of(&no_beds).iter().map(label).collect::<Vec<_>>(), vec!["Astoria · condo"]);
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
