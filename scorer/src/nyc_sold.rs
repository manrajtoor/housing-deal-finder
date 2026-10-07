//! NYC sold rows (Zillow recently sold, `zl:*`, comps only): building type
//! and neighbourhood, filled in before scoring from the StreetEasy rows of
//! the same pass (DESIGN.md "NYC sold comps").
//!
//! Zillow's sold cards say `CONDO` for most NYC apartments, co-ops included,
//! and `APARTMENT` for others (units of one building come either way), and
//! they carry no StreetEasy neighbourhood. So, for each sold row without a
//! neighbourhood:
//!
//! - **Building type.** StreetEasy rows (`se:*`) at the same normalised street
//!   address in the same borough give it (their majority condo / co-op; a
//!   tie is no answer). Without one, only an explicit Zillow `COOPERATIVE`
//!   (crawled as `coop`) is kept; any other type is uncertain and the row is
//!   set to `other`, which keeps it out of every typed (NYC) group.
//! - **Neighbourhood.** The StreetEasy rows of that same building give it
//!   when there are any (StreetEasy's own label for the building); otherwise
//!   the majority neighbourhood of the [`K`] nearest StreetEasy rows within
//!   [`RADIUS_M`] in the same borough (ties: the nearest one's). None near: no
//!   neighbourhood, so no group, so not a comp.
//!
//! Sold rows are never subjects (the scorer refuses non-active rows); this
//! only decides which comp group they join.

use std::collections::HashMap;

use serde_json::Value;

/// Neighbours that vote on a sold row's neighbourhood.
pub const K: usize = 5;
/// The farthest a voting StreetEasy row may be.
pub const RADIUS_M: f64 = 600.0;

const EARTH_M: f64 = 6_371_000.0;

/// Great-circle distance in metres.
pub fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = p2 - p1;
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * EARTH_M * a.sqrt().min(1.0).asin()
}

/// Words that start a unit at the end of an address.
const UNIT_WORDS: [&str; 9] = ["apt", "apartment", "unit", "ste", "suite", "rm", "room", "fl", "floor"];

fn abbreviation(w: &str) -> &str {
    match w {
        "avenue" | "av" | "aven" => "ave",
        "street" | "str" => "st",
        "road" => "rd",
        "boulevard" | "boul" => "blvd",
        "place" => "pl",
        "drive" => "dr",
        "lane" => "ln",
        "court" => "ct",
        "parkway" | "pky" => "pkwy",
        "terrace" => "ter",
        "square" => "sq",
        "plaza" => "plz",
        "highway" => "hwy",
        "expressway" => "expy",
        "turnpike" => "tpke",
        "circle" => "cir",
        "loop" => "lp",
        "east" => "e",
        "west" => "w",
        "north" => "n",
        "south" => "s",
        "first" => "1",
        "second" => "2",
        "third" => "3",
        "fourth" => "4",
        "fifth" => "5",
        "sixth" => "6",
        "seventh" => "7",
        "eighth" => "8",
        "ninth" => "9",
        "tenth" => "10",
        "eleventh" => "11",
        "twelfth" => "12",
        w => w,
    }
}

/// "21st" → "21", "2nd" → "2"; other words unchanged.
fn strip_ordinal(w: &str) -> &str {
    for suf in ["st", "nd", "rd", "th"] {
        if let Some(n) = w.strip_suffix(suf) {
            if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) {
                return n;
            }
        }
    }
    w
}

/// The street address as a building key: lower case, no punctuation, the
/// unit cut off ("APT 3B", "UNIT 4", "#2E"), street words abbreviated
/// ("Avenue" → "ave", "Street" → "st", "East" → "e", "Fifth" → "5"),
/// ordinals bare ("21st" → "21") and the hyphen of a Queens house number
/// dropped ("166-25" → "16625"). "" when nothing is left.
///
/// `normalize_street("1408 Avenue O APT 3B") == normalize_street("1408 Ave O")`.
pub fn normalize_street(addr: &str) -> String {
    let lower = addr.to_lowercase();
    // A "#" starts the unit wherever it is: "12 Main St #4", "12 Main St, #4".
    let cut = lower.split('#').next().unwrap_or("");
    let cleaned: String = cut.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == ' ' { c } else { ' ' }).collect();
    let mut out: Vec<String> = Vec::new();
    for (i, w) in cleaned.split_whitespace().enumerate() {
        if i > 0 && UNIT_WORDS.contains(&w) {
            break;
        }
        let w = if i == 0 { w.replace('-', "") } else { w.trim_matches('-').to_string() };
        if w.is_empty() {
            continue;
        }
        out.push(strip_ordinal(abbreviation(&w)).to_string());
    }
    out.join(" ")
}

fn text<'a>(l: &'a Value, k: &str) -> Option<&'a str> {
    l.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn num(l: &Value, k: &str) -> Option<f64> {
    l.get(k).and_then(Value::as_f64).filter(|x| x.is_finite())
}

/// [`normalize_street`] without the spaces, so "Mac Donough St" and
/// "Macdonough Street", or "Van Cortlandt Ave W" and "Vancortlandt Avenue
/// West", are one building.
pub fn street_key(addr: &str) -> String {
    normalize_street(addr).replace(' ', "")
}

fn building_key(l: &Value) -> Option<String> {
    let b = text(l, "borough")?;
    let a = street_key(text(l, "address")?);
    (!a.is_empty()).then(|| format!("{b}|{a}"))
}

/// The most common value; a tie goes to the one `rank` puts first (lower is
/// better), or is no answer when `rank` is None.
fn majority<'a>(votes: &[(&'a str, f64)], tie_by_rank: bool) -> Option<&'a str> {
    let mut count: HashMap<&str, (usize, f64)> = HashMap::new();
    for (v, rank) in votes {
        let e = count.entry(v).or_insert((0, f64::INFINITY));
        e.0 += 1;
        e.1 = e.1.min(*rank);
    }
    let best = count.values().map(|c| c.0).max()?;
    let mut top: Vec<(&str, f64)> = count.iter().filter(|(_, c)| c.0 == best).map(|(v, c)| (*v, c.1)).collect();
    if top.len() > 1 && !tie_by_rank {
        return None;
    }
    top.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(b.0)));
    top.first().map(|t| t.0)
}

/// How the sold rows were placed, per borough.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BoroughStats {
    pub sold: usize,
    /// Building type from StreetEasy rows at the same address.
    pub type_from_streeteasy: usize,
    /// No StreetEasy match, Zillow said COOPERATIVE: kept as a co-op.
    pub type_from_zillow_coop: usize,
    /// No StreetEasy match, any other Zillow type: set to `other` (not a comp).
    pub type_uncertain: usize,
    pub nbhd_from_building: usize,
    pub nbhd_from_neighbours: usize,
    pub nbhd_none: usize,
    /// Typed and placed: the rows that join a comp group (beds permitting).
    pub usable: usize,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stats {
    pub by_borough: std::collections::BTreeMap<String, BoroughStats>,
    /// (crawled Zillow type, StreetEasy type) of the rows typed by StreetEasy:
    /// how far Zillow's own type can be trusted.
    pub zillow_vs_streeteasy: std::collections::BTreeMap<(String, String), usize>,
}

impl Stats {
    pub fn total(&self) -> BoroughStats {
        let mut t = BoroughStats::default();
        for b in self.by_borough.values() {
            t.sold += b.sold;
            t.type_from_streeteasy += b.type_from_streeteasy;
            t.type_from_zillow_coop += b.type_from_zillow_coop;
            t.type_uncertain += b.type_uncertain;
            t.nbhd_from_building += b.nbhd_from_building;
            t.nbhd_from_neighbours += b.nbhd_from_neighbours;
            t.nbhd_none += b.nbhd_none;
            t.usable += b.usable;
        }
        t
    }
}

/// A StreetEasy row as the neighbourhood vote sees it.
struct Se<'a> {
    lat: f64,
    lon: f64,
    borough: &'a str,
    neighborhood: &'a str,
}

/// Grid of StreetEasy rows in cells about [`RADIUS_M`] wide, so the rows
/// within the radius of a point are in its cell or the 8 around it.
struct Grid<'a> {
    cells: HashMap<(i64, i64), Vec<Se<'a>>>,
    dlat: f64,
    dlon: f64,
}

impl<'a> Grid<'a> {
    fn new(rows: Vec<Se<'a>>) -> Grid<'a> {
        let dlat = RADIUS_M / 111_320.0;
        // Cells are narrower in longitude at NYC's latitude; use the
        // northernmost (narrowest) one so 3×3 cells always cover the radius.
        let dlon = dlat / 41.0f64.to_radians().cos();
        let mut cells: HashMap<(i64, i64), Vec<Se>> = HashMap::new();
        for r in rows {
            cells.entry(((r.lat / dlat).floor() as i64, (r.lon / dlon).floor() as i64)).or_default().push(r);
        }
        Grid { cells, dlat, dlon }
    }

    /// The neighbourhood most of the K nearest rows within the radius, in
    /// `borough`, agree on (ties: the nearest).
    fn vote(&self, lat: f64, lon: f64, borough: &str) -> Option<&'a str> {
        let (ci, cj) = ((lat / self.dlat).floor() as i64, (lon / self.dlon).floor() as i64);
        let mut near: Vec<(f64, &'a str)> = Vec::new();
        for di in -1..=1 {
            for dj in -1..=1 {
                for r in self.cells.get(&(ci + di, cj + dj)).into_iter().flatten() {
                    if r.borough != borough {
                        continue;
                    }
                    let d = haversine_m(lat, lon, r.lat, r.lon);
                    if d <= RADIUS_M {
                        near.push((d, r.neighborhood));
                    }
                }
            }
        }
        near.sort_by(|a, b| a.0.total_cmp(&b.0));
        near.truncate(K);
        let votes: Vec<(&str, f64)> = near.iter().map(|(d, n)| (*n, *d)).collect();
        majority(&votes, true)
    }
}

fn is_streeteasy(l: &Value) -> bool {
    text(l, "id").is_some_and(|id| id.starts_with("se:"))
}

/// Fills in `homeType` and `neighborhood` of the NYC sold rows of `rows`
/// that have no neighbourhood (see the module comment). Other rows are left
/// as they are.
pub fn enrich(rows: &mut [Value]) -> Stats {
    // StreetEasy buildings: (types, neighbourhoods) by borough|street.
    let mut buildings: HashMap<String, (Vec<String>, Vec<String>)> = HashMap::new();
    let mut se_points: Vec<(f64, f64, String, String)> = Vec::new();
    for l in rows.iter().filter(|l| is_streeteasy(l) && text(l, "status").unwrap_or("active") == "active") {
        if let Some(k) = building_key(l) {
            let e = buildings.entry(k).or_default();
            if let Some(t) = text(l, "homeType").filter(|t| *t == "condo" || *t == "coop") {
                e.0.push(t.to_string());
            }
            if let Some(n) = text(l, "neighborhood") {
                e.1.push(n.to_string());
            }
        }
        if let (Some(lat), Some(lon), Some(b), Some(n)) = (num(l, "lat"), num(l, "lon"), text(l, "borough"), text(l, "neighborhood")) {
            se_points.push((lat, lon, b.to_string(), n.to_string()));
        }
    }
    let grid = Grid::new(
        se_points.iter().map(|(lat, lon, b, n)| Se { lat: *lat, lon: *lon, borough: b, neighborhood: n }).collect(),
    );

    let mut stats = Stats::default();
    for l in rows.iter_mut() {
        if text(l, "market") != Some("nyc") || text(l, "status") != Some("sold") || text(l, "neighborhood").is_some() {
            continue;
        }
        let borough = text(l, "borough").unwrap_or("").to_string();
        let st = stats.by_borough.entry(if borough.is_empty() { "?".into() } else { borough.clone() }).or_default();
        st.sold += 1;
        let building = building_key(l).and_then(|k| buildings.get(&k));

        let se_type = building.and_then(|(types, _)| {
            let v: Vec<(&str, f64)> = types.iter().map(|t| (t.as_str(), 0.0)).collect();
            majority(&v, false).map(str::to_string)
        });
        let zillow_type = text(l, "homeType").unwrap_or("other").to_string();
        let home_type = match se_type {
            Some(t) => {
                st.type_from_streeteasy += 1;
                *stats.zillow_vs_streeteasy.entry((zillow_type, t.clone())).or_default() += 1;
                t
            }
            None if text(l, "homeType") == Some("coop") => {
                st.type_from_zillow_coop += 1;
                "coop".to_string()
            }
            None => {
                st.type_uncertain += 1;
                "other".to_string()
            }
        };

        let from_building = building.and_then(|(_, n)| {
            let v: Vec<(&str, f64)> = n.iter().map(|t| (t.as_str(), 0.0)).collect();
            majority(&v, true).map(str::to_string)
        });
        let nbhd = match from_building {
            Some(n) => {
                st.nbhd_from_building += 1;
                Some(n)
            }
            None => match (num(l, "lat"), num(l, "lon")) {
                (Some(lat), Some(lon)) => grid.vote(lat, lon, &borough).map(str::to_string),
                _ => None,
            }
            .inspect(|_| st.nbhd_from_neighbours += 1),
        };
        if nbhd.is_none() {
            st.nbhd_none += 1;
        }
        if nbhd.is_some() && home_type != "other" {
            st.usable += 1;
        }
        if let Value::Object(m) = l {
            m.insert("homeType".into(), Value::from(home_type));
            m.insert("neighborhood".into(), nbhd.map_or(Value::Null, Value::from));
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn addresses_normalise_to_one_building() {
        let same = [
            ("1408 Avenue O APT 3B", "1408 Ave O"),
            ("150 W 51st St APT 2116", "150 West 51 Street"),
            ("235 East 21st Street", "235 E 21st St #4C"),
            ("166-25 Powells Cove Boulevard", "16625 Powells Cove Blvd"),
            ("1 Fifth Avenue", "1 5th Ave Unit 10"),
            ("50 Fort Pl #B3-b/a", "50 Fort Place"),
            ("10 W. End Ave., Apt. 4", "10 West End Avenue"),
            ("55 AUSTIN PL APT 7K", "55 Austin Place"),
        ];
        for (a, b) in same {
            assert_eq!(normalize_street(a), normalize_street(b), "{a} / {b}");
        }
        assert_eq!(normalize_street("1408 Avenue O APT 3B"), "1408 ave o");
        assert_eq!(normalize_street("235 East 21st Street"), "235 e 21 st");
        for (a, b) in [("150 W 51st St", "150 E 51st St"), ("1408 Avenue O", "1408 Avenue P"), ("12 Main St", "21 Main St")] {
            assert_ne!(normalize_street(a), normalize_street(b), "{a} / {b}");
        }
        assert_eq!(normalize_street(""), "");
        assert_eq!(street_key("723 Mac Donough St"), street_key("723 Macdonough Street"));
        assert_eq!(street_key("91 Van Cortlandt Ave W"), street_key("91 Vancortlandt Avenue West"));
        assert_eq!(street_key("2934 Brighton 4 St"), street_key("2934 Brighton Fourth Street"));
    }

    #[test]
    fn haversine_is_in_metres() {
        // One thousandth of a degree of latitude is ~111 m.
        let d = haversine_m(40.7, -73.9, 40.701, -73.9);
        assert!((d - 111.2).abs() < 0.5, "{d}");
    }

    fn se(id: &str, addr: &str, t: &str, nbhd: &str, lat: f64, lon: f64) -> Value {
        json!({"id": id, "market": "nyc", "status": "active", "address": addr, "homeType": t,
               "neighborhood": nbhd, "borough": "brooklyn", "lat": lat, "lon": lon})
    }

    fn sold(id: &str, addr: &str, t: &str, lat: f64, lon: f64) -> Value {
        json!({"id": id, "market": "nyc", "status": "sold", "address": addr, "homeType": t,
               "borough": "brooklyn", "lat": lat, "lon": lon, "soldAt": "2026-09-01T00:00:00Z"})
    }

    /// ~111 m north per 0.001.
    const LAT: f64 = 40.6;
    const LON: f64 = -73.95;

    #[test]
    fn building_type_comes_from_streeteasy_at_the_same_address() {
        let mut rows = vec![
            se("se:1", "1408 Avenue O", "coop", "Midwood", LAT, LON),
            se("se:2", "1408 Avenue O", "coop", "Midwood", LAT, LON),
            sold("zl:1", "1408 Ave O", "condo", LAT, LON),
            sold("zl:2", "99 Elsewhere St", "condo", LAT, LON),
            sold("zl:3", "98 Elsewhere St", "coop", LAT, LON),
        ];
        let st = enrich(&mut rows);
        assert_eq!(rows[2]["homeType"], "coop", "StreetEasy wins over Zillow's CONDO");
        assert_eq!(rows[2]["neighborhood"], "Midwood");
        assert_eq!(rows[3]["homeType"], "other", "an unmatched Zillow CONDO is uncertain");
        assert_eq!(rows[4]["homeType"], "coop", "an explicit Zillow COOPERATIVE is kept");
        let b = &st.by_borough["brooklyn"];
        assert_eq!((b.sold, b.type_from_streeteasy, b.type_uncertain, b.type_from_zillow_coop), (3, 1, 1, 1));
        assert_eq!(st.zillow_vs_streeteasy.get(&("condo".to_string(), "coop".to_string())), Some(&1));
        assert_eq!((b.nbhd_from_building, b.nbhd_from_neighbours, b.usable), (1, 2, 2));
        assert_eq!(rows[0]["homeType"], "coop", "StreetEasy rows are untouched");

        // Another borough's building at the same street address is another building.
        let mut rows = vec![se("se:1", "1408 Avenue O", "coop", "Midwood", LAT, LON), sold("zl:1", "1408 Ave O", "condo", LAT, LON)];
        rows[1]["borough"] = json!("queens");
        enrich(&mut rows);
        assert_eq!(rows[1]["homeType"], "other");
        assert!(rows[1]["neighborhood"].is_null(), "and the StreetEasy row is in another borough");

        // A building StreetEasy lists as condo and co-op in equal numbers: no answer.
        let mut rows = vec![
            se("se:1", "1 Main St", "coop", "Midwood", LAT, LON),
            se("se:2", "1 Main St", "condo", "Midwood", LAT, LON),
            sold("zl:1", "1 Main St APT 2", "condo", LAT, LON),
        ];
        enrich(&mut rows);
        assert_eq!(rows[2]["homeType"], "other");
    }

    #[test]
    fn neighbourhood_is_the_majority_of_the_nearest_within_600_m() {
        let mut rows = vec![
            // 3 Midwood rows ~110-330 m north, 2 Homecrest rows ~55 m south.
            se("se:1", "1 A St", "condo", "Midwood", LAT + 0.001, LON),
            se("se:2", "2 A St", "condo", "Midwood", LAT + 0.002, LON),
            se("se:3", "3 A St", "condo", "Midwood", LAT + 0.003, LON),
            se("se:4", "4 A St", "condo", "Homecrest", LAT - 0.0005, LON),
            se("se:5", "5 A St", "condo", "Homecrest", LAT - 0.0005, LON),
            // Far away (2.2 km) but many: not neighbours.
            se("se:6", "6 A St", "condo", "Far", LAT + 0.02, LON),
            se("se:7", "7 A St", "condo", "Far", LAT + 0.02, LON),
            se("se:8", "8 A St", "condo", "Far", LAT + 0.02, LON),
            sold("zl:1", "9 B St", "coop", LAT, LON),
        ];
        enrich(&mut rows);
        assert_eq!(rows[8]["neighborhood"], "Midwood", "3 of the 5 nearest");

        // Only one row in range, 500 m away: it decides. 700 m: none.
        let mut rows = vec![se("se:1", "1 A St", "condo", "Midwood", LAT + 0.0045, LON), sold("zl:1", "9 B St", "coop", LAT, LON)];
        enrich(&mut rows);
        assert_eq!(rows[1]["neighborhood"], "Midwood");
        let mut rows = vec![se("se:1", "1 A St", "condo", "Midwood", LAT + 0.0063, LON), sold("zl:1", "9 B St", "coop", LAT, LON)];
        let st = enrich(&mut rows);
        assert!(rows[1]["neighborhood"].is_null());
        assert_eq!((st.by_borough["brooklyn"].nbhd_none, st.by_borough["brooklyn"].usable), (1, 0));

        // A tie (2 and 2 with a fifth elsewhere): the nearest row's neighbourhood.
        let mut rows = vec![
            se("se:1", "1 A St", "condo", "Midwood", LAT + 0.002, LON),
            se("se:2", "2 A St", "condo", "Midwood", LAT + 0.0021, LON),
            se("se:3", "3 A St", "condo", "Homecrest", LAT - 0.001, LON),
            se("se:4", "4 A St", "condo", "Homecrest", LAT - 0.0022, LON),
            sold("zl:1", "9 B St", "coop", LAT, LON),
        ];
        enrich(&mut rows);
        assert_eq!(rows[4]["neighborhood"], "Homecrest");

        // Other boroughs do not vote; rows already placed are left alone.
        let mut rows = vec![se("se:1", "1 A St", "condo", "Midwood", LAT, LON), sold("zl:1", "9 B St", "coop", LAT, LON)];
        rows[0]["borough"] = json!("queens");
        enrich(&mut rows);
        assert!(rows[1]["neighborhood"].is_null());
        let mut placed = vec![se("se:1", "1 A St", "condo", "Midwood", LAT, LON), sold("zl:1", "9 B St", "condo", LAT, LON)];
        placed[1]["neighborhood"] = json!("Kept");
        assert_eq!(enrich(&mut placed).total().sold, 0);
        assert_eq!((placed[1]["neighborhood"].as_str(), placed[1]["homeType"].as_str()), (Some("Kept"), Some("condo")));
    }

    #[test]
    fn the_grid_finds_neighbours_across_cell_edges() {
        // Rows 590 m east and west, 590 m north and south: all in range.
        let dlon = 590.0 / (111_320.0 * LAT.to_radians().cos());
        let dlat = 590.0 / 111_320.0;
        let mut rows = vec![
            se("se:1", "1 A St", "condo", "X", LAT, LON + dlon),
            se("se:2", "2 A St", "condo", "X", LAT, LON - dlon),
            se("se:3", "3 A St", "condo", "X", LAT + dlat, LON),
            se("se:4", "4 A St", "condo", "X", LAT - dlat, LON),
            sold("zl:1", "9 B St", "coop", LAT, LON),
        ];
        enrich(&mut rows);
        assert_eq!(rows[4]["neighborhood"], "X");
    }
}
