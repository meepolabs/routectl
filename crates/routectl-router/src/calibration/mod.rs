//! Learned per-lane correction for the router's token estimate.
//!
//! The router estimates a request's token count as serialized bytes over
//! four, so the estimate is wrong per model and wrong differently per model.
//! Every dispatched attempt now persists the estimate alongside the
//! upstream's own cache-inclusive prompt total, which makes the per-lane
//! error measurable: this module reduces those pairs into a single
//! multiplicative correction and hands it to the one decision site that
//! consumes it, the proactive context-window gate.
//!
//! Three properties are load-bearing:
//!
//! - **A lane is `(provider_kind, served nickname)`.** The nickname is the
//!   operator-facing label, never the upstream wire id -- pricing keys on the
//!   wire id, this does not. A target missing either half forms NO lane, so
//!   no fallback label can make the live write and a later ledger-driven
//!   rebuild disagree about which lane a sample belongs to.
//! - **The whole state machine is `Option<Factor>`.** Cold, thin, stale,
//!   out-of-range and switched-off all collapse to `None`, which means the
//!   gate skips the multiply and behaves exactly as it does without this
//!   module. One fallback path, so there is one place for it to be wrong.
//! - **Out-of-range is REFUSED, not clamped.** A reduced ratio outside the
//!   sane band is evidence the lane is mis-keyed or fed garbage, not evidence
//!   of a real extreme correction. Clamping to the bound would let a
//!   mis-keyed lane still move a routing decision; refusing sends it back to
//!   the uncorrected estimate.
//!
//! Evidence survives a restart: [`rebuild`] replays the persisted pairs back
//! through the same store write at boot, and a hot reload carries the live
//! store over instead of re-reading history.

pub mod factor;
pub mod rebuild;
pub mod store;

pub use factor::Factor;
pub use rebuild::{
    CalibrationLedgerReader, CalibrationLedgerRow, CalibrationRebuildSummary, rebuild_into,
};
pub use store::{CalibrationStore, LaneKey, cohort_of};
