//! Config version preflight + non-schema validation.

use crate::config::Config;

/// Current config schema version this build writes and fully understands.
/// `1` -> `2` retires the legacy `[cache_pricing]` override table into the
/// catalog overlay; `2` -> `3` retires the raw-status retry
/// allow/deny escape hatch in favor of per-class policy
/// (`[retry.classes.*]` / provider `class_overrides`). Both transforms
/// run via `crate::config_migrate` under `config migrate`.
pub const CURRENT_CONFIG_VERSION: u32 = 3;

pub(super) const fn default_config_version() -> u32 {
    1
}

/// The config file names a schema version newer than this build
/// understands. Kept a distinct type so the too-new posture (upgrade the
/// binary) stays separate from the too-old posture (migrate the config).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "config version {found} is newer than the {supported} this build supports; upgrade \
     routectl or downgrade the config's `version` key"
)]
pub struct VersionTooNewError {
    /// Schema version found in the config file.
    pub found: u32,
    /// Highest schema version this build supports.
    pub supported: u32,
}

/// Outcome of [`preflight_config_version`] when the file's schema version is
/// out of bounds either way. Both bounds fail closed here, before any typed
/// parse or in-place migration, so a too-old config is never mutated on
/// load. The single wording every caller shares -- the loader and the
/// `config` CLI both surface these Display strings verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ConfigVersionError {
    /// The config version is newer than this build supports.
    #[error(transparent)]
    TooNew(#[from] VersionTooNewError),

    /// The config predates what this build writes. Names `config migrate`
    /// as the fix rather than mutating the file on load.
    #[error(
        "config version {found} predates the {supported} this build writes; run \
         `config migrate` to bring it forward, or edit it with a routectl binary that \
         matches its version. Nothing was written."
    )]
    TooOld {
        /// Schema version found in the config file.
        found: u32,
        /// Schema version this build writes.
        supported: u32,
    },
}

/// Read the `version` key straight off the RAW TOML text, before `Config`'s
/// full (`#[serde(deny_unknown_fields)]`) deserialize runs. A config written
/// by a newer routectl may carry fields this build does not know about;
/// deserializing it directly would fail with a confusing "unknown field"
/// error that buries the real cause. This preflight catches that case
/// explicitly and, in the same pass, rejects a config OLDER than this build
/// writes: a `version` outside `[CURRENT_CONFIG_VERSION, CURRENT_CONFIG_VERSION]`
/// fails closed here with a clear message. The too-old branch points at
/// `config migrate` and, crucially, never mutates the file -- the loader no
/// longer migrates on load.
///
/// This preflight only ever speaks about the `version` key, so it never
/// masks a genuine error behind a version message. TOML that fails to parse
/// at all, or a `version` that is present but is not a plain non-negative
/// integer, fall through as `Ok` -- the normal typed deserialize reports
/// those with a precise syntax / type error. A MISSING `version` key is the
/// one value this reads as legacy `1`: a config with no `version` predates
/// the schema and so surfaces the too-old message. Callers wire this in at
/// both cold startup (propagate the error, fail hard) and hot config reload
/// (reject the reload, keep the prior router live).
pub fn preflight_config_version(raw_toml: &str) -> Result<u32, ConfigVersionError> {
    // A TOML parse failure is not ours to report -- let the typed deserialize
    // surface the real syntax error.
    let Ok(value) = toml::from_str::<toml::Value>(raw_toml) else {
        return Ok(default_config_version());
    };

    let found = match value.get("version") {
        // Absent key -> legacy v1 config.
        None => default_config_version(),
        // Present but not a non-negative integer that fits: leave the type
        // error for the typed deserialize rather than mislabel it too-old.
        Some(v) => match v.as_integer().and_then(|i| u32::try_from(i).ok()) {
            Some(n) => n,
            None => return Ok(default_config_version()),
        },
    };

    if found > CURRENT_CONFIG_VERSION {
        return Err(ConfigVersionError::TooNew(VersionTooNewError {
            found,
            supported: CURRENT_CONFIG_VERSION,
        }));
    }
    if found < CURRENT_CONFIG_VERSION {
        return Err(ConfigVersionError::TooOld {
            found,
            supported: CURRENT_CONFIG_VERSION,
        });
    }
    Ok(found)
}

/// At `version >= CURRENT_CONFIG_VERSION`, a non-empty legacy
/// `[cache_pricing]` table is a startup-time misconfiguration, not
/// silently-ignored data: the config migrate ladder
/// (`crate::config_migrate::plan_migration`) already folds
/// `[cache_pricing]` into the catalog overlay and clears it from
/// `config.toml`, so a non-empty table at v2+ means the file was
/// hand-edited back into an inconsistent state (or authored fresh from a
/// stale example). Names the migrator so the operator knows the fix.
pub fn validate_cache_pricing_retired(config: &Config) -> Result<(), String> {
    if config.version >= CURRENT_CONFIG_VERSION && !config.cache_pricing.is_empty() {
        return Err(format!(
            "config version {} carries a non-empty [cache_pricing] table ({} entries), but \
             [cache_pricing] is retired as of version {CURRENT_CONFIG_VERSION} -- it should \
             have been migrated into the catalog overlay by the `config_migrate` ladder; run \
             `config migrate` to fold it forward, or remove [cache_pricing] by hand",
            config.version,
            config.cache_pricing.len(),
        ));
    }
    Ok(())
}

/// The exact `[providers.X]` replacement block named in
/// [`LegacyMitmCredentialSourceError`] and the CHANGELOG: a forwarded
/// credential is now a provider-level choice, not a `[mitm]`-level one.
const LEGACY_MITM_CREDENTIAL_SOURCE_REPLACEMENT_BLOCK: &str = "[providers.anthropic-forwarded]\n\
     kind = \"anthropic-api\"\n\
     base_url = \"https://api.anthropic.com\"\n\
     credential_source = \"forwarded\"";

/// Error from [`preflight_legacy_mitm_credential_source`]: the config
/// still carries the removed `[mitm] credential_source` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "config carries the removed `[mitm] credential_source` key -- a forwarded credential is \
     now a per-provider choice, not a `[mitm]`-level one. Replace it with a provider block:\n\n\
     {LEGACY_MITM_CREDENTIAL_SOURCE_REPLACEMENT_BLOCK}\n\n\
     (no `api_key_ref` line -- a forwarded provider has no configured credential of its own)"
)]
pub struct LegacyMitmCredentialSourceError;

/// Read the `[mitm]` table off the RAW TOML text, before `Config`'s full
/// (`#[serde(deny_unknown_fields)]`) deserialize runs, and check for the
/// removed `credential_source` key. Without this preflight, an old
/// config carrying that key still fails at load (`deny_unknown_fields`
/// rejects it), but with serde's generic "unknown field" message, which
/// does not tell the operator what replaces it. This preflight exists
/// only to make that ONE failure actionable -- it never masks a genuine
/// parse error or any other unknown field, both of which fall through
/// to the normal typed deserialize untouched.
///
/// Same pattern as [`preflight_config_version`]; callers wire this in
/// alongside it, before the typed deserialize, at both cold startup
/// (propagate the error, fail hard) and hot config reload (reject the
/// reload, keep the prior router live).
pub fn preflight_legacy_mitm_credential_source(
    raw_toml: &str,
) -> Result<(), LegacyMitmCredentialSourceError> {
    let carries_legacy_key = toml::from_str::<toml::Value>(raw_toml)
        .ok()
        .and_then(|v| v.get("mitm").and_then(toml::Value::as_table).cloned())
        .is_some_and(|mitm| mitm.contains_key("credential_source"));

    if carries_legacy_key {
        return Err(LegacyMitmCredentialSourceError);
    }
    Ok(())
}

#[cfg(test)]
#[path = "validate_tests.rs"]
mod validate_tests;
