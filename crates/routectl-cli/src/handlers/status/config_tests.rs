use super::*;
use crate::handlers::status::DaemonMeta;
use crate::server::AppState;
use arc_swap::ArcSwap;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use routectl_router::config::{AliasValue, PricingConfig};
use routectl_router::prices::{PriceSources, parse_and_validate};
use routectl_router::{
    CatalogOverlay, CatalogRow, Config, Router, TuningSource, derive_effective_view,
};
use routectl_testkit::ScopedEnv;
use serde_json::Value;
use std::path::PathBuf;
use tower::ServiceExt;

/// A fixed daemon snapshot so the source-strip assertions pin exact
/// values instead of racing a live clock.
fn sample_daemon() -> DaemonMetaSnapshot {
    DaemonMetaSnapshot {
        listen_addr: "127.0.0.1:9000".to_string(),
        version: env!("CARGO_PKG_VERSION"),
        config_loaded_age_ms: Some(4_000),
    }
}

/// Build the wire DTO through the SAME derivation the handler uses, so a
/// config-shaped assertion exercises the real path end to end.
fn panel_from_config(config: &Config) -> ConfigPanel {
    let effective = derive_effective_view(config, &CatalogOverlay::default(), None, None);
    build_panel(
        effective,
        &ActivationState::default(),
        None,
        sample_daemon(),
    )
}

/// A router over `config` with `overlay` RETAINED on it, matching what the
/// server builder installs -- the panel derives its view from these two,
/// so a test that wants an overlay in the view injects it here rather than
/// writing a file the panel no longer reads.
fn router_with(config: Config, overlay: CatalogOverlay) -> Router {
    let mut router = Router::new(Arc::new(config));
    router.install_catalog_overlay(Arc::new(overlay));
    router
}

fn test_state() -> Arc<StatusState> {
    let router = router_with(Config::default(), CatalogOverlay::default());
    let app = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
    Arc::new(StatusState::from_app(
        &app,
        Some(PathBuf::from("/nonexistent/config.toml")),
        DaemonMeta::for_test(),
    ))
}

fn present_model(nickname: &str, source: Source) -> ModelCell {
    ModelCell {
        nickname: nickname.to_string(),
        provider: "anthropic".to_string(),
        provider_kind: "anthropic-api".to_string(),
        upstream: "claude-opus-4-8".to_string(),
        row: EffectiveRow::Present {
            row: CatalogRow::sentinel(),
            source,
            verified_at: "2026-07-01".to_string(),
        },
        context_window: None,
        fill: None,
        pricing: PricingConfig::default(),
        price_sources: unknown_sources(),
    }
}

fn unknown_sources() -> PriceSources {
    PriceSources {
        input: PriceSource::Unknown,
        output: PriceSource::Unknown,
        cache_read: PriceSource::Unknown,
        cache_write_5m: PriceSource::Unknown,
        cache_write_1h: PriceSource::Unknown,
    }
}

#[test]
fn source_tokens_pin_every_catalog_layer() {
    assert_eq!(source_token(Source::Baked), "baked");
    assert_eq!(source_token(Source::Import), "import");
    assert_eq!(source_token(Source::User), "user");
}

#[test]
fn class_source_and_class_tokens_are_kebab_case() {
    assert_eq!(class_source_token(ClassPolicySource::Config), "config");
    assert_eq!(
        class_source_token(ClassPolicySource::BakedDefault),
        "baked-default"
    );
    assert_eq!(class_token(ConfigFailureClass::RateLimited), "rate-limited");
    assert_eq!(
        class_token(ConfigFailureClass::FeatureUnsupported),
        "feature-unsupported"
    );
}

#[test]
fn capability_tokens_reuse_the_routing_filter_vocabulary() {
    assert_eq!(verdict_token(OverrideVerdict::RouteAway), "route-away");
    assert_eq!(
        verdict_token(OverrideVerdict::ForceSupported),
        "force-supported"
    );
}

#[test]
fn build_panel_maps_every_effective_surface_with_provenance() {
    let effective = EffectiveView {
        models: vec![
            present_model("opus", Source::User),
            ModelCell {
                nickname: "ghost".to_string(),
                provider: "ghost".to_string(),
                provider_kind: String::new(),
                upstream: "nope".to_string(),
                row: EffectiveRow::Missing,
                context_window: None,
                fill: None,
                pricing: PricingConfig::default(),
                price_sources: unknown_sources(),
            },
        ],
        classes: vec![ClassPolicyCell {
            class: ConfigFailureClass::RateLimited,
            retry_cap: 7,
            fallback: true,
            source: ClassPolicySource::Config,
        }],
        capabilities: vec![OverrideRow {
            target_spec: "anthropic".to_string(),
            capability_key: "web_search".to_string(),
            verdict: OverrideVerdict::RouteAway,
        }],
        aliases: Vec::new(),
        providers: Vec::new(),
        tuning: Vec::new(),
        price_tables: PriceTableDates::default(),
    };
    let activation = ActivationState::default();

    let panel = build_panel(effective, &activation, None, sample_daemon());

    let opus = &panel.models[0];
    assert_eq!(opus.source, "user");
    assert_eq!(opus.verified_at.as_deref(), Some("2026-07-01"));
    assert_eq!(panel.models[1].source, "missing");
    assert_eq!(panel.models[1].verified_at, None);

    assert_eq!(panel.classes[0].class, "rate-limited");
    assert_eq!(panel.classes[0].retry_cap, 7);
    assert_eq!(panel.classes[0].source, "config");

    assert_eq!(panel.capabilities[0].verdict, "route-away");
    assert_eq!(panel.capabilities[0].provenance, "override");

    assert!(panel.activation.is_empty());
}

/// A `Single` alias serializes as a one-element chain and a `Chain` keeps
/// the operator's order verbatim -- the chain order IS the fallback
/// sequence dispatch walks, so a sort or a dedupe would misreport routing.
#[test]
fn alias_chains_preserve_single_and_chain_order() {
    let mut config = Config::default();
    config
        .aliases
        .insert("solo".to_string(), AliasValue::Single("opus".to_string()));
    config.aliases.insert(
        "fast".to_string(),
        AliasValue::Chain(vec![
            "small".to_string(),
            "smaller".to_string(),
            "smallest".to_string(),
        ]),
    );

    let panel = panel_from_config(&config);

    let solo = panel
        .aliases
        .iter()
        .find(|a| a.alias == "solo")
        .expect("single alias present");
    assert_eq!(solo.chain, vec!["opus".to_string()]);

    let fast = panel
        .aliases
        .iter()
        .find(|a| a.alias == "fast")
        .expect("chain alias present");
    assert_eq!(
        fast.chain,
        vec![
            "small".to_string(),
            "smaller".to_string(),
            "smallest".to_string()
        ],
        "the fallback chain must keep the configured order verbatim"
    );
}

/// The source strip's counts derive from the SAME effective view the
/// tables render from, and every declared field reaches the wire.
#[test]
fn source_object_carries_every_declared_field() {
    let mut config = Config::default();
    config
        .aliases
        .insert("solo".to_string(), AliasValue::Single("opus".to_string()));
    config.aliases.insert(
        "fast".to_string(),
        AliasValue::Chain(vec!["small".to_string()]),
    );

    let effective = derive_effective_view(&config, &CatalogOverlay::default(), None, None);
    let panel = build_panel(
        effective,
        &ActivationState::default(),
        Some("/etc/routectl/config.toml"),
        sample_daemon(),
    );

    let value = serde_json::to_value(&panel.source).unwrap();
    let obj = value.as_object().unwrap();
    for key in [
        "config_path",
        "loaded_age_ms",
        "alias_count",
        "provider_count",
        "listen_addr",
        "version",
    ] {
        assert!(obj.contains_key(key), "source strip must carry `{key}`");
    }
    assert_eq!(
        panel.source.config_path.as_deref(),
        Some("/etc/routectl/config.toml")
    );
    assert_eq!(panel.source.loaded_age_ms, Some(4_000));
    assert_eq!(panel.source.alias_count, 2);
    assert_eq!(panel.source.provider_count, 0);
    assert_eq!(panel.source.listen_addr, "127.0.0.1:9000");
    assert_eq!(panel.source.version, env!("CARGO_PKG_VERSION"));
}

/// A server bound without an on-disk config reports `None`, never an
/// invented placeholder path.
#[test]
fn source_object_reports_no_path_without_an_on_disk_config() {
    let panel = panel_from_config(&Config::default());
    assert!(panel.source.config_path.is_none());
}

/// `debits_breaker` is read from the router's own transient-health set, so
/// a recoverable class debits the breaker and a caller-shaped one does not.
/// The set itself is never restated here -- only its verdict is asserted.
#[test]
fn debits_breaker_is_true_for_transient_and_false_for_caller_shaped_classes() {
    let panel = panel_from_config(&Config::default());
    let debits = |class: &str| {
        panel
            .classes
            .iter()
            .find(|c| c.class == class)
            .unwrap_or_else(|| panic!("class `{class}` present in the effective view"))
            .debits_breaker
    };

    assert!(debits("rate-limited"));
    assert!(debits("server-error"));
    assert!(debits("timeout"));
    assert!(debits("network-error"));
    assert!(debits("overloaded"));

    assert!(!debits("auth"));
    assert!(!debits("bad-request"));
    assert!(!debits("content-policy"));
    assert!(!debits("context-window"));
    assert!(!debits("feature-unsupported"));
}

#[test]
fn activation_wire_carries_the_contract_fields() {
    let value = serde_json::to_value(ActivationWire {
        provider_id: "anthropic".to_string(),
        provider_kind: "anthropic-api".to_string(),
        status: "unresolved",
        reason: Some("oauth_missing".to_string()),
        referenced_by_aliases: true,
    })
    .unwrap();
    let obj = value.as_object().unwrap();
    assert!(obj.contains_key("provider_id"));
    assert!(obj.contains_key("provider_kind"));
    assert!(obj.contains_key("status"));
    assert!(obj.contains_key("reason"));
    assert!(obj.contains_key("referenced_by_aliases"));
}

/// The three-provider fixture the RPM contract is stated over: an
/// immediately-throttled entry, an unlimited one, and a capped one.
fn config_with_three_rpm_states() -> Config {
    toml::from_str(
        r#"
version = 2

[providers.throttled]
kind = "anthropic-api"
api_key_ref = "env://THROTTLED_KEY"
base_url = "https://throttled.example/v1"
rpm_limit = 0

[providers.unlimited]
kind = "anthropic-api"
api_key_ref = "env://UNLIMITED_KEY"
base_url = "https://unlimited.example:8443/v1"

[providers.capped]
kind = "openai-compat"
api_key_ref = "file:///nonexistent/capped.key"
base_url = "https://capped.example/v1"
rpm_limit = 60
"#,
    )
    .expect("valid config")
}

fn provider_json(config: &Config, id: &str) -> Value {
    let panel = panel_from_config(config);
    let value = serde_json::to_value(&panel.providers).unwrap();
    value
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["provider_id"] == id)
        .unwrap_or_else(|| panic!("provider `{id}` on the wire"))
        .clone()
}

/// The RPM field's three states must be DISTINGUISHABLE on the wire, not
/// merely distinguishable in Rust. An immediately-throttled provider
/// carries an explicit numeric `0`; an unlimited one carries an explicit
/// `null`. Skipping the absent case (a `skip_serializing_if`) would
/// collapse "unlimited" into "the field never arrived", which the client
/// reports as a different word -- so the key is always present.
#[test]
fn zero_rpm_reaches_the_wire_as_an_explicit_zero_and_unlimited_as_null() {
    let config = config_with_three_rpm_states();

    let throttled = provider_json(&config, "throttled");
    assert_eq!(
        throttled["rpm_limit"],
        Value::from(0),
        "a zero cap must reach the wire as an explicit numeric 0"
    );

    let unlimited = provider_json(&config, "unlimited");
    assert!(
        unlimited.as_object().unwrap().contains_key("rpm_limit"),
        "the unlimited case must carry the key explicitly, never omit it"
    );
    assert_eq!(unlimited["rpm_limit"], Value::Null);

    assert_eq!(
        provider_json(&config, "capped")["rpm_limit"],
        Value::from(60)
    );
}

/// The wire's `rpm_limit` is the provider entry's OWN configured cap, so a
/// pool's `rpm_limit` leaves an unset member at `null`; the inherited value
/// is visible on the member's resolved tuning row instead, sourced `pool`.
/// The panel's shape and meaning are unchanged, so its version stays 3.
#[test]
fn a_pool_rpm_limit_leaves_member_provider_rows_null() {
    let config: Config = toml::from_str(
        r#"
version = 2

[providers.seat-a]
kind = "anthropic-api"
api_key_ref = "env://SEAT_A"

[providers.seat-b]
kind = "anthropic-api"
api_key_ref = "env://SEAT_B"
rpm_limit = 15

[pools.seats]
members = ["seat-a", "seat-b"]
rpm_limit = 40
"#,
    )
    .expect("valid config");

    assert_eq!(provider_json(&config, "seat-a")["rpm_limit"], Value::Null);
    assert_eq!(
        provider_json(&config, "seat-b")["rpm_limit"],
        Value::from(15)
    );
    let effective = derive_effective_view(&config, &CatalogOverlay::default(), None, None);
    let seat_a = effective
        .tuning
        .iter()
        .find(|row| row.target == "seat-a")
        .expect("seat-a tuning row");
    assert_eq!(seat_a.tuning.rpm_limit.value, Some(40));
    assert_eq!(seat_a.tuning.rpm_limit.source, TuningSource::Pool);
    assert_eq!(SCHEMA_VERSION, 4);
}

/// Every declared provider field reaches the wire, and the endpoint is the
/// derivation's ORIGIN -- never widened back toward a fuller URL here.
#[test]
fn provider_wire_carries_the_contract_fields_and_only_the_origin() {
    let config = config_with_three_rpm_states();
    let unlimited = provider_json(&config, "unlimited");
    let obj = unlimited.as_object().unwrap();
    for key in [
        "provider_id",
        "provider_kind",
        "endpoint_origin",
        "credential_ref_scheme",
        "auth_token",
        "rpm_limit",
    ] {
        assert!(obj.contains_key(key), "provider row must carry `{key}`");
    }
    assert_eq!(unlimited["provider_kind"], "anthropic-api");
    assert_eq!(
        unlimited["endpoint_origin"],
        "https://unlimited.example:8443"
    );
    assert_eq!(unlimited["credential_ref_scheme"], "env");
    assert_eq!(unlimited["auth_token"], "api-key");

    // The variant with no auth discriminant reports absence rather than a
    // spanning token, and its ref contributes the scheme only.
    let capped = provider_json(&config, "capped");
    assert_eq!(capped["auth_token"], Value::Null);
    assert_eq!(capped["credential_ref_scheme"], "file");
}

/// A `base_url` carrying a credential in userinfo, query, and fragment is
/// an ACCEPTED provider config, so the panel is the last line before it
/// would reach a browser. Nothing secret-shaped may survive the whole
/// serialized provider list.
#[test]
fn provider_rows_never_carry_a_credential_or_a_ref_body() {
    let config: Config = toml::from_str(
        r#"
version = 2

[providers.leaky]
kind = "anthropic-api"
api_key_ref = "env://LEAKY_SECRET_VAR"
base_url = "https://user:sk-live-LEAKED@internal.example/v1?key=sk-live-LEAKED#sk-live-LEAKED"
"#,
    )
    .expect("valid config");

    let panel = panel_from_config(&config);
    // Scan the WHOLE serialized panel, not just the provider rows: a future
    // field anywhere on this wire that copies config text must fail here
    // too. This is a Serialize scan on purpose -- a `Debug` scan proves only
    // the Debug projection, and a hand-written redacting Debug (this repo
    // has two, both of which keep base_url) would pass it while the wire
    // leaks.
    let text = serde_json::to_string(&panel).unwrap();

    for forbidden in ["sk-live-LEAKED", "user:", "LEAKY_SECRET_VAR", "/v1"] {
        assert!(
            !text.contains(forbidden),
            "the config panel wire leaked `{forbidden}`: {text}"
        );
    }
    assert!(text.contains("https://internal.example"));
}

/// This panel's UNAVAILABLE envelope carries a fixed reason code and
/// nothing else. `effective_view()` is infallible now, so no loader error
/// string exists to leak -- but `guard_panel` takes a `&'static str` code,
/// and widening that to `String` (the natural move for someone wanting
/// richer reasons) would silently let a formatted error onto this wire.
/// Anchored on the SERIALIZED envelope, not Debug, per the sibling no-leak
/// test above.
#[test]
fn the_unavailable_envelope_carries_only_a_fixed_code() {
    let panel: Panel<ConfigPanel> = Panel::unavailable(SCHEMA_VERSION, codes::CONFIG_UNAVAILABLE);
    let text = serde_json::to_string(&panel).unwrap();
    let value: Value = serde_json::from_str(&text).unwrap();

    assert_eq!(value["unavailable"], codes::CONFIG_UNAVAILABLE);
    assert!(value["data"].is_null(), "data must be dropped: {text}");
    assert!(value["as_of"].is_null(), "as_of must be dropped: {text}");
    // A filesystem path, a config value, or a secret ref would all have to
    // arrive through the reason field; none of these can appear in a code.
    for forbidden in ["/", "literal:", "env://", "catalog overlay"] {
        assert!(
            !value["unavailable"].as_str().unwrap().contains(forbidden),
            "the unavailable reason leaked `{forbidden}`: {text}"
        );
    }
}

/// The source strip's provider count and the provider table derive from the
/// SAME effective view, so a rendered row can never be missing from the
/// count.
#[test]
fn provider_count_matches_the_rendered_rows() {
    let panel = panel_from_config(&config_with_three_rpm_states());
    assert_eq!(panel.source.provider_count, panel.providers.len());
    assert_eq!(panel.providers.len(), 3);
}

/// The panel serves the LAST ACCEPTED catalog generation, so a corrupt
/// overlay sitting on disk cannot take it down while routing is healthy.
/// The overlay file here is structurally invalid (no `cells` key, which
/// `catalog_overlay::load` rejects -- a merely MISSING file loads as an
/// empty overlay and would prove nothing), and the ambient config dir is
/// pointed at it, so a panel that re-read disk would fold that load error
/// into an unavailable payload.
///
/// `#[serial]` per the `ScopedEnv` contract.
#[tokio::test]
#[serial_test::serial]
async fn a_corrupt_overlay_on_disk_never_unavailables_the_panel() {
    // Arrange: an isolated config dir holding a corrupt overlay file, and
    // a live router carrying the accepted generation in memory.
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let overlay_dir = dir.path().join("routectl");
    std::fs::create_dir_all(&overlay_dir).unwrap();
    std::fs::write(
        overlay_dir.join("catalog_overlay.json"),
        br#"{"schema_version":1,"revision":4}"#,
    )
    .unwrap();
    assert!(
        routectl_router::load_catalog_overlay(&routectl_router::overlay_default_path()).is_err(),
        "the fixture must be a genuinely CORRUPT overlay, not a missing one"
    );

    let config: Config = toml::from_str(
        r#"
version = 2

[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "env://ACCEPTED_KEY"
base_url = "https://accepted.example/v1"

[models.opus]
provider = "anthropic"
upstream = "claude-opus-4-8"
"#,
    )
    .expect("valid config");
    // The accepted generation, in the same JSON shape the loader parses,
    // so the fixture cannot drift from what a real accepted overlay is.
    let accepted: CatalogOverlay = serde_json::from_str(
        r#"{"schema_version":1,"revision":2,"cells":{"anthropic-api:claude-opus-4-8*":
                 {"source":"user","verified_at":"2026-07-01","wm":9.5}}}"#,
    )
    .expect("valid overlay");
    let router = router_with(config, accepted);
    let app = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
    let state = Arc::new(StatusState::from_app(&app, None, DaemonMeta::for_test()));

    // Act
    let panel = build(&state).await;

    // Assert: available, and rendering the ACCEPTED generation's
    // provenance -- not the baked fallback and not an unavailable code.
    assert!(
        panel.unavailable.is_none(),
        "a corrupt overlay on disk must not unavailable the panel: {:?}",
        panel.unavailable
    );
    let data = panel.data.expect("the panel must carry data");
    let opus = data
        .models
        .iter()
        .find(|m| m.nickname == "opus")
        .expect("the configured model reaches the wire");
    assert_eq!(
        opus.source, "user",
        "the panel must render the accepted overlay's provenance, not the baked layer"
    );
    assert_eq!(
        opus.verified_at.as_deref(),
        Some("2026-07-01"),
        "the stamp must come from the accepted overlay cell"
    );
}

/// A read that lands mid-reload must skew CONSERVATIVELY: the source strip
/// reports the NEW alias counts beside an age still measured from the
/// PREVIOUS config load. The inverse -- a fresh age beside pre-reload
/// counts -- reads as "my edit was ignored", so it must be unreachable.
///
/// Sequenced writes, no threads: the daemon snapshot is pinned FIRST (as
/// `build` pins it), the reload's router swap lands SECOND, and the panel
/// is built from both. That `build` actually reads in that order is pinned
/// by `build_reads_the_daemon_stamp_before_the_router_snapshot`.
#[test]
fn a_reload_after_the_daemon_read_skews_the_age_and_never_the_counts() {
    let router_swap = Arc::new(ArcSwap::from_pointee(Router::new(Arc::new(
        Config::default(),
    ))));
    let app = AppState::for_test(router_swap.clone());
    let state = StatusState::from_app(&app, None, DaemonMeta::for_test());
    let pinned_daemon = DaemonMetaSnapshot {
        listen_addr: "127.0.0.1:9000".to_string(),
        version: env!("CARGO_PKG_VERSION"),
        config_loaded_age_ms: Some(60_000),
    };

    let mut reloaded = Config::default();
    reloaded
        .aliases
        .insert("fresh".to_string(), AliasValue::Single("opus".to_string()));
    router_swap.store(Arc::new(Router::new(Arc::new(reloaded))));
    let effective = state.router.view().effective_view(None, None);
    let panel = build_panel(effective, &ActivationState::default(), None, pinned_daemon);

    assert_eq!(
        panel.source.alias_count, 1,
        "the counts must come from the post-reload router, never the pre-reload one"
    );
    assert_eq!(
        panel.source.loaded_age_ms,
        Some(60_000),
        "the age must stay the pre-reload (conservative) one, never be refreshed \
             to sit beside stale counts"
    );
}

/// Structural guard on `build`'s read ORDER, which IS the mechanism: the
/// daemon stamp is snapshotted BEFORE the router view is pinned. Reachable
/// reload states are ordered `gen(router) >= gen(stamp)`, so the reverse
/// order yields the harmful skew -- a fresh age beside pre-reload counts.
/// A refactor back to the intuitive router-first shape lands here.
#[test]
fn build_reads_the_daemon_stamp_before_the_router_snapshot() {
    let production = include_str!("config.rs");

    let stamp_read = production
        .find("daemon_meta.snapshot(")
        .expect("`build` snapshots the daemon meta");
    let router_read = production
        .find("router.view()")
        .expect("`build` pins the router view");

    assert!(
        stamp_read < router_read,
        "the daemon stamp must be read before the router view, or the source \
             strip reports a fresh load age beside pre-reload counts"
    );
}

#[tokio::test]
async fn handler_returns_available_panel_with_effective_view() {
    let app = super::super::status_router().with_state(test_state());
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/status/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["schema_version"], SCHEMA_VERSION);
    assert!(json["unavailable"].is_null());
    let as_of = json["as_of"].as_str().expect("as_of present");
    assert!(
        chrono::DateTime::parse_from_rfc3339(as_of).is_ok(),
        "as_of must be RFC3339: {as_of}"
    );
    // A default config has no models and no capability overrides, but every
    // failure class is represented in the effective class policy view.
    assert!(json["data"]["models"].is_array());
    assert!(json["data"]["classes"].as_array().unwrap().len() >= 10);
    assert!(json["data"]["capabilities"].is_array());
    assert!(json["data"]["aliases"].is_array());
    assert!(json["data"]["providers"].is_array());
    assert!(json["data"]["source"].is_object());
    assert!(json["data"]["activation"].is_array());
}

/// One model priced by every source the wire can name, a host-matched table
/// provider whose id and host are planted strings no wire field may carry,
/// and a configured window and fill.
fn priced_panel_json() -> Value {
    let config: Config = toml::from_str(
        r#"
version = 2

[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "env://K"

[providers.compat]
kind = "openai-compat"
base_url = "https://compat.example/v1"
api_key_ref = "env://K"

[models.opus]
provider = "anthropic"
upstream = "claude-opus-4-8"
context_window = 200000

[models.opus.pricing]
output_per_mtok = 30.0

[models.glm]
provider = "compat"
upstream = "glm-5"
max_output_tokens = 9000
"#,
    )
    .expect("valid config");
    let table = |date: &str, opus_input: f64| {
        let json = serde_json::json!({
            "schema_version": 1,
            "generated_at": date,
            "providers": [
                {
                    "id": "anthropic",
                    "models": [{ "id": "claude-opus-4-8", "input": opus_input }],
                },
                {
                    "id": "planted-provider-id",
                    "api_host": "https://compat.example/v1",
                    "models": [{ "id": "glm-5", "input": 1.0 }],
                },
            ],
        });
        parse_and_validate(&serde_json::to_vec(&json).expect("json")).expect("valid table")
    };
    let refreshed = table("2026-11-05", 5.0);
    let shipped = table("2026-10-01", 4.0);
    let effective = derive_effective_view(
        &config,
        &CatalogOverlay::default(),
        Some(&refreshed),
        Some(&shipped),
    );
    let panel = build_panel(
        effective,
        &ActivationState::default(),
        None,
        sample_daemon(),
    );
    serde_json::to_value(&panel).expect("panel serializes")
}

fn wire_model<'a>(json: &'a Value, nickname: &str) -> &'a Value {
    json["models"]
        .as_array()
        .expect("models array")
        .iter()
        .find(|m| m["nickname"] == nickname)
        .unwrap_or_else(|| panic!("{nickname} on the wire"))
}

#[test]
fn model_wire_carries_window_fill_and_each_price_with_its_source() {
    let json = priced_panel_json();

    let opus = wire_model(&json, "opus");
    assert_eq!(opus["context_window"], 200_000);
    assert_eq!(
        opus["fill"],
        serde_json::json!({ "tokens": 64_000, "source": "default" })
    );
    assert_eq!(
        opus["prices"],
        serde_json::json!([
            { "field": "input_per_mtok", "per_mtok": 5.0,
              "source": "refreshed", "table_date": "2026-11-05" },
            { "field": "output_per_mtok", "per_mtok": 30.0,
              "source": "model-config", "table_date": null },
            { "field": "cache_read_per_mtok", "per_mtok": null,
              "source": "unknown", "table_date": null },
            { "field": "cache_write_5m_per_mtok", "per_mtok": null,
              "source": "unknown", "table_date": null },
            { "field": "cache_write_1h_per_mtok", "per_mtok": 10.0,
              "source": "derived", "table_date": null },
        ])
    );

    let glm = wire_model(&json, "glm");
    assert!(glm["context_window"].is_null());
    assert_eq!(
        glm["fill"],
        serde_json::json!({ "tokens": 9000, "source": "model-config" })
    );
    assert_eq!(glm["prices"][0]["source"], "refreshed");
}

#[test]
fn model_wire_carries_no_economics_block() {
    let json = priced_panel_json();

    for model in json["models"].as_array().expect("models array") {
        let keys: Vec<&str> = model
            .as_object()
            .expect("model object")
            .keys()
            .map(String::as_str)
            .collect();
        assert!(keys.contains(&"prices"), "keys: {keys:?}");
        assert!(!keys.contains(&"economics"), "keys: {keys:?}");
    }
}

#[test]
fn no_table_provider_string_reaches_the_wire() {
    let json = priced_panel_json();
    let text = json.to_string();

    assert_eq!(
        wire_model(&json, "glm")["prices"][0]["per_mtok"],
        1.0,
        "the planted provider must be the one that priced glm"
    );
    assert!(!text.contains("planted-provider-id"), "{text}");
    assert!(!text.contains("compat.example/v1"), "{text}");
}

/// The panel prices against the status state's price book: a table file
/// present in the book's path reaches the wire as a `refreshed` price with
/// that table's date, read at build time.
#[tokio::test]
async fn panel_reports_a_refreshed_price_from_the_status_price_book() {
    let dir = tempfile::tempdir().expect("tempdir");
    let book_path = dir.path().join("prices.json");
    let table = serde_json::json!({
        "schema_version": 1,
        "generated_at": "2026-11-07",
        "providers": [{
            "id": "anthropic",
            "models": [{ "id": "claude-opus-4-8", "input": 7.5 }],
        }],
    });
    std::fs::write(&book_path, serde_json::to_vec(&table).expect("json")).expect("write table");
    let config: Config = toml::from_str(
        r#"
version = 2

[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "env://K"

[models.opus]
provider = "anthropic"
upstream = "claude-opus-4-8"
"#,
    )
    .expect("valid config");
    let router = router_with(config, CatalogOverlay::default());
    let app = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
    let mut state = StatusState::from_app(&app, None, DaemonMeta::for_test());
    state.prices = Arc::new(routectl_router::prices::PriceBook::new(book_path));

    let panel = build(&state).await;

    let json = serde_json::to_value(panel.data.expect("panel data")).expect("serializes");
    assert_eq!(
        wire_model(&json, "opus")["prices"][0],
        serde_json::json!({
            "field": "input_per_mtok",
            "per_mtok": 7.5,
            "source": "refreshed",
            "table_date": "2026-11-07",
        })
    );
}
