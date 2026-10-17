//! The served doctor gather's inputs: what a running daemon hands the doctor in
//! place of a disk load.
//!
//! A served gather reads no catalog overlay file, parses no config file, and
//! replays no ledger. The config and overlay are the generation the daemon
//! accepted, the learned layer is the daemon's resident registry, and the boot
//! warm's report says how that registry was filled.

use std::sync::Arc;
use std::time::Instant;

use routectl_core::UpstreamReason;
use routectl_router::{
    CatalogOverlay, Config, Finding, LearnedRegistryEntry, MatrixWarm, SeedClearMarker, Status,
};

use crate::commands::seat_report::safe;
use crate::server::capability_rebuild::{WarmOutcome, WarmReport};
use crate::server::reload_failure::ReloadFailureSnapshot;

use super::gather::replay_summary;
use super::{CapabilityMatrixSource, MatrixOrigin};

/// Where the no-network gather takes its config, overlay, and learned layer
/// from.
pub enum GatherSources {
    /// Load config and overlay from disk and replay the usage ledger
    /// read-only (the CLI `doctor`).
    Disk,
    /// A running daemon's in-memory state (`/status/doctor`).
    Served(Box<ServedInputs>),
}

/// The daemon's accepted state, owned so the gather can run on a blocking
/// worker. The fields are private to the doctor module: a holder can only pass
/// the value to the gather, never read the config out of it.
pub struct ServedInputs {
    config: Arc<Config>,
    overlay: CatalogOverlay,
    learned: ResidentLearned,
    warm: WarmReport,
    reload_failure: Option<ReloadFailureSnapshot>,
    parked_seats: Vec<ParkedSeat>,
}

/// One dispatch seat whose credential the running daemon has parked: the
/// seat's runtime key, the reason the upstream gave, and the epoch-ms the park
/// lapses into a re-test. Only a running daemon holds parks, so only a served
/// gather carries any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParkedSeat {
    /// The seat's runtime state key (`nickname[#member]`).
    pub state_key: String,
    /// Why the upstream rejected the credential.
    pub reason: UpstreamReason,
    /// Epoch-ms the park lapses.
    pub parked_until_ms: i64,
}

/// The resident learned registry, read once, with the clock anchors the read
/// was taken against.
pub struct ResidentLearned {
    /// Every resident entry under the kind the served config gives its lane.
    pub entries: Vec<LearnedRegistryEntry>,
    /// Every resident seed-clear marker.
    pub seed_clears: Vec<SeedClearMarker>,
    /// The `Instant` anchor, taken after the entries were read.
    pub now: Instant,
    /// The epoch-millisecond anchor paired with `now`.
    pub now_ms: i64,
}

impl ServedInputs {
    /// `config` and `overlay` must come from the same router, so one gather
    /// never pairs a config generation with a foreign overlay generation.
    pub const fn new(
        config: Arc<Config>,
        overlay: CatalogOverlay,
        learned: ResidentLearned,
        warm: WarmReport,
    ) -> Self {
        Self {
            config,
            overlay,
            learned,
            warm,
            reload_failure: None,
            parked_seats: Vec::new(),
        }
    }

    /// Attach the seats whose credential the daemon has parked.
    #[must_use]
    pub fn with_parked_seats(self, parked_seats: Vec<ParkedSeat>) -> Self {
        Self {
            parked_seats,
            ..self
        }
    }

    /// Attach the daemon's last rejected reload, if one is still current.
    #[must_use]
    pub fn with_reload_failure(self, reload_failure: Option<ReloadFailureSnapshot>) -> Self {
        Self {
            reload_failure,
            ..self
        }
    }
}

/// One parked seat as an `auth` Warn: the seat's state key, the reason token,
/// and the deadline as RFC3339 UTC. The state key carries an operator-written
/// member label, so it routes through the log-safe helper.
pub(super) fn parked_seat_finding(seat: &ParkedSeat) -> Finding {
    let deadline = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(seat.parked_until_ms)
        .map_or_else(
            || format!("epoch ms {}", seat.parked_until_ms),
            |at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );
    Finding {
        section: "auth",
        name: format!("parked seat {}", safe(&seat.state_key)),
        status: Status::Warn,
        detail: format!(
            "credential parked ({}) until {deadline}; requests skip this seat until then",
            seat.reason.as_str(),
        ),
        remediation: Some(
            "renew or replace the credential; a credentials or config reload lifts every park, \
             otherwise the seat is re-tested once the park lapses"
                .to_string(),
        ),
    }
}

/// The per-layer inputs the shared gather body draws from, whichever source
/// produced them.
pub(super) struct GatheredLayers {
    pub(super) config: Config,
    /// The already-redacted config parse error.
    pub(super) config_parse_error: Option<String>,
    /// The already-redacted config-or-overlay load error.
    pub(super) config_load_error: Option<String>,
    /// The schema version of a config the daemon already accepted; `None`
    /// when the config came from disk and its version must be read off the
    /// raw file.
    pub(super) accepted_config_version: Option<u32>,
    pub(super) overlay: Option<CatalogOverlay>,
    pub(super) capability_matrix: CapabilityMatrixSource,
    pub(super) matrix_origin: MatrixOrigin,
    /// The daemon's last rejected reload; always `None` from disk.
    pub(super) reload_failure: Option<ReloadFailureSnapshot>,
    /// The daemon's credential-parked seats; always empty from disk, which
    /// cannot see daemon state.
    pub(super) parked_seats: Vec<ParkedSeat>,
}

/// The served layers. The daemon accepted this config and overlay, so neither
/// carries a load error.
pub(super) fn served_layers(inputs: Box<ServedInputs>) -> GatheredLayers {
    let ServedInputs {
        config,
        overlay,
        learned,
        warm,
        reload_failure,
        parked_seats,
    } = *inputs;
    GatheredLayers {
        accepted_config_version: Some(config.version),
        config: Arc::unwrap_or_clone(config),
        config_parse_error: None,
        config_load_error: None,
        overlay: Some(overlay),
        capability_matrix: resident_matrix(learned, &warm),
        matrix_origin: MatrixOrigin::Resident(matrix_warm(&warm)),
        reload_failure,
        parked_seats,
    }
}

/// Classify the resident registry. An empty registry after a warm that failed
/// to read the ledger is the warm failure, never an honest empty: the registry
/// is empty because nothing could be read, not because nothing was learned.
///
/// The warm runs once, at daemon boot, so an `Unavailable` here reflects the
/// BOOT warm for the whole process lifetime: a ledger that became readable
/// later does not clear it until a live entry lands or the daemon restarts.
pub(super) fn resident_matrix(
    learned: ResidentLearned,
    warm: &WarmReport,
) -> CapabilityMatrixSource {
    let ResidentLearned {
        entries,
        seed_clears,
        now,
        now_ms,
    } = learned;
    if !entries.is_empty() {
        return CapabilityMatrixSource::Available {
            entries,
            now,
            now_ms,
            replay: None,
            seed_clears,
        };
    }
    match warm.outcome {
        outcome @ (WarmOutcome::Unreadable(_) | WarmOutcome::RestateFailed) => {
            CapabilityMatrixSource::Unavailable(outcome.as_str())
        }
        WarmOutcome::NotRun
        | WarmOutcome::Replayed
        | WarmOutcome::Restated
        | WarmOutcome::Cold
        | WarmOutcome::NoTombstone => CapabilityMatrixSource::Empty {
            replay: None,
            seed_clears,
        },
    }
}

/// The boot warm as the matrix panel renders it. An `unreadable` warm carries
/// its failure class alongside the coarse outcome token.
fn matrix_warm(warm: &WarmReport) -> MatrixWarm {
    let class = match warm.outcome {
        WarmOutcome::Unreadable(class) => Some(class.to_string()),
        WarmOutcome::NotRun
        | WarmOutcome::Replayed
        | WarmOutcome::Restated
        | WarmOutcome::RestateFailed
        | WarmOutcome::Cold
        | WarmOutcome::NoTombstone => None,
    };
    MatrixWarm {
        outcome: warm.outcome.as_str().to_string(),
        class,
        summary: warm
            .summary
            .as_ref()
            .map(|summary| replay_summary(summary, warm.loaded_rows)),
    }
}

#[cfg(test)]
#[path = "served_tests.rs"]
mod tests;
