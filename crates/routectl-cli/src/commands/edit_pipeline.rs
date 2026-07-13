//! Building blocks shared by the `config.toml`-mutating commands
//! (`config set`/`unset` in [`super::config_edit`] and `provider add` in
//! [`super::provider_add`]): the raw version/legacy preflights, the
//! in-memory validation gate plus its error rendering, and the pre-lock
//! high-consequence confirmation prompt.
//!
//! These live in one place so every mutating command refuses stale/legacy
//! files, re-validates candidates, and prompts on egress-defining edits
//! identically -- their behavior can never drift apart.

use std::io::Write as _;

use routectl_core::{Error, Result};
use routectl_router::{
    Config, ConfigWriteError, parse_config, preflight_config_version,
    preflight_legacy_mitm_credential_source,
};

use super::config::validation_report;

/// The `edit_fn` closure's error shared by the config-mutating commands: the
/// candidate re-validated under the write lock against the SAME bytes the
/// pre-check accepted, yet failed. The revision check makes the re-read
/// deterministic, so this is a belt-and-suspenders guard, not a path an
/// ordinary edit reaches.
#[derive(Debug, thiserror::Error)]
#[error("config candidate failed re-validation under the write lock")]
pub(crate) struct RelockValidationError;

/// Map a [`ConfigWriteError`] (conflict, IO, parse, or the closure's
/// [`RelockValidationError`]) to a user-facing config error.
pub(crate) fn render_write_error(err: ConfigWriteError<RelockValidationError>) -> Error {
    Error::Config(err.to_string())
}

/// Raw preflights matching the loader: refuse a config whose version is out
/// of bounds (older or newer than this build writes) before any edit
/// touches it, and reject the removed `[mitm] credential_source` key with
/// its actionable message. The version wording is single-sourced in
/// `preflight_config_version`, so the CLI and the loader never diverge.
pub(crate) fn preflight(raw_text: &str) -> Result<()> {
    preflight_config_version(raw_text).map_err(|e| Error::Config(e.to_string()))?;
    preflight_legacy_mitm_credential_source(raw_text).map_err(|e| Error::Config(e.to_string()))?;
    Ok(())
}

/// The shared validation gate: `parse_config` (free did-you-mean) then the
/// centralized validator suite the reload path also runs. Returns the
/// validated `Config` or the rendered error lines.
pub(crate) fn gate(candidate_text: &str) -> std::result::Result<Config, Vec<String>> {
    let config = parse_config(candidate_text).map_err(|e| vec![e])?;
    let report = validation_report(&config, Some(candidate_text));
    if report.errors.is_empty() {
        Ok(config)
    } else {
        Err(report.errors)
    }
}

pub(crate) fn render_gate_errors(errors: &[String]) {
    eprintln!(
        "config rejected ({} error(s)); nothing written:",
        errors.len()
    );
    for e in errors {
        eprintln!("  - {e}");
    }
}

/// Prompt before a high-consequence (egress-defining) edit. `--yes`
/// bypasses it. Called BEFORE the write lock is acquired, never while
/// holding it.
pub(crate) fn confirm_high_consequence(fields: &[&str], yes: bool) -> bool {
    if yes {
        return true;
    }
    println!(
        "this edit changes egress-defining settings: {}",
        fields.join(", ")
    );
    print!("apply anyway? [y/N] ");
    let _ = std::io::stdout().flush();
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}
