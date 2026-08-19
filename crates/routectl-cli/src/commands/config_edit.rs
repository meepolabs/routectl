//! `routectl config set` / `config unset` -- the single write pipeline for
//! `config.toml` edits, routed through the shared [`edit_config_toml`]
//! primitive.
//!
//! Every step runs IN MEMORY before any byte touches disk, so any refusal
//! (a stale schema version, an unknown key, a value that fails the shared
//! validation gate) leaves the file byte-identical:
//!
//!   1. RAW version preflight on the snapshot bytes FIRST -- a config whose
//!      `version` is out of bounds (older or newer than this build writes)
//!      is refused here with the single-sourced `preflight_config_version`
//!      message, before any shared-loader call. A too-old file is never
//!      migrated in place. The legacy-key preflight runs raw alongside it.
//!   2. Schema-driven path validation ([`validate_config_path`]); `set`
//!      requires a scalar leaf (a table target is rejected), while `unset`
//!      may also target a whole table node to drop an override block.
//!   3. Parse the snapshot to `prev` for the diff classifiers.
//!   4. Scalar type inference on the raw value (bool / int / float / string).
//!   5. Apply the edit to a candidate document and run the SAME gate the
//!      reload path runs (`parse_config` + `collect_config_validation`);
//!      any failure renders via the shared report machinery and writes
//!      nothing.
//!   6. Classify the diff: a high-consequence (egress-defining) change
//!      prompts for confirmation BEFORE the lock is acquired (`--yes`
//!      bypasses); restart-required field names are remembered for output.
//!   7. [`edit_config_toml`] re-reads under the lock, re-applies the same
//!      deterministic mutation, and commits atomically (or writes nothing
//!      on a no-op).
//!   8. On a real write, emit exactly one audit event -- surface, verb,
//!      dotted path, restart-required list, high-consequence bool, NEVER
//!      the value (it may be a literal secret) -- and print the
//!      restart-required field names (or a short ok line).
//!
//! The pipeline is factored around [`run`] + [`EditKind`] so `config unset`
//! reuses it: it shares steps 1-3 and 6-8 and swaps only the mutation.

use std::path::Path;

use routectl_core::{Error, Result};
use routectl_router::{
    EditOutcome, PathError, PathShape, edit_config_toml, parse_config, validate_config_path,
};
use toml_edit::{DocumentMut, Item, Table};

use super::edit_pipeline::{
    RelockValidationError, confirm_high_consequence, gate, preflight, render_gate_errors,
    render_write_error,
};
use crate::config_classify::{collect_high_consequence_changes, collect_restart_required_changes};

/// What edit the pipeline performs. `Set` carries the raw value string
/// (scalar type inferred). `Unset` removes a key and prunes any parent
/// tables the removal empties.
pub enum EditKind {
    /// Assign a scalar value to a leaf key.
    Set(String),
    /// Remove a key (or a whole override table), pruning now-empty parents.
    Unset,
}

/// Outcome of a completed [`run`], for the caller and for tests. Failures
/// (version refusal, unknown path, gate rejection, conflict) surface as
/// `Err` instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetResult {
    /// The file was rewritten. `restart_required` lists the changed fields
    /// that a running daemon applies only on restart (empty = live).
    Written {
        restart_required: Vec<String>,
        high_consequence: bool,
    },
    /// The requested value already matched; nothing was written.
    NoChange,
    /// A high-consequence edit was declined at the confirmation prompt.
    Aborted,
}

/// Run the config-edit pipeline against `config_path`. Prints the human
/// result line and emits the audit event on a real write; returns the
/// structured outcome for the caller/tests. Validation and refusal paths
/// render their diagnostics and return `Err`.
pub fn run(config_path: &Path, dotted_path: &str, kind: EditKind, yes: bool) -> Result<SetResult> {
    let verb = match kind {
        EditKind::Set(_) => "set",
        EditKind::Unset => "unset",
    };

    let snapshot = std::fs::read(config_path).map_err(|e| {
        Error::Config(format!(
            "cannot read config `{}`: {e}",
            config_path.display()
        ))
    })?;
    let snapshot_text = String::from_utf8(snapshot.clone()).map_err(|e| {
        Error::Config(format!(
            "config `{}` is not UTF-8: {e}",
            config_path.display()
        ))
    })?;

    preflight(&snapshot_text)?;

    let shape = validate_config_path(dotted_path).map_err(render_path_error)?;
    if matches!(shape, PathShape::Table) && matches!(kind, EditKind::Set(_)) {
        return Err(Error::Config(format!(
            "`{dotted_path}` names a table, not a scalar value; set a leaf key beneath it"
        )));
    }

    let prev = parse_config(&snapshot_text).map_err(|e| {
        Error::Config(format!(
            "current config does not parse; fix it before editing: {e}"
        ))
    })?;

    let segments: Vec<&str> = dotted_path.split('.').collect();

    // Build and gate the candidate in memory.
    let candidate_text = {
        let mut doc = parse_document(&snapshot_text)?;
        apply_edit(&mut doc, &segments, &kind);
        doc.to_string()
    };
    let next = gate(&candidate_text).map_err(|errors| {
        render_gate_errors(&errors);
        Error::Config(format!("{} config error(s)", errors.len()))
    })?;

    let restart_required: Vec<String> = collect_restart_required_changes(&prev, &next)
        .into_iter()
        .map(String::from)
        .collect();
    let high = collect_high_consequence_changes(&prev, &next);
    let is_high = !high.is_empty();

    if is_high && !confirm_high_consequence(&high, yes) {
        println!("aborted; nothing written.");
        return Ok(SetResult::Aborted);
    }

    let result = edit_config_toml::<RelockValidationError, _>(config_path, &snapshot, |doc| {
        apply_edit(doc, &segments, &kind);
        if doc.to_string() == snapshot_text {
            return Ok(EditOutcome::Unchanged);
        }
        let text = doc.to_string();
        match gate(&text) {
            Ok(_) => Ok(EditOutcome::Modified),
            Err(_) => Err(RelockValidationError),
        }
    })
    .map_err(render_write_error)?;

    if result.outcome == EditOutcome::Unchanged {
        match kind {
            EditKind::Set(_) => {
                println!("`{dotted_path}` already set to that value; nothing written.")
            }
            EditKind::Unset => println!("`{dotted_path}` is not set; nothing written."),
        }
        return Ok(SetResult::NoChange);
    }

    tracing::info!(
        surface = "cli",
        verb,
        path = %dotted_path,
        restart_required = ?restart_required,
        high_consequence = is_high,
        "config edit committed",
    );

    if restart_required.is_empty() {
        println!("updated `{dotted_path}`.");
    } else {
        println!(
            "updated `{dotted_path}`; restart required for: {}",
            restart_required.join(", ")
        );
    }

    Ok(SetResult::Written {
        restart_required,
        high_consequence: is_high,
    })
}

/// Map a schema path-validation failure to a user error, giving array
/// targets the hand-edit-only message this version's scope calls for.
fn render_path_error(err: PathError) -> Error {
    match err {
        PathError::ArrayTarget { segment } => Error::Config(format!(
            "cannot set array `{segment}`: array values are hand-edit-only in this version"
        )),
        other => Error::Config(other.to_string()),
    }
}

fn parse_document(text: &str) -> Result<DocumentMut> {
    text.parse::<DocumentMut>()
        .map_err(|e| Error::Config(format!("config does not parse: {e}")))
}

/// Apply the kind's mutation to `doc`. Deterministic given the same input
/// document (the write closure relies on this).
fn apply_edit(doc: &mut DocumentMut, segments: &[&str], kind: &EditKind) {
    match kind {
        EditKind::Set(raw) => set_leaf(doc, segments, infer_scalar(raw)),
        EditKind::Unset => {
            remove_and_prune(doc.as_table_mut(), segments);
        }
    }
}

/// Assign `value` at the leaf named by `segments`, creating any missing
/// intermediate as a standard (non-inline) table. Existing intermediates of
/// either table shape descend via `toml_edit`'s `Index` impl (which
/// supports inline tables); the missing-check uses `as_table_like` so an
/// inline intermediate is recognized as present rather than shadowed by a
/// fresh standard table. The final `node[*last] = value` replaces only the
/// value item, leaving each key's own decoration (leading comments,
/// ordering) intact so a targeted edit preserves formatting.
fn set_leaf(doc: &mut DocumentMut, segments: &[&str], value: Item) {
    let (last, parents) = segments.split_last().expect("validated path is non-empty");
    let mut node: &mut Item = doc.as_item_mut();
    for seg in parents {
        let missing = node.as_table_like().is_some_and(|t| !t.contains_key(seg));
        if missing && let Some(table) = node.as_table_mut() {
            table.insert(seg, Item::Table(Table::new()));
        }
        node = &mut node[*seg];
    }
    node[*last] = value;
}

/// Remove the key named by `segments` from `table`, then recursively prune
/// any parent table the removal leaves empty (contract rule 4: an empty
/// override table is indistinguishable from an absent one). A parent that
/// keeps sibling keys or sub-tables survives. Handles standard AND inline
/// tables (`aliases = { default = "m1" }`) via `TableLike` -- an inline map
/// is still a map. A path whose intermediates do not resolve to a table
/// shape is a no-op (missing key -> nothing written). Returns whether
/// `table` is empty after the operation.
fn remove_and_prune(table: &mut dyn toml_edit::TableLike, segments: &[&str]) -> bool {
    let (first, rest) = segments.split_first().expect("validated path is non-empty");
    if rest.is_empty() {
        table.remove(first);
    } else if let Some(child) = table.get_mut(first).and_then(Item::as_table_like_mut)
        && remove_and_prune(child, rest)
    {
        table.remove(first);
    }
    table.is_empty()
}

/// Infer a scalar TOML value from the raw string: `true`/`false` -> bool,
/// then `i64`, then `f64`, else a string. The re-parse + re-validate gate
/// is the backstop for a mistyped value, so inference stays deliberately
/// simple (no schema-driven typing).
fn infer_scalar(raw: &str) -> Item {
    if raw == "true" {
        toml_edit::value(true)
    } else if raw == "false" {
        toml_edit::value(false)
    } else if let Ok(i) = raw.parse::<i64>() {
        toml_edit::value(i)
    } else if let Ok(f) = raw.parse::<f64>() {
        toml_edit::value(f)
    } else {
        toml_edit::value(raw)
    }
}

#[cfg(test)]
mod tests {
    use routectl_router::CURRENT_CONFIG_VERSION;

    use super::*;

    /// A minimal valid config at the version this build writes, rendered
    /// from the const so the next schema bump needs no fixture edit here.
    fn current_base() -> String {
        format!("version = {CURRENT_CONFIG_VERSION}\n\n{BASE_BODY}")
    }

    const BASE_BODY: &str = "\
[server]
host = \"127.0.0.1\"
port = 8787

[providers.fast]
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"literal:test-key\"

[models.gpt]
provider = \"fast\"
upstream = \"gpt-4o\"

[aliases]
default = \"gpt\"
";

    fn write_config(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("config.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    fn set(path: &std::path::Path, dotted: &str, value: &str) -> Result<SetResult> {
        run(path, dotted, EditKind::Set(value.to_string()), true)
    }

    fn unset(path: &std::path::Path, dotted: &str) -> Result<SetResult> {
        run(path, dotted, EditKind::Unset, true)
    }

    // -----------------------------------------------------------------
    // Scalar inference
    // -----------------------------------------------------------------

    #[test]
    fn infers_bool_int_float_string() {
        assert!(infer_scalar("true").as_bool().unwrap());
        assert!(!infer_scalar("false").as_bool().unwrap());
        assert_eq!(infer_scalar("42").as_integer().unwrap(), 42);
        assert!((infer_scalar("2.5").as_float().unwrap() - 2.5).abs() < f64::EPSILON);
        assert_eq!(infer_scalar("hello").as_str().unwrap(), "hello");
        // A dotted numeric string that is not an i64 falls to f64, not string.
        assert!(infer_scalar("1.0").as_float().is_some());
    }

    // -----------------------------------------------------------------
    // Format-preserving happy path
    // -----------------------------------------------------------------

    #[test]
    fn edits_leaf_preserving_comments_and_order() {
        let dir = tempfile::tempdir().unwrap();
        let body = format!(
            "\
# operator note
version = {CURRENT_CONFIG_VERSION}

[retry]
max_attempts = 2

[retry.classes.server-error]
# tuned for this upstream
retry = 2

[providers.fast]
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"literal:test-key\"

[models.gpt]
provider = \"fast\"
upstream = \"gpt-4o\"

[aliases]
default = \"gpt\"
"
        );
        let path = write_config(dir.path(), &body);

        let result = set(&path, "retry.classes.server-error.retry", "4").expect("set");
        assert!(matches!(result, SetResult::Written { .. }));

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("retry = 4"), "{on_disk}");
        assert!(on_disk.contains("# operator note"), "{on_disk}");
        assert!(on_disk.contains("# tuned for this upstream"), "{on_disk}");
        // Order: the note still precedes the retry section header.
        assert!(
            on_disk.find("# operator note").unwrap() < on_disk.find("[retry]").unwrap(),
            "{on_disk}"
        );
    }

    // -----------------------------------------------------------------
    // No-write matrix: every case leaves the file byte-identical + Err.
    // -----------------------------------------------------------------

    fn assert_no_write(body: &str, dotted: &str, value: &str) {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), body);
        let before = std::fs::read(&path).unwrap();

        let err = set(&path, dotted, value);
        assert!(err.is_err(), "expected refusal for `{dotted} = {value}`");

        let after = std::fs::read(&path).unwrap();
        assert_eq!(before, after, "file must be byte-identical after refusal");
    }

    #[test]
    fn bad_type_leaves_file_unchanged() {
        assert_no_write(&current_base(), "server.port", "not-a-number");
    }

    #[test]
    fn unknown_path_leaves_file_unchanged() {
        assert_no_write(&current_base(), "server.nope", "1");
    }

    #[test]
    fn array_target_leaves_file_unchanged() {
        assert_no_write(&current_base(), "bedrock.allowed_betas", "429");
    }

    #[test]
    fn invalid_cross_field_leaves_file_unchanged() {
        // An alias pointing at an undefined model target passes the parse
        // but fails the shared cross-field validator suite.
        assert_no_write(&current_base(), "aliases.broken", "no-such-model");
    }

    #[test]
    fn legacy_key_trip_leaves_file_unchanged() {
        let body = format!(
            "\
version = {CURRENT_CONFIG_VERSION}

[server]
host = \"127.0.0.1\"
port = 8787

[mitm]
credential_source = \"forwarded\"

[providers.fast]
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"literal:test-key\"

[models.gpt]
provider = \"fast\"
upstream = \"gpt-4o\"

[aliases]
default = \"gpt\"
"
        );
        assert_no_write(&body, "server.port", "9999");
    }

    // -----------------------------------------------------------------
    // Migration refusal: a v1 file is refused, byte-identical, unstamped.
    // -----------------------------------------------------------------

    #[test]
    fn v1_file_is_refused_byte_identical() {
        let body = "\
[server]
host = \"127.0.0.1\"
port = 8787

[providers.fast]
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"literal:test-key\"

[models.gpt]
provider = \"fast\"
upstream = \"gpt-4o\"

[aliases]
default = \"gpt\"
";
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), body);

        let err = set(&path, "server.port", "9999").expect_err("v1 must be refused");
        assert!(err.to_string().contains("version"), "err: {err}");

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, body, "v1 file must be byte-identical after refusal");
        assert!(
            !after.contains(&format!("version = {CURRENT_CONFIG_VERSION}")),
            "no version stamp may be written"
        );
    }

    // -----------------------------------------------------------------
    // Restart-required + no-op
    // -----------------------------------------------------------------

    #[test]
    fn restart_required_field_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), &current_base());

        let result = set(&path, "usage.retention_days", "30").expect("set");
        match result {
            SetResult::Written {
                restart_required, ..
            } => {
                assert!(
                    restart_required.iter().any(|f| f == "usage.retention_days"),
                    "got {restart_required:?}"
                );
            }
            other => panic!("expected Written, got {other:?}"),
        }
    }

    #[test]
    fn no_op_set_reports_no_change_and_no_restart_notice() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), &current_base());
        let before = std::fs::read(&path).unwrap();

        let result = set(&path, "server.port", "8787").expect("set");
        assert_eq!(result, SetResult::NoChange);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "no-op must not rewrite"
        );
    }

    // -----------------------------------------------------------------
    // High-consequence prompt / --yes bypass
    // -----------------------------------------------------------------

    #[test]
    fn high_consequence_edit_bypassed_by_yes() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), &current_base());

        let result =
            set(&path, "providers.fast.base_url", "http://127.0.0.1:2").expect("set with --yes");
        assert!(
            matches!(
                result,
                SetResult::Written {
                    high_consequence: true,
                    ..
                }
            ),
            "got {result:?}"
        );
    }

    #[test]
    fn high_consequence_edit_declined_when_not_confirmed() {
        // yes=false and a non-interactive stdin (EOF) -> confirm returns
        // false -> abort with the file untouched.
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), &current_base());
        let before = std::fs::read(&path).unwrap();

        let result = run(
            &path,
            "providers.fast.base_url",
            EditKind::Set("http://127.0.0.1:2".to_string()),
            false,
        )
        .expect("declining is not an error");
        assert_eq!(result, SetResult::Aborted);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "decline must not write"
        );
    }

    // -----------------------------------------------------------------
    // Audit event: exactly one, with the required fields and NO value.
    // -----------------------------------------------------------------

    #[test]
    fn emits_one_audit_event_without_the_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), &current_base());

        let events = routectl_testkit::capture_events(|| {
            set(&path, "usage.retention_days", "30").expect("set");
        });

        let audit: Vec<_> = events
            .iter()
            .filter(|e| e.field("surface") == Some("cli") && e.field("verb") == Some("set"))
            .collect();
        assert_eq!(audit.len(), 1, "exactly one audit event expected");

        let event = audit[0];
        assert_eq!(event.field("path"), Some("usage.retention_days"));
        assert_eq!(event.field("high_consequence"), Some("false"));
        assert!(
            event
                .field("restart_required")
                .unwrap()
                .contains("usage.retention_days"),
            "restart_required must list the field"
        );
        // The value the operator typed is never a structured field.
        assert!(
            event.field("value").is_none(),
            "the value must never be audited"
        );
    }

    // -----------------------------------------------------------------
    // config unset -- removal + recursive empty-parent prune
    // -----------------------------------------------------------------

    use routectl_core::failure_class::FailureClass;

    /// Config whose only retry override is a single sparse class leaf, so
    /// removing it empties the whole `[retry.classes.*]` chain.
    fn retry_override() -> String {
        format!("version = {CURRENT_CONFIG_VERSION}\n\n{RETRY_OVERRIDE_BODY}")
    }

    const RETRY_OVERRIDE_BODY: &str = "\
[server]
host = \"127.0.0.1\"
port = 8787

[retry.classes.server-error]
retry = 5

[providers.fast]
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"literal:test-key\"

[models.gpt]
provider = \"fast\"
upstream = \"gpt-4o\"

[aliases]
default = \"gpt\"
";

    #[test]
    fn unset_removes_override_falling_back_to_baked_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), &retry_override());

        // The override is in force before removal.
        let before = parse_config(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(before.retry.resolved_class(&FailureClass::ServerError).0, 5);

        let result = unset(&path, "retry.classes.server-error.retry").expect("unset");
        assert!(matches!(result, SetResult::Written { .. }));

        // Re-parse: the key is gone and the class resolves to the baked
        // default (max_attempts, no override present).
        let text = std::fs::read_to_string(&path).unwrap();
        let after = parse_config(&text).unwrap();
        assert!(
            after.retry.classes.is_empty(),
            "override table must be absent: {text}"
        );
        assert_eq!(
            after.retry.resolved_class(&FailureClass::ServerError).0,
            after.retry.max_attempts,
        );
    }

    #[test]
    fn unset_prunes_all_now_empty_parent_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), &retry_override());

        unset(&path, "retry.classes.server-error.retry").expect("unset");

        // server-error emptied -> classes emptied -> retry emptied: the
        // whole chain is pruned, no orphan headers left behind.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("[retry"),
            "no retry header may survive: {text}"
        );
        assert!(!text.contains("server-error"), "{text}");
    }

    #[test]
    fn unset_reaches_into_an_inline_table() {
        // A top-level `aliases = { ... }` inline table: unset must treat it
        // as the map it is, not silently no-op. (Bare keys precede section
        // headers in TOML, hence the fixture layout.)
        let dir = tempfile::tempdir().unwrap();
        let body = current_base()
            .replace(
                &format!("version = {CURRENT_CONFIG_VERSION}\n"),
                &format!(
                    "version = {CURRENT_CONFIG_VERSION}\naliases = {{ default = \"gpt\", \
                     second = \"gpt\" }}\n"
                ),
            )
            .replace("[aliases]\ndefault = \"gpt\"\n", "");
        let path = write_config(dir.path(), &body);

        let result = unset(&path, "aliases.second").expect("unset");

        assert!(matches!(result, SetResult::Written { .. }), "{result:?}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("second"), "{text}");
        assert!(text.contains("default"), "sibling must survive: {text}");
    }

    #[test]
    fn set_reaches_into_an_inline_table() {
        let dir = tempfile::tempdir().unwrap();
        let body = current_base()
            .replace(
                &format!("version = {CURRENT_CONFIG_VERSION}\n"),
                &format!("version = {CURRENT_CONFIG_VERSION}\naliases = {{ default = \"gpt\" }}\n"),
            )
            .replace("[aliases]\ndefault = \"gpt\"\n", "");
        let path = write_config(dir.path(), &body);

        set(&path, "aliases.default", "gpt").expect("noop set parses the inline map");
        // Different value writes through the inline table.
        let result = set(&path, "aliases.second", "gpt").expect("set");
        assert!(matches!(result, SetResult::Written { .. }), "{result:?}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("second"), "{text}");
    }

    #[test]
    fn unset_keeps_parent_with_a_surviving_sibling_key() {
        let dir = tempfile::tempdir().unwrap();
        let body = format!(
            "\
version = {CURRENT_CONFIG_VERSION}

[server]
host = \"127.0.0.1\"
port = 8787

[retry]
max_attempts = 4

[retry.classes.server-error]
retry = 5

[providers.fast]
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"literal:test-key\"

[models.gpt]
provider = \"fast\"
upstream = \"gpt-4o\"

[aliases]
default = \"gpt\"
"
        );
        let path = write_config(dir.path(), &body);

        unset(&path, "retry.classes.server-error.retry").expect("unset");

        let text = std::fs::read_to_string(&path).unwrap();
        // classes emptied and pruned, but [retry] keeps max_attempts.
        assert!(text.contains("max_attempts = 4"), "{text}");
        assert!(text.contains("[retry]"), "{text}");
        assert!(!text.contains("classes"), "{text}");
    }

    #[test]
    fn unset_keeps_parent_with_a_surviving_sibling_table() {
        let dir = tempfile::tempdir().unwrap();
        let body = format!(
            "\
version = {CURRENT_CONFIG_VERSION}

[server]
host = \"127.0.0.1\"
port = 8787

[retry.classes.server-error]
retry = 5

[retry.classes.rate-limited]
retry = 3

[providers.fast]
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"literal:test-key\"

[models.gpt]
provider = \"fast\"
upstream = \"gpt-4o\"

[aliases]
default = \"gpt\"
"
        );
        let path = write_config(dir.path(), &body);

        unset(&path, "retry.classes.server-error.retry").expect("unset");

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("server-error"), "{text}");
        assert!(
            text.contains("rate-limited"),
            "sibling table must survive: {text}"
        );
    }

    #[test]
    fn unset_targeting_a_whole_table_removes_the_block() {
        // PathShape::Table is accepted for unset (rejected only for set):
        // naming the table node drops the whole override block at once.
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), &retry_override());

        let result = unset(&path, "retry.classes.server-error").expect("unset table node");
        assert!(matches!(result, SetResult::Written { .. }));

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("[retry"), "{text}");
    }

    #[test]
    fn unset_preserves_comments_and_order_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let body = format!(
            "\
# operator note
version = {CURRENT_CONFIG_VERSION}

[server]
host = \"127.0.0.1\"
port = 8787

[retry.classes.server-error]
# tuned for this upstream
retry = 5

[providers.fast]
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"literal:test-key\"

[models.gpt]
provider = \"fast\"
upstream = \"gpt-4o\"

[aliases]
default = \"gpt\"
"
        );
        let path = write_config(dir.path(), &body);

        unset(&path, "retry.classes.server-error.retry").expect("unset");

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# operator note"), "{text}");
        assert!(text.contains("[server]"), "{text}");
        assert!(
            text.find("# operator note").unwrap() < text.find("[server]").unwrap(),
            "{text}"
        );
        // The removed section's own comment leaves with it.
        assert!(!text.contains("# tuned for this upstream"), "{text}");
    }

    #[test]
    fn unset_missing_key_reports_no_change_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), &current_base());
        let before = std::fs::read(&path).unwrap();

        let result = unset(&path, "retry.max_attempts").expect("unset missing key");
        assert_eq!(result, SetResult::NoChange);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "removing an absent key must not rewrite"
        );
    }

    #[test]
    fn unset_v1_file_is_refused_byte_identical() {
        let body = "\
[server]
host = \"127.0.0.1\"
port = 8787

[providers.fast]
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"literal:test-key\"

[models.gpt]
provider = \"fast\"
upstream = \"gpt-4o\"

[aliases]
default = \"gpt\"
";
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), body);

        let err = unset(&path, "server.port").expect_err("v1 must be refused");
        assert!(err.to_string().contains("version"), "err: {err}");

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, body, "v1 file must be byte-identical after refusal");
        assert!(
            !after.contains(&format!("version = {CURRENT_CONFIG_VERSION}")),
            "no version stamp may be written"
        );
    }

    #[test]
    fn unset_that_breaks_the_schema_leaves_file_unchanged() {
        // Removing a required leaf (a model's `upstream`, which has no
        // serde default) makes the candidate fail the parse gate, so
        // nothing is written.
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), &current_base());
        let before = std::fs::read(&path).unwrap();

        let err = unset(&path, "models.gpt.upstream");
        assert!(err.is_err(), "removing a required key must be refused");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "gate failure must leave the file byte-identical"
        );
    }

    #[test]
    fn unset_emits_one_audit_event_with_verb_unset_and_no_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), &retry_override());

        let events = routectl_testkit::capture_events(|| {
            unset(&path, "retry.classes.server-error.retry").expect("unset");
        });

        let audit: Vec<_> = events
            .iter()
            .filter(|e| e.field("surface") == Some("cli") && e.field("verb") == Some("unset"))
            .collect();
        assert_eq!(audit.len(), 1, "exactly one unset audit event expected");
        assert_eq!(
            audit[0].field("path"),
            Some("retry.classes.server-error.retry")
        );
        assert!(
            audit[0].field("value").is_none(),
            "the value must never be audited"
        );
    }

    // -----------------------------------------------------------------
    // Secret hygiene: a gate parse failure never echoes the offending
    // source line or a verbatim mistyped value (either may carry a
    // `literal:` credential). config set/unset renders gate failures
    // through the shared redacting gate.
    // -----------------------------------------------------------------

    const FAKE_SECRET: &str = "sk-THIS-IS-A-FAKE-CREDENTIAL-value";

    #[test]
    fn gate_parse_failure_does_not_echo_a_secret_bearing_source_line() {
        // An unknown field carrying a fake secret value: parse_config rejects
        // it, and toml's diagnostic would frame the offending source line --
        // carrying the secret -- unless the preview is redacted.
        let candidate = format!(
            "version = {CURRENT_CONFIG_VERSION}\n\n[server]\nhost = \"127.0.0.1\"\nport = \
             8787\nbogus_secret_key = \"{FAKE_SECRET}\"\n"
        );

        let errors = gate(&candidate).expect_err("an unknown field must fail the gate");
        for e in &errors {
            assert!(
                !e.contains("FAKE-CREDENTIAL"),
                "the secret-bearing source line must be redacted, got: {e}"
            );
        }
        // The failure kind still classes the error and the header keeps the
        // line/column for locating it; only the user-controlled value/name go.
        assert!(
            errors.iter().any(|e| e.contains("unknown field")),
            "the redacted error must still class the failure, got: {errors:?}"
        );
        assert!(
            errors.iter().all(|e| !e.contains("bogus_secret_key")),
            "the user-controlled field name must be dropped, got: {errors:?}"
        );
    }

    #[test]
    fn gate_type_mismatch_in_non_string_field_does_not_survive() {
        // A fake secret mistyped into the numeric `port` field: serde renders
        // `invalid type: string "...", expected u16`, embedding it verbatim on
        // a clause the snippet-row filter never sees -- it must still be gone.
        let candidate = format!(
            "version = {CURRENT_CONFIG_VERSION}\n\n[server]\nhost = \"127.0.0.1\"\nport = \
                 \"{FAKE_SECRET}\"\n"
        );

        let errors = gate(&candidate).expect_err("a type mismatch must fail the gate");
        for e in &errors {
            assert!(
                !e.contains("FAKE-CREDENTIAL"),
                "the mistyped secret must not survive redaction, got: {e}"
            );
        }
    }

    #[test]
    fn gate_quoted_secret_key_does_not_survive() {
        // A `literal:` credential used as a quoted TOML key surfaces as the
        // backtick token of an `unknown field` clause; it must be dropped while
        // the schema candidate names survive.
        let candidate = format!(
            "version = {CURRENT_CONFIG_VERSION}\n\n[server]\nhost = \"127.0.0.1\"\nport = \
             8787\n\"literal:{FAKE_SECRET}\" = 1\n"
        );

        let errors = gate(&candidate).expect_err("a quoted secret key must fail the gate");
        for e in &errors {
            assert!(
                !e.contains("FAKE-CREDENTIAL"),
                "the quoted secret key must not survive redaction, got: {e}"
            );
            assert!(
                !e.contains("literal:"),
                "the literal credential prefix must not survive, got: {e}"
            );
        }
    }
}
