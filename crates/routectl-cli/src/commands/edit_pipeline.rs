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
use routectl_router::{
    Config, ConfigWriteError, ProviderEntry, parse_config, preflight_raw_config,
};
use toml_edit::{Array, DocumentMut, Item, Table, TableLike};

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

/// The shared validation gate: the loader's raw-config preflight, then
/// `parse_config` (free did-you-mean), then the centralized validator suite
/// the reload path also runs. Returns the validated `Config` or the rendered
/// error lines. The preflight runs on the CANDIDATE, not only the snapshot:
/// an edit can itself produce a file the loader refuses (a `version` edited
/// out of range or removed), and writing it would leave the daemon unable to
/// restart. The `parse_config` error is stripped of its verbatim source-line
/// preview and value-bearing clauses first -- toml/serde echo the offending
/// config line into the diagnostic, and that line could carry a `literal:`
/// credential.
pub(crate) fn gate(candidate_text: &str) -> std::result::Result<Config, Vec<String>> {
    preflight_raw_config(candidate_text).map_err(|e| vec![e.to_string()])?;
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
///
/// # Errors
///
/// `pools`, the `[pools.<pool>]` block or its `members` exists with a shape
/// that cannot hold the edit (not a table, not an array). The caller asked
/// for a pool edit; leaving the document unchanged would confirm a write
/// that never happened.
pub(crate) fn upsert_pool_members(
    doc: &mut DocumentMut,
    pool: &str,
    members: &[&str],
) -> Result<()> {
    let block = pool_block_mut(doc, pool)?;
    if !block.contains_key("members") {
        block.insert("members", toml_edit::value(Array::new()));
    }
    let arr = block
        .get_mut("members")
        .and_then(Item::as_array_mut)
        .ok_or_else(|| {
            Error::Config(format!("`pools.{pool}.members` exists but is not an array"))
        })?;
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
    Ok(())
}

/// Mark `[pools.<pool>]` as one future logins may grow, creating the block
/// when absent. Only a writer that CREATES the pool calls this: an existing
/// pool's marker, or its absence, is the operator's statement.
///
/// # Errors
///
/// `pools` or the pool block exists but is not a table.
pub(crate) fn set_accepts_new_logins(doc: &mut DocumentMut, pool: &str) -> Result<()> {
    pool_block_mut(doc, pool)?.insert("accepts_new_logins", toml_edit::value(true));
    Ok(())
}

/// The `[pools.<pool>]` block, creating `pools` and the block when absent.
/// Descends through `as_table_like_mut`, so an inline `pools = { ... }`
/// is walked like a standard table.
fn pool_block_mut<'a>(doc: &'a mut DocumentMut, pool: &str) -> Result<&'a mut dyn TableLike> {
    let root = doc.as_table_mut();
    if !root.contains_key("pools") {
        let mut table = Table::new();
        table.set_implicit(true);
        root.insert("pools", Item::Table(table));
    }
    let pools = root
        .get_mut("pools")
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| Error::Config("`pools` exists but is not a table".into()))?;
    if !pools.contains_key(pool) {
        pools.insert(pool, Item::Table(Table::new()));
    }
    pools
        .get_mut(pool)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| Error::Config(format!("`pools.{pool}` exists but is not a table")))
}

/// Give `entry`, which is about to join `pool`, the cloak keys the pool's
/// FIRST member (in declared `members` order) sets explicitly: both
/// `cloak_mode` and `cloak_strict_mode` are copied as written, so a key
/// that member leaves unset stays unset here.
///
/// Explicit keys only, never the effective defaults: a copied default
/// would become an override that silently changes meaning when the
/// default does. A validated config already holds every member to one
/// effective cloak, so matching the first member matches them all.
///
/// A pool or first member `config` does not hold leaves `entry` untouched.
pub(crate) fn inherit_pool_cloak(config: &Config, pool: &str, entry: &mut ProviderEntry) {
    let Some(first) = config
        .pools
        .get(pool)
        .and_then(|pool| pool.members.first())
        .and_then(|member| config.providers.get(member))
    else {
        return;
    };
    entry.cloak_mode = first.cloak_mode;
    entry.cloak_strict_mode = first.cloak_strict_mode;
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
        let rows: [(&str, String, ClassMatcher); 4] = [
            ("too old", at(current - 1, ""), |e| {
                matches!(e, Refusal::Version(_))
            }),
            ("too new", at(current + 1, ""), |e| {
                matches!(e, Refusal::Version(_))
            }),
            ("retired table", at(current, "[bedrock]\n"), |e| {
                matches!(e, Refusal::RetiredKeys(_))
            }),
            (
                "several retired keys",
                at(
                    current,
                    "[mitm]\ncredential_source = \"forwarded\"\n\
                     [fidelity.legacy]\nx = 1\n\
                     [models.m]\neffort_levels = []\n",
                ),
                |e| matches!(e, Refusal::RetiredKeys(_)),
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

    /// Every retired key in the file is named in the one refusal, not only
    /// the first.
    #[test]
    fn preflight_names_every_retired_key_at_once() {
        let clean = format!(
            "version = {}\n[server]\nhost = \"127.0.0.1\"\n",
            routectl_router::CURRENT_CONFIG_VERSION
        );
        preflight(&clean).expect("control: the file without the keys passes");

        let err = preflight(&format!("{clean}[fidelity.legacy]\nx = 1\n[bedrock]\n"))
            .expect_err("the retired keys must be refused");

        let shown = err.to_string();
        assert!(shown.contains("`bedrock` is retired: "), "{shown}");
        assert!(shown.contains("`fidelity` is retired: "), "{shown}");
        assert_eq!(shown.lines().count(), 2, "{shown}");
    }

    /// The edit gate refuses a candidate setting `[server] max_body_bytes =
    /// 0` with the shared validator's line, and accepts `1`.
    #[test]
    fn gate_refuses_a_zero_body_limit_and_accepts_one_byte() {
        let candidate = |value: u32| {
            format!(
                "version = {}\n[server]\nmax_body_bytes = {value}\n",
                routectl_router::CURRENT_CONFIG_VERSION
            )
        };

        let refused = gate(&candidate(0)).expect_err("zero must be refused");
        let accepted = gate(&candidate(1)).expect("one byte must pass the gate");

        assert!(
            refused
                .iter()
                .any(|e| e.contains("`server.max_body_bytes` is 0; omit it for the 32 MiB default")),
            "{refused:?}"
        );
        assert_eq!(accepted.server.max_body_bytes, 1);
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
        upsert_pool_members(&mut doc, "anthropic", &["a", "b"]).expect("upsert");
        let after_first = doc.to_string();
        upsert_pool_members(&mut doc, "anthropic", &["b", "c"]).expect("upsert");
        let after_second = doc.to_string();
        upsert_pool_members(&mut doc, "anthropic", &["a", "b", "c"]).expect("upsert");

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
        upsert_pool_members(&mut doc, "anthropic", &["a", "a", "b", "a"]).expect("upsert");

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
             seat_selection = \"fill-first\" # drain one seat at a time\n\
             accepts_new_logins = false\n",
        );

        // Act
        upsert_pool_members(&mut doc, "anthropic", &["b"]).expect("upsert");

        // Assert
        let out = doc.to_string();
        assert!(out.contains(r#"members = ["a", "b"]"#), "{out}");
        assert!(
            out.contains("seat_selection = \"fill-first\" # drain one seat at a time"),
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
        upsert_pool_members(&mut doc, "anthropic", &["b"]).expect("upsert");

        // Assert
        assert!(doc.to_string().contains(r#""a", "b""#), "{doc}");
    }

    /// A child created under an inline parent stays inline and leaves the
    /// file reparsing; a standard parent gets a standard child (control).
    #[test]
    fn a_child_created_under_an_inline_parent_is_inline_and_reparses() {
        let block = || {
            let mut block = Table::new();
            block.insert("format", toml_edit::value("anthropic-messages"));
            block
        };
        type Edit = fn(&mut DocumentMut, Table) -> Result<()>;
        let rows: [(&str, &str, Edit, &str); 4] = [
            (
                "pools = {}",
                "version = 4\npools = {}\n",
                |doc, _| {
                    upsert_pool_members(doc, "p", &["a"])?;
                    set_accepts_new_logins(doc, "p")
                },
                "version = 4\npools = { p = { members = [\"a\"], accepts_new_logins = true } }\n",
            ),
            (
                "providers = {}",
                "version = 4\nproviders = {}\n",
                |doc, block| insert_provider_block(doc, "a", block),
                "version = 4\nproviders = { a = { format = \"anthropic-messages\" } }\n",
            ),
            (
                "standard pools control",
                "version = 4\n[pools]\n",
                |doc, _| {
                    upsert_pool_members(doc, "p", &["a"])?;
                    set_accepts_new_logins(doc, "p")
                },
                "version = 4\n[pools]\n\n[pools.p]\nmembers = [\"a\"]\naccepts_new_logins = true\n",
            ),
            (
                "standard providers control",
                "version = 4\n[providers]\n",
                |doc, block| insert_provider_block(doc, "a", block),
                "version = 4\n[providers]\n\n[providers.a]\nformat = \"anthropic-messages\"\n",
            ),
        ];

        for (row, text, edit, expected) in rows {
            // Arrange
            let mut doc = doc_of(text);

            // Act
            edit(&mut doc, block()).unwrap_or_else(|e| panic!("row `{row}`: {e}"));

            // Assert
            let out = doc.to_string();
            assert_eq!(out, expected, "row `{row}`");
            out.parse::<DocumentMut>()
                .unwrap_or_else(|e| panic!("row `{row}` must reparse: {e}"));
        }
    }

    /// A pool target whose shape cannot hold the edit is an error, never a
    /// silent no-op that would confirm a write which did not happen. The
    /// well-formed control shows the same call succeeding.
    #[test]
    fn a_pool_edit_on_a_malformed_pool_target_is_an_error() {
        let rows = [
            ("pools is a number", "version = 4\npools = 5\n"),
            ("the pool is a string", "version = 4\n[pools]\np = \"x\"\n"),
            (
                "members is a number",
                "version = 4\n[pools.p]\nmembers = 5\n",
            ),
        ];
        let mut control = doc_of("version = 4\n[pools.p]\nmembers = [\"a\"]\n");
        upsert_pool_members(&mut control, "p", &["b"]).expect("control: a well-formed pool grows");

        for (row, text) in rows {
            // Arrange
            let mut doc = doc_of(text);

            // Act
            let result = upsert_pool_members(&mut doc, "p", &["b"]);

            // Assert
            assert!(result.is_err(), "row `{row}` must be an error");
            assert_eq!(
                doc.to_string(),
                text,
                "row `{row}` must leave the document alone"
            );
        }
    }

    #[test]
    fn marking_a_pool_whose_target_is_not_a_table_is_an_error() {
        let mut control = doc_of("version = 4\n");
        set_accepts_new_logins(&mut control, "p").expect("control: an absent pool is created");
        assert!(
            control.to_string().contains("accepts_new_logins = true"),
            "{control}"
        );

        for text in [
            "version = 4\npools = 5\n",
            "version = 4\n[pools]\np = \"x\"\n",
        ] {
            let mut doc = doc_of(text);

            assert!(set_accepts_new_logins(&mut doc, "p").is_err(), "{text}");
        }
    }

    fn pool_config(members: &[(&str, &str)]) -> Config {
        let mut text = String::new();
        for (name, cloak_lines) in members {
            text.push_str(&format!(
                "[providers.{name}]\n\
                 format = \"anthropic-messages\"\n\
                 base_url = \"https://api.anthropic.com\"\n\
                 credential = \"oauth://anthropic#{name}\"\n\
                 auth = \"bearer\"\n\
                 {cloak_lines}"
            ));
        }
        let names: Vec<String> = members.iter().map(|(n, _)| format!("\"{n}\"")).collect();
        text.push_str(&format!("[pools.p]\nmembers = [{}]\n", names.join(", ")));
        toml::from_str(&text).expect("fixture config parses")
    }

    fn joining_entry() -> ProviderEntry {
        ProviderEntry::for_managed_login("anthropic", "oauth://anthropic#new".to_string())
            .expect("anthropic is a managed login")
    }

    /// Only the keys the FIRST declared member writes are copied, as
    /// written. A later member's explicit key, even one that equals the
    /// effective default the first member inherits, is never copied: it
    /// would become an override the operator did not write on this pool's
    /// leading seat.
    #[test]
    fn a_joining_entry_takes_exactly_the_first_members_explicit_cloak_keys() {
        use routectl_providers::anthropic_api::CloakMode;
        type Row<'a> = (
            &'a str,
            Vec<(&'a str, &'a str)>,
            Option<CloakMode>,
            Option<bool>,
        );
        let rows: [Row<'_>; 4] = [
            (
                "both keys set",
                vec![("a", "cloak_mode = \"never\"\ncloak_strict_mode = true\n")],
                Some(CloakMode::Never),
                Some(true),
            ),
            (
                "mode only",
                vec![("a", "cloak_mode = \"always\"\n")],
                Some(CloakMode::Always),
                None,
            ),
            ("no keys", vec![("a", "")], None, None),
            (
                "a later member spells out the default",
                vec![
                    ("a", ""),
                    ("b", "cloak_mode = \"auto\"\ncloak_strict_mode = false\n"),
                ],
                None,
                None,
            ),
        ];

        for (row, members, mode, strict) in rows {
            // Arrange
            let config = pool_config(&members);
            let mut entry = joining_entry();

            // Act
            inherit_pool_cloak(&config, "p", &mut entry);

            // Assert
            assert_eq!(entry.cloak_mode, mode, "row `{row}`");
            assert_eq!(entry.cloak_strict_mode, strict, "row `{row}`");
        }
    }

    /// Declared order, not map order: the first member is the one listed
    /// first, even when its name sorts last.
    #[test]
    fn the_first_member_is_the_first_one_the_pool_lists() {
        // Arrange
        let config = pool_config(&[
            ("z", "cloak_strict_mode = true\n"),
            ("a", "cloak_strict_mode = false\n"),
        ]);
        let mut entry = joining_entry();

        // Act
        inherit_pool_cloak(&config, "p", &mut entry);

        // Assert
        assert_eq!(entry.cloak_strict_mode, Some(true));
    }
}
