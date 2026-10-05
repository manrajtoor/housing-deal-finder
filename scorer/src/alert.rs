//! The alert rule (DESIGN.md): at least 15% under the baseline, enough comps
//! for the market, not a thin (fallback) group, at most $900k, an active
//! listing the crawler did not mark comp-only, and in Michigan only
//! `great_lakes` or `inland` frontage: unread (`other`), river/pond and
//! shared `access` listings are scored but never alert.
//! The 40% plausibility cap is enforced by the scorer itself (it refuses).

use crate::listing::{Facts, Market};
use crate::score::Score;

#[derive(Debug, Clone, PartialEq)]
pub struct AlertRule {
    pub min_discount_pct: f64,
    pub max_price: f64,
    pub min_comps_nyc: usize,
    pub min_comps_mi: usize,
}

impl Default for AlertRule {
    fn default() -> Self {
        AlertRule { min_discount_pct: 15.0, max_price: 900_000.0, min_comps_nyc: 8, min_comps_mi: 6 }
    }
}

impl AlertRule {
    pub fn min_comps(&self, m: Market) -> usize {
        match m {
            Market::Nyc => self.min_comps_nyc,
            Market::Mi => self.min_comps_mi,
        }
    }

    pub fn accepts(&self, f: &Facts, s: &Score) -> bool {
        s.discount_pct >= self.min_discount_pct
            && s.n >= self.min_comps(f.market)
            && !s.thin
            && f.price <= self.max_price
            && f.active
            && !f.removed
            && !f.comp_only
            && (f.market != Market::Mi || f.water == "great_lakes" || f.water == "inland")
    }
}
