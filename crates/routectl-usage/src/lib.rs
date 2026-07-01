//! Usage-accounting persistence for routectl.
//!
//! This crate owns the canonical `UsageRecord` shape, the closed
//! `Outcome` enum, the SQLite persistence layer (schema DDL,
//! migrate-on-open, connection open), and the off-hot-path bounded
//! writer subsystem. Request handlers hold a `Clone` `UsageHandle` and
//! call `try_send`, which never blocks; a dedicated OS thread owns the
//! blocking connection and performs the INSERTs.

mod cost;
mod db;
mod handle;
mod migrate;
mod query;
mod record;
mod retention;
mod schema;
mod writer;

pub use cost::{CostBreakdown, Rates, estimate_cost, estimate_cost_tokens};
pub use db::{OpenError, UsageDb, open, open_readonly};
pub use handle::{UsageCounters, UsageHandle};
pub use migrate::MigrateError;
pub use query::{
    AggRow, GroupKey, KCalibration, QueryError, QuotaSnapshot, ReuseSampleRow,
    ShadowMisfireSummary, WouldTrimSummary, aggregate, k_calibration_summary, latest_quota,
    read_reuse_samples_since, shadow_misfire_summary, ttfbs, would_trim_summary,
};
pub use record::{Outcome, ParseOutcomeError, UsageRecord};
pub use retention::{PruneOutcome, prune};
pub use schema::{META_CREATED_AT_MS, META_SCHEMA_VERSION, SCHEMA_VERSION};
pub use writer::{CHANNEL_CAPACITY, UsageWriter};
