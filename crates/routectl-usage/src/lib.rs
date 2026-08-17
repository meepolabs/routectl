#![deny(rustdoc::broken_intra_doc_links)]
#![warn(missing_docs)]
//! Usage-accounting persistence for routectl.
//!
//! This crate owns the canonical `UsageRecord` shape, the closed
//! `Outcome` enum, the SQLite persistence layer (schema DDL,
//! migrate-on-open, connection open), and the off-hot-path bounded
//! writer subsystem. Request handlers hold a `Clone` `UsageHandle` and
//! call `try_send`, which never blocks; a dedicated OS thread owns the
//! blocking connection and performs the INSERTs.

mod capability_event;
mod cost;
mod db;
mod handle;
mod learn_event;
mod migrate;
mod query;
mod record;
mod retention;
mod schema;
mod writer;

pub use capability_event::{CapabilityEvent, insert_capability_event};
pub use cost::{CostBreakdown, Rates, estimate_cost, estimate_cost_tokens};
pub use db::{OpenError, UsageDb, open, open_readonly, open_readonly_fastfail, open_rw};
pub use handle::{UsageCounters, UsageHandle};
pub use learn_event::CapabilityLearnEvent;
pub use migrate::MigrateError;
pub use query::{
    AggRow, BucketSpec, CacheDecisionSummary, CalibrationSampleRow, CapabilityEventRow, CostStatus,
    DeadlineGuard, GroupDim, GroupKey, KCalibration, NearLosslessAttributionSummary, QueryError,
    QueryGroup, QueryMetrics, QueryResult, QuerySeries, QuerySpec, QueryTotals, QuotaSnapshot,
    ReductionSummary, ReuseSampleRow, RowCost, SeriesBucket, ShadowMisfireSummary, TombstoneRow,
    WouldTrimSummary, aggregate, cache_decision_summary, earliest_ts_start, errors_by_class,
    k_calibration_summary, latest_quota_by_seat, latest_tombstone,
    near_lossless_attribution_summary, query, read_calibration_samples_since,
    read_capability_events_after, read_reuse_samples_since, reduction_summary,
    shadow_misfire_summary, ttfbs, would_trim_summary,
};
pub use record::{
    Outcome, PREFIX_EPOCH_RESEEDED, PREFIX_EPOCH_REWRITTEN, PREFIX_EPOCH_STABLE, ParseOutcomeError,
    UsageRecord,
};
pub use schema::SCHEMA_VERSION;
pub use writer::{CHANNEL_CAPACITY, UsageWriter};
