//! Building blocks shared by the `config.toml`-mutating commands
//! (`config set`/`unset` in [`super::config_edit`], `provider add` in
//! [`super::provider_add`], and the login auto-surface in
//! [`super::login_surface`]): the raw version/legacy preflights, the
//! in-memory validation gate plus its error rendering, the pre-lock
//! high-consequence confirmation prompt, and the document parse +
//! provider-block insert every one of them writes through.
//!
//! These live in one place so every mutating command refuses stale/legacy
//! files, re-validates candidates, and prompts on egress-defining edits
//! identically -- their behavior can never drift apart. That includes the
//! non-interactive contract: with no TTY on stdin the confirmation declines
//! rather than reading, so a silent pipe cannot hang any of them.

use std::io::{IsTerminal as _, Write as _};

use routectl_core::{Error, Result};
use routectl_router::{
    Config, ConfigWriteError, parse_config, preflight_config_version,
    preflight_legacy_mitm_credential_source,
};
use toml_edit::{DocumentMut, Item, Table};

use super::config::validation_report;
use super::parse_error_redaction::redact_parse_error;

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
/// validated `Config` or the rendered error lines. The `parse_config` error is
/// stripped of its verbatim source-line preview and value-bearing clauses first
/// -- toml/serde echo the offending config line into the diagnostic, and that
/// line could carry a `literal:` credential.
pub(crate) fn gate(candidate_text: &str) -> std::result::Result<Config, Vec<String>> {
    let config = parse_config(candidate_text).map_err(|e| vec![redact_parse_error(&e)])?;
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
/// bypasses it. A non-interactive run (no TTY on stdin) without `--yes`
/// declines immediately without reading, so a silent pipe cannot hang it;
/// the field list is printed either way, so a scripted caller sees what was
/// declined. Called BEFORE the write lock is acquired, never while
/// holding it.
pub(crate) fn confirm_high_consequence(fields: &[&str], yes: bool) -> bool {
    if yes {
        return true;
    }
    println!(
        "this edit changes egress-defining settings: {}",
        fields.join(", ")
    );
    // A non-interactive caller with an open-but-silent stdin (a pipe that
    // never sends a line or EOF) would otherwise block `read_line`
    // forever. With no TTY there is no one to answer the prompt, so
    // decline immediately -- the documented non-interactive contract is
    // `--yes`.
    if !std::io::stdin().is_terminal() {
        println!("stdin is not a terminal; declining without prompting. Pass `--yes` to apply.");
        return false;
    }
    print!("apply anyway? [y/N] ");
    let _ = std::io::stdout().flush();
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Parse candidate config text into a format-preserving document.
///
/// The error deliberately carries no source-line preview: the offending
/// line can hold credential material, and every caller here is about to
/// print it.
pub(crate) fn parse_document(text: &str) -> Result<DocumentMut> {
    text.parse::<DocumentMut>()
        .map_err(|e| Error::Config(format!("config does not parse: {e}")))
}

/// Insert `block` at `[providers.<name>]`, descending into (or creating) the
/// `providers` table via `as_table_like_mut` so existing providers' comments
/// and ordering survive. A same-name insert replaces the whole block
/// (`provider add --overwrite`). Deterministic given the same input
/// document -- the write closures rely on this to reproduce under the lock
/// exactly what planning gated.
pub(crate) fn insert_provider_block(doc: &mut DocumentMut, name: &str, block: Table) -> Result<()> {
    let root = doc.as_table_mut();
    if !root.contains_key("providers") {
        let mut providers = Table::new();
        providers.set_implicit(true);
        root.insert("providers", Item::Table(providers));
    }
    let providers = root
        .get_mut("providers")
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| Error::Config("`providers` exists but is not a table".into()))?;
    providers.insert(name, Item::Table(block));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirm_high_consequence_declines_immediately_on_non_tty_without_yes() {
        // Under the test harness stdin is not a TTY, so the terminal gate
        // must fire and decline WITHOUT reaching read_line -- a silent
        // pipe can no longer hang the prompt.
        assert!(
            !std::io::stdin().is_terminal(),
            "test harness stdin must be non-interactive for this assertion",
        );
        assert!(
            !confirm_high_consequence(&["providers.base_url"], false),
            "non-TTY without --yes must decline",
        );
        assert!(
            confirm_high_consequence(&["providers.base_url"], true),
            "--yes must still proceed byte-identically",
        );
    }
}
