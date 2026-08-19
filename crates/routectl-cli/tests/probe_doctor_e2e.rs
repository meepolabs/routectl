//! Integration floor for the two READ-ONLY inspection commands, `provider
//! probe` and `doctor`, driven against the REAL binary. The owning command
//! modules unit-test each classification branch; this binary pins the seams
//! of the whole composition: that a real process invocation over a config
//! mixing every credential shape mutates nothing on disk, exits per the
//! PASS/WARN=0 / FAIL=nonzero contract deterministically, and emits valid
//! `--json` carrying a `schema_version`.
//!
//! Isolation: every run points `--config` at a temp config and scopes
//! `XDG_CONFIG_HOME` at a fresh tempdir (so credentials.json, the catalog
//! overlay, the managed secret store, and the usage DB all resolve inside
//! the tempdir, never the developer's real `~/.config/routectl`). The scope
//! is set on the CHILD process only -- the parent's environment is never
//! mutated -- so the tests need no `#[serial]` guard and stay isolated even
//! under parallel execution. A completed OAuth login is simulated by
//! provisioning a future-expiry credentials.json into the scoped dir. No run
//! touches a live network: the unreachable provider points at a closed
//! loopback port, which refuses immediately.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use routectl_router::CURRENT_CONFIG_VERSION as CURRENT;

mod common;

/// The real `routectl` binary under test, resolved by cargo for this
/// integration crate.
const BIN: &str = env!("CARGO_BIN_EXE_routectl");

/// A generous wall-clock cap on any single command. The unreachable probe
/// resolves near-instantly (loopback connection refused), so this is a
/// no-hang backstop, not a synchronization delay: it is enforced by a
/// bounded channel receive, never a fixed sleep.
const COMMAND_BUDGET: Duration = Duration::from_mins(1);

/// Config mixing all three credential kinds the probe classifier branches
/// on: a `forwarded` provider (informational skip, no build, no billed
/// call), an `oauth://` provider (probed read-only, no refresh), and a
/// static api-key provider pointed at a closed loopback port (Unreachable
/// -> Fail -> nonzero).
fn mixed_config() -> String {
    format!("version = {}\n{MIXED_CONFIG_BODY}", CURRENT)
}

const MIXED_CONFIG_BODY: &str = "\
[providers.anthropic-forwarded]
kind = \"anthropic-api\"
base_url = \"https://api.anthropic.com\"
credential_source = \"forwarded\"

[providers.anthropic]
kind = \"anthropic-api\"
api_key_ref = \"oauth://anthropic\"

[providers.unreachable]
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"__API_KEY_REF__\"

[models.sonnet]
provider = \"anthropic\"
upstream = \"claude-sonnet-4-5\"

[aliases]
default = \"sonnet\"
";

/// A clean config: the forwarded provider (skip) plus the oauth provider
/// (reachable, future-expiry credential). No unreachable provider, so a
/// probe or doctor run over it is all PASS/WARN -> exit 0.
fn healthy_config() -> String {
    format!("version = {CURRENT}\n{HEALTHY_CONFIG_BODY}")
}

const HEALTHY_CONFIG_BODY: &str = "\
[providers.anthropic-forwarded]
kind = \"anthropic-api\"
base_url = \"https://api.anthropic.com\"
credential_source = \"forwarded\"

[providers.anthropic]
kind = \"anthropic-api\"
api_key_ref = \"oauth://anthropic\"

[models.sonnet]
provider = \"anthropic\"
upstream = \"claude-sonnet-4-5\"

[aliases]
default = \"sonnet\"
";

/// The oauth id the healthy/mixed configs authenticate against; the seed
/// credential and the config's `oauth://` ref must name the same id.
const OAUTH_PROVIDER: &str = "anthropic";

/// A config with two routed lanes on a single openai-compat provider (pointed
/// at a closed loopback port so its probe refuses instantly) plus a
/// provider-scoped override. Seeds the doctor capability matrix with two
/// lanes to pivot and an override layer that overrules a learned negative.
fn capability_config() -> String {
    format!("version = {}\n{CAPABILITY_CONFIG_BODY}", CURRENT)
}

const CAPABILITY_CONFIG_BODY: &str = "\
[providers.local]
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"__API_KEY_REF__\"

[models.laneA]
provider = \"local\"
upstream = \"m-a\"

[models.laneB]
provider = \"local\"
upstream = \"m-b\"

[capability.overrides.local]
force_supported = [\"prompt_caching\"]
";

struct CmdResult {
    code: i32,
    stdout: String,
    stderr: String,
}

impl CmdResult {
    fn context(&self) -> String {
        format!(
            "exit={}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.code, self.stdout, self.stderr
        )
    }
}

/// The `$XDG_CONFIG_HOME/routectl` directory every artifact resolves under.
fn routectl_dir(xdg: &Path) -> PathBuf {
    xdg.join("routectl")
}

/// Run the real binary with `--config <config>` and a child-scoped
/// `XDG_CONFIG_HOME`, on a worker thread whose result is delivered through a
/// bounded receive: a hang (a regression that blocked on the network or a
/// prompt) never sends, so `recv_timeout` fails the test instead of stalling
/// the suite. This is the no-hang budget -- enforced without a fixed sleep.
fn run_bounded(xdg: &Path, config: &Path, args: &[&str]) -> CmdResult {
    let xdg = xdg.to_path_buf();
    let config = config.to_path_buf();
    let args: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut cmd = Command::new(BIN);
        cmd.arg("--config").arg(&config);
        for a in &args {
            cmd.arg(a);
        }
        // Scope the child's config root and clear the two ambient variables
        // that could otherwise steer resolution away from the tempdir.
        // ROUTECTL_LOG is deliberately left unset: the binary routes tracing
        // to stderr, so default-level logs never contaminate `--json` stdout.
        cmd.env("XDG_CONFIG_HOME", &xdg)
            .env_remove("ROUTECTL_CONFIG")
            .env_remove("ANTHROPIC_API_KEY")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let _ = tx.send(cmd.output());
    });

    let out = rx
        .recv_timeout(COMMAND_BUDGET)
        .expect("command must complete within the bounded budget (no hang)")
        .expect("the routectl binary must spawn");
    CmdResult {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn write_config(xdg: &Path, body: &str) -> PathBuf {
    let dir = routectl_dir(xdg);
    std::fs::create_dir_all(&dir).expect("create routectl config dir");
    let path = dir.join("config.toml");
    let body = body.replace("__API_KEY_REF__", &common::file_ref("probe-key-not-real"));
    std::fs::write(&path, body).expect("write config");
    path
}

/// Provision a future-expiry OAuth token for `provider` into the scoped
/// credentials.json (schema v1, 0600 on Unix), simulating a completed login.
/// Future expiry makes the read-only probe classify it Reachable, and the
/// byte-identical assertion then proves neither command refreshed it.
fn seed_credentials(xdg: &Path, provider: &str) -> PathBuf {
    let dir = routectl_dir(xdg);
    std::fs::create_dir_all(&dir).expect("create routectl config dir");
    let path = dir.join("credentials.json");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let creds = serde_json::json!({
        "schema_version": 1,
        "providers": {
            provider: {
                "access_token": format!("provisioned-access-{provider}"),
                "refresh_token": format!("provisioned-refresh-{provider}"),
                "token_type": "Bearer",
                "expires_at_unix": now + 3600,
                "scopes": ["user:inference"],
                "account": { "email": null, "account_id": format!("acct-{provider}") },
                "obtained_at_unix": now
            }
        }
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&creds).unwrap()).expect("write credentials");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("chmod credentials 0600");
    }
    path
}

/// Seed a valid, current-schema catalog overlay at the default path so both
/// commands read it (they load through the effective-config loader) and the
/// byte-identical assertion over it is non-vacuous.
fn seed_overlay(xdg: &Path) -> PathBuf {
    let dir = routectl_dir(xdg);
    std::fs::create_dir_all(&dir).expect("create routectl config dir");
    let path = dir.join("catalog_overlay.json");
    std::fs::write(&path, br#"{"schema_version":1,"revision":0,"cells":{}}"#)
        .expect("write catalog overlay");
    path
}

/// Seed a real, current-schema usage DB at the default path. Doctor opens it
/// read-only (the would-trim panel); the byte-identical assertion over the
/// main DB file then proves the open never wrote to it.
fn seed_usage_db(xdg: &Path) -> PathBuf {
    let dir = routectl_dir(xdg);
    std::fs::create_dir_all(&dir).expect("create routectl config dir");
    let path = dir.join("usage.db");
    // Opening migrates a fresh file to the current schema, then the handle is
    // dropped so the writer connection is closed before the snapshot is taken.
    let db = routectl_usage::open(&path).expect("create + migrate usage db");
    drop(db);
    path
}

/// Seed the usage ledger with a matched boot tombstone plus three
/// post-boundary capability events across two lanes: a verified live
/// positive, a probe negative, and a live negative the config override
/// overrules. Stamped at the current instant and the baked catalog version /
/// overlay revision 0, so the doctor's read-only replay resolves the boundary
/// and admits every row. Mirrors the persistence lifecycle suite's seeding.
fn seed_capability_ledger(xdg: &Path) -> PathBuf {
    let dir = routectl_dir(xdg);
    std::fs::create_dir_all(&dir).expect("create routectl config dir");
    let path = dir.join("usage.db");
    let db = routectl_usage::open(&path).expect("create + migrate usage db");
    let cat = i64::from(routectl_router::CATALOG_VERSION);
    let ts = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .expect("epoch ms fits i64");

    let event = |lane: &str, cap: &str, verdict: &str, phase: &str, source: &str, evidence| {
        routectl_usage::CapabilityEvent {
            ts,
            lane_key: lane.to_string(),
            capability: cap.to_string(),
            verdict: verdict.to_string(),
            phase: phase.to_string(),
            source: source.to_string(),
            tier: "self-identifying".to_string(),
            evidence_class: evidence,
            upstream_token: None,
            catalog_version: cat,
            overlay_revision: 0,
        }
    };
    let insert = |e: &routectl_usage::CapabilityEvent| {
        routectl_usage::insert_capability_event(db.conn(), e).expect("insert capability event");
    };

    insert(&routectl_usage::CapabilityEvent::tombstone(ts, cat, 0));
    insert(&event(
        "laneA",
        "web_search",
        "verified",
        "f3",
        "live",
        Some("schema_parse".to_string()),
    ));
    insert(&event(
        "laneA",
        "computer_use",
        "broken",
        "f1",
        "probe",
        None,
    ));
    insert(&event(
        "laneA",
        "prompt_caching",
        "broken",
        "f1",
        "live",
        None,
    ));
    drop(db);
    path
}

/// Full recursive byte snapshot of `dir`, EXCLUDING the WAL sidecars the
/// usage DB opens alongside its main file (`*-wal` / `*-shm`). Those are
/// runtime-managed and not part of the read-only contract; the main DB
/// file's byte-identity is asserted directly instead.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(cur) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&cur) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && (name.ends_with("-wal") || name.ends_with("-shm"))
            {
                continue;
            }
            if let Ok(bytes) = std::fs::read(&path) {
                out.insert(path, bytes);
            }
        }
    }
    out
}

/// Parse `--json` stdout, failing with the full command context on invalid
/// JSON so a broken serialization is a clear failure, not a silent panic.
fn parse_json(result: &CmdResult) -> serde_json::Value {
    serde_json::from_str(&result.stdout).unwrap_or_else(|e| {
        panic!(
            "--json output must be valid JSON: {e}\n{}",
            result.context()
        )
    })
}

// ---------------------------------------------------------------------
// Read-only end to end: neither command mutates the config, the credentials
// (no oauth refresh), the catalog overlay, or the usage DB. Asserted per
// artifact AND over the whole config tree.
// ---------------------------------------------------------------------

#[test]
fn probe_and_doctor_leave_every_artifact_byte_identical() {
    let xdg = tempfile::tempdir().unwrap();
    let config = write_config(xdg.path(), &mixed_config());
    let creds = seed_credentials(xdg.path(), OAUTH_PROVIDER);
    let overlay = seed_overlay(xdg.path());
    let usage = seed_usage_db(xdg.path());

    let config_before = std::fs::read(&config).unwrap();
    let creds_before = std::fs::read(&creds).unwrap();
    let overlay_before = std::fs::read(&overlay).unwrap();
    let usage_before = std::fs::read(&usage).unwrap();
    let tree_before = snapshot(xdg.path());

    // Every read-only surface: both commands, both renders.
    run_bounded(xdg.path(), &config, &["provider", "probe"]);
    run_bounded(xdg.path(), &config, &["provider", "probe", "--json"]);
    run_bounded(xdg.path(), &config, &["doctor"]);
    run_bounded(xdg.path(), &config, &["doctor", "--json"]);

    assert_eq!(
        std::fs::read(&config).unwrap(),
        config_before,
        "the config file must be byte-identical after both commands"
    );
    assert_eq!(
        std::fs::read(&creds).unwrap(),
        creds_before,
        "the credentials file must be byte-identical (no oauth refresh)"
    );
    assert_eq!(
        std::fs::read(&overlay).unwrap(),
        overlay_before,
        "the catalog overlay must be byte-identical after both commands"
    );
    assert_eq!(
        std::fs::read(&usage).unwrap(),
        usage_before,
        "the usage DB must be byte-identical after both commands"
    );
    assert_eq!(
        snapshot(xdg.path()),
        tree_before,
        "a read-only run must not create, mutate, or delete any file"
    );
}

// ---------------------------------------------------------------------
// Exit-code contract: PASS/WARN=0, FAIL nonzero, deterministic across
// repeats regardless of probe completion ordering.
// ---------------------------------------------------------------------

#[test]
fn probe_exit_is_zero_when_healthy_and_nonzero_with_an_unreachable_provider() {
    let xdg = tempfile::tempdir().unwrap();
    seed_credentials(xdg.path(), OAUTH_PROVIDER);
    seed_overlay(xdg.path());

    let healthy = write_config(xdg.path(), &healthy_config());
    let healthy_run = run_bounded(xdg.path(), &healthy, &["provider", "probe"]);
    assert_eq!(
        healthy_run.code,
        0,
        "a forwarded skip + a reachable oauth provider must exit 0:\n{}",
        healthy_run.context()
    );

    let mixed = write_config(xdg.path(), &mixed_config());
    let mut codes = Vec::new();
    for _ in 0..3 {
        let run = run_bounded(xdg.path(), &mixed, &["provider", "probe"]);
        assert_ne!(
            run.code,
            0,
            "a config with an unreachable provider must exit nonzero:\n{}",
            run.context()
        );
        codes.push(run.code);
    }
    assert!(
        codes.windows(2).all(|w| w[0] == w[1]),
        "the exit code must be deterministic across repeats: {codes:?}"
    );
}

#[test]
fn doctor_exit_is_zero_when_healthy_and_nonzero_with_an_unreachable_provider() {
    let xdg = tempfile::tempdir().unwrap();
    seed_credentials(xdg.path(), OAUTH_PROVIDER);
    seed_overlay(xdg.path());
    seed_usage_db(xdg.path());

    let healthy = write_config(xdg.path(), &healthy_config());
    let healthy_run = run_bounded(xdg.path(), &healthy, &["doctor"]);
    assert_eq!(
        healthy_run.code,
        0,
        "a clean config (no FAIL findings) must exit 0:\n{}",
        healthy_run.context()
    );

    let mixed = write_config(xdg.path(), &mixed_config());
    let mut codes = Vec::new();
    for _ in 0..3 {
        let run = run_bounded(xdg.path(), &mixed, &["doctor"]);
        assert_ne!(
            run.code,
            0,
            "an unreachable provider drives a probe FAIL -> nonzero:\n{}",
            run.context()
        );
        codes.push(run.code);
    }
    assert!(
        codes.windows(2).all(|w| w[0] == w[1]),
        "the exit code must be deterministic across repeats: {codes:?}"
    );
}

// ---------------------------------------------------------------------
// --json is valid JSON carrying schema_version, for both commands.
// ---------------------------------------------------------------------

#[test]
fn json_stdout_stays_clean_while_a_provider_is_built_without_a_log_override() {
    let xdg = tempfile::tempdir().unwrap();
    seed_credentials(xdg.path(), OAUTH_PROVIDER);
    seed_overlay(xdg.path());
    seed_usage_db(xdg.path());
    // The unreachable provider forces a real provider build (the probe
    // constructs it to attempt the connection), whose instrument span closes
    // at the default INFO level -- the exact tracing that used to interleave
    // with `--json` stdout. `run_bounded` sets no ROUTECTL_LOG, so this pins
    // the default-level contract, not a suppressed one.
    let config = write_config(xdg.path(), &mixed_config());

    for args in [
        ["provider", "probe", "--json"].as_slice(),
        ["doctor", "--json"].as_slice(),
    ] {
        let run = run_bounded(xdg.path(), &config, args);
        // A single clean JSON document: serde_json rejects any interleaved log
        // line as trailing/leading garbage, so a successful parse proves stdout
        // carries nothing but the document.
        let value = parse_json(&run);
        assert!(
            value.get("schema_version").is_some(),
            "{args:?} stdout must be a single clean JSON document:\n{}",
            run.context()
        );
        // Logging is moved off stdout, not disabled: the provider-build span
        // still emits on stderr at the default level.
        assert!(
            run.stderr.contains("provider"),
            "{args:?} must still emit provider-build tracing on stderr:\n{}",
            run.context()
        );
        assert!(
            !run.stdout.contains("time.busy"),
            "{args:?} stdout must carry no span-close tracing:\n{}",
            run.context()
        );
    }
}

#[test]
fn probe_json_is_valid_and_carries_schema_version() {
    let xdg = tempfile::tempdir().unwrap();
    seed_credentials(xdg.path(), OAUTH_PROVIDER);
    seed_overlay(xdg.path());
    let config = write_config(xdg.path(), &mixed_config());

    let run = run_bounded(xdg.path(), &config, &["provider", "probe", "--json"]);
    let value = parse_json(&run);
    assert!(
        value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            .is_some(),
        "probe --json must carry a numeric schema_version:\n{}",
        run.context()
    );
    assert!(
        value
            .get("providers")
            .is_some_and(serde_json::Value::is_array),
        "probe --json must carry a providers array:\n{}",
        run.context()
    );
}

#[test]
fn doctor_json_is_valid_and_carries_schema_version() {
    let xdg = tempfile::tempdir().unwrap();
    seed_credentials(xdg.path(), OAUTH_PROVIDER);
    seed_overlay(xdg.path());
    seed_usage_db(xdg.path());
    let config = write_config(xdg.path(), &mixed_config());

    let run = run_bounded(xdg.path(), &config, &["doctor", "--json"]);
    let value = parse_json(&run);
    assert!(
        value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            .is_some(),
        "doctor --json must carry a numeric schema_version:\n{}",
        run.context()
    );
    assert!(
        value
            .get("findings")
            .is_some_and(serde_json::Value::is_array),
        "doctor --json must carry a findings array:\n{}",
        run.context()
    );
}

// ---------------------------------------------------------------------
// The doctor binary surfaces the read-only capability-matrix panel AND the
// catalog-freshness rows in BOTH renders, driven by a real seeded ledger.
// The unit modules pin the panel/section shapes; this pins that the wired
// binary loads the config, resolves the boundary from the seeded overlay
// revision, replays the ledger, and emits both on stdout.
// ---------------------------------------------------------------------

#[test]
fn doctor_binary_renders_seeded_capability_matrix_and_freshness() {
    let xdg = tempfile::tempdir().unwrap();
    seed_overlay(xdg.path());
    seed_capability_ledger(xdg.path());
    let config = write_config(xdg.path(), &capability_config());

    // Human render: the replayed matrix state line, the seeded verdict tokens
    // (an override cell overruling the seeded negative), and the freshness
    // section with its always-present baked-catalog row.
    let human = run_bounded(xdg.path(), &config, &["doctor"]);
    assert!(
        human.stdout.contains("capability matrix:")
            && human.stdout.contains("learned registry replayed"),
        "doctor human render must carry the replayed matrix panel:\n{}",
        human.context()
    );
    for token in [
        "verified[live]",
        "broken[probe]",
        "forced_supported[override]",
    ] {
        assert!(
            human.stdout.contains(token),
            "doctor human matrix must render {token}:\n{}",
            human.context()
        );
    }
    assert!(
        human.stdout.contains("Catalog freshness") && human.stdout.contains("baked catalog v"),
        "doctor human render must carry the catalog-freshness rows:\n{}",
        human.context()
    );

    // JSON render: the panel is Available with the seeded cells, and the
    // findings carry the freshness section.
    let json = run_bounded(xdg.path(), &config, &["doctor", "--json"]);
    let value = parse_json(&json);

    let panel = &value["panels"]["capability_matrix"];
    assert_eq!(
        panel["availability"]["state"],
        serde_json::json!("available"),
        "seeded ledger must drive an Available matrix:\n{}",
        json.context()
    );
    let cell = |lane: &str, cap: &str| -> serde_json::Value {
        let columns = panel["columns"].as_array().expect("columns array");
        let ci = columns
            .iter()
            .position(|c| c == cap)
            .unwrap_or_else(|| panic!("column {cap} missing"));
        let lanes = panel["lanes"].as_array().expect("lanes array");
        let lane_obj = lanes
            .iter()
            .find(|l| l["lane"] == serde_json::json!(lane))
            .unwrap_or_else(|| panic!("lane {lane} missing"));
        lane_obj["cells"][ci].clone()
    };
    let verified = cell("laneA", "web_search");
    assert_eq!(verified["verdict"], serde_json::json!("verified"));
    assert_eq!(verified["source"], serde_json::json!("live"));
    let overridden = cell("laneA", "prompt_caching");
    assert_eq!(
        overridden["verdict"],
        serde_json::json!("forced_supported"),
        "the override must overrule the seeded live negative: {overridden}"
    );
    assert_eq!(overridden["source"], serde_json::json!("override"));

    let findings = value["findings"].as_array().expect("findings array");
    assert!(
        findings
            .iter()
            .any(|f| f["section"] == serde_json::json!("freshness")),
        "doctor --json findings must carry the catalog-freshness section:\n{}",
        json.context()
    );
}

// ---------------------------------------------------------------------
// The forwarded provider is an INFORMATIONAL skip in both surfaces: a
// Skipped outcome / a Pass finding, never a WARN, and it bills no call.
// ---------------------------------------------------------------------

#[test]
fn forwarded_provider_is_an_informational_skip_in_both_surfaces() {
    let xdg = tempfile::tempdir().unwrap();
    seed_credentials(xdg.path(), OAUTH_PROVIDER);
    seed_overlay(xdg.path());
    seed_usage_db(xdg.path());
    let config = write_config(xdg.path(), &mixed_config());

    // provider probe --json: the forwarded provider's outcome is `Skipped`.
    let probe = run_bounded(xdg.path(), &config, &["provider", "probe", "--json"]);
    let probe_json = parse_json(&probe);
    let providers = probe_json["providers"].as_array().expect("providers array");
    let forwarded = providers
        .iter()
        .find(|p| p["name"] == serde_json::json!("anthropic-forwarded"))
        .expect("forwarded provider present in probe report");
    assert!(
        forwarded["outcome"].get("Skipped").is_some(),
        "forwarded probe outcome must be Skipped, got {}",
        forwarded["outcome"]
    );

    // doctor --json: the forwarded provider's probe finding is a Pass with no
    // remediation -- informational, not a WARN.
    let doctor = run_bounded(xdg.path(), &config, &["doctor", "--json"]);
    let doctor_json = parse_json(&doctor);
    let findings = doctor_json["findings"].as_array().expect("findings array");
    let forwarded_finding = findings
        .iter()
        .find(|f| {
            f["section"] == serde_json::json!("probe")
                && f["name"] == serde_json::json!("anthropic-forwarded")
        })
        .expect("forwarded probe finding present in doctor report");
    assert_eq!(
        forwarded_finding["status"],
        serde_json::json!("Pass"),
        "forwarded doctor probe finding must be Pass (informational), not WARN: {forwarded_finding}"
    );
    assert!(
        forwarded_finding["remediation"].is_null(),
        "an informational skip carries no remediation: {forwarded_finding}"
    );
}

// ---------------------------------------------------------------------
// Secret hygiene: a config whose parse error would inline a secret-shaped
// value never echoes that value in the doctor version-section finding --
// neither in the human render nor in `--json`. serde frames the offending
// line, and a mistyped secret in a non-string field lands verbatim in the
// `invalid type:` clause, so the version section must render a redacted
// detail.
// ---------------------------------------------------------------------

#[test]
fn doctor_version_finding_never_echoes_a_secret_from_a_parse_error() {
    const SECRET: &str = "sk-live-DOCTOR-VERSION-LEAK";
    let xdg = tempfile::tempdir().unwrap();
    // The current version stamp passes the raw version preflight, so the typed load runs and
    // fails on the mistyped `port`: serde emits `invalid type: string
    // "sk-live-...", expected u16`, inlining the secret unless redacted.
    let body =
        format!("version = {CURRENT}\n\n[server]\nhost = \"127.0.0.1\"\nport = \"{SECRET}\"\n");
    let config = write_config(xdg.path(), &body);

    let human = run_bounded(xdg.path(), &config, &["doctor"]);
    assert!(
        !human.stdout.contains(SECRET) && !human.stderr.contains(SECRET),
        "the doctor human render leaked a config secret:\n{}",
        human.context()
    );

    let json = run_bounded(xdg.path(), &config, &["doctor", "--json"]);
    assert!(
        !json.stdout.contains(SECRET) && !json.stderr.contains(SECRET),
        "the doctor --json output leaked a config secret:\n{}",
        json.context()
    );

    // The version finding is still surfaced (broken config does not report
    // all-Pass): the redacted detail names the field, just not the value.
    let doctor_json = parse_json(&json);
    let findings = doctor_json["findings"].as_array().expect("findings array");
    let version = findings
        .iter()
        .find(|f| f["section"] == serde_json::json!("version"))
        .expect("version finding present");
    assert_eq!(
        version["status"],
        serde_json::json!("Fail"),
        "a broken config must fail the version section: {version}"
    );
}

// ---------------------------------------------------------------------
// Credential-path hygiene: a credentials store that fails to open never
// discloses its filesystem path in the doctor auth-section finding -- the
// OAuthError Display embeds the FULL store path (via a `path` field or an
// interpolated Io message), so the finding must render a class-only message.
// ---------------------------------------------------------------------

#[test]
fn doctor_auth_finding_never_discloses_the_credentials_store_path() {
    let xdg = tempfile::tempdir().unwrap();
    // A syntactically broken credentials.json makes the store open fail. The
    // full store path lives under the scoped XDG dir; it must not surface.
    let dir = routectl_dir(xdg.path());
    std::fs::create_dir_all(&dir).unwrap();
    let creds = dir.join("credentials.json");
    std::fs::write(&creds, b"<<not valid json>>").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&creds, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let config = write_config(xdg.path(), &healthy_config());

    let store_path = creds.display().to_string();
    let dir_path = dir.display().to_string();

    let human = run_bounded(xdg.path(), &config, &["doctor"]);
    assert!(
        !human.stdout.contains(&store_path) && !human.stdout.contains(&dir_path),
        "the doctor human render leaked the credentials-store path:\n{}",
        human.context()
    );

    let json = run_bounded(xdg.path(), &config, &["doctor", "--json"]);
    assert!(
        !json.stdout.contains(&store_path) && !json.stdout.contains(&dir_path),
        "the doctor --json output leaked the credentials-store path:\n{}",
        json.context()
    );
}
