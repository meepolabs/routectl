//! Runtime boundary smoke for `config migrate`: migrate a temp v1 and a temp
//! v2 config through the real command pipeline, then boot the server on the
//! migrated result and confirm it serves `/health`.
//!
//! Every config here is a TEMP copy -- the command is never pointed at a live
//! config (see the config-command test-isolation learning). Health is polled
//! generously rather than probed once, so the boot is not raced.

use std::sync::Arc;
use std::time::Duration;

use routectl_cli::commands::config_migrate_cmd::{self, MigrateResult};
use tokio::net::TcpListener;

mod common;

const V1_CONFIG: &str = "\
[server]
host = \"127.0.0.1\"
port = 8787

[cache_pricing]
\"openai-compat:grok-*\" = { wm = 1.5, override_acknowledges_cost_risk = true }

[providers.fast]
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"__API_KEY_REF__\"

[models.gpt]
provider = \"fast\"
upstream = \"gpt-4o\"

[aliases]
default = \"gpt\"
";

const V2_CONFIG: &str = "\
version = 2

[server]
host = \"127.0.0.1\"
port = 8787

[retry]
max_attempts = 2
retry_allowlist = []
retry_denylist = []

[providers.fast]
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"__API_KEY_REF__\"

[models.gpt]
provider = \"fast\"
upstream = \"gpt-4o\"

[aliases]
default = \"gpt\"
";

/// Bind to an ephemeral port, boot the server on `config`, and return the base
/// URL once `/health` responds. The usage DB is isolated so the booted writer
/// never touches the real one.
async fn boot_and_await_health(config: Arc<routectl_router::Config>) -> String {
    let config = common::isolate_usage_db(config);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");

    tokio::spawn(async move {
        routectl_cli::server::serve_on_listener(config, listener, None)
            .await
            .expect("server failed to serve");
    });

    // Poll generously rather than sleeping a fixed tick, so a slow boot on a
    // loaded CI box does not flake the smoke.
    for _ in 0..100 {
        if let Ok(resp) = reqwest::get(format!("{base}/health")).await
            && resp.status().is_success()
        {
            return base;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("server did not become healthy in time");
}

/// Migrate `body` in a temp dir with `--yes`, returning the migrated config
/// text and the (parsed) Config ready to boot.
async fn migrate_temp(
    body: &str,
    expected_from: u32,
) -> (tempfile::TempDir, routectl_router::Config) {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let overlay_path = dir.path().join("catalog_overlay.json");
    // A credentials path inside the temp dir: the store opens EMPTY, so the
    // migration's seat-materialization phase is a no-op and the operator's
    // real credentials are never read.
    let credentials_path = dir.path().join("credentials.json");
    let body = body.replace("__API_KEY_REF__", &common::file_ref("test-key"));
    std::fs::write(&config_path, body).unwrap();

    let result =
        config_migrate_cmd::run_at(&config_path, &overlay_path, &credentials_path, false, true)
            .await
            .expect("migration must succeed");
    assert_eq!(
        result,
        MigrateResult::Migrated {
            from_version: expected_from
        }
    );

    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(
        text.contains(&format!(
            "version = {}",
            routectl_router::CURRENT_CONFIG_VERSION
        )),
        "migrated file: {text}"
    );
    let config = routectl_router::parse_config(&text).expect("migrated config parses");
    (dir, config)
}

#[tokio::test]
async fn serve_boots_on_a_migrated_v2_config() {
    let (_dir, config) = migrate_temp(V2_CONFIG, 2).await;
    let base = boot_and_await_health(Arc::new(config)).await;

    let resp = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn serve_boots_on_a_migrated_v1_config() {
    let (_dir, config) = migrate_temp(V1_CONFIG, 1).await;
    let base = boot_and_await_health(Arc::new(config)).await;

    let resp = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(resp.status(), 200);
}

/// A previous-version config whose provider entry carries a BARE `oauth://`
/// ref and the retired provider-level `seat_selection`. With more than one
/// stored seat for the family, the migration materializes explicit account
/// entries plus a pool -- and the emitted file must BOOT, not merely pass
/// `config check` (check never calls `build_provider`).
fn previous_version_bare_oauth_config() -> String {
    format!(
        "version = {}\n\
         \n\
         [server]\n\
         host = \"127.0.0.1\"\n\
         port = 0\n\
         \n\
         [providers.anthropic-managed]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic\"\n\
         seat_selection = \"round-robin\"\n\
         \n\
         [models.opus]\n\
         provider = \"anthropic-managed\"\n\
         upstream = \"claude-opus-4-8\"\n\
         \n\
         [aliases]\n\
         default = \"opus\"\n",
        routectl_router::CURRENT_CONFIG_VERSION - 1
    )
}

/// Seed a credential store at `path` with one inert record per seat key, at
/// the `0o600` the store requires. Written directly rather than through a
/// login flow: the migration only ever reads seat KEYS.
fn seed_seats(path: &std::path::Path, seat_keys: &[&str]) {
    use std::os::unix::fs::PermissionsExt as _;

    let record = serde_json::json!({
        "access_token": "not-a-real-token",
        "refresh_token": "not-a-real-token",
        "expires_at_unix": 4_000_000_000_u64,
        "obtained_at_unix": 1_000_u64,
    });
    let providers: serde_json::Map<String, serde_json::Value> = seat_keys
        .iter()
        .map(|key| ((*key).to_string(), record.clone()))
        .collect();
    let body = serde_json::json!({
        "schema_version": routectl_auth::oauth::SCHEMA_VERSION,
        "providers": providers,
    });
    std::fs::write(path, body.to_string()).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[tokio::test]
async fn serve_boots_on_a_migrated_multi_seat_pool_config() {
    // Arrange: two stored seats, so the bare ref materializes into a pool.
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let overlay_path = dir.path().join("catalog_overlay.json");
    let credentials_path = dir.path().join("credentials.json");
    std::fs::write(&config_path, previous_version_bare_oauth_config()).unwrap();
    seed_seats(&credentials_path, &["anthropic", "anthropic#work"]);

    // Act
    config_migrate_cmd::run_at(&config_path, &overlay_path, &credentials_path, false, true)
        .await
        .expect("the combined migration must succeed");

    // Assert: the emitted file carries the pool and its account entries ...
    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(text.contains("[pools.anthropic]"), "migrated file: {text}");
    assert!(
        text.contains("[providers.anthropic-work]"),
        "migrated file: {text}"
    );
    // ... parses, and BOOTS.
    let config = routectl_router::parse_config(&text).expect("migrated config parses");
    let base = boot_and_await_health(Arc::new(config)).await;
    let resp = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(resp.status(), 200);
}

/// The deprecated `--force` alias still skips the confirmation and migrates,
/// exercising the real CLI's `yes || force` normalization plus its deprecation
/// notice -- the one-release compatibility promise the canonical `--yes` swap
/// must not break.
#[test]
fn migrate_force_alias_still_skips_confirmation_and_warns() {
    let bin = env!("CARGO_BIN_EXE_routectl");
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, V2_CONFIG).unwrap();

    let out = std::process::Command::new(bin)
        .args(["config", "migrate", "--force"])
        .arg("--config")
        .arg(&config_path)
        .output()
        .expect("run `config migrate --force`");

    assert!(
        out.status.success(),
        "the deprecated alias must still migrate: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("deprecated") && stderr.contains("--yes"),
        "the alias must warn and point at --yes:\n{stderr}"
    );
    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(
        text.contains(&format!(
            "version = {}",
            routectl_router::CURRENT_CONFIG_VERSION
        )),
        "the alias must skip the confirm and write the current version: {text}"
    );
}

/// A v2 config whose provider carries an obviously-fake secret in BOTH accepted
/// positions: `base_url` userinfo (only the `[mitm]` validator rejects it) and a
/// `literal:` key ref. `--dry-run` must reproduce these bytes verbatim.
const V2_CONFIG_WITH_SECRETS: &str = "\
version = 2

[server]
host = \"127.0.0.1\"
port = 8787

[retry]
max_attempts = 2
retry_allowlist = []
retry_denylist = []

[providers.fast]
kind = \"openai-compat\"
base_url = \"https://svc:sk-userinfo-FAKE@internal.example:8443/v1\"
api_key_ref = \"literal:sk-literal-FAKE\"

[models.gpt]
provider = \"fast\"
upstream = \"gpt-4o\"

[aliases]
default = \"gpt\"
";

/// Run `config migrate` with `args` on a temp copy of `body`, returning
/// (stdout, stderr, final file text).
fn run_migrate(body: &str, args: &[&str]) -> (String, String, String) {
    let bin = env!("CARGO_BIN_EXE_routectl");
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, body).unwrap();

    let out = std::process::Command::new(bin)
        .args(["config", "migrate"])
        .args(args)
        .arg("--config")
        .arg(&config_path)
        // Pin the child's config dir into the tempdir. Without this the child
        // resolves the catalog overlay from the ambient `~/.config/routectl/`,
        // so a fixture that produces an overlay write (any v1 config carrying
        // `[cache_pricing]`) would mutate the operator's live overlay.
        .env("XDG_CONFIG_HOME", dir.path())
        .output()
        .expect("run `config migrate`");
    assert!(
        out.status.success(),
        "migrate {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (
        String::from_utf8(out.stdout).expect("stdout is utf-8"),
        String::from_utf8(out.stderr).expect("stderr is utf-8"),
        std::fs::read_to_string(&config_path).unwrap(),
    )
}

/// Pull the candidate block out of `--dry-run` stdout, between the framing
/// lines.
fn candidate_block(stdout: &str) -> String {
    let after_header = stdout
        .split_once("---\n")
        .expect("stdout must carry the candidate header")
        .1;
    after_header
        .split_once("--- end candidate ---")
        .expect("stdout must carry the end marker")
        .0
        .to_string()
}

/// The dry-run contract is byte-exactness: the candidate on STDOUT must equal
/// the bytes a real migration writes, secrets included. The credential warning
/// therefore lives on STDERR -- a future change that redacts stdout must fail
/// this test.
#[test]
fn dry_run_stdout_is_byte_exact_and_the_credential_warning_is_on_stderr() {
    let (stdout, stderr, unwritten) = run_migrate(V2_CONFIG_WITH_SECRETS, &["--dry-run"]);
    assert_eq!(unwritten, V2_CONFIG_WITH_SECRETS, "dry-run must not write");

    let (_, _, written) = run_migrate(V2_CONFIG_WITH_SECRETS, &["--yes"]);
    assert_eq!(
        candidate_block(&stdout),
        written,
        "the dry-run candidate must be byte-identical to what a real migration writes"
    );

    for secret in ["sk-userinfo-FAKE", "literal:sk-literal-FAKE"] {
        assert!(
            stdout.contains(secret),
            "`{secret}` must survive verbatim on stdout; stdout:\n{stdout}"
        );
        assert!(
            !stderr.contains(secret),
            "the warning must not echo `{secret}`; stderr:\n{stderr}"
        );
    }

    for phrase in ["byte-exact", "credentials", "bug report", "ROTATE"] {
        assert!(
            stderr.contains(phrase),
            "the stderr warning must mention `{phrase}`; stderr:\n{stderr}"
        );
    }
    // Exactly ONE `warning:` line, not a duplicate and not split across lines:
    // the phrase checks above would pass either way.
    let warning_lines: Vec<&str> = stderr
        .lines()
        .filter(|l| l.starts_with("warning:"))
        .collect();
    assert_eq!(
        warning_lines.len(),
        1,
        "expected exactly one `warning:` line on stderr; stderr:\n{stderr}"
    );
    for phrase in ["byte-exact", "credentials", "bug report", "ROTATE"] {
        assert!(
            warning_lines[0].contains(phrase),
            "the single warning line must carry `{phrase}`; line:\n{}",
            warning_lines[0]
        );
    }
    assert!(
        !stdout.contains("warning:"),
        "the warning must not reach stdout; stdout:\n{stdout}"
    );
}
