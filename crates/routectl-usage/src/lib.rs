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

mod capability_ack;
mod capability_batch;
mod capability_event;
mod cost;
mod db;
mod handle;
mod learn_event;
mod migrate;
mod paid_probe;
mod paid_probe_command;
mod paid_probe_lifecycle;
mod paid_probe_read;
/// Cross-crate paid-probe test fixtures. Compiled only under `cfg(test)` or the
/// `test-utils` feature, so the seams never ship in a release build -- one of
/// them can lower a committed paid-probe count, and a committed unit is never
/// given back.
#[cfg(any(test, feature = "test-utils"))]
mod paid_probe_test_support;
mod query;
mod record;
mod retention;
mod schema;
mod writer;

// The acknowledged single capability-event write. `CapabilityEventReceipt` is
// public because `admit_acknowledged_capability_event` RETURNS it and the caller
// awaits it; `AcknowledgedCapabilityEvent` is deliberately NOT re-exported -- it is
// the writer message's payload, constructed only inside the admission, and
// publishing it would invite a downstream producer to build one and bypass the
// admission's enabled gate and accounting.
pub use capability_ack::CapabilityEventReceipt;
pub use capability_batch::{
    BatchCommit, BatchReceipt, CapabilityBatch, handle_over_channel, handle_over_channel_with,
    handle_with_closed_channel,
};
// The writer's message type, for the test seams above only: a caller supplying
// its own channel to `handle_over_channel` has to name what flows over it.
// `#[doc(hidden)]` because the message vocabulary is this crate's internal
// sequencing, not a surface for a downstream producer to construct.
pub use capability_event::{
    CapabilityEvent, insert_capability_event, insert_capability_events_atomic,
};
pub use cost::{CostBreakdown, Rates, estimate_cost, estimate_cost_tokens};
pub use db::{OpenError, UsageDb, open, open_readonly, open_readonly_fastfail, open_rw};
pub use handle::{UsageCounters, UsageHandle};
pub use learn_event::CapabilityLearnEvent;
pub use migrate::MigrateError;
pub use paid_probe_command::{PaidProbeAdmission, PaidProbeCommit, PaidProbeReceipt};
pub use paid_probe_read::{PaidProbeUsage, committed_units};
#[cfg(any(test, feature = "test-utils"))]
pub use paid_probe_test_support::{
    plant_paid_probe_units_for_tests, reserve_paid_probe_unit_for_tests,
};
pub use query::{
    AggRow, BucketSpec, CacheDecisionSummary, CalibrationSampleRow, CapabilityEventRow, CostStatus,
    DeadlineGuard, GroupDim, GroupKey, KCalibration, NearLosslessAttributionSummary, QueryError,
    QueryGroup, QueryMetrics, QueryResult, QuerySeries, QuerySpec, QueryTotals, QuotaSnapshot,
    ReductionSummary, ReuseSampleRow, RowCost, SUPPRESSED_SESSION_CAP, SeriesBucket,
    ShadowMisfireSummary, SuppressedSessionRow, SuppressedSessions, TombstoneRow, WouldTrimSummary,
    aggregate, cache_decision_summary, earliest_ts_start, errors_by_class, k_calibration_summary,
    latest_quota_by_seat, latest_tombstone, near_lossless_attribution_summary, query,
    read_calibration_samples_since, read_capability_events_after, read_reuse_samples_since,
    reduction_summary, shadow_misfire_summary, suppressed_sessions, ttfbs, would_trim_summary,
};
pub use record::{
    Outcome, PREFIX_EPOCH_RESEEDED, PREFIX_EPOCH_REWRITTEN, PREFIX_EPOCH_STABLE, ParseOutcomeError,
    UsageRecord,
};
pub use schema::SCHEMA_VERSION;
pub use writer::CapabilityEventWrite;
#[doc(hidden)]
pub use writer::WriterMessage;
pub use writer::{CHANNEL_CAPACITY, UsageWriter};
