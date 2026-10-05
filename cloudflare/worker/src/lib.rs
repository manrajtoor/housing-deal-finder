//! housedeals-api Cloudflare Worker (workers-rs).
//!
//! Module map (one job each):
//! - [`sql`]      database-neutral statements (`Stmt`, `Param`)
//! - [`auth`]     bearer-token check, where reads are served
//! - [`ingest`]   validation and the upsert planner (listings, detail pages)
//! - [`scores`]   score input for the crawl job, its score/alert writes, and
//!                the deals/alerts/stats/needs-detail/expiry SQL
//! - [`alerts`]   the alert rule from the vars, the `Notifier` seam
//! - [`dispatch`] starting the GitHub Actions crawl from the crons
//! - [`service`]  use cases over the `Store` port
//! - `d1`         D1 implementation of `Store` (wasm only)
//! - `entry`      fetch + scheduled handlers, the composition root (wasm only)
//!
//! Everything except `d1` and `entry` is plain Rust, so `cargo test` runs it natively.
//!
//! The Worker never scores: the crawl job runs `housedeals-score` (scorer/)
//! over `GET /api/score-input` and posts the result to `POST /api/scores`.

pub mod alerts;
pub mod auth;
pub mod dispatch;
pub mod ingest;
pub mod scores;
pub mod service;
pub mod sql;

#[cfg(target_arch = "wasm32")]
mod d1;
#[cfg(target_arch = "wasm32")]
mod entry;
