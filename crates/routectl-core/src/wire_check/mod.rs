//! Contract types and the diff engine for comparing what a client sent with
//! what routectl sent upstream on the same attempt.
//!
//! Everything here is content-free by construction: a [`WirePath`] carries
//! only sanitized key names and indices, the closed enums label findings,
//! and the carriage types that do hold body bytes never render them through
//! `Debug`. Nothing in this module performs I/O.

#![deny(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

mod carriage;
mod diff;
mod edits;
mod lane;
mod path;

pub use carriage::{AttemptRecord, IngressWire, TapContents, WireTap};
pub use diff::{DiffOutcome, Finding, MAX_DIFF_DEPTH, MAX_DIFF_NODES, diff};
pub use edits::{
    DeclaredEdit, EditKind, EditPatternError, EditSite, PathClass, RejectionProvenance,
};
pub use lane::{EgressKind, IngressDialect, Lane, LaneClass, Leg};
pub use path::{MAX_WIRE_PATH_DEPTH, MAX_WIRE_PATH_LEN, WILDCARD_SEGMENT, WirePath};
