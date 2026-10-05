//! housedeals scorer: comp-based baselines for homes for sale (DESIGN.md).
//!
//! Pure Rust with no wasm or Workers dependencies; the Worker links it and
//! `cargo test` runs it natively.
//!
//! Module map:
//! - [`listing`] the facts read from a contract listing, and the Deal fields
//! - [`group`]   comp groups in fallback order, and their labels
//! - [`score`]   the `Scorer`: medians per group, refusals, the Deal JSON
//! - [`alert`]   the alert rule
//! - [`stats`]   medians and percentiles (with one element left out)
//! - [`dates`]   ISO date arithmetic

pub mod alert;
pub mod dates;
pub mod group;
pub mod listing;
pub mod score;
pub mod stats;

pub use alert::AlertRule;
pub use score::{deal_json, Basis, Options, Refusal, Score, Scorer};
