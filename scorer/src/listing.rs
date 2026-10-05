//! The listing facts the scorer reads, taken from the contract's camelCase
//! JSON (CONTRACT.md "Listing"), and the Deal shape it writes.

use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Market {
    Nyc,
    Mi,
}

impl Market {
    pub fn parse(s: &str) -> Option<Market> {
        match s {
            "nyc" => Some(Market::Nyc),
            "mi" => Some(Market::Mi),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Market::Nyc => "nyc",
            Market::Mi => "mi",
        }
    }

    /// Plausible price per sq ft. Outside it the sq ft (or the price) is bad
    /// data, and the listing is treated as having no sq ft.
    pub fn ppsf_bounds(self) -> (f64, f64) {
        match self {
            Market::Nyc => (100.0, 5_000.0),
            Market::Mi => (50.0, 2_000.0),
        }
    }
}

/// Below this the sq ft is a typo or a parking space: ignored.
pub const MIN_SQFT: f64 = 300.0;

/// Michigan water types (CONTRACT.md `waterType`). Null means "not read yet"
/// and is scored as `other`.
pub const WATER_TYPES: [&str; 4] = ["great_lakes", "inland", "access", "other"];

#[derive(Debug, Clone, PartialEq)]
pub struct Facts {
    pub id: String,
    pub market: Market,
    pub active: bool,
    pub sold: bool,
    /// Present (non-null `removedAt`): expired or delisted, never a comp.
    pub removed: bool,
    pub price: f64,
    pub sold_at: Option<String>,
    pub beds: Option<i64>,
    pub sqft: Option<f64>,
    pub home_type: String,
    pub neighborhood: Option<String>,
    pub borough: Option<String>,
    pub area: Option<String>,
    /// Always one of [`WATER_TYPES`] for Michigan (null mapped to `other`).
    pub water: String,
    pub comp_only: bool,
}

fn text<'a>(l: &'a Value, k: &str) -> Option<&'a str> {
    l.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

impl Facts {
    /// `None` when the listing cannot take part at all (no id, unknown
    /// market, no positive price).
    pub fn from_json(l: &Value) -> Option<Facts> {
        let id = text(l, "id")?.to_string();
        let market = Market::parse(text(l, "market")?)?;
        let price = l.get("price").and_then(Value::as_f64).filter(|p| p.is_finite() && *p > 0.0)?;
        let status = text(l, "status").unwrap_or("active");
        let water = text(l, "waterType").filter(|w| WATER_TYPES.contains(w)).unwrap_or("other").to_string();
        Some(Facts {
            id,
            market,
            active: status == "active",
            sold: status == "sold",
            removed: l.get("removedAt").is_some_and(|v| !v.is_null()),
            price,
            sold_at: text(l, "soldAt").map(str::to_string),
            beds: l.get("beds").and_then(Value::as_f64).filter(|b| *b >= 0.0).map(|b| b as i64),
            sqft: l.get("sqft").and_then(Value::as_f64).filter(|s| s.is_finite() && *s > 0.0),
            home_type: text(l, "homeType").unwrap_or("other").to_string(),
            neighborhood: text(l, "neighborhood").map(str::to_string),
            borough: text(l, "borough").map(str::to_string),
            area: text(l, "area").map(str::to_string),
            water,
            comp_only: l.get("compOnly") == Some(&Value::Bool(true)),
        })
    }

    /// Price per sq ft when the sq ft is usable: at least [`MIN_SQFT`] and a
    /// price per sq ft inside the market's plausible band.
    pub fn ppsf(&self) -> Option<f64> {
        let sqft = self.sqft.filter(|s| *s >= MIN_SQFT)?;
        let p = self.price / sqft;
        let (lo, hi) = self.market.ppsf_bounds();
        (lo..=hi).contains(&p).then_some(p)
    }
}

/// Listing fields a Deal carries (CONTRACT.md "Deal"), plus `market` and
/// `status` so a reader never has to guess.
pub const DEAL_FIELDS: [&str; 27] = [
    "id", "market", "status", "url", "address", "unit", "city", "neighborhood", "borough", "county", "area",
    "price", "beds", "baths", "sqft", "lotSqft", "homeType", "zestimate", "daysOnMarket", "photoUrl",
    "waterType", "waterBody", "waterSource", "frontageFt", "maintenance", "taxes", "compOnly",
];

/// The listing part of a Deal: every [`DEAL_FIELDS`] key, null when absent.
pub fn deal_base(l: &Value) -> Map<String, Value> {
    DEAL_FIELDS
        .iter()
        .filter(|k| **k != "compOnly")
        .map(|k| (k.to_string(), l.get(*k).cloned().unwrap_or(Value::Null)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn facts_read_the_contract_shape() {
        let f = Facts::from_json(&json!({"id": "zl:1", "market": "mi", "status": "sold", "price": 500000,
            "soldAt": "2026-05-01", "sqft": 2000, "homeType": "single_family", "area": "traverse",
            "waterType": null, "compOnly": true}))
        .unwrap();
        assert!(f.sold && !f.active && f.comp_only);
        assert_eq!(f.water, "other", "null water type is scored as other");
        assert_eq!(f.ppsf(), Some(250.0));
        assert!(Facts::from_json(&json!({"id": "x", "market": "la", "price": 1})).is_none());
        assert!(Facts::from_json(&json!({"id": "x", "market": "nyc", "price": 0})).is_none());
    }

    #[test]
    fn bad_sqft_is_ignored() {
        let f = |price: i64, sqft: i64, market: &str| {
            Facts::from_json(&json!({"id": "a", "market": market, "price": price, "sqft": sqft})).unwrap().ppsf()
        };
        assert_eq!(f(500_000, 250, "nyc"), None, "under 300 sq ft");
        assert_eq!(f(500_000, 10_000, "nyc"), None, "$50/sq ft in NYC is bad data");
        assert_eq!(f(500_000, 10_000, "mi"), Some(50.0), "but plausible up north");
        assert_eq!(f(9_000_000, 1_000, "nyc"), None, "$9 000/sq ft");
        assert_eq!(f(900_000, 400, "mi"), None, "$2 250/sq ft in Michigan");
    }
}
