//! Building blocks shared by the `config.toml`-mutating commands
//! (`config set`/`unset` in [`super::config_edit`], `provider add` in
//! [`super::provider_add`], and the login auto-surface in
//! [`super::login_surface`]): the raw-config preflight, the
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
use routectl_router::{Config, ConfigWriteError, parse_config, preflight_raw_config};
use toml_edit::{Array, DocumentMut, Item, Table};

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

/// The raw-config preflight the loader runs, before any edit touches the
/// file: [`preflight_raw_config`] refuses an out-of-range or missing `version`
/// and each removed or retired key with its actionable message. The router owns the checks and their wording, so
/// the CLI and the loader never diverge.
pub(crate) fn preflight(raw_text: &str) -> Result<()> {
    preflight_raw_config(raw_text).map_err(|e| Error::Config(e.to_string()))
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

/// Write `[pools.<pool>]`: union `members` into whatever member list the
/// block already carries, creating the `pools` table and the block itself
/// when absent.
///
/// Format-preserving and IDEMPOTENT: a member already listed is not
/// duplicated, so re-running over this function's own output changes no
/// bytes. A repeated login of the same seat therefore leaves the file
/// untouched, and an operator-written member list is grown, never rewritten.
///
/// Nothing else on the block is touched: an existing `seat_selection` or
/// `accepts_new_logins` marker is an operator statement and survives
/// verbatim.
pub(crate) fn upsert_pool_members(doc: &mut DocumentMut, pool: &str, members: &[&str]) {
    let root = doc.as_table_mut();
    if !root.contains_key("pools") {
        let mut table = Table::new();
        table.set_implicit(true);
        root.insert("pools", Item::Table(table));
    }
    let Some(pools) = root.get_mut("pools").and_then(Item::as_table_like_mut) else {
        return;
    };
    if !pools.contains_key(pool) {
        pools.insert(pool, Item::Table(Table::new()));
    }
    let Some(block) = pools.get_mut(pool).and_then(Item::as_table_like_mut) else {
        return;
    };
    if !block.contains_key("members") {
        block.insert("members", toml_edit::value(Array::new()));
    }
    if let Some(arr) = block.get_mut("members").and_then(Item::as_array_mut) {
        let mut seen: std::collections::BTreeSet<String> = arr
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        for member in members {
            // Insert BEFORE the push, so a duplicate within `members`
            // itself is dropped too -- a caller assembling a list from two
            // sources must not be able to write `["a", "a"]`.
            if seen.insert((*member).to_string()) {
                arr.push(*member);
            }
        }
    }
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
    #[test]
    fn preflight_refuses_each_raw_class_with_the_router_drivers_message() {
        use routectl_router::{CURRENT_CONFIG_VERSION, RawConfigPreflightError as Refusal};

        let at = |version: u32, body: &str| format!("version = {version}\n{body}");
        let current = CURRENT_CONFIG_VERSION;
        type ClassMatcher = fn(&Refusal) -> bool;
        let rows: [(&str, String, ClassMatcher); 6] = [
            ("too old", at(current - 1, ""), |e| {
                matches!(e, Refusal::Version(_))
            }),
            ("too new", at(current + 1, ""), |e| {
                matches!(e, Refusal::Version(_))
            }),
            (
                "legacy mitm credential_source",
                at(current, "[mitm]\ncredential_source = \"forwarded\"\n"),
                |e| matches!(e, Refusal::LegacyMitmCredentialSource(_)),
            ),
            ("retired capability key", at(current, "[bedrock]\n"), |e| {
                matches!(e, Refusal::RetiredCapabilityKeys(_))
            }),
            (
                "paid probe caps",
                at(current, "[fidelity.paid_probe_daily_caps]\nx = 1\n"),
                |e| matches!(e, Refusal::RemovedPaidProbeCaps(_)),
            ),
            (
                "removed model key",
                at(current, "[models.m]\nmax_output_tokens = 8000\n"),
                |e| matches!(e, Refusal::RetiredModelKnobs(_)),
            ),
        ];

        for (row, raw, is_class) in rows {
            let driver = preflight_raw_config(&raw)
                .expect_err(&format!("row `{row}`: the driver must refuse the fixture"));
            assert!(is_class(&driver), "row `{row}`: wrong class {driver:?}");

            let err = preflight(&raw).expect_err(&format!("row `{row}` must be refused"));

            assert_eq!(err.to_string(), format!("config: {driver}"), "row `{row}`");
        }
    }

    #[test]
    fn preflight_refuses_paid_probe_daily_caps_by_name() {
        let clean = format!(
            "version = {}\n[server]\nhost = \"127.0.0.1\"\n",
            routectl_router::CURRENT_CONFIG_VERSION
        );
        preflight(&clean).expect("control: the file without the key passes");

        let err = preflight(&format!("{clean}[fidelity.paid_probe_daily_caps]\nx = 1\n"))
            .expect_err("the removed key must be refused");

        assert!(
            err.to_string()
                .contains("paid probes were removed; delete this key"),
            "err: {err}"
        );
    }

    fn doc_of(text: &str) -> DocumentMut {
        text.parse::<DocumentMut>().unwrap()
    }

    /// The union is what lets repeated writers grow one pool without either
    /// knowing what the other wrote: a rerun must add no member and change
    /// no byte.
    #[test]
    fn upsert_pool_members_unions_without_duplicating_and_is_byte_idempotent() {
        // Arrange
        let mut doc = doc_of("version = 4\n");

        // Act
        upsert_pool_members(&mut doc, "anthropic", &["a", "b"]);
        let after_first = doc.to_string();
        upsert_pool_members(&mut doc, "anthropic", &["b", "c"]);
        let after_second = doc.to_string();
        upsert_pool_members(&mut doc, "anthropic", &["a", "b", "c"]);

        // Assert
        assert!(
            after_first.contains(r#"members = ["a", "b"]"#),
            "{after_first}"
        );
        assert!(
            after_second.contains(r#"members = ["a", "b", "c"]"#),
            "{after_second}"
        );
        assert_eq!(doc.to_string(), after_second, "a rerun must change no byte");
    }

    /// The union covers duplicates WITHIN one call's member list too, not
    /// just against what the block already carries: a caller assembling
    /// the list from two sources (an entry name plus a pool's members)
    /// must not be able to write a duplicate.
    #[test]
    fn upsert_pool_members_drops_duplicates_within_one_calls_member_list() {
        // Arrange
        let mut doc = doc_of("version = 4\n");

        // Act
        upsert_pool_members(&mut doc, "anthropic", &["a", "a", "b", "a"]);

        // Assert
        assert!(doc.to_string().contains(r#"members = ["a", "b"]"#), "{doc}");
    }

    /// Everything else on the block is the operator's statement and
    /// survives a member union verbatim -- including the growth marker,
    /// which login never flips.
    #[test]
    fn upsert_pool_members_leaves_seat_selection_and_the_growth_marker_untouched() {
        // Arrange
        let mut doc = doc_of(
            "version = 4\n\
             [pools.anthropic]\n\
             members = [\"a\"]\n\
             seat_selection = \"round-robin\" # spread the load\n\
             accepts_new_logins = false\n",
        );

        // Act
        upsert_pool_members(&mut doc, "anthropic", &["b"]);

        // Assert
        let out = doc.to_string();
        assert!(out.contains(r#"members = ["a", "b"]"#), "{out}");
        assert!(
            out.contains("seat_selection = \"round-robin\" # spread the load"),
            "{out}"
        );
        assert!(out.contains("accepts_new_logins = false"), "{out}");
    }

    /// An inline `pools = { ... }` shape must be walked as well as the
    /// standard-table one, or a member union silently no-ops.
    #[test]
    fn upsert_pool_members_descends_into_an_inline_pools_table() {
        // Arrange
        let mut doc = doc_of("version = 4\npools = { anthropic = { members = [\"a\"] } }\n");

        // Act
        upsert_pool_members(&mut doc, "anthropic", &["b"]);

        // Assert
        assert!(doc.to_string().contains(r#""a", "b""#), "{doc}");
    }
}
