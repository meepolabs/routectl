//! The served doctor gather's inputs: what a running daemon hands the doctor in
//! place of a disk load.
//!
//! A served gather parses no config file and replays no capability ledger.
//! The config is the generation the daemon accepted, the learned layer is the
//! daemon's resident registry, and the boot warm's report says how that
//! registry was filled. The shared gather body still reads the usage ledger
//! read-only, on both sources: the all-time would-trim summary and the
//! cache-ignore window, the latter under [`ServedInputs`]' cache read budget.

use std::sync::Arc;
use std::time::{Duration, Instant};

use routectl_auth::SecretRef;
use routectl_auth::oauth::known_provider_ids;
use routectl_core::{EligibilityReason, UpstreamReason};
use routectl_router::runtime_state::UnusableCause;
use routectl_router::{Config, Finding, LearnedRegistryEntry, MatrixWarm, SeedClearMarker, Status};

use routectl_router::prices::PriceBook;

use crate::commands::pricing::PriceSnapshot;
use crate::commands::seat_report::safe;
use crate::server::capability_rebuild::{WarmOutcome, WarmReport};
use crate::server::reload_failure::ReloadFailureSnapshot;

use super::cache_section::SERVED_CACHE_READ_BUDGET;
use super::gather::{replay_summary, scheme_label};
use super::sections::{oauth_login_command, oauth_login_remediation};
use super::{CapabilityMatrixSource, MatrixOrigin};

/// Where the no-network gather takes its config and learned layer from.
pub enum GatherSources {
    /// Load config from disk and replay the usage ledger
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
    learned: ResidentLearned,
    warm: WarmReport,
    prices: ServedPrices,
    reload_failure: Option<ReloadFailureSnapshot>,
    parked_seats: Vec<ParkedSeat>,
    unusable_seats: Vec<UnusableSeat>,
    cache_read_budget: Duration,
}

/// The price tables a served gather prices against.
pub enum ServedPrices {
    /// The daemon's price book, read once when the gather runs, on the
    /// gather's blocking worker.
    Book(Arc<PriceBook>),
    /// Tables already pinned.
    #[cfg(test)]
    Pinned(PriceSnapshot),
}

impl ServedPrices {
    fn pin(self) -> PriceSnapshot {
        match self {
            Self::Book(book) => PriceSnapshot::pin(&book),
            #[cfg(test)]
            Self::Pinned(snapshot) => snapshot,
        }
    }
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

/// One dispatch seat whose credential the running daemon holds as unusable:
/// the seat's runtime key, the eligibility reason requests skip it for, the
/// cause, the epoch-ms the mark was first set, and the command that clears it.
/// The mark lives only in daemon memory, so only a served gather carries any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnusableSeat {
    /// The seat's runtime state key (`nickname[#member]`).
    pub state_key: String,
    /// Why requests skip the seat; always `credential_unusable`.
    pub reason: EligibilityReason,
    /// Why the credential is unusable.
    pub cause: UnusableCause,
    /// Epoch-ms the seat was first marked with this cause.
    pub since_ms: i64,
    /// What the operator runs or changes to readmit the seat.
    pub fix: String,
}

/// What readmits a seat dispatching through one `[providers]` entry: a
/// re-login for an `oauth://` seat whose provider this build can log in to, a
/// config correction for one whose provider it cannot, else a readable
/// credential for the entry. Derived from the served config each time it is
/// shown, never stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeatFix {
    /// Log the seat in again.
    Login {
        /// The `oauth://` ref's provider.
        provider: String,
        /// The ref's seat label, `None` for the default seat.
        label: Option<String>,
    },
    /// Correct an `oauth://` ref whose provider `routectl login` does not
    /// accept, then reload the config.
    UnknownProvider {
        /// The `[providers]` entry.
        entry: String,
        /// The ref's provider token.
        provider: String,
    },
    /// Provide a readable credential for the entry and reload the config.
    Credential {
        /// The `[providers]` entry.
        entry: String,
        /// The scheme of the entry's credential ref, `None` when it has none.
        scheme: Option<&'static str>,
    },
}

impl SeatFix {
    /// The fix for a seat on `[providers]` entry `entry` of `config`.
    pub fn of(config: &Config, entry: &str) -> Self {
        let uris = config
            .providers
            .get(entry)
            .map(routectl_router::ProviderEntry::secret_uris)
            .unwrap_or_default();
        let oauth = uris.iter().find_map(|uri| match SecretRef::parse(uri) {
            Ok(SecretRef::OAuth { provider, label }) => Some((provider, label)),
            _ => None,
        });
        match oauth {
            Some((provider, label)) => Self::for_oauth_ref(entry, provider, label),
            None => Self::Credential {
                entry: entry.to_string(),
                scheme: uris.first().map(|uri| scheme_label(uri)),
            },
        }
    }

    /// The fix for an `oauth://<provider>[#<label>]` ref on `[providers]`
    /// entry `entry`: a login only when `routectl login` accepts `provider`.
    pub fn for_oauth_ref(entry: &str, provider: String, label: Option<String>) -> Self {
        if known_provider_ids().contains(&provider.as_str()) {
            Self::Login { provider, label }
        } else {
            Self::UnknownProvider {
                entry: entry.to_string(),
                provider,
            }
        }
    }

    /// The bare line: the exact `routectl login` command for a login fix,
    /// else the correction sentence. Names the entry, scheme or provider
    /// token, never the ref's value.
    pub fn command(&self) -> String {
        match self {
            Self::Login { provider, label } => oauth_login_command(provider, label.as_deref()),
            Self::UnknownProvider { entry, provider } => unknown_provider_sentence(entry, provider),
            Self::Credential { entry, scheme } => credential_sentence(entry, *scheme),
        }
    }

    /// The line as a doctor remediation: the login command wrapped as an
    /// instruction to run it, else the credential sentence.
    pub fn remediation(&self) -> String {
        match self {
            Self::Login { provider, label } => {
                oauth_login_remediation(Some(provider), label.as_deref())
            }
            Self::UnknownProvider { entry, provider } => unknown_provider_sentence(entry, provider),
            Self::Credential { entry, scheme } => credential_sentence(entry, *scheme),
        }
    }
}

pub(super) fn unknown_provider_sentence(entry: &str, provider: &str) -> String {
    format!(
        "correct the unknown oauth provider `{}` of provider entry `{}`, then reload the config",
        safe(provider),
        safe(entry),
    )
}

fn credential_sentence(entry: &str, scheme: Option<&str>) -> String {
    let shown = safe(entry);
    match scheme {
        Some(scheme) => format!(
            "provide a readable {scheme} credential for provider entry `{shown}`, then reload the \
             config",
        ),
        None => format!("fix the credential of provider entry `{shown}`, then reload the config"),
    }
}

/// The doctor remediation for a seat dispatching through `[providers]` entry
/// `entry` of `config` ([`SeatFix::remediation`]).
pub fn unusable_seat_fix(config: &Config, entry: &str) -> String {
    SeatFix::of(config, entry).remediation()
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
    /// `prices` are the tables the daemon's own cost reads use, pinned once
    /// by the gather.
    pub const fn new(
        config: Arc<Config>,
        learned: ResidentLearned,
        warm: WarmReport,
        prices: ServedPrices,
    ) -> Self {
        Self {
            config,
            learned,
            warm,
            prices,
            reload_failure: None,
            parked_seats: Vec::new(),
            unusable_seats: Vec::new(),
            cache_read_budget: SERVED_CACHE_READ_BUDGET,
        }
    }

    /// How long the gather's cache-ignore ledger read may run.
    pub(super) const fn cache_read_budget(&self) -> Duration {
        self.cache_read_budget
    }

    /// Replace the cache-ignore read budget.
    #[cfg(test)]
    #[must_use]
    pub fn with_cache_read_budget(self, cache_read_budget: Duration) -> Self {
        Self {
            cache_read_budget,
            ..self
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

    /// Attach the seats whose credential the daemon holds as unusable.
    #[must_use]
    pub fn with_unusable_seats(self, unusable_seats: Vec<UnusableSeat>) -> Self {
        Self {
            unusable_seats,
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
    let deadline = rfc3339_or_epoch_ms(seat.parked_until_ms);
    Finding {
        section: "auth",
        name: format!("parked seat {}", safe(&seat.state_key)),
        status: Status::Warn,
        detail: format!(
            "credential parked ({}) until {deadline}; requests skip this seat until then",
            seat.reason.as_str(),
        ),
        remediation: Some(
            "renew or replace the credential; changing an OAuth seat's credentials entry lifts \
             its park and a config reload that changes any other account's provider entry lifts \
             that account's, otherwise the account is re-tested once the park lapses"
                .to_string(),
        ),
    }
}

/// One unusable seat as an `auth` Warn: the seat's state key, the reason and
/// cause tokens, the instant it was first marked as RFC3339 UTC, and its fix
/// line as the remediation.
pub(super) fn unusable_seat_finding(seat: &UnusableSeat) -> Finding {
    Finding {
        section: "auth",
        name: format!("unusable seat {}", safe(&seat.state_key)),
        status: Status::Warn,
        detail: format!(
            "{} ({}) since {}; requests skip this seat until its credential is fixed",
            seat.reason.as_str(),
            seat.cause.as_str(),
            rfc3339_or_epoch_ms(seat.since_ms),
        ),
        remediation: Some(seat.fix.clone()),
    }
}

fn rfc3339_or_epoch_ms(epoch_ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(epoch_ms).map_or_else(
        || format!("epoch ms {epoch_ms}"),
        |at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

/// The per-layer inputs the shared gather body draws from, whichever source
/// produced them.
pub(super) struct GatheredLayers {
    pub(super) config: Config,
    /// The already-redacted config parse error.
    pub(super) config_parse_error: Option<String>,
    /// The already-redacted config load error.
    pub(super) config_load_error: Option<String>,
    /// The schema version of a config the daemon already accepted; `None`
    /// when the config came from disk and its version must be read off the
    /// raw file.
    pub(super) accepted_config_version: Option<u32>,
    pub(super) capability_matrix: CapabilityMatrixSource,
    pub(super) matrix_origin: MatrixOrigin,
    /// The daemon's last rejected reload; always `None` from disk.
    pub(super) reload_failure: Option<ReloadFailureSnapshot>,
    /// The daemon's credential-parked seats; always empty from disk, which
    /// cannot see daemon state.
    pub(super) parked_seats: Vec<ParkedSeat>,
    /// The daemon's unusable seats; always empty from disk, which cannot see
    /// daemon memory.
    pub(super) unusable_seats: Vec<UnusableSeat>,
    /// The price tables the pricing section resolves against, pinned once
    /// per gather.
    pub(super) prices: PriceSnapshot,
}

/// The served layers. The daemon accepted this config, so it carries no load
/// error.
pub(super) fn served_layers(inputs: Box<ServedInputs>) -> GatheredLayers {
    let ServedInputs {
        config,
        learned,
        warm,
        prices,
        reload_failure,
        parked_seats,
        unusable_seats,
        cache_read_budget: _,
    } = *inputs;
    GatheredLayers {
        accepted_config_version: Some(config.version),
        config: Arc::unwrap_or_clone(config),
        config_parse_error: None,
        config_load_error: None,
        capability_matrix: resident_matrix(learned, &warm),
        matrix_origin: MatrixOrigin::Resident(matrix_warm(&warm)),
        reload_failure,
        parked_seats,
        unusable_seats,
        prices: prices.pin(),
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
