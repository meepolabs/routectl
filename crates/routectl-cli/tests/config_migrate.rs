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
