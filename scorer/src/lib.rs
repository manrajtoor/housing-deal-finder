//! housedeals scorer: comp-based baselines for homes for sale (DESIGN.md).
//!
//! Pure Rust with no wasm or Workers dependencies. The crawl job runs it as
//! the `housedeals-score` binary; `cargo test` runs it natively.
//!
//! Module map:
//! - [`listing`] the facts read from a contract listing, and the Deal fields
//! - [`group`]   comp groups in fallback order, and their labels
//! - [`score`]   the `Scorer`: medians per group, refusals, the Deal JSON
//! - [`alert`]   the alert rule
//! - [`stats`]   medians and percentiles (with one element left out)
//! - [`dates`]   ISO date arithmetic
//! - [`nyc_sold`] building type and neighbourhood of NYC sold rows (Zillow), from StreetEasy rows
//! - [`batch`]   one market's scoring pass for the crawl job (`housedeals-score`)

pub mod alert;
pub mod batch;
pub mod dates;
pub mod group;
pub mod listing;
pub mod nyc_sold;
pub mod score;
pub mod stats;

pub use alert::AlertRule;
pub use score::{deal_json, Basis, Options, Refusal, Score, Scorer};
