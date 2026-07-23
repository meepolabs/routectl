//! Effective-config load, parse, validate, and world-readable warning.

use std::path::Path;

use routectl_core::Result;
use routectl_router::{CatalogOverlay, Config};

/// Maximum incoming JSON body size for `/v1/chat/completions` and
/// `/v1/messages`. Operator-configurable via `[server] max_body_bytes`
/// (default 32 MiB; see `routectl_router::ServerConfig`). Used by
/// `compute_max_body_bytes` as the fallback when the operator-supplied
/// value is zero.
const DEFAULT_MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Compute the effective `DefaultBodyLimit` value. Mirrors the legacy
/// behavior: zero in the config means fall through to the library
/// default; a non-zero value is honored.
pub(super) fn compute_max_body_bytes(config: &Config) -> usize {
    let raw = usize::try_from(config.server.max_body_bytes).unwrap_or(usize::MAX);
    if raw == 0 {
        DEFAULT_MAX_BODY_BYTES
    } else {
        raw
    }
}

/// SINGLE shared config loader: preflight the schema `version`, parse
/// `config.toml`, load the catalog overlay (fail-closed per
/// `routectl_router::load_catalog_overlay`'s matrix), and run the startup
/// validators. Used by BOTH the CLI's cold-start `load_config` (`main.rs`)
/// and this module's hot-reload path (`read_parse_validate_config`) --
/// PRE-EXISTING split-brain this closed: only the cold-start path used to
/// merge the sidecar, so a config reload silently dropped sidecar /
/// `[cache_pricing]` data. A reload now re-reads the overlay from disk too
/// -- both a config-file touch and a dedicated overlay-file write
/// (`WatchTarget::CatalogOverlay` in `file_watch.rs`) trigger this same
/// re-read via `ReloadRequest::Config` / `ReloadRequest::CatalogOverlay`.
/// The load NEVER migrates the file in place: a `version` outside the
/// range this build writes fails closed in the preflight (a too-old file
/// points at `config migrate`; a too-new file at upgrading routectl). A
/// cold-start error propagates; a reload rejects and keeps the prior
/// router live, same posture as every other load failure below.
///
/// The loaded overlay rides back on [`LoadedConfig::catalog_overlay`] --
/// callers that build a Router thread it into
/// [`build_router_from_config_with_overlay`](super::router_build::build_router_from_config_with_overlay) so the two-layer merge
/// (`routectl_router::apply_catalog_overlay`) sees the SAME overlay this
/// call validated, at both cold start and every config reload.
///
/// Overlay / parse / validate failure ALWAYS returns `Err` here; callers
/// choose the posture -- cold startup propagates the error (fails hard),
/// a hot reload logs a warn and keeps the prior config + router live
/// (`read_parse_validate_config` below does exactly that).
pub fn load_effective_config(path: &Path) -> Result<LoadedConfig, String> {
    let loaded = load_effective_config_unvalidated(path)?;
    validate_effective_config(&loaded.config)?;
    warn_deprecated_capability_lists(&loaded.config);
    Ok(loaded)
}

/// Emit ONE structured deprecation WARN when a serve-loaded config carries
/// any legacy capability-list key, naming which keys are present, the
/// `[capability.overrides]` successor, and the `config migrate` command that
/// rewrites them. Runs on the serve cold-start AND hot-reload load paths
/// (both flow through [`load_effective_config`]); `config check` loads via
/// [`load_effective_config_unvalidated`], which never calls this, so the
/// check surface stays silent per the settled constraint. The WARN carries
/// key NAMES only -- no config values (secrets can live near these tables).
fn warn_deprecated_capability_lists(config: &Config) {
    let present = crate::commands::capability_legacy::present_legacy_capability_keys(config);
    if present.is_empty() {
        return;
    }

    tracing::warn!(
        event = "legacy_deprecation",
        legacy_keys = ?present,
        successor = "[capability.overrides]",
        migrate_command = "config migrate",
        "deprecated capability-list keys are set; they are tolerated for one release cycle and \
         rejected at the next config schema version. Move them under [capability.overrides] with \
         `config migrate`.",
    );
}

/// The parse + overlay body of [`load_effective_config`], WITHOUT the
/// fail-fast [`validate_effective_config`] gate.
///
/// Only `config check` uses this: it is the showcase surface that runs the
/// FULL shared validator suite itself and renders EVERY error with a source
/// line, so it must receive a parseable-but-semantically-invalid config
/// intact rather than have the load abort on the first semantic error.
///
/// Parse-level failures (unreadable file, version out of range, unknown
/// fields, the legacy `[mitm] credential_source` key) still return `Err`
/// here -- a config that does not parse has nothing for `check` to render
/// against, and this keeps the did-you-mean / migration guidance those
/// preflights emit. In particular a too-old `version` is REJECTED with the
/// `config migrate` pointer on this path identically to the serve/reload
/// path; neither path mutates the file on load.
///
/// Every other caller (serve cold start, hot reload, test, prompt-size) goes
/// through [`load_effective_config`], which wraps this and keeps the
/// fail-fast validation posture unchanged.
pub fn load_effective_config_unvalidated(path: &Path) -> Result<LoadedConfig, String> {
    let config = parse_config_only(path)?;
    let catalog_overlay = load_overlay_default()?;
    Ok(LoadedConfig {
        config,
        catalog_overlay,
    })
}

/// Parse `config.toml` ONLY -- version preflight, the legacy-mitm preflight,
/// and the typed `deny_unknown_fields` deserialize -- WITHOUT loading the
/// catalog overlay. Doctor's capability panel uses this so an unreadable
/// overlay degrades the catalog priors alone while the config-derived
/// override rows still render. Produces the SAME wrapped error strings as
/// the coupled [`load_effective_config_unvalidated`] so callers redact them
/// through one shared path.
pub fn parse_config_only(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read config `{}`: {e}", path.display()))?;

    // PREFLIGHT: read `version` off the raw TOML before the full,
    // `deny_unknown_fields` typed deserialize below. This rejects a config
    // whose schema version is out of the range this build writes -- a
    // too-new file (whose unknown fields would otherwise fail the typed
    // parse with a confusing "unknown field" error) and, equally, a too-old
    // file (pointed at `config migrate`, never mutated on load). A hot
    // reload hitting either rejects and keeps the prior router live (the
    // caller of this function on that path, `read_parse_validate_config`,
    // already treats any `Err` from here that way).
    routectl_router::preflight_config_version(&text).map_err(|e| e.to_string())?;

    // Same pre-parse pattern for the removed `[mitm] credential_source`
    // key: the raw serde "unknown field" error would not tell the operator
    // how to migrate, so detect the legacy key first and return the error
    // that names the exact provider-block replacement.
    routectl_router::preflight_legacy_mitm_credential_source(&text).map_err(|e| e.to_string())?;

    routectl_router::parse_config(&text)
        .map_err(|e| format!("config parse error in `{}`: {e}", path.display()))
}

/// Load the default catalog overlay independently of any config parse
/// (fail-closed per `routectl_router::load_catalog_overlay`'s matrix).
/// Doctor loads this layer separately so an unreadable overlay degrades the
/// capability priors without tainting the config-derived override rows.
pub fn load_overlay_default() -> Result<CatalogOverlay, String> {
    let overlay_path = routectl_router::overlay_default_path();
    routectl_router::load_catalog_overlay(&overlay_path)
        .map_err(|e| format!("catalog overlay load error: {e}"))
}

/// Return of [`load_effective_config`]: the parsed `Config` alongside the
/// catalog overlay loaded from the SAME call, so a caller building a Router
/// (`build_router_from_config_with_overlay`) never has the two drift apart
/// by loading them at different times.
pub struct LoadedConfig {
    pub config: Config,
    pub catalog_overlay: CatalogOverlay,
}

/// The startup validators shared by [`load_effective_config`] and
/// `build_router_from_config`'s own validation pass (intentionally
/// redundant: this is the cheap fail-fast gate before the heavier router
/// rebuild).
fn validate_effective_config(config: &Config) -> Result<(), String> {
    // The suite returns bare messages; re-add the `config: ` prefix so the
    // surfaced error reads the same as when these validators propagated
    // through `Error::Config`'s Display on this path.
    match routectl_router::collect_config_validation(config)
        .errors
        .into_iter()
        .next()
    {
        Some(first) => Err(format!("config: {first}")),
        None => Ok(()),
    }
}

/// Read, parse, and validate the config at `path` via the shared
/// [`load_effective_config`]. Returns `None` and emits a warn on any
/// failure so the coordinator can keep the previous config installed.
/// Pulled out of `handle_config_reload` to keep that function focused on
/// the swap + diff phases.
pub(super) fn read_parse_validate_config(path: &Path) -> Option<LoadedConfig> {
    let loaded = match load_effective_config(path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "config reload failed; keeping previous config",
            );
            return None;
        }
    };

    // Unix-only: WARN when the config file is group/world-readable
    // and carries sensitive values. Non-fatal so dev setups with
    // literal: secrets still start; the operator is informed and can
    // restrict permissions when it matters. A second small read is
    // acceptable on this rarely-hit reload path -- `load_effective_config`
    // does not hand back the raw text, and this check is orthogonal to
    // parsing.
    #[cfg(unix)]
    if let Ok(text) = std::fs::read_to_string(path) {
        warn_if_config_world_readable(path, &loaded.config, &text);
    }

    Some(loaded)
}

/// Emit a one-time WARN when `path` is group/world-readable AND the
/// config text carries listener auth tokens or `literal:` secrets.
/// Non-fatal: the caller keeps the config regardless so dev setups
/// that store credentials in plain TOML still start. Operators running
/// in shared environments should restrict the file to `0600`.
#[cfg(unix)]
fn warn_if_config_world_readable(path: &Path, config: &Config, raw_text: &str) {
    use std::os::unix::fs::MetadataExt;
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return,
    };
    let mode = meta.mode();
    // Group-read (0o040) or world-read (0o004).
    if (mode & 0o044) == 0 {
        return;
    }
    let has_server_tokens = config
        .server
        .auth
        .as_ref()
        .is_some_and(|a| !a.tokens.is_empty());
    let has_literal_secret = raw_text.contains("literal:");
    if has_server_tokens || has_literal_secret {
        tracing::warn!(
            path = %path.display(),
            mode = format!("{:04o}", mode & 0o777),
            "config file is group/world-readable and carries secrets \
             ([server.auth].tokens or literal: values); restrict to 0600 \
             to prevent credential exposure",
        );
    }
}

#[cfg(test)]
#[path = "config_load_tests.rs"]
mod config_load_tests;
