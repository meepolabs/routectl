use super::*;
use crate::server::test_support::isolate_usage_db;
use routectl_auth::MemoryStore;

#[tokio::test]
async fn build_router_twice_from_same_secrets_handle_succeeds() {
    // Hot-reload smoke test: rebuild a Router twice from the
    // same `Arc<dyn SecretStore>` handle. Pinning the no-panic
    // contract -- a regression that drops the per-provider
    // single-flight refresh mutex on rebuild would either error
    // on the second build or surface a dangling Arc here.
    use routectl_auth::MemoryStore;
    use routectl_router::Config;
    use std::sync::Arc;

    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let config = Arc::new(Config::default());

    let r1 = build_router_from_config(config.clone(), secrets.clone())
        .await
        .expect("first router build");
    let r2 = build_router_from_config(config.clone(), secrets.clone())
        .await
        .expect("second router build");

    // Sanity: each call returns a fresh Router but they share
    // the same Config (Arc-shared at construction time).
    assert!(Arc::ptr_eq(&r1.config, &config));
    assert!(Arc::ptr_eq(&r2.config, &config));
}

/// A config whose one provider relays the client's forwarded credential to
/// `base_url`.
fn forwarded_config_on(base_url: &str) -> (Config, tempfile::TempDir) {
    use routectl_router::ProviderEntry;
    use routectl_router::config::Auth;

    let mut config = Config::default();
    let usage_dir = isolate_usage_db(&mut config);
    config.providers.insert(
        "sneaky".into(),
        ProviderEntry::anthropic_api("forwarded")
            .with_base_url(base_url)
            .with_auth(Auth::Bearer),
    );
    (config, usage_dir)
}

/// The provider plan is wired into
/// `build_router_from_config` itself (not only reachable
/// via the separate `commands::config::check` call to the same
/// validator) -- a forwarded provider pointed at a non-Anthropic host
/// must fail the router build, i.e. serve startup and hot reload,
/// which both call this builder directly. The same entry on the pinned
/// host builds, so the refusal is the host's.
#[tokio::test]
async fn build_router_from_config_rejects_forwarded_provider_on_non_anthropic_host() {
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());

    let (pinned, _pinned_dir) = forwarded_config_on("https://api.anthropic.com");
    if let Err(e) = build_router_from_config(Arc::new(pinned), secrets.clone()).await {
        panic!("control: the forwarded entry on the pinned host must build: {e:?}");
    }

    let (config, _usage_dir) = forwarded_config_on("https://evil.example.com");
    // `Router` is not `Debug`, so match rather than `expect_err`.
    let err = match build_router_from_config(Arc::new(config), secrets).await {
        Ok(_) => panic!("forwarded provider off the pinned host must fail the router build"),
        Err(e) => e,
    };
    let Error::Config(message) = &err else {
        panic!("expected a config error, got: {err:?}");
    };
    assert!(message.contains("provider `sneaky`"), "{message}");
    assert!(message.contains("credential = \"forwarded\""), "{message}");
    assert!(!message.contains("evil.example.com"), "{message}");
}

/// A hostile provider key reaching an advisory startup warning must not
/// survive into the log line: the `%`-rendered field writes its value
/// verbatim, so a newline plus an ANSI CSI sequence in a `[providers.X]`
/// key would forge a whole startup log record.
///
/// The first assertion is the positive control -- it proves the validator's
/// own message really does carry the raw control bytes, so the sanitized
/// assertion below is testing the sanitizer rather than an inert fixture.
#[tokio::test]
async fn startup_warning_log_lines_strip_control_bytes_from_operator_keys() {
    let mut config: Config = toml::from_str(
        "[providers.\"evil\\nkey\\u001B[31m\"]\n\
         format = \"gemini-cloud-code\"\n\
         base_url = \"https://cloudcode-pa.googleapis.com\"\n\
         credential = \"oauth://antigravity\"\n\
         auth = \"bearer\"\n",
    )
    .expect("config must parse");
    let _usage_dir = isolate_usage_db(&mut config);

    let raw = routectl_router::cloudcode_host_warnings(&config);
    let raw_message = raw.first().expect("the production-pin warning must fire");
    assert!(
        raw_message.contains('\n') && raw_message.contains('\u{1b}'),
        "fixture must carry raw control bytes pre-sanitize: {raw_message:?}"
    );

    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let config = Arc::new(config);
    let (result, lines) = routectl_testkit::capture_lines(Box::pin(async {
        log_advisory_warnings(&config);
        build_router_from_config(config.clone(), secrets).await
    }))
    .await;
    // `Router` is not `Debug`, so match rather than `expect`.
    if let Err(e) = result {
        panic!("an advisory finding must not fail the router build: {e:?}");
    }

    let warned: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("cloud-code host warning"))
        .collect();
    assert_eq!(warned.len(), 1, "captured lines: {lines:?}");
    let line = warned[0];
    assert!(!line.contains('\n'), "{line:?}");
    assert!(!line.contains('\u{1b}'), "{line:?}");
    assert!(line.contains("evil?key?[31m"), "{line:?}");
}

/// The whole captured startup line SET must be control-byte clean, not just
/// the one loop a single test happens to name. The fixture fires two
/// independent hostile loops -- the cloud-code host advisory off one
/// provider key and the auth-reserved header advisory off a model key --
/// plus the provider-build failure warn that an absent `env://` ref on a
/// second provider produces, so a sink added to any of those paths without a sanitizer
/// fails here.
///
/// Positive controls come first, as above: both validators' raw messages are
/// asserted hostile before the rendered lines are asserted clean.
#[tokio::test]
async fn no_captured_startup_line_carries_control_bytes_from_operator_keys() {
    let mut config: Config = toml::from_str(
        "[providers.\"evil\\nkey\\u001B[31m\"]\n\
         format = \"openai-chat\"\n\
         base_url = \"https://example.invalid\"\n\
         credential = \"env://ROUTECTL_TEST_ABSENT\"\n\
         auth = \"bearer\"\n\
         [providers.\"evil\\ngem\\u001B[31m\"]\n\
         format = \"gemini-cloud-code\"\n\
         base_url = \"https://cloudcode-pa.googleapis.com\"\n\
         credential = \"oauth://antigravity\"\n\
         auth = \"bearer\"\n\
         [models.\"evil\\nmodel\\u001B[31m\"]\n\
         provider = \"evil\\nkey\\u001B[31m\"\n\
         upstream = \"some-upstream\"\n\
         stream_first_byte_timeout_ms = 0\n\
         [models.\"evil\\nmodel\\u001B[31m\".header_extras]\n\
         authorization = \"x\"\n",
    )
    .expect("config must parse");
    let _usage_dir = isolate_usage_db(&mut config);

    for (label, raw) in [
        (
            "cloud-code host",
            routectl_router::cloudcode_host_warnings(&config),
        ),
        (
            "auth-reserved header",
            routectl_router::auth_reserved_header_warnings(&config),
        ),
    ] {
        let message = raw
            .first()
            .unwrap_or_else(|| panic!("the {label} advisory must fire for this fixture"));
        assert!(
            message.contains('\n') && message.contains('\u{1b}'),
            "{label} fixture must carry raw control bytes pre-sanitize: {message:?}"
        );
    }

    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let config = Arc::new(config);
    let (result, lines) = routectl_testkit::capture_lines(Box::pin(async {
        log_advisory_warnings(&config);
        build_router_from_config(config.clone(), secrets).await
    }))
    .await;
    // `Router` is not `Debug`, so match rather than `expect`.
    if let Err(e) = result {
        panic!("advisory warnings and an absent secret ref must not fail the build: {e:?}");
    }

    for line in &lines {
        assert!(!line.contains('\u{1b}'), "captured line: {line:?}");
        // The subscriber terminates each record with a newline, so an
        // interior one is the forged-record signal.
        assert!(
            !line.trim_end_matches('\n').contains('\n'),
            "captured line: {line:?}"
        );
    }
    for msg in [
        "cloud-code host warning",
        "auth-reserved header warning",
        "skipping provider (build failed)",
    ] {
        assert!(
            lines.iter().any(|l| l.contains(msg)),
            "expected a `{msg}` line; captured: {lines:?}"
        );
    }
}

/// The builder runs the WHOLE shared validation suite, not a hand-picked
/// subset: a validator present only in `collect_config_validation` would
/// leave a programmatic caller (this crate's own tests, a library embedder)
/// building a Router whose invalid setting sits inert. `[capability]
/// essential` is the probe -- the builder never named that validator
/// individually, so its rejection here can only come from the suite.
#[tokio::test]
async fn build_router_from_config_runs_the_shared_validation_suite() {
    let mut config: Config =
        toml::from_str("[capability]\nessential = [\"advisorr\"]\n").expect("config must parse");
    let _usage_dir = isolate_usage_db(&mut config);
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());

    // `Router` is not `Debug`, so match rather than `expect_err`.
    let err = match build_router_from_config(Arc::new(config), secrets).await {
        Ok(_) => panic!("an unknown [capability] essential key must fail the router build"),
        Err(e) => e,
    };
    let Error::Config(message) = &err else {
        panic!("expected a config error, got: {err:?}");
    };
    assert!(message.contains("advisorr"), "{message}");
}

/// The Bedrock invoke-lane model-family gate is wired into
/// `build_router_from_config` itself, not only into the
/// collected-validation path that `config check` and the serve pre-parse
/// use. Serve startup and hot reload call this builder directly, so a
/// gate reachable only from collected validation would leave both
/// unprotected.
///
/// Not feature-gated: this crate declares no provider-gating features
/// (see its manifest), so the whole provider set is always compiled and
/// a `#[cfg(feature = "bedrock")]` here would silently exclude the test.
#[tokio::test]
async fn build_router_from_config_rejects_a_non_anthropic_model_on_the_invoke_lane() {
    use routectl_router::Config;

    let mut config: Config = toml::from_str(
        "[providers.aws]\n\
         format = \"bedrock-invoke\"\n\
         cloud = { name = \"aws\", region = \"us-west-2\" }\n\
         credential = \"aws://chain\"\n\
         auth = \"aws-sigv4\"\n\
         [models.seat]\n\
         provider = \"aws\"\n\
         upstream = \"meta.llama3-70b-instruct-v1:0\"\n",
    )
    .expect("config must parse");
    let _usage_dir = isolate_usage_db(&mut config);
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());

    // `Router` is not `Debug`, so match rather than `expect_err`.
    let err = match build_router_from_config(Arc::new(config), secrets).await {
        Ok(_) => panic!("a non-Anthropic model on the invoke lane must fail the router build"),
        Err(e) => e,
    };
    let Error::Config(message) = &err else {
        panic!("expected a config error, got: {err:?}");
    };
    // The operator must be able to act on this without reading source:
    // the offending id plus both lane names.
    assert!(
        message.contains("meta.llama3-70b-instruct-v1:0"),
        "{message}"
    );
    assert!(message.contains("invoke"), "{message}");
    assert!(message.contains("converse"), "{message}");
}
