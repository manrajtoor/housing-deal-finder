//! Alerts: the crawl job (`housedeals-score`) decides which fresh listings
//! (new and young, relisted, or cheaper) pass the alert rule and posts them
//! to `POST /api/scores`, which stores each once in `deal_alerts`, keyed by
//! (listing id, asking price), and hands the new ones to a [`Notifier`].
//! There is no notifier yet (dashboard only); the trait is the seam for one.
//!
//! The Worker reads the rule only for NYC needs-detail (`ALERT_MAX_PRICE`).

use serde_json::Value;

use scorer::AlertRule;

/// Delivers new alerts somewhere (e-mail, Telegram, ...). Returns how many
/// it delivered; delivered alerts get `notified_at`.
#[allow(async_fn_in_trait)] // single-threaded wasm: no Send bound needed
pub trait Notifier {
    async fn notify(&self, deals: &[Value]) -> Result<usize, String>;
}

/// Alerts stay on the dashboard.
pub struct NoNotifier;

impl Notifier for NoNotifier {
    async fn notify(&self, _deals: &[Value]) -> Result<usize, String> {
        Ok(0)
    }
}

/// The rule from the Worker vars; a bad value is reported and the default kept.
pub fn rule_from_vars(get: impl Fn(&str) -> Option<String>) -> (AlertRule, Vec<String>) {
    let mut r = AlertRule::default();
    let mut errors = Vec::new();
    let mut num = |k: &str, min: f64, max: f64| -> Option<f64> {
        let v = get(k)?.trim().to_string();
        if v.is_empty() {
            return None;
        }
        match v.parse::<f64>().ok().filter(|n| n.is_finite() && (min..=max).contains(n)) {
            Some(n) => Some(n),
            None => {
                errors.push(format!("{k}: {v:?} is not a number in {min}..{max}"));
                None
            }
        }
    };
    if let Some(n) = num("ALERT_MIN_DISCOUNT_PCT", 0.1, 99.0) {
        r.min_discount_pct = n;
    }
    if let Some(n) = num("ALERT_MAX_PRICE", 1.0, 1e9) {
        r.max_price = n;
    }
    if let Some(n) = num("ALERT_MIN_COMPS_NYC", 1.0, 1000.0) {
        r.min_comps_nyc = n as usize;
    }
    if let Some(n) = num("ALERT_MIN_COMPS_MI", 1.0, 1000.0) {
        r.min_comps_mi = n as usize;
    }
    (r, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_reads_the_vars_and_reports_bad_ones() {
        let (r, e) = rule_from_vars(|k| match k {
            "ALERT_MAX_PRICE" => Some("900000".into()),
            "ALERT_MIN_DISCOUNT_PCT" => Some("12".into()),
            "ALERT_MIN_COMPS_NYC" => Some("ten".into()),
            "ALERT_MIN_COMPS_MI" => Some("5".into()),
            _ => None,
        });
        assert_eq!((r.max_price, r.min_discount_pct, r.min_comps_nyc, r.min_comps_mi), (900000.0, 12.0, 8, 5));
        assert_eq!(e.len(), 1);
    }
}
