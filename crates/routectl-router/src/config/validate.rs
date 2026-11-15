//! Config version preflight + non-schema validation.

use std::collections::BTreeMap;

use super::retired_keys::{RETIRED_KEYS, RetiredKeysError, preflight_retired_keys};
use crate::config::{Config, PricingConfig};

/// Current config schema version this build writes and fully understands.
/// `1` -> `2` retires the legacy `[cache_pricing]` override table into the
/// catalog overlay; `2` -> `3` retires the raw-status retry
/// allow/deny escape hatch in favor of per-class policy
/// (`[retry.classes.*]` / provider `class_overrides`); `3` -> `4` moves the
/// `seat_selection` knob off the provider entry onto the `[pools.<name>]`
/// block that groups the accounts, making explicit pools the only
/// multi-seat shape; `4` -> `5` folds the provider / model
/// `unsupported_features` lists into `[capability.overrides]` and retires
/// the egress allowlists. A file at any earlier version, or with no
/// `version` at all, is refused by [`preflight_config_version`].
pub const CURRENT_CONFIG_VERSION: u32 = 5;

pub(super) const fn default_config_version() -> u32 {
    1
}

/// The config file names a schema version newer than this build
/// understands. Kept a distinct type so the too-new posture (upgrade the
/// binary) stays separate from the too-old posture (replace the config).
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

/// The remediation every too-old or versionless refusal ends with: the
/// [`ConfigVersionError::TooOld`] and [`ConfigVersionError::Missing`]
/// messages and doctor's matching remediation all read this one string, so
/// the advice cannot drift between them. The old file has to be moved aside
/// because `routectl init` refuses to edit a file it cannot load.
pub const CONFIG_VERSION_REMEDIATION: &str = "Move the old file aside, run `routectl init` to \
     write a new one, and see docs/CONFIGURATION.md for the current schema. Nothing was written.";

/// Outcome of [`preflight_config_version`] when the file's schema version is
/// out of bounds either way, or absent. Every case fails closed here, before
/// any typed parse, so a refused config is never mutated on load. The single
/// wording every caller shares -- the loader and the `config` CLI both
/// surface these Display strings verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ConfigVersionError {
    /// The config version is newer than this build supports.
    #[error(transparent)]
    TooNew(#[from] VersionTooNewError),

    /// The config names a schema version this build no longer reads.
    #[error(
        "config version {found} is no longer supported; v{supported} required. {}",
        CONFIG_VERSION_REMEDIATION
    )]
    TooOld {
        /// Schema version found in the config file.
        found: u32,
        /// Schema version this build reads.
        supported: u32,
    },

    /// The config carries no `version` key at all, so it predates the
    /// versioned schema.
    #[error(
        "config version missing; v{supported} required. {}",
        CONFIG_VERSION_REMEDIATION
    )]
    Missing {
        /// Schema version this build reads.
        supported: u32,
    },
}

/// Read the `version` key straight off the RAW TOML text, before `Config`'s
/// full (`#[serde(deny_unknown_fields)]`) deserialize runs. A config written
/// by a newer routectl may carry fields this build does not know about;
/// deserializing it directly would fail with a confusing "unknown field"
/// error that buries the real cause. This preflight catches that case
/// explicitly and, in the same pass, rejects a config OLDER than this build
/// reads: a `version` outside `[CURRENT_CONFIG_VERSION, CURRENT_CONFIG_VERSION]`
/// fails closed here with a clear message, and a file with no `version` key
/// at all fails closed as [`ConfigVersionError::Missing`]. Both refusals end
/// in [`CONFIG_VERSION_REMEDIATION`] and never mutate the file.
///
/// An out-of-range or missing `version` deliberately takes precedence over
/// every other refusal or typed error in the same file. TOML that fails to
/// parse at all, or a `version` that is present but is not a plain
/// non-negative integer, fall through as `Ok` -- the normal typed
/// deserialize reports those with a precise syntax / type error. Config
/// loading reaches this through [`preflight_raw_config`], which owns the
/// order of the raw-text checks and runs this one first.
pub fn preflight_config_version(raw_toml: &str) -> Result<u32, ConfigVersionError> {
    // A TOML parse failure is not ours to report -- let the typed deserialize
    // surface the real syntax error.
    let Ok(value) = toml::from_str::<toml::Value>(raw_toml) else {
        return Ok(default_config_version());
    };

    let found = match value.get("version") {
        None => {
            return Err(ConfigVersionError::Missing {
                supported: CURRENT_CONFIG_VERSION,
            });
        }
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

/// Refuse a `[models.X] context_window` or `max_output_tokens` of `0`, naming
/// every offending dotted key on its own line. A zero window would make every
/// request "too large" for the window gate, and a zero ceiling would read as
/// unset to the egress fill, so neither is a usable value.
pub fn validate_model_token_limits(config: &Config) -> Result<(), String> {
    let lines: Vec<String> = config
        .models
        .iter()
        .flat_map(|(nickname, entry)| {
            [
                ("context_window", entry.context_window),
                ("max_output_tokens", entry.max_output_tokens),
            ]
            .into_iter()
            .filter(|(_, value)| *value == Some(0))
            .map(move |(key, _)| {
                let path = message_safe_key(&format!("models.{nickname}.{key}"));
                format!("`{path}` is 0; set a positive token count or delete the key")
            })
        })
        .collect();
    if lines.is_empty() {
        Ok(())
    } else {
        Err(lines.join("\n"))
    }
}

/// Refuse `[server] max_body_bytes = 0`. A zero cap would reject every
/// request body, so it is never a usable value; omitting the key keeps the
/// schema default.
pub fn validate_server_body_limit(config: &Config) -> Result<(), String> {
    if config.server.max_body_bytes == 0 {
        Err(
            "`server.max_body_bytes` is 0; omit it for the 32 MiB default or set a \
             positive byte count"
                .to_string(),
        )
    } else {
        Ok(())
    }
}

/// Refuse a `[models.X.pricing]` value that is not finite or is negative,
/// and a `cache_write_per_mtok` key that is not a cache lifetime, naming
/// every offending dotted key on its own line. A price feeds cost
/// arithmetic, where NaN or infinity poisons every sum it joins and a
/// negative price is never a real rate.
pub fn validate_model_pricing(config: &Config) -> Result<(), String> {
    let lines: Vec<String> = config
        .models
        .iter()
        .filter_map(|(nickname, entry)| Some((nickname, entry.pricing.as_ref()?)))
        .flat_map(|(nickname, pricing)| {
            let prefix = format!("models.{nickname}.pricing");
            let fixed = [
                ("input_per_mtok", pricing.input_per_mtok),
                ("output_per_mtok", pricing.output_per_mtok),
                ("cache_read_per_mtok", pricing.cache_read_per_mtok),
                ("cache_write_5m_per_mtok", pricing.cache_write_5m_per_mtok),
                ("cache_write_1h_per_mtok", pricing.cache_write_1h_per_mtok),
            ]
            .into_iter()
            .filter_map(|(key, value)| {
                let value = value?;
                let problem = price_value_problem(value)?;
                let path = message_safe_key(&format!("{prefix}.{key}"));
                Some(format!("`{path}` is `{value}`; {problem}"))
            });
            let mut lines: Vec<String> = fixed.collect();
            lines.extend(cache_write_map_problems(pricing, &prefix));
            lines
        })
        .collect();
    if lines.is_empty() {
        Ok(())
    } else {
        Err(lines.join("\n"))
    }
}

/// Why `value` is not a usable price, or `None` when it is one.
const fn price_value_problem(value: f64) -> Option<&'static str> {
    if !value.is_finite() {
        Some("must be a finite number")
    } else if value < 0.0 {
        Some("must not be negative")
    } else {
        None
    }
}

/// One line per problem in `pricing.cache_write_per_mtok`, each naming the
/// dotted key under `prefix` (`models.<nick>.pricing`): a key that is not a
/// cache lifetime, two keys naming the same lifetime, or a value that is not
/// finite or is negative.
pub fn cache_write_map_problems(pricing: &PricingConfig, prefix: &str) -> Vec<String> {
    let Some(map) = pricing.cache_write_per_mtok.as_ref() else {
        return Vec::new();
    };
    let mut lifetimes: BTreeMap<u32, &str> = BTreeMap::new();
    let mut lines = Vec::new();
    for (key, value) in map {
        let path = message_safe_key(&format!("{prefix}.cache_write_per_mtok.{key}"));
        match routectl_core::parse_cache_lifetime(key) {
            None => lines.push(format!(
                "`{path}` is not a cache lifetime; write a positive whole number and one of \
                 s, m, h, d (for example \"30m\")"
            )),
            Some(seconds) => {
                if let Some(first) = lifetimes.insert(seconds, key) {
                    let first = message_safe_key(first);
                    lines.push(format!(
                        "`{path}` names the same cache lifetime as `{first}`; keep one of them"
                    ));
                }
            }
        }
        if let Some(problem) = price_value_problem(*value) {
            lines.push(format!("`{path}` is `{value}`; {problem}"));
        }
    }
    lines
}

/// Character ceiling for one operator-written key rendered into an error
/// message by [`message_safe_key`].
const MAX_MESSAGE_KEY_CHARS: usize = 256;

/// An operator-written TOML key (or a dotted path built from one) made safe
/// to interpolate into an error message. A quoted key may carry an ESC, CR or
/// LF byte; rendered raw, it would split the message or forge a terminal
/// escape. Printable ASCII passes through unchanged.
pub fn message_safe_key(key: &str) -> String {
    routectl_core::sanitize_for_log_with_cap(key, MAX_MESSAGE_KEY_CHARS)
}

/// A refusal from [`preflight_raw_config`]. `Display` is the wrapped
/// check's own message, unchanged, so every caller surfaces the same wording.
///
/// New refusal variants may be added, so a `match` outside this crate needs
/// a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RawConfigPreflightError {
    /// The schema version is too old, too new, or missing for this build.
    #[error(transparent)]
    Version(ConfigVersionError),

    /// The config carries one or more retired keys.
    #[error(transparent)]
    RetiredKeys(RetiredKeysError),
}

/// Run every raw-text config check, before `Config`'s typed deserialize:
/// [`preflight_config_version`], then the retired-key table, which reports
/// every retired key in one refusal. Version comes first so an older or
/// versionless file is told its version is refused rather than to delete
/// keys by hand.
///
/// TOML that does not parse passes every check, so the typed deserialize
/// reports the real syntax error.
pub fn preflight_raw_config(raw: &str) -> Result<(), RawConfigPreflightError> {
    preflight_config_version(raw).map_err(RawConfigPreflightError::Version)?;
    preflight_retired_keys(raw, RETIRED_KEYS).map_err(RawConfigPreflightError::RetiredKeys)?;
    Ok(())
}

#[cfg(test)]
#[path = "validate_tests.rs"]
mod validate_tests;
