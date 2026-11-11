//! Effective-config load, parse, validate, and world-readable warning.

use std::path::Path;

use routectl_core::Result;
use routectl_router::Config;

use super::reload_failure::ReloadFailure;

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
/// `config.toml`, and run the startup validators. Used by the CLI's
/// cold-start `load_config` (`main.rs`); the hot-reload path
/// (`read_parse_validate_config`) runs this SAME function, so the two cannot
/// diverge on what loads or in which order. The load NEVER
/// rewrites the file in place: a `version` outside the range this build
/// reads, or a missing one, fails closed in the preflight (a too-old or
/// versionless file is told to move it aside and run `routectl init`; a
/// too-new file to upgrade routectl).
///
/// Parse / validate failure ALWAYS returns `Err`; callers choose the posture
/// -- cold startup propagates the error (fails hard), a hot reload logs a
/// warn and keeps the prior config + router live.
pub fn load_effective_config(path: &Path) -> Result<LoadedConfig, String> {
    let loaded = load_effective_config_unvalidated(path)?;
    validate_effective_config(&loaded.config)?;
    Ok(loaded)
}

/// The parse body of [`load_effective_config`], WITHOUT the fail-fast
/// `validate_effective_config` gate.
///
/// Only `config check` uses this: it is the showcase surface that runs the
/// FULL shared validator suite itself and renders EVERY error with a source
/// line, so it must receive a parseable-but-semantically-invalid config
/// intact rather than have the load abort on the first semantic error.
///
/// Parse-level failures (unreadable file, version out of range, unknown
/// fields, the legacy `[mitm] credential_source` key) still return `Err`
/// here -- a config that does not parse has nothing for `check` to render
/// against, and this keeps the did-you-mean / remediation guidance those
/// preflights emit. In particular a too-old or missing `version` is
/// REJECTED with the same message on this path as on the serve/reload
/// path; neither path mutates the file on load.
///
/// Serve cold start, test and prompt-size go through
/// [`load_effective_config`] and hot reload through
/// `read_parse_validate_config`; both keep the fail-fast validation posture.
pub fn load_effective_config_unvalidated(path: &Path) -> Result<LoadedConfig, String> {
    let text = read_config_text(path)?;
    Ok(LoadedConfig {
        config: parse_config_text(path, &text)?,
        raw_text: text,
    })
}

/// Parse `config.toml` ONLY -- the router's raw-config preflight
/// (`routectl_router::preflight_raw_config`: schema version, then removed
/// and retired keys) and the typed `deny_unknown_fields` deserialize.
/// Produces the SAME wrapped error strings as
/// [`load_effective_config_unvalidated`] so callers redact them through one
/// shared path.
pub fn parse_config_only(path: &Path) -> Result<Config, String> {
    parse_config_text(path, &read_config_text(path)?)
}

fn read_config_text(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read config `{}`: {e}", path.display()))
}

/// The parse body of [`parse_config_only`] over `text`, already read from
/// `path` (named in errors only).
fn parse_config_text(path: &Path, text: &str) -> Result<Config, String> {
    // The raw-text checks run before the `deny_unknown_fields` typed
    // deserialize, whose generic "unknown field" error would neither name a
    // refused `version` nor a removed key. A hot reload
    // refused here keeps the prior router live (`read_parse_validate_config`
    // treats any `Err` from this function that way).
    routectl_router::preflight_raw_config(text).map_err(|e| e.to_string())?;

    routectl_router::parse_config(text)
        .map_err(|e| format!("config parse error in `{}`: {e}", path.display()))
}

/// Return of [`load_effective_config`]: the parsed `Config` and the text it
/// was parsed from, read once.
pub struct LoadedConfig {
    pub config: Config,
    pub raw_text: String,
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

/// Hot-reload entry to [`load_effective_config`]: emits a warn on any failure so the
/// coordinator can keep the previous config installed.
pub(super) fn read_parse_validate_config(path: &Path) -> Result<LoadedConfig, ReloadFailure> {
    let loaded = match load_effective_config(path) {
        Ok(loaded) => loaded,
        Err(message) => {
            // The loader error can inline the offending config VALUE (a
            // secret mistyped into a non-string field or a `literal:`
            // credential on the failing source line) plus local paths.
            // Route it through the same fail-safe redactor the cold-start
            // seam and `doctor` use before it reaches structured logs.
            let redacted =
                crate::commands::parse_error_redaction::redact_config_load_error(&message);
            let (error, reasons) = single_line_reasons(&redacted);
            tracing::warn!(
                path = %path.display(),
                error = %error,
                reasons,
                "config reload failed; keeping previous config",
            );
            return Err(ReloadFailure::ConfigLoadFailed);
        }
    };

    // Unix-only: WARN when the config file is group/world-readable
    // and carries sensitive values. Non-fatal so dev setups with
    // literal: secrets still start; the operator is informed and can
    // restrict permissions when it matters.
    #[cfg(unix)]
    warn_if_config_world_readable(path, &loaded.config, &loaded.raw_text);

    Ok(loaded)
}

/// A multi-line loader error as one log field: its non-empty lines joined
/// with `"; "`, and how many lines there were. A log shipper splits a field
/// carrying a newline into separate records, so the reload WARN must stay on
/// one line; the cold-start error printed to stderr keeps its line breaks.
fn single_line_reasons(error: &str) -> (String, usize) {
    let reasons: Vec<&str> = error
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    (reasons.join("; "), reasons.len())
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

#[cfg(test)]
#[path = "config_load_removed_key_tests.rs"]
mod config_load_removed_key_tests;

#[cfg(test)]
#[path = "config_load_reload_warn_tests.rs"]
mod config_load_reload_warn_tests;

#[cfg(test)]
#[path = "config_load_preflight_tests.rs"]
mod config_load_preflight_tests;
