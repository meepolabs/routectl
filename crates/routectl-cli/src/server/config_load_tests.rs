use super::*;
use routectl_testkit::ScopedEnv;

/// A config older than this build writes is REJECTED at load, never
/// migrated in place. Both the serve/reload loader
/// (`load_effective_config`) and the `config check` unvalidated path
/// (`load_effective_config_unvalidated`) must reject it identically,
/// point at `config migrate`, and leave the file byte-identical -- the
/// mutate-on-load incident class this replaces.
#[test]
#[serial_test::serial]
fn load_rejects_a_too_old_config_and_leaves_it_byte_identical() {
    // Arrange: a v1 config (no explicit `version`) under an isolated
    // temp dir -- never the live config, per the loader learnings.
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let cfg_path = dir.path().join("config.toml");
    let body = "[server]\nhost = \"127.0.0.1\"\nport = 4000\n";
    std::fs::write(&cfg_path, body).expect("write config.toml");

    // Act: the serve/reload path.
    let serve_err = match load_effective_config(&cfg_path) {
        Ok(_) => panic!("a too-old config must be rejected on the serve path"),
        Err(e) => e,
    };
    // Act: the `config check` unvalidated path.
    let check_err = match load_effective_config_unvalidated(&cfg_path) {
        Ok(_) => panic!("a too-old config must be rejected on the check path"),
        Err(e) => e,
    };

    // Assert: both reject with the single-sourced migrate pointer.
    for err in [&serve_err, &check_err] {
        assert!(err.contains("config migrate"), "err: {err}");
        assert!(
            err.contains(&routectl_router::CURRENT_CONFIG_VERSION.to_string()),
            "err: {err}"
        );
    }

    // Assert: the file was not touched -- no stamp, no rewrite.
    let after = std::fs::read_to_string(&cfg_path).expect("read config after reject");
    assert_eq!(after, body, "a rejected config must stay byte-identical");
}

/// A current-version config loads unchanged: it passes the preflight,
/// the load never merges the legacy sidecar or mutates the file, and the
/// overlay -- untouched by this load -- stays as it was on disk.
#[test]
#[serial_test::serial]
fn load_leaves_a_current_version_config_unchanged() {
    // Arrange: a v2 config, plus a sidecar file that the load must NOT
    // fold in (the load no longer merges sidecars at any version).
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let cfg_path = dir.path().join("config.toml");
    let body = "version = 3\n[server]\nhost = \"127.0.0.1\"\nport = 4000\n";
    std::fs::write(&cfg_path, body).expect("write config.toml");

    let sidecar_dir = dir.path().join("routectl");
    std::fs::create_dir_all(&sidecar_dir).expect("create sidecar dir");
    std::fs::write(
        sidecar_dir.join("pricing_verifications.json"),
        r#"{"verified":{"openai-compat:grok-*":"2026-06-30"}}"#,
    )
    .expect("write sidecar");

    // Act
    let loaded = load_effective_config(&cfg_path).expect("load must succeed");

    // Assert: version preserved, nothing folded, file byte-identical.
    assert_eq!(
        loaded.config.version,
        routectl_router::CURRENT_CONFIG_VERSION
    );
    assert!(loaded.config.cache_pricing.is_empty());
    assert!(loaded.catalog_overlay.cells.is_empty());
    let after = std::fs::read_to_string(&cfg_path).expect("read config after load");
    assert_eq!(
        after, body,
        "a current-version config must not be rewritten"
    );
}

/// Cold-start posture: a config whose `version` is newer than this
/// build supports fails the load closed with a clear message, via the
/// preflight raw-TOML read -- before the `deny_unknown_fields` typed
/// deserialize would otherwise mask it behind an unknown-field error.
#[test]
fn load_effective_config_rejects_a_version_newer_than_supported() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, "version = 99\n[server]\nhost = \"127.0.0.1\"\n")
        .expect("write config.toml");

    // Act. `LoadedConfig` is not `Debug`, so match rather than
    // `expect_err`.
    let err = match load_effective_config(&cfg_path) {
        Ok(_) => panic!("version 99 must be rejected"),
        Err(e) => e,
    };

    // Assert
    assert!(err.contains("99"), "err: {err}");
    assert!(
        err.contains(&routectl_router::CURRENT_CONFIG_VERSION.to_string()),
        "err: {err}"
    );
}

/// `validate_provider_credential_sources` is wired into
/// `validate_effective_config`, which `load_effective_config` calls at
/// the end of its own pre-parse gate -- this proves the rejection
/// fires on the actual serve/reload load path (the function
/// `read_parse_validate_config` calls), not only via the separate
/// `commands::config::check` entry point that also happens to call
/// the same validator.
#[test]
#[serial_test::serial]
fn load_effective_config_rejects_forwarded_provider_on_non_anthropic_host() {
    // Arrange: a current-version config so the load path exercises
    // only the credential-source validator, not the version preflight.
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(
        &cfg_path,
        "version = 3\n\
             [providers.sneaky]\n\
             kind = \"anthropic-api\"\n\
             base_url = \"https://evil.example.com\"\n\
             credential_source = \"forwarded\"\n",
    )
    .expect("write config.toml");

    // Act. `LoadedConfig` is not `Debug`, so match rather than
    // `expect_err`.
    let err = match load_effective_config(&cfg_path) {
        Ok(_) => panic!("forwarded provider off the pinned host must be rejected"),
        Err(e) => e,
    };

    // Assert
    assert!(err.contains("sneaky"), "err: {err}");
    assert!(err.contains("forwarded"), "err: {err}");
}

/// The serve/reload pre-parse gate (`validate_effective_config`, called
/// by `load_effective_config`) routes through the same centralized
/// suite as `config check` / `test` / `prompt-size`, so the identical
/// bad configs are rejected here too. Pins the no-fork acceptance on
/// the fourth caller path.
#[test]
#[serial_test::serial]
fn load_effective_config_rejects_each_centralized_bad_config() {
    // The loader preflight-rejects a stale version, and each remaining
    // case is a valid v3 config that a centralized validator refuses.
    let cases = [
        (
            "unknown-alias-target",
            "version = 3\n[aliases]\nfast = \"ghost\"\n",
        ),
        (
            "reserved-class-override",
            "version = 3\n[retry.classes.feature-unsupported]\nfallback = false\n",
        ),
    ];

    for (name, body) in cases {
        let dir = tempfile::tempdir().expect("tempdir");
        let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(&cfg_path, body).expect("write config.toml");

        assert!(
            load_effective_config(&cfg_path).is_err(),
            "serve pre-parse gate must reject `{name}`"
        );
    }
}

/// A serve-loadable config that sets all three legacy capability-list
/// keys AND passes the shared validator suite: an `openai-compat`
/// provider carrying `unsupported_features`, plus the two `[bedrock]`
/// allowlists (non-empty). No Bedrock provider is configured, so the
/// Bedrock allowlist validator short-circuits and the config loads
/// cleanly -- letting the same file exercise both the serve WARN and
/// the silent `config check` path.
const LEGACY_LIST_CONFIG: &str = "version = 3\n\
         [providers.p]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://api.example.com\"\n\
         api_key_ref = \"env://SOME_KEY\"\n\
         unsupported_features = [\"web_search\"]\n\
         [bedrock]\n\
         allowed_betas = [\"some-beta\"]\n\
         allowed_body_fields = [\"messages\", \"anthropic_version\", \"max_tokens\"]\n";

fn deprecation_warns(
    events: &[routectl_testkit::CapturedEvent],
) -> Vec<&routectl_testkit::CapturedEvent> {
    events
        .iter()
        .filter(|e| e.field("legacy_keys").is_some())
        .collect()
}

/// Serve COLD START (`load_effective_config`, the loader both the
/// cold-start `load_config_with_overlay` and the hot-reload
/// `read_parse_validate_config` flow through) emits exactly ONE
/// deprecation WARN on a legacy-list config, naming which keys are
/// present plus the successor + migrate pointer -- and no config VALUES.
#[test]
#[serial_test::serial]
fn serve_load_warns_once_on_legacy_capability_lists() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, LEGACY_LIST_CONFIG).expect("write config.toml");

    // Act
    let events = routectl_testkit::capture_events(|| {
        load_effective_config(&cfg_path).expect("legacy-list config must still load");
    });

    // Assert: exactly one deprecation WARN.
    let warns = deprecation_warns(&events);
    assert_eq!(
        warns.len(),
        1,
        "serve cold-start load must emit exactly one deprecation WARN; got {}",
        warns.len()
    );
    let warn = warns[0];
    assert_eq!(warn.level, tracing::Level::WARN);
    assert_eq!(warn.field("event"), Some("legacy_deprecation"));

    // Names all three present legacy keys, the successor, the command.
    let keys = warn.field("legacy_keys").expect("legacy_keys field");
    assert!(keys.contains("unsupported_features"), "keys: {keys}");
    assert!(keys.contains("allowed_betas"), "keys: {keys}");
    assert!(keys.contains("allowed_body_fields"), "keys: {keys}");
    assert_eq!(warn.field("successor"), Some("[capability.overrides]"));
    assert_eq!(warn.field("migrate_command"), Some("config migrate"));

    // Log hygiene: the WARN carries key NAMES, never the operator's
    // list VALUES (which can sit next to secrets).
    let blob = format!("{} {:?}", warn.message, warn.fields);
    assert!(!blob.contains("web_search"), "leaked value: {blob}");
    assert!(!blob.contains("some-beta"), "leaked value: {blob}");
}

/// HOT RELOAD (`read_parse_validate_config`, the synchronous loader the
/// reload path runs off-runtime) emits exactly ONE deprecation WARN on a
/// legacy-list config -- the same site as cold start, fired once.
#[test]
#[serial_test::serial]
fn hot_reload_load_warns_once_on_legacy_capability_lists() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, LEGACY_LIST_CONFIG).expect("write config.toml");

    // Act
    let events = routectl_testkit::capture_events(|| {
        read_parse_validate_config(&cfg_path).expect("legacy-list config must reload");
    });

    // Assert
    assert_eq!(
        deprecation_warns(&events).len(),
        1,
        "hot reload must emit exactly one deprecation WARN"
    );
}

/// `config check` loads via `load_effective_config_unvalidated` (never
/// `load_effective_config`), so the SAME legacy-list config produces NO
/// deprecation WARN there -- and still PASSES the shared validator suite,
/// so existing configs keep passing `check`. Asserts both sides.
#[test]
#[serial_test::serial]
fn config_check_stays_silent_and_passing_on_legacy_capability_lists() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, LEGACY_LIST_CONFIG).expect("write config.toml");

    // Act: the loader `config check` uses, under capture.
    let mut loaded = None;
    let events = routectl_testkit::capture_events(|| {
        loaded = Some(
            load_effective_config_unvalidated(&cfg_path)
                .expect("legacy-list config must parse for check"),
        );
    });

    // Assert (silent): no deprecation WARN on the check load path.
    assert!(
        deprecation_warns(&events).is_empty(),
        "config check load path must emit no deprecation WARN"
    );

    // Assert (passing): the shared validator suite `config check` runs
    // finds no errors, so the legacy-list config still passes.
    let config = loaded.expect("config loaded").config;
    let validation = routectl_router::collect_config_validation(&config);
    assert!(
        validation.errors.is_empty(),
        "legacy-list config must still pass config check; errors: {:?}",
        validation.errors
    );
}

/// A config with no legacy list set (empty lists are the pass-through
/// default) emits no deprecation WARN on the serve load path.
#[test]
#[serial_test::serial]
fn serve_load_is_silent_without_legacy_capability_lists() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(
        &cfg_path,
        "version = 3\n[server]\nhost = \"127.0.0.1\"\nport = 0\n",
    )
    .expect("write config.toml");

    // Act
    let events = routectl_testkit::capture_events(|| {
        load_effective_config(&cfg_path).expect("clean config must load");
    });

    // Assert
    assert!(
        deprecation_warns(&events).is_empty(),
        "a config with no legacy lists must emit no deprecation WARN"
    );
}
