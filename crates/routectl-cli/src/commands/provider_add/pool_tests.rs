//! Loaded via `#[cfg(test)] #[path = "pool_tests.rs"] mod pool_tests;` in
//! `mod.rs`.
//!
//! `provider add --login ... --pool <name>`: the provider block and the pool
//! edit are one candidate, gated once, written in one commit or not at all.

use std::sync::Mutex;

use routectl_router::CURRENT_CONFIG_VERSION;
use routectl_testkit::ScopedEnv;

use super::toml_edit::{Candidate, commit, publish_count};
use super::*;

/// A stub [`AddIo`] whose login records the seat and runs `hook` (standing
/// in for another writer landing between the snapshot and the commit).
#[derive(Default)]
struct LoginIo {
    logins: Mutex<Vec<(String, Option<String>)>>,
    hook: Option<Box<dyn Fn() + Send + Sync>>,
}

#[async_trait]
impl AddIo for LoginIo {
    fn stdin_is_terminal(&self) -> bool {
        false
    }
    fn read_stdin(&self) -> Result<String> {
        panic!("a login add never reads stdin")
    }
    fn confirm_env_offer(&self, _var: &str) -> bool {
        panic!("a login add never offers an env var")
    }
    fn prompt_hidden(&self, _provider_name: &str) -> Result<String> {
        panic!("a login add never prompts for a key")
    }
    async fn login(&self, provider: &str, label: Option<&str>, _rerun: Option<&str>) -> Result<()> {
        self.logins
            .lock()
            .unwrap()
            .push((provider.to_string(), label.map(str::to_string)));
        if let Some(hook) = &self.hook {
            hook();
        }
        Ok(())
    }
}

impl LoginIo {
    fn login_count(&self) -> usize {
        self.logins.lock().unwrap().len()
    }
}

fn base() -> String {
    format!("version = {CURRENT_CONFIG_VERSION}\n[server]\nhost = \"127.0.0.1\"\nport = 8787\n")
}

/// An anthropic seat entry named `name` with `extra` lines appended.
fn seat(name: &str, credential: &str, extra: &str) -> String {
    format!(
        "\n[providers.{name}]\n\
         format = \"anthropic-messages\"\n\
         base_url = \"https://api.anthropic.com\"\n\
         credential = \"{credential}\"\n\
         auth = \"bearer\"\n\
         {extra}"
    )
}

/// `base()` with the default anthropic seat pooled in `[pools.team]`.
fn pooled(first_member: &str) -> String {
    format!(
        "{}{first_member}\n[pools.team]\nmembers = [\"first\"]\n",
        base()
    )
}

fn pool_args(name: &str, label: &str, pool: &str) -> ProviderAddArgs {
    ProviderAddArgs {
        format: None,
        login: Some("anthropic".to_string()),
        label: Some(label.to_string()),
        pool: Some(pool.to_string()),
        auth: None,
        name: name.to_string(),
        base_url: None,
        region: None,
        api_key_env: None,
        secret_ref: None,
        api_key_stdin: false,
        credential_source: None,
        overwrite: false,
        yes: true,
    }
}

fn write_config(dir: &Path, body: &str) -> std::path::PathBuf {
    let path = dir.join("config.toml");
    std::fs::write(&path, body).unwrap();
    path
}

/// The candidate `run_with_io` builds for `args` over `body`.
fn candidate_over<'a>(body: &str, args: &'a ProviderAddArgs) -> Candidate<'a> {
    let prev = parse_config(body).expect("fixture parses");
    let (mut entry, _, _) = build::build_entry(args, &LoginIo::default()).expect("entry builds");
    build_candidate(&prev, &args.name, args.pool.as_deref(), &mut entry).expect("candidate")
}

#[tokio::test]
async fn a_join_writes_the_entry_and_its_membership_in_one_commit() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let body = pooled(&seat("first", "oauth://anthropic#first", ""));
    let path = write_config(dir.path(), &body);
    let io = LoginIo::default();
    let publishes_before = publish_count::publishes();

    // Act
    let result = run_with_io(&path, pool_args("work", "work", "team"), &io)
        .await
        .expect("the join writes");

    // Assert: one publish carried both halves, so no reader of the file
    // ever sees the entry without its membership.
    assert_eq!(result, AddResult::Written);
    assert_eq!(
        publish_count::publishes() - publishes_before,
        1,
        "the entry and the pool are one publish"
    );
    assert_eq!(io.login_count(), 1);
    let text = std::fs::read_to_string(&path).unwrap();
    let config = parse_config(&text).expect("reparses");
    assert!(gate(&text).is_ok(), "{text}");
    assert_eq!(
        config.providers["work"].credential,
        "oauth://anthropic#work"
    );
    let pool = &config.pools["team"];
    assert_eq!(pool.members, ["first", "work"]);
    assert!(
        !pool.accepts_new_logins,
        "joining an existing pool never adds the growth marker"
    );
    let first = seat("first", "oauth://anthropic#first", "");
    assert!(
        text.contains(&first),
        "the member block keeps its bytes:\n{text}"
    );
}

#[tokio::test]
async fn a_pool_the_add_creates_lists_the_entry_and_accepts_new_logins() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &base());

    // Act
    run_with_io(
        &path,
        pool_args("work", "work", "team"),
        &LoginIo::default(),
    )
    .await
    .expect("the create writes");

    // Assert
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("[pools.team]\nmembers = [\"work\"]\naccepts_new_logins = true\n"),
        "{text}"
    );
    assert!(gate(&text).is_ok(), "{text}");
}

/// Each row's pool differs on the named pool-key field: the first four
/// rows' first member differs from the joining anthropic seat, and the
/// cloak row's two members already differ on explicit cloak settings (the
/// joining seat copies the first member's cloak keys, so only a pool that
/// already disagrees can mismatch there). The gate refuses the one
/// candidate before the login runs and the file keeps every byte.
/// `oauth://acme` is a vendor routectl does not manage, so containment
/// leaves the member alone.
#[tokio::test]
async fn a_mismatched_pool_refuses_before_the_login_and_writes_nothing() {
    let cloak_split = format!(
        "{}{}{}\n[pools.team]\nmembers = [\"first\", \"second\"]\n",
        base(),
        seat(
            "first",
            "oauth://anthropic#first",
            "cloak_mode = \"never\"\n"
        ),
        seat("second", "oauth://anthropic#second", ""),
    );
    let rows = [
        (
            "format",
            pooled(&seat("first", "oauth://acme", "").replace("anthropic-messages", "openai-chat")),
            "`format`",
            true,
        ),
        (
            "destination",
            pooled(
                &seat("first", "oauth://anthropic#first", "")
                    .replace("api.anthropic.com\"", "api.anthropic.com/v1\""),
            ),
            "destination",
            true,
        ),
        (
            "auth",
            pooled(&seat("first", "oauth://acme", "").replace("\"bearer\"", "\"x-api-key\"")),
            "`auth`",
            true,
        ),
        (
            "vendor",
            pooled(&seat("first", "oauth://acme", "")),
            "OAuth vendor",
            true,
        ),
        ("cloak", cloak_split, "`cloak_mode`", false),
    ];
    for (row, body, field, snapshot_gates) in rows {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            gate(&body).is_ok(),
            snapshot_gates,
            "row `{row}`: the fixture's own validity"
        );
        let path = write_config(dir.path(), &body);
        let io = LoginIo::default();
        let args = pool_args("work", "work", "team");
        let errors = gate(&candidate_over(&body, &args).text(&body).unwrap())
            .expect_err("the candidate is refused");

        // Act
        let result = run_with_io(&path, args, &io).await;

        // Assert
        assert!(result.is_err(), "row `{row}` must refuse");
        assert_eq!(io.login_count(), 0, "row `{row}`: no login before refusal");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body, "row `{row}`");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("differs") && e.contains(field)),
            "row `{row}`: {errors:?}"
        );
    }
}

/// Positive control for the mismatch rows: a vendor-matching pool with no
/// cloak keys writes no cloak key onto the joining entry.
#[test]
fn a_pool_without_cloak_keys_gives_the_joining_entry_none() {
    let body = pooled(&seat("first", "oauth://anthropic#first", ""));
    let args = pool_args("work", "work", "team");

    let shown = candidate_over(&body, &args).render();

    assert!(shown.contains("[providers.work]"), "{shown}");
    assert!(!shown.contains("cloak"), "{shown}");
}

#[tokio::test]
async fn joining_a_cloaked_pool_writes_and_shows_the_copied_cloak_keys() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let body = pooled(&seat(
        "first",
        "oauth://anthropic#first",
        "cloak_mode = \"never\"\ncloak_strict_mode = true\n",
    ));
    let path = write_config(dir.path(), &body);
    let args = pool_args("work", "work", "team");
    let shown = candidate_over(&body, &args).render();

    // Act
    run_with_io(&path, args, &LoginIo::default())
        .await
        .expect("the cloaked join writes");

    // Assert
    assert!(shown.contains("cloak_mode        = \"never\""), "{shown}");
    assert!(shown.contains("cloak_strict_mode = true"), "{shown}");
    assert!(
        shown.contains("[pools.team]\nmembers = [\"first\", \"work\"]\n"),
        "{shown}"
    );
    let config = parse_config(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let entry = &config.providers["work"];
    assert_eq!(
        entry.cloak_mode,
        Some(routectl_providers::anthropic_api::CloakMode::Never)
    );
    assert_eq!(entry.cloak_strict_mode, Some(true));
    assert_eq!(config.pools["team"].members, ["first", "work"]);
}

/// A pool target that cannot hold a member list is an error from the one
/// apply step, never a provider-only write. The well-formed control shows
/// the same candidate applying.
#[tokio::test]
async fn a_malformed_pool_target_is_an_error_not_a_provider_only_write() {
    // Arrange
    let body = base().replace("[server]", "pools = 5\n[server]");
    let args = pool_args("work", "work", "team");
    let candidate = candidate_over(&base(), &args);
    candidate
        .text(&base())
        .expect("control: a config without pools takes the candidate");

    // Act
    let applied = candidate.text(&body);
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &body);
    let io = LoginIo::default();
    let ran = run_with_io(&path, pool_args("work", "work", "team"), &io).await;

    // Assert
    assert!(applied.is_err(), "`pools = 5` cannot take a member");
    assert!(ran.is_err(), "the command refuses");
    assert_eq!(io.login_count(), 0);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
}

#[tokio::test]
#[serial_test::serial]
async fn a_conflict_after_the_login_writes_neither_half_and_names_the_pool_rerun() {
    // Arrange: a chosen config file, so the rerun names it.
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path().join("default"));
    let _env = ScopedEnv::unset("ROUTECTL_CONFIG");
    let body = pooled(&seat("first", "oauth://anthropic#first", ""));
    let path = write_config(dir.path(), &body);
    let other_writer = format!("{body}# another writer\n");
    let (racing_path, racing_bytes) = (path.clone(), other_writer.clone());
    let io = LoginIo {
        hook: Some(Box::new(move || {
            std::fs::write(&racing_path, &racing_bytes).unwrap();
        })),
        ..LoginIo::default()
    };

    // Act
    let msg = run_with_io(&path, pool_args("work", "work", "my team"), &io)
        .await
        .expect_err("the conflict errors")
        .to_string();

    // Assert
    assert_eq!(io.login_count(), 1, "the login ran and is kept");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), other_writer);
    let expected = format!(
        "re-run `routectl --config '{}' provider add --login anthropic --label work \
         --pool 'my team' --name work --yes`",
        path.display()
    );
    assert!(msg.contains(&expected), "{msg}");
}

/// The pool name follows the rerun's exact-or-absent rule: shell-quoted
/// when the shell would split it, and no literal command at all when it
/// holds a control character.
#[test]
fn the_pool_name_in_the_rerun_is_exact_or_absent() {
    let rows = [
        ("plain", "team", Some("--pool team")),
        ("space", "my team", Some("--pool 'my team'")),
        ("expansion", "$(id)", Some("--pool '$(id)'")),
        ("escape", "te\u{1b}[2Jam", None),
    ];
    for (row, pool, shown) in rows {
        let command = login_rerun_command(None, &pool_args("work", "work", pool));

        let expected = shown.map(|words| {
            format!(
                "routectl provider add --login anthropic --label work {words} --name work --yes"
            )
        });
        assert_eq!(command, expected, "row `{row}`");
    }
}

/// `--pool` without `--login` is refused before any credential capture: no
/// stdin read, prompt or login. A pool member must be an oauth seat.
#[tokio::test]
async fn a_pool_without_a_login_is_refused_before_any_capture() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &base());
    let args = ProviderAddArgs {
        format: Some("anthropic-messages".to_string()),
        login: None,
        label: None,
        api_key_stdin: true,
        ..pool_args("key", "unused", "team")
    };
    let io = LoginIo::default();

    // Act
    let err = run_with_io(&path, args, &io)
        .await
        .expect_err("refused")
        .to_string();

    // Assert: the stub panics on any stdin read or prompt.
    assert!(err.contains("`--pool`") && err.contains("--login"), "{err}");
    assert_eq!(io.login_count(), 0);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), base());
}

/// An empty inline parent is a valid config; the one candidate creates its
/// first child there and the written file reparses and gates.
#[tokio::test]
async fn a_pool_add_under_an_empty_inline_parent_writes_a_valid_config() {
    let rows = [
        (
            "pools = {}",
            base().replace("[server]", "pools = {}\n[server]"),
        ),
        (
            "providers = {}",
            base().replace("[server]", "providers = {}\n[server]"),
        ),
    ];
    for (row, body) in rows {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), &body);

        // Act
        let result = run_with_io(
            &path,
            pool_args("work", "work", "team"),
            &LoginIo::default(),
        )
        .await;

        // Assert
        assert_eq!(result.ok(), Some(AddResult::Written), "row `{row}`");
        let text = std::fs::read_to_string(&path).unwrap();
        let config = parse_config(&text).unwrap_or_else(|e| panic!("row `{row}`: {e}"));
        assert!(gate(&text).is_ok(), "row `{row}`: {text}");
        assert_eq!(config.pools["team"].members, ["work"], "row `{row}`");
        assert!(config.pools["team"].accepts_new_logins, "row `{row}`");
    }
}

/// Re-running a completed `--pool` add is a no-op: the block is identical
/// and the pool already lists the entry.
#[tokio::test]
async fn a_rerun_of_a_completed_pool_add_writes_nothing() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &base());
    run_with_io(
        &path,
        pool_args("work", "work", "team"),
        &LoginIo::default(),
    )
    .await
    .expect("first add writes");
    let after_first = std::fs::read_to_string(&path).unwrap();

    // Act
    let result = run_with_io(
        &path,
        pool_args("work", "work", "team"),
        &LoginIo::default(),
    )
    .await
    .expect("rerun");

    // Assert
    assert_eq!(result, AddResult::NoChange);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), after_first);
}

/// An identical entry not yet in the pool gets only the pool half: its own
/// block (comments and key order included) keeps every byte.
#[tokio::test]
async fn an_identical_entry_outside_the_pool_is_joined_without_rewriting_its_block() {
    // Arrange
    let hand_block = "\n[providers.work]\n\
                      auth = \"bearer\" # subscription\n\
                      format = \"anthropic-messages\"\n\
                      credential = \"oauth://anthropic#work\"\n\
                      base_url = \"https://api.anthropic.com\"\n";
    let body = format!(
        "{}{}{hand_block}\n[pools.team]\nmembers = [\"first\"]\n",
        base(),
        seat("first", "oauth://anthropic#first", "")
    );
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &body);

    // Act
    let result = run_with_io(
        &path,
        pool_args("work", "work", "team"),
        &LoginIo::default(),
    )
    .await
    .expect("the join writes");

    // Assert
    assert_eq!(result, AddResult::Written);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains(hand_block), "{text}");
    assert!(text.contains("members = [\"first\", \"work\"]"), "{text}");
}

/// The stale-snapshot path through `commit` writes neither half.
#[test]
fn a_stale_snapshot_commits_neither_the_entry_nor_the_pool() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &base());
    let stale = base();
    let rewritten = format!("{stale}# out of band\n");
    std::fs::write(&path, &rewritten).unwrap();
    let args = pool_args("work", "work", "team");
    let candidate = candidate_over(&stale, &args);

    // Act
    let result = commit(&path, stale.as_bytes(), &stale, &candidate);

    // Assert
    assert!(result.is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), rewritten);
}
