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

/// NYC home types the scorer prices: the target is apartments. Houses,
/// townhouses, multi-family buildings and StreetEasy's unknown types are
/// neither subjects nor comps there.
pub const NYC_APARTMENT_TYPES: [&str; 2] = ["condo", "coop"];

/// StreetEasy areas that are (almost) all restricted housing: Mitchell-Lama
/// and ex-Mitchell-Lama complexes resell under income limits or at
/// regulated prices, so open-market comps cannot price them.
pub const RESTRICTED_NEIGHBORHOODS: [&str; 5] =
    ["starrett city", "spring creek", "co-op city", "rochdale village", "penn south"];

/// Words in the address, unit or description that mark a sale comps cannot
/// price: restricted resale, a partial interest, or a price that is not an
/// asking price. Matched case-insensitively.
pub const SPECIAL_WORDS: [(&str, &str); 19] = [
    ("hdfc", "HDFC co-op (income-restricted)"),
    ("mitchell-lama", "Mitchell-Lama (restricted resale)"),
    ("mitchell lama", "Mitchell-Lama (restricted resale)"),
    ("income restrict", "income-restricted"),
    ("income-restrict", "income-restricted"),
    ("income limit", "income-restricted"),
    ("affordable housing", "income-restricted"),
    ("auction", "auction"),
    ("land lease", "land lease"),
    ("ground lease", "land lease"),
    ("leasehold", "land lease"),
    ("55+", "55+ community"),
    ("55 and over", "55+ community"),
    ("age restricted", "55+ community"),
    ("age-restricted", "55+ community"),
    ("senior community", "55+ community"),
    ("timeshare", "fractional ownership"),
    ("fractional", "fractional ownership"),
    ("life estate", "life estate"),
];

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
    pub baths: Option<f64>,
    pub sqft: Option<f64>,
    pub home_type: String,
    pub neighborhood: Option<String>,
    pub borough: Option<String>,
    pub area: Option<String>,
    /// Always one of [`WATER_TYPES`] for Michigan (null mapped to `other`).
    pub water: String,
    pub comp_only: bool,
    /// The building: market, normalised street address and home type (NYC
    /// units of one building share it; the unit is a separate field).
    pub building: Option<String>,
    /// Why comps cannot price this listing (see [`SPECIAL_WORDS`],
    /// [`RESTRICTED_NEIGHBORHOODS`]), when they cannot.
    pub special: Option<&'static str>,
}

fn special_of(l: &Value, neighborhood: Option<&str>) -> Option<&'static str> {
    if let Some(n) = neighborhood {
        let n = n.to_lowercase();
        if RESTRICTED_NEIGHBORHOODS.contains(&n.as_str()) {
            return Some("restricted complex (Mitchell-Lama)");
        }
    }
    let text = ["address", "unit", "description"]
        .iter()
        .filter_map(|k| text(l, k))
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    SPECIAL_WORDS.iter().find(|(w, _)| text.contains(w)).map(|(_, why)| *why)
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
            baths: l.get("baths").and_then(Value::as_f64).filter(|b| b.is_finite() && *b > 0.0),
            sqft: l.get("sqft").and_then(Value::as_f64).filter(|s| s.is_finite() && *s > 0.0),
            home_type: text(l, "homeType").unwrap_or("other").to_string(),
            neighborhood: text(l, "neighborhood").map(str::to_string),
            borough: text(l, "borough").map(str::to_string),
            area: text(l, "area").map(str::to_string),
            water,
            comp_only: l.get("compOnly") == Some(&Value::Bool(true)),
            building: text(l, "address").map(|a| {
                let a = a.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
                format!("{}|{a}|{}", market.as_str(), text(l, "homeType").unwrap_or("other"))
            }),
            special: special_of(l, text(l, "neighborhood")),
        })
    }

    /// Whether the scorer prices this kind of home in its market: NYC
    /// apartments only, any Michigan home.
    pub fn priced_type(&self) -> bool {
        self.market != Market::Nyc || NYC_APARTMENT_TYPES.contains(&self.home_type.as_str())
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
    fn special_sales_and_buildings() {
        let f = |v: serde_json::Value| Facts::from_json(&v).unwrap();
        let base = json!({"id": "a", "market": "nyc", "price": 1, "address": "246 East  51st Street", "homeType": "coop",
                          "neighborhood": "Turtle Bay"});
        let a = f(base.clone());
        assert_eq!((a.special, a.building.as_deref()), (None, Some("nyc|246 east 51st street|coop")));
        assert!(a.priced_type());
        for (k, v, why) in [
            ("unit", "#4B HDFC", "HDFC co-op (income-restricted)"),
            ("description", "Mitchell-Lama co-op, income limits apply", "Mitchell-Lama (restricted resale)"),
            ("description", "Sold at AUCTION", "auction"),
            ("description", "Land lease through 2060", "land lease"),
            ("description", "A 55+ community on the lake", "55+ community"),
            ("neighborhood", "Starrett City", "restricted complex (Mitchell-Lama)"),
        ] {
            let mut l = base.clone();
            l[k] = json!(v);
            assert_eq!(f(l).special, Some(why), "{k}: {v}");
        }
        let mut house = base.clone();
        house["homeType"] = json!("multi_family");
        assert!(!f(house.clone()).priced_type());
        house["market"] = json!("mi");
        assert!(f(house).priced_type(), "Michigan prices houses");
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
