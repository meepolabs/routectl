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

pub use cost::{estimate_cost, estimate_cost_tokens, CostBreakdown, Rates};
pub use db::{open, open_readonly, OpenError, UsageDb};
pub use handle::{UsageCounters, UsageHandle};
pub use migrate::MigrateError;
pub use query::{aggregate, latest_quota, ttfbs, AggRow, GroupKey, QueryError, QuotaSnapshot};
pub use record::{Outcome, ParseOutcomeError, UsageRecord};
pub use retention::{prune, PruneOutcome};
pub use schema::{META_CREATED_AT_MS, META_SCHEMA_VERSION, SCHEMA_VERSION};
pub use writer::{UsageWriter, CHANNEL_CAPACITY};
