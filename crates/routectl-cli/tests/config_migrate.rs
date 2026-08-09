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
fn migrate_temp(body: &str, expected_from: u32) -> (tempfile::TempDir, routectl_router::Config) {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let overlay_path = dir.path().join("catalog_overlay.json");
    let body = body.replace("__API_KEY_REF__", &common::file_ref("test-key"));
    std::fs::write(&config_path, body).unwrap();

    let result = config_migrate_cmd::run_at(&config_path, &overlay_path, false, true)
        .expect("migration must succeed");
    assert_eq!(
        result,
        MigrateResult::Migrated {
            from_version: expected_from
        }
    );

    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(text.contains("version = 3"), "migrated file: {text}");
    let config = routectl_router::parse_config(&text).expect("migrated config parses");
    (dir, config)
}

#[tokio::test]
async fn serve_boots_on_a_migrated_v2_config() {
    let (_dir, config) = migrate_temp(V2_CONFIG, 2);
    let base = boot_and_await_health(Arc::new(config)).await;

    let resp = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn serve_boots_on_a_migrated_v1_config() {
    let (_dir, config) = migrate_temp(V1_CONFIG, 1);
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
        text.contains("version = 3"),
        "the alias must skip the confirm and write v3: {text}"
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
