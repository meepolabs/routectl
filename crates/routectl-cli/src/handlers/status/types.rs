//! Wire types for the read-only `/status` family: the per-panel `Panel<T>`
//! envelope, a UTC timestamp helper, and the fixed token vocabulary the panel
//! DTOs share with the routing/event surface.

use serde::Serialize;

/// One status panel's envelope. Every panel -- whether it carries data or
/// is unavailable -- serializes to the same four snake_case keys, so a
/// consumer reads a stable shape regardless of the panel's health.
///
/// The two states are mutually exclusive and enforced by the constructors:
/// an available panel carries `data` + `as_of` and never an `unavailable`
/// code; an unavailable panel carries only the code and never `data`/`as_of`.
#[derive(Debug, Clone, Serialize)]
pub struct Panel<T> {
    /// Wire-shape version for this panel's `data` DTO. Bumped by the owning
    /// panel when its payload changes in a non-additive way.
    pub schema_version: u32,
    /// RFC3339-UTC instant the underlying data was read. `None` on an
    /// unavailable panel.
    pub as_of: Option<String>,
    /// The panel payload. `None` on an unavailable panel.
    pub data: Option<T>,
    /// Reason code (see [`vocabulary::codes`]) when the panel could not be
    /// built. `None` on an available panel.
    pub unavailable: Option<String>,
}

impl<T> Panel<T> {
    /// An available panel. Structurally clears `unavailable`, so the
    /// available/unavailable states can never both be set.
    pub const fn available(schema_version: u32, as_of: String, data: T) -> Self {
        Self {
            schema_version,
            as_of: Some(as_of),
            data: Some(data),
            unavailable: None,
        }
    }

    /// An unavailable panel carrying `code`. Structurally clears `data` and
    /// `as_of`, so an unavailable panel can never leak a stale payload.
    pub fn unavailable(schema_version: u32, code: &str) -> Self {
        Self {
            schema_version,
            as_of: None,
            data: None,
            unavailable: Some(code.to_string()),
        }
    }
}

/// Current instant as an RFC3339-UTC string, the format every panel's
/// `as_of` uses.
pub fn now_utc_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Fixed snake_case wire tokens the status DTOs reuse from the event
/// surface, plus the `unavailable` reason codes. Centralized so a panel
/// builder never re-spells a token that must stay identical to what the
/// routing/event surface already emits.
pub mod vocabulary {
    /// Runtime-state map key; mirrors `RouteTargetStatus::state_key`.
    pub const STATE_KEY: &str = "state_key";
    /// Normalized capability key; mirrors the learned/override registries.
    pub const CAPABILITY_KEY: &str = "capability_key";
    /// Learned-capability signal strength field name.
    pub const SIGNAL_TIER: &str = "signal_tier";

    /// Provenance / filter-source value tokens. A contract with the routing
    /// consult's `source` label (see `routectl_router` `FilterSource`).
    pub mod provenance {
        /// Legacy per-provider `unsupported_features`.
        pub const PROVIDER: &str = "provider";
        /// Legacy per-model `unsupported_features`.
        pub const MODEL: &str = "model";
        /// A `[capability.overrides.<spec>]` entry.
        pub const OVERRIDE: &str = "override";
        /// A non-expired acting negative in the learned registry.
        pub const LEARNED: &str = "learned";
    }

    /// Signal-tier value tokens (mirror `routectl_core` `SignalTier::as_str`).
    pub mod signal_tier {
        /// The upstream declared the capability itself.
        pub const SELF_IDENTIFYING: &str = "self-identifying";
        /// The capability was inferred from observed behavior.
        pub const INFERRED: &str = "inferred";
    }

    /// `unavailable` reason codes carried in `Panel::unavailable`.
    pub mod codes {
        /// Usage: the ledger query returned no rows for the window.
        pub const NO_DATA: &str = "no_data";
        /// Usage: the on-disk schema version is outside the known range.
        pub const SCHEMA_MISMATCH: &str = "schema_mismatch";
        /// Usage: the ledger is locked / busy.
        pub const DB_BUSY: &str = "db_busy";
        /// A panel's data source could not be read: the usage ledger file
        /// is missing or unreadable, or a panel builder failed unexpectedly
        /// (the panic-isolation guard's catch-all).
        pub const DB_UNAVAILABLE: &str = "db_unavailable";
        /// Config: the per-request catalog-overlay load failed, so the
        /// effective view cannot be rendered. The raw loader error (which can
        /// carry a filesystem path or a config value) is never surfaced -- it
        /// is redacted and logged, and only this fixed code reaches the wire.
        pub const CONFIG_UNAVAILABLE: &str = "config_unavailable";
        /// Doctor: the no-network gather failed (or the builder panicked), so
        /// no report could be produced.
        pub const DOCTOR_UNAVAILABLE: &str = "doctor_unavailable";
        /// Doctor: no on-disk config path is bound to this server, so the
        /// disk-based doctor gather has nothing to read.
        pub const NO_CONFIG_PATH: &str = "no_config_path";
    }
}
