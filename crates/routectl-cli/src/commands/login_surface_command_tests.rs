//! Loaded via `#[cfg(test)] #[path = "login_surface_command_tests.rs"] mod
//! command_tests;` in `login_surface.rs`.
//!
//! The command half, over real temp config files. The properties under test
//! are the ones no unit test on the planner can reach: that a write leaves a
//! config the shared gate accepts, that every non-accepted path leaves the
//! file BYTE-identical, and that a failure AFTER acceptance is the one
//! nonzero exit -- carrying the credential-kept wording and the same delta
//! the decline path prints.
//!
//! Every test runs with `yes = true` unless it is about the confirmation:
//! `confirm_high_consequence` reads stdin, which under the test harness is
//! at EOF, so `yes = false` deterministically DECLINES.

use routectl_router::{CURRENT_CONFIG_VERSION, parse_config};

use super::{SkipReason, SurfaceOutcome, render_delta, surface, surface_against};

/// A minimal config at the version this build writes, rendered from the
/// const so the next schema bump needs no fixture edit.
fn base() -> String {
    format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [server]\n\
         host = \"127.0.0.1\"\n\
         port = 8787\n"
    )
}

/// `base()` plus the anthropic default seat already wired into a growth
/// pool -- the shape a first login leaves behind.
fn base_with_default_seat() -> String {
    format!(
        "{}\n\
         [providers.anthropic-default]\n\
         kind = \"anthropic-api\"\n\
         auth_kind = \"oauth-bearer\"\n\
         api_key_ref = \"oauth://anthropic\"\n\
         \n\
         [pools.anthropic]\n\
         members = [\"anthropic-default\"]\n\
         accepts_new_logins = true\n",
        base()
    )
}

fn write_config(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
    let path = dir.join("config.toml");
    std::fs::write(&path, body).expect("write fixture config");
    path
}

fn read(path: &std::path::Path) -> Vec<u8> {
    std::fs::read(path).expect("read config")
}

// -----------------------------------------------------------------
// The accepted write: the resulting file passes the shared gate.
// -----------------------------------------------------------------

#[test]
fn an_accepted_write_lands_the_entry_and_the_pool_and_passes_the_gate() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), &base());

    // Act
    let outcome = surface(&path, "anthropic", None, true).expect("a fresh write succeeds");

    // Assert: the outcome names the entry, and the FILE parses and
    // validates through the same gate the reload path runs.
    assert_eq!(
        outcome,
        SurfaceOutcome::Written {
            entry_name: "anthropic-default".to_string()
        }
    );
    let text = String::from_utf8(read(&path)).expect("utf8");
    let config = parse_config(&text).expect("the written config parses");
    assert!(super::gate(&text).is_ok(), "the written config must gate");

    let entry = config
        .providers
        .get("anthropic-default")
        .expect("the entry landed");
    assert_eq!(entry.api_key_ref(), Some("oauth://anthropic"));
    let pool = config.pools.get("anthropic").expect("the pool landed");
    assert_eq!(pool.members, ["anthropic-default"]);
    assert!(
        pool.accepts_new_logins,
        "a pool login creates is one login may grow"
    );
}

#[test]
fn a_second_labelled_login_joins_the_growth_pool_without_rewriting_its_marker() {
    // Arrange: the first seat is already pooled with the growth marker.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), &base_with_default_seat());

    // Act
    let outcome = surface(&path, "anthropic", Some("work"), true).expect("the join succeeds");

    // Assert
    assert_eq!(
        outcome,
        SurfaceOutcome::Written {
            entry_name: "anthropic-work".to_string()
        }
    );
    let config = parse_config(&String::from_utf8(read(&path)).expect("utf8")).expect("parses");
    let pool = config.pools.get("anthropic").expect("pool");
    assert_eq!(pool.members, ["anthropic-default", "anthropic-work"]);
    assert_eq!(
        config
            .providers
            .get("anthropic-work")
            .expect("entry")
            .api_key_ref(),
        Some("oauth://anthropic#work")
    );
}

/// A pinned pool (`accepts_new_logins` absent) is an operator statement
/// that only an explicit edit grows it, so a matched seat inside one is
/// never auto-joined -- and the marker itself is never flipped.
#[test]
fn a_pinned_pool_is_left_byte_identical() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let pinned = base_with_default_seat().replace("accepts_new_logins = true\n", "");
    let path = write_config(dir.path(), &pinned);
    let before = read(&path);

    // Act
    let outcome = surface(&path, "anthropic", None, true).expect("a pinned pool is not an error");

    // Assert
    assert_eq!(outcome, SurfaceOutcome::Nothing);
    assert_eq!(read(&path), before, "a pinned pool must not be rewritten");
}

/// The idempotence property: a re-login over a config that already reaches
/// the seat proposes nothing, so no write is attempted at all -- no rename,
/// therefore no file-watch event for the daemon to reload on.
#[test]
fn an_idempotent_relogin_writes_nothing_and_leaves_the_inode_alone() {
    // Arrange: the config the FIRST login produced.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), &base());
    surface(&path, "anthropic", None, true).expect("first login writes");
    let before = read(&path);
    let before_meta = std::fs::metadata(&path).expect("metadata");

    // Act: the same login again.
    let outcome = surface(&path, "anthropic", None, true).expect("re-login is not an error");

    // Assert: nothing proposed, nothing written, and the atomic writer's
    // rename never ran (a new inode is exactly what a watcher fires on).
    assert_eq!(outcome, SurfaceOutcome::Nothing);
    assert_eq!(read(&path), before);
    let after_meta = std::fs::metadata(&path).expect("metadata");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        assert_eq!(
            before_meta.ino(),
            after_meta.ino(),
            "a rewrite would rename a new inode into place and fire the watcher"
        );
    }
    assert_eq!(
        before_meta.modified().ok(),
        after_meta.modified().ok(),
        "an unchanged config must not be touched"
    );
}

// -----------------------------------------------------------------
// Decline: byte-identical, exit 0, the delta printed for hand-pasting.
// -----------------------------------------------------------------

#[test]
fn declining_leaves_the_file_byte_identical_and_is_not_an_error() {
    // Arrange: yes=false with the harness's EOF stdin declines.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), &base());
    let before = read(&path);

    // Act
    let outcome = surface(&path, "anthropic", None, false).expect("declining is not an error");

    // Assert
    assert_eq!(outcome, SurfaceOutcome::Declined);
    assert_eq!(read(&path), before, "a decline must not write a byte");
}

// -----------------------------------------------------------------
// Skips: no file, a file this build must not edit, a file that does not
// parse. All exit 0 -- the credential is stored either way.
// -----------------------------------------------------------------

#[test]
fn an_absent_config_file_is_skipped_and_never_created() {
    // Arrange: an empty directory.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");

    // Act
    let outcome = surface(&path, "anthropic", None, true).expect("no config is not an error");

    // Assert: login never creates config.toml -- that is `config init`'s
    // decision, including where the file goes and what else it holds.
    assert_eq!(outcome, SurfaceOutcome::Skipped(SkipReason::NoConfigFile));
    assert!(!path.exists(), "login must not create a config file");
}

/// A pre-v4 config is the migrate case: skipped, pointed at `config
/// migrate`, exit 0. Nonzero here would break every credential-only login
/// against a stale config.
#[test]
fn a_previous_version_config_is_skipped_with_the_migrate_pointer() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let stale = base().replace(
        &format!("version = {CURRENT_CONFIG_VERSION}"),
        &format!("version = {}", CURRENT_CONFIG_VERSION - 1),
    );
    let path = write_config(dir.path(), &stale);
    let before = read(&path);

    // Act
    let outcome = surface(&path, "anthropic", None, true).expect("a stale config is not an error");

    // Assert: the preflight's own wording, so the pointer cannot drift.
    let SurfaceOutcome::Skipped(SkipReason::Unwritable { detail }) = &outcome else {
        panic!("expected an unwritable skip, got {outcome:?}");
    };
    assert!(
        detail.contains("config migrate"),
        "the skip must point at the migrator: {detail}"
    );
    assert_eq!(read(&path), before);
}

#[test]
fn an_unparseable_config_is_skipped_and_the_parse_error_carries_no_config_value() {
    // Arrange: the fixture's RAW toml error must itself echo the sentinel,
    // or this test is vacuous -- an error pointing at some OTHER line passes
    // the no-leak assertion with the redactor deleted. An UNQUOTED secret-
    // shaped value is the shape that does it: toml renders the offending
    // source line into the diagnostic snippet verbatim.
    let sentinel = "keep-me-exactly";
    let body = format!(
        "{}[providers.x]\n\
         kind = \"openai-compat\"\n\
         base_url = \"http://127.0.0.1:1\"\n\
         api_key_ref = literal:{sentinel}\n",
        base()
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), &body);
    let before = read(&path);

    // The premise, asserted rather than assumed: without this the test
    // cannot distinguish a working redactor from a missing one.
    let raw = parse_config(&body).expect_err("the fixture must fail to parse");
    assert!(
        raw.contains(sentinel),
        "the RAW error must echo the value, else this test proves nothing: {raw}"
    );

    // Act
    let outcome =
        surface(&path, "anthropic", None, true).expect("an unparseable config is not an error");

    // Assert: skipped, byte-identical, and the redactor kept the value the
    // raw error DID carry out of the printed detail.
    let SurfaceOutcome::Skipped(SkipReason::Unparseable { detail }) = &outcome else {
        panic!("expected an unparseable skip, got {outcome:?}");
    };
    assert!(
        !detail.contains(sentinel),
        "the parse error must not echo a config value: {detail}"
    );
    assert_eq!(read(&path), before);
}

// -----------------------------------------------------------------
// Post-acceptance failure: the ONE nonzero path.
// -----------------------------------------------------------------

/// An out-of-band write between the snapshot and the commit is a conflict.
/// The snapshot is what the commit's byte comparison is made against, so
/// handing `surface_against` bytes the file no longer holds exercises
/// exactly that arm -- no thread race, no flake.
#[test]
fn a_conflict_after_acceptance_is_nonzero_says_the_credential_is_kept_and_keeps_the_other_writer() {
    // Arrange: the snapshot describes a file state that another writer has
    // already replaced.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), &base());
    let stale_snapshot = read(&path);

    let other_writer = format!("{}# the other writer's bytes\n", base());
    std::fs::write(&path, &other_writer).expect("out-of-band write");

    // Act
    let err = surface_against(&path, stale_snapshot, "anthropic", None, true)
        .expect_err("a conflict after acceptance must be nonzero");

    // Assert: the wording is truthful about the credential, and it carries
    // the SAME delta the decline path prints so the operator can paste it.
    let msg = err.to_string();
    assert!(
        msg.contains("credential is stored"),
        "the failure must state the credential stands: {msg}"
    );
    assert!(
        msg.contains("Nothing was rolled back"),
        "the failure must state nothing was rolled back: {msg}"
    );
    // And it names the CLASS of failure, never the file: login resolved that
    // path itself, and this message gets pasted into bug reports.
    assert!(
        msg.contains("changed on disk"),
        "the failure must name the conflict: {msg}"
    );
    assert!(
        !msg.contains(&path.display().to_string())
            && !msg.contains(dir.path().to_str().expect("utf8 tempdir")),
        "the failure must not disclose the config path: {msg}"
    );
    let expected_delta = render_delta(
        &super::plan(
            &parse_config(&String::from_utf8(read(&path)).expect("utf8")).expect("parses"),
            "anthropic",
            None,
        )
        .expect("plans"),
    );
    assert!(
        msg.contains(expected_delta.trim_end()),
        "the recovery block must be the same render_delta: {msg}"
    );

    // And the file still holds the OTHER writer's bytes, untouched.
    assert_eq!(
        String::from_utf8(read(&path)).expect("utf8"),
        other_writer,
        "a conflict must not overwrite the other writer"
    );
}

/// A conflict binds to ONE snapshot: it must not silently re-plan against
/// the fresh bytes and write anyway. Proven by the byte assertion above
/// plus this one -- the entry the plan would have created is absent.
#[test]
fn a_conflict_never_retries_against_the_fresh_bytes() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), &base());
    let stale_snapshot = read(&path);
    std::fs::write(&path, format!("{}# raced\n", base())).expect("out-of-band write");

    // Act
    let _ = surface_against(&path, stale_snapshot, "anthropic", None, true)
        .expect_err("a conflict is nonzero");

    // Assert
    let config = parse_config(&String::from_utf8(read(&path)).expect("utf8")).expect("parses");
    assert!(
        config.providers.is_empty() && config.pools.is_empty(),
        "a conflict must write nothing, not retry: {config:?}"
    );
}

// -----------------------------------------------------------------
// Availability (D6): the routing gap, on the write and Nothing paths.
// -----------------------------------------------------------------

/// The scans run against the config that was just COMMITTED, not the
/// pre-write one -- the entry they ask about only exists in the candidate.
#[test]
fn the_written_config_is_the_one_the_availability_scan_reads() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), &base());

    // Act
    surface(&path, "anthropic", None, true).expect("writes");
    let config = parse_config(&String::from_utf8(read(&path)).expect("utf8")).expect("parses");

    // Assert: the scan finds the just-written entry pooled, and reports the
    // model gap against the POOL (naming the entry would not let a second
    // seat serve the same model).
    let gap = crate::commands::login_surface_availability::availability_gap(
        &config,
        "anthropic-default",
        Some("anthropic"),
    )
    .expect("a freshly written seat has no model yet");
    assert!(gap.contains(r#"provider = "anthropic""#), "{gap}");
}

/// The no-HOME shape: with neither `XDG_CONFIG_HOME` nor a home directory
/// resolvable, `resolve_config_path` lands on a relative path whose
/// ancestors do not exist. That is still the absent-file case, not an IO
/// failure -- a credential-only login in a bare container must exit 0.
#[test]
fn a_config_path_whose_parent_directories_do_not_exist_is_the_absent_file_case() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir
        .path()
        .join("no")
        .join("such")
        .join("dir")
        .join("config.toml");

    // Act
    let outcome = surface(&path, "anthropic", None, true).expect("an absent tree is not an error");

    // Assert: skipped, and nothing was created along the way.
    assert_eq!(outcome, SurfaceOutcome::Skipped(SkipReason::NoConfigFile));
    assert!(!path.exists());
    assert!(!dir.path().join("no").exists(), "no directory was created");
}

/// The three no-write surfaces render the delta through ONE function, so
/// they cannot describe different entries. Proven by comparing what the
/// decline path prints against the planner's own render for the same
/// config -- the recovery block's copy is asserted in the conflict test.
#[test]
fn the_decline_and_the_planner_render_the_same_delta() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), &base());
    let before = read(&path);

    let expected = render_delta(
        &super::plan(
            &parse_config(&String::from_utf8(before.clone()).expect("utf8")).expect("parses"),
            "anthropic",
            None,
        )
        .expect("plans"),
    );

    // Act: decline, then re-plan against the (unchanged) file.
    let outcome = surface(&path, "anthropic", None, false).expect("decline");

    // Assert: the file the decline left behind still plans the SAME delta,
    // which is what makes the printed block pasteable.
    assert_eq!(outcome, SurfaceOutcome::Declined);
    let after = render_delta(
        &super::plan(
            &parse_config(&String::from_utf8(read(&path)).expect("utf8")).expect("parses"),
            "anthropic",
            None,
        )
        .expect("plans"),
    );
    assert_eq!(after, expected);
    assert!(
        expected.contains("[providers.anthropic-default]")
            && expected.contains("[pools.anthropic]"),
        "the delta must carry both halves: {expected}"
    );
}

/// The delta that was SHOWN and the bytes that were WRITTEN must describe
/// the same config. Proven structurally: the written file, re-parsed, must
/// carry every key/value pair the printed TOML declared.
#[test]
fn every_field_of_the_shown_delta_lands_in_the_written_config() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), &base());
    let planned = super::plan(
        &parse_config(&base()).expect("base parses"),
        "anthropic",
        None,
    )
    .expect("plans");
    let shown = render_delta(&planned);

    // Act
    surface(&path, "anthropic", None, true).expect("writes");

    // Assert: parse the SHOWN delta as a standalone config and compare the
    // entry and pool it declares against the file's own.
    let declared: routectl_router::Config =
        toml::from_str(&format!("version = {CURRENT_CONFIG_VERSION}\n{shown}"))
            .expect("the shown delta parses");
    let written = parse_config(&String::from_utf8(read(&path)).expect("utf8")).expect("parses");

    for (name, entry) in &declared.providers {
        let landed = written
            .providers
            .get(name)
            .unwrap_or_else(|| panic!("the shown entry `{name}` did not land"));
        assert_eq!(
            format!("{entry:?}"),
            format!("{landed:?}"),
            "shown and written entry `{name}` disagree"
        );
    }
    for (name, pool) in &declared.pools {
        let landed = written
            .pools
            .get(name)
            .unwrap_or_else(|| panic!("the shown pool `{name}` did not land"));
        assert_eq!(pool.members, landed.members, "pool `{name}` members");
        assert_eq!(
            pool.accepts_new_logins, landed.accepts_new_logins,
            "the shown growth marker and the written one must agree for `{name}`"
        );
    }
}

/// The growth marker is written ONLY for a pool this edit creates. A JOIN
/// must not add it -- an operator who removed it did so deliberately, and
/// `render_delta` does not show it on a join either.
#[test]
fn joining_a_pool_never_adds_the_growth_marker_the_delta_does_not_show() {
    // Arrange: a growth pool for anthropic (so the join is allowed) plus an
    // unrelated PINNED pool that must stay pinned.
    let dir = tempfile::tempdir().expect("tempdir");
    let body = format!(
        "{}\n\
         [providers.codex-default]\n\
         kind = \"openai-responses\"\n\
         auth_kind = \"chatgpt-oauth\"\n\
         api_key_ref = \"oauth://codex\"\n\
         \n\
         [pools.codex]\n\
         members = [\"codex-default\"]\n",
        base_with_default_seat()
    );
    let path = write_config(dir.path(), &body);

    // Act
    surface(&path, "anthropic", Some("work"), true).expect("joins");

    // Assert
    let text = String::from_utf8(read(&path)).expect("utf8");
    let config = parse_config(&text).expect("parses");
    assert!(
        !config.pools.get("codex").expect("pool").accepts_new_logins,
        "an unrelated pinned pool must stay pinned"
    );
    assert_eq!(
        text.matches("accepts_new_logins").count(),
        1,
        "a join must add no marker: {text}"
    );
}

/// A gate-rejected candidate is a REPORTED value, not a nonzero exit: the
/// operator was never offered anything and nothing was written. Driven by a
/// config whose own `[models]` table names a provider that does not exist,
/// so the candidate fails the shared validator for a reason the delta did
/// not cause and cannot fix.
#[test]
fn a_candidate_the_gate_rejects_is_reported_and_is_not_an_error() {
    // Arrange: a config that PARSES but does not validate.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(
        dir.path(),
        &format!(
            "{}[models.ghost]\n\
             provider = \"nobody\"\n\
             upstream = \"x\"\n",
            base()
        ),
    );
    let before = read(&path);

    // Act
    let outcome =
        surface(&path, "anthropic", None, true).expect("a gate rejection must not be an error");

    // Assert
    assert_eq!(outcome, SurfaceOutcome::Rejected);
    assert_eq!(read(&path), before, "a rejected candidate writes nothing");
}

/// The exit-code contract's shape, as a structural guard: inside `surface`
/// and its helpers there must be exactly ONE construction of an `Err` that
/// an operator can reach, and it must be the post-commit one. A new `?` on a
/// pre-acceptance step would silently turn a stored credential into a
/// nonzero exit -- the exact regression D5 forbids and no other test sees.
///
/// The whole file is scanned with no `#[cfg(test)]` cut: both test modules
/// here are SIDECARS, so none of their text is in this file, while cutting
/// on the first `#[cfg(test)]` truncates at the planner's own `mod tests;`
/// declaration -- which sits ABOVE the entire impure half the guard exists
/// to cover.
#[test]
fn the_only_operator_reachable_failure_is_the_post_commit_one() {
    let production = include_str!("login_surface.rs");

    let error_sites = production.matches("Error::Config(").count();
    assert_eq!(
        error_sites, 3,
        "expected exactly three `Error::Config` sites: `plan`'s two unreachable \
         refusals (no provider shape, an unparseable seat ref) and the post-commit \
         failure. A fourth means a pre-acceptance step can now exit nonzero, which \
         breaks the contract that a stored credential never fails the command."
    );
    assert!(
        production.contains("the credential is stored and remains valid"),
        "the post-commit failure must state the credential stands"
    );
}

// -----------------------------------------------------------------
// The pinned-pool invariant on the ZERO-match arm. A pinned pool (no
// `accepts_new_logins`) is an operator statement that only an explicit
// edit grows it -- and login must never flip that marker. The one-match
// arm carried this check from the start; the fresh-entry arm did not.
// -----------------------------------------------------------------

/// THE regression: a FRESH login (no entry carries the ref yet) into a
/// family whose pool is PINNED must write the account entry and leave the
/// pool byte-untouched. Before the fix this joined the pool AND flipped its
/// marker to `true`, converting a pinned pool into a growing one with no
/// operator action.
#[test]
fn a_fresh_login_never_joins_or_marks_a_pinned_family_pool() {
    // Arrange: the migrate-shaped config -- a pool with members and NO
    // growth marker -- and no entry yet carrying the labelled seat's ref.
    let dir = tempfile::tempdir().expect("tempdir");
    let pinned = base_with_default_seat().replace("accepts_new_logins = true\n", "");
    let path = write_config(dir.path(), &pinned);

    // Act: a fresh labelled login (zero ref matches for `#work`).
    let outcome = surface(&path, "anthropic", Some("work"), true).expect("the entry still writes");

    // Assert: the account surfaced ...
    assert_eq!(
        outcome,
        SurfaceOutcome::Written {
            entry_name: "anthropic-work".to_string()
        }
    );
    let text = String::from_utf8(read(&path)).expect("utf8");
    let config = parse_config(&text).expect("parses");
    assert_eq!(
        config
            .providers
            .get("anthropic-work")
            .expect("the new entry landed")
            .api_key_ref(),
        Some("oauth://anthropic#work")
    );

    // ... and the pinned pool is untouched, both in members and in marker.
    let pool = config.pools.get("anthropic").expect("pool");
    assert_eq!(
        pool.members,
        ["anthropic-default"],
        "a pinned pool must gain no member"
    );
    assert!(
        !pool.accepts_new_logins,
        "login must never flip an operator's pinned marker"
    );
    assert!(
        !text.contains("accepts_new_logins"),
        "the marker must not appear at all: {text}"
    );
}

/// The same invariant as RAW BYTES: the `[pools.anthropic]` block the
/// operator wrote must survive verbatim, decorations included. A parsed
/// comparison can pass while the block was rewritten.
#[test]
fn the_pinned_pool_block_survives_a_fresh_login_byte_for_byte() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let pinned = base_with_default_seat().replace("accepts_new_logins = true\n", "");
    let path = write_config(dir.path(), &pinned);

    let pool_block = |text: &str| {
        text.split("[pools.anthropic]")
            .nth(1)
            .expect("the pool block is present")
            .to_string()
    };
    let before = pool_block(&pinned);

    // Act
    surface(&path, "anthropic", Some("work"), true).expect("the entry writes");

    // Assert: the entry is appended elsewhere; the pool block itself is
    // byte-identical from its header to end of file.
    let after_text = String::from_utf8(read(&path)).expect("utf8");
    assert_eq!(
        pool_block(&after_text),
        before,
        "the pinned pool block must not be rewritten"
    );
}

/// The most reachable path, end to end and with no hand-editing: `config
/// migrate` writes pool blocks WITHOUT the growth marker, so a login
/// straight after a migration is exactly the pinned-pool case. A NEW label
/// plans the entry only; an EXISTING seat still plans nothing.
#[test]
fn a_login_over_migrate_output_writes_the_entry_and_leaves_the_unmarked_pool_alone() {
    // Arrange: the shape the migration ladder produces -- suffixed account
    // entries plus the plain-named pool listing them, no marker.
    let mut doc: toml_edit::DocumentMut = format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [providers.anthropic-managed]\n\
         kind = \"anthropic-api\"\n\
         auth_kind = \"oauth-bearer\"\n\
         api_key_ref = \"oauth://anthropic\"\n"
    )
    .parse()
    .expect("fixture parses");
    routectl_router::upsert_pool_members(&mut doc, "anthropic", &["anthropic-managed"]);
    let migrated = doc.to_string();
    assert!(
        !migrated.contains("accepts_new_logins"),
        "the migrator writes no growth marker -- that is what makes this the pinned case"
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), &migrated);

    // Act: a login for a label the migrated config does not carry.
    let outcome = surface(&path, "anthropic", Some("work"), true).expect("the entry writes");

    // Assert: entry only.
    assert_eq!(
        outcome,
        SurfaceOutcome::Written {
            entry_name: "anthropic-work".to_string()
        }
    );
    let text = String::from_utf8(read(&path)).expect("utf8");
    let config = parse_config(&text).expect("parses");
    assert!(config.providers.contains_key("anthropic-work"));
    assert_eq!(
        config.pools.get("anthropic").expect("pool").members,
        ["anthropic-managed"],
        "the migrated pool must gain no member"
    );
    assert!(!text.contains("accepts_new_logins"), "{text}");

    // And the seat the migration already wired stays a no-op.
    let before = read(&path);
    assert_eq!(
        surface(&path, "anthropic", None, true).expect("re-login"),
        SurfaceOutcome::Nothing
    );
    assert_eq!(read(&path), before);
}

/// The delta-vs-written equality in the POOL-EXISTS arm. The pool-absent
/// case is covered above; this is the one that broke -- the delta rendered a
/// one-member `Create` while the write unioned into the existing block.
///
/// A `Join`'s delta deliberately omits `accepts_new_logins` (the pool
/// already carries whatever the operator wrote), so the property is: the
/// MEMBERS the delta showed are the members that landed, and the marker is
/// untouched relative to the pre-write file. Comparing the marker against
/// the delta parsed standalone would be wrong -- an absent key reads
/// `false` there while the file legitimately holds `true`.
#[test]
fn every_field_of_the_shown_delta_lands_when_the_family_pool_already_exists() {
    // Arrange: a growth-marked pool, so the plan is a Join over an existing
    // block rather than a Create.
    let dir = tempfile::tempdir().expect("tempdir");
    let body = base_with_default_seat();
    let path = write_config(dir.path(), &body);
    let before = parse_config(&body).expect("parses");
    let shown = render_delta(&super::plan(&before, "anthropic", Some("work")).expect("plans"));

    // Act
    surface(&path, "anthropic", Some("work"), true).expect("writes");

    // Assert
    let declared: routectl_router::Config =
        toml::from_str(&format!("version = {CURRENT_CONFIG_VERSION}\n{shown}"))
            .expect("the shown delta parses");
    let written = parse_config(&String::from_utf8(read(&path)).expect("utf8")).expect("parses");
    assert!(
        declared.pools.contains_key("anthropic"),
        "the delta must describe the pool it joins: {shown}"
    );
    for (name, pool) in &declared.pools {
        let landed = written
            .pools
            .get(name)
            .unwrap_or_else(|| panic!("the shown pool `{name}` did not land"));
        assert_eq!(
            pool.members, landed.members,
            "the members the delta showed are not the members that landed for `{name}`"
        );
        assert_eq!(
            before.pools.get(name).map(|p| p.accepts_new_logins),
            Some(landed.accepts_new_logins),
            "a join must leave the growth marker of `{name}` exactly as the operator wrote it"
        );
    }
    assert!(
        !shown.contains("accepts_new_logins"),
        "a join must not show a marker it does not write: {shown}"
    );
}

// -----------------------------------------------------------------
// The printed delta must be valid TOML for every name config can hold.
// Pool member names are operator-written and only checked for
// non-emptiness, so a control character reaches the renderer.
// -----------------------------------------------------------------

/// A pool member name carrying a newline must not break the SHOWN delta.
/// `toml_edit` escapes properly on the write path, so before the fix the
/// committed file stayed valid while the printed block was unparseable --
/// an operator pasting it hit a syntax error routectl appeared to author.
#[test]
fn a_control_character_in_a_member_name_keeps_the_shown_delta_parseable() {
    // Arrange: a growth-marked pool whose existing member name embeds a
    // newline and a quote, so the join renders it into `members = [...]`.
    let hostile = "sneaky\nname\"x";
    let mut doc: toml_edit::DocumentMut = format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [providers.anthropic-default]\n\
         kind = \"anthropic-api\"\n\
         auth_kind = \"oauth-bearer\"\n\
         api_key_ref = \"oauth://anthropic\"\n\
         [pools.anthropic]\n\
         accepts_new_logins = true\n"
    )
    .parse()
    .expect("fixture parses");
    routectl_router::upsert_pool_members(&mut doc, "anthropic", &["anthropic-default", hostile]);
    let body = doc.to_string();
    let config = parse_config(&body).expect("the fixture config parses");

    // Act
    let shown = render_delta(&super::plan(&config, "anthropic", Some("work")).expect("plans"));

    // Assert: the rendered delta is valid TOML, and the member survives the
    // round trip intact rather than being mangled or dropped.
    assert!(
        !shown.contains("sneaky\nname"),
        "a raw newline must not reach the rendered TOML: {shown:?}"
    );
    let reparsed: routectl_router::Config =
        toml::from_str(&format!("version = {CURRENT_CONFIG_VERSION}\n{shown}"))
            .unwrap_or_else(|e| panic!("the shown delta must re-parse: {e}\n{shown}"));
    let members = &reparsed.pools.get("anthropic").expect("pool").members;
    assert!(
        members.iter().any(|m| m == hostile),
        "the member must round-trip byte-exact, got {members:?}"
    );
    assert!(members.iter().any(|m| m == "anthropic-work"));
}

/// The same property on the ENTRY key rather than a member value: a label
/// with a control character reaches `toml_key`, which delegates to
/// `toml_string` for anything not a bare key.
#[test]
fn a_control_character_in_a_label_keeps_the_printed_entry_parseable() {
    // Arrange + Act
    let block = crate::commands::login_provider_block::provider_block("anthropic", Some("a\nb"))
        .expect("block");
    let shown = block.render();

    // Assert
    assert!(
        !shown.contains("a\nb"),
        "a raw newline must not reach the table key: {shown:?}"
    );
    let reparsed: routectl_router::Config = toml::from_str(&shown)
        .unwrap_or_else(|e| panic!("the printed entry must re-parse: {e}\n{shown}"));
    assert!(
        reparsed.providers.contains_key("anthropic-a\nb"),
        "the entry name must round-trip, got {:?}",
        reparsed.providers.keys().collect::<Vec<_>>()
    );
}

/// The Unreadable skip and the post-acceptance failure both describe a file
/// login resolved itself, so neither may echo its path -- login output gets
/// pasted into bug reports, and the path embeds the operator's home
/// directory.
#[test]
fn no_config_surface_message_echoes_the_config_path() {
    // Arrange: a path with a recognizable "username" segment, made
    // unreadable as a directory so the read fails with something other than
    // NotFound.
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("someoperator");
    std::fs::create_dir_all(&home).expect("mkdir");
    let path = home.join("config.toml");
    std::fs::create_dir(&path).expect("a directory where the file belongs");

    // Act
    let outcome = surface(&path, "anthropic", None, true).expect("a read failure is not an error");

    // Assert: the class, with no path anywhere in it.
    assert_eq!(outcome, SurfaceOutcome::Skipped(SkipReason::Unreadable));
    let rendered = SkipReason::Unreadable.to_string();
    assert!(
        !rendered.contains("someoperator") && !rendered.contains('/'),
        "the skip message must not disclose the path: {rendered}"
    );
}
