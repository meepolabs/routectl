use super::*;
use crate::class_policy::ClassPolicy;
use crate::config::{
    BreakerThreshold, Config, PricingConfig, Resolved, ResolvedTuning, TuningSource,
};
use crate::override_registry::OverrideVerdict;

fn config_with_opus_model() -> Config {
    toml::from_str(
        r#"
version = 2

[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "env://ANTHROPIC_API_KEY"

[models.opus]
provider = "anthropic"
upstream = "claude-opus-4-8"
"#,
    )
    .expect("valid config")
}

#[test]
fn unknown_provider_reference_yields_empty_provider_kind() {
    let mut config = config_with_opus_model();
    config.models.get_mut("opus").unwrap().provider = "ghost".to_string();

    let view = derive_effective_view(&config, None, None);
    let derived = view.models.iter().find(|m| m.nickname == "opus").unwrap();
    assert_eq!(derived.provider_kind, "");
}

#[test]
fn class_with_operator_leaf_tags_config_others_baked_default() {
    // Arrange: an operator leaf on rate-limited only.
    let mut config = Config::default();
    config.retry.classes.insert(
        ConfigFailureClass::RateLimited,
        ClassPolicy {
            retry: Some(7),
            fallback: None,
        },
    );

    // Act
    let view = derive_effective_view(&config, None, None);

    // Assert: rate-limited tagged config with the overridden cap; a class
    // with no leaf tags baked-default.
    let rate_limited = view
        .classes
        .iter()
        .find(|c| c.class == ConfigFailureClass::RateLimited)
        .unwrap();
    assert_eq!(rate_limited.source, ClassPolicySource::Config);
    assert_eq!(rate_limited.retry_cap, 7);

    let auth = view
        .classes
        .iter()
        .find(|c| c.class == ConfigFailureClass::Auth)
        .unwrap();
    assert_eq!(auth.source, ClassPolicySource::BakedDefault);

    // Every class is represented exactly once.
    assert_eq!(view.classes.len(), ALL_CONFIG_CLASSES.len());
}

#[test]
fn seeded_override_appears_as_capability_cell_with_override_source() {
    // Arrange: an operator capability override force-marks web_search
    // unsupported for the opus model's provider.
    let mut config = config_with_opus_model();
    config.capability.overrides.insert(
        "anthropic".to_string(),
        crate::config::OverrideEntry {
            unsupported: vec!["web_search".to_string()],
            force_supported: Vec::new(),
        },
    );

    // Act
    let view = derive_effective_view(&config, None, None);

    // Assert: the capability layer carries the cell with the route-away
    // verdict.
    let cell = view
        .capabilities
        .iter()
        .find(|c| c.target_spec == "anthropic" && c.capability_key == "web_search")
        .expect("seeded override must surface as a capability cell");
    assert_eq!(cell.verdict, OverrideVerdict::RouteAway);
}

/// The exact fixture `factory::validate_tests` uses to pin that a REJECTED
/// base_url's embedded credential never reaches an error message. Here it
/// pins the ACCEPTED case: a config carrying userinfo passes validation, so
/// the projection is the only thing standing between it and a read surface.
const LEAKY_BASE_URL: &str =
    "https://user:sk-live-LEAKED@internal.example/v1?key=sk-live-LEAKED#sk-live-LEAKED";

fn config_with_provider_base_url(base_url: &str) -> Config {
    toml::from_str(&format!(
        r#"
version = 2

[providers.acme]
kind = "anthropic-api"
api_key_ref = "env://ACME_KEY"
base_url = "{base_url}"
"#
    ))
    .expect("valid config")
}

#[test]
fn endpoint_origin_strips_userinfo_path_query_and_fragment() {
    // Arrange: an accepted config whose base_url embeds a credential in
    // userinfo, in the query, and in the fragment.
    let config = config_with_provider_base_url(LEAKY_BASE_URL);

    // Act
    let view = derive_effective_view(&config, None, None);

    // Assert: only the origin survives.
    let cell = &view.providers[0];
    assert_eq!(
        cell.endpoint_origin.as_deref(),
        Some("https://internal.example")
    );
}

#[test]
fn credential_never_appears_anywhere_in_the_projection() {
    // Arrange
    let config = config_with_provider_base_url(LEAKY_BASE_URL);

    // Act: scan the WHOLE projection, not just the endpoint field -- a
    // future field that copies config text must fail this too.
    let view = derive_effective_view(&config, None, None);
    let rendered = format!("{:?}", view.providers);

    // Assert
    assert!(
        !rendered.contains("sk-live-LEAKED"),
        "credential must not surface; got: {rendered}"
    );
    assert!(
        !rendered.contains("user:"),
        "userinfo must not surface; got: {rendered}"
    );
}

#[test]
fn endpoint_origin_keeps_a_nondefault_port() {
    let config = config_with_provider_base_url("http://127.0.0.1:8080/v1");
    let view = derive_effective_view(&config, None, None);
    assert_eq!(
        view.providers[0].endpoint_origin.as_deref(),
        Some("http://127.0.0.1:8080")
    );
}

#[test]
fn unparseable_endpoint_yields_none_not_a_raw_fallback() {
    // A non-URL base_url must project to None. A raw-string fallback here
    // would republish whatever the operator wrote, which is the entire
    // hazard this projection exists to close.
    let config = config_with_provider_base_url("sk-live-LEAKED-not-a-url");
    let view = derive_effective_view(&config, None, None);
    assert_eq!(view.providers[0].endpoint_origin, None);
    assert!(!format!("{:?}", view.providers).contains("sk-live-LEAKED"));
}

#[test]
fn credential_ref_scheme_carries_the_scheme_only() {
    let config = config_with_provider_base_url("https://upstream.example");
    let view = derive_effective_view(&config, None, None);
    let cell = &view.providers[0];
    assert_eq!(cell.credential_ref_scheme.as_deref(), Some("env"));
    assert!(
        !format!("{cell:?}").contains("ACME_KEY"),
        "the ref body must not surface"
    );
}

fn config_with_provider_api_key_ref(api_key_ref: &str) -> Config {
    let mut config = config_with_provider_base_url("https://upstream.example");
    if let Some(ProviderEntry::AnthropicApi {
        api_key_ref: slot, ..
    }) = config.providers.get_mut("acme")
    {
        *slot = api_key_ref.to_string();
    }
    config
}

/// An unrecognized ref is a bare value someone pasted into the field, not a
/// ref with an exotic scheme -- and a bare value IS secret material, so the
/// text before its first colon is a key id, not a scheme.
#[test]
fn unrecognized_credential_ref_publishes_none_of_its_bytes() {
    for (pasted, marker) in [
        ("AKIAEXAMPLE:wJalrXUtnFEMI", "AKIAEXAMPLE"),
        ("sk-live-abc:def", "sk-live-abc"),
        ("sk-live-no-colon-at-all", "sk-live-no-colon"),
        (":sk-live-leading", "sk-live-leading"),
    ] {
        let config = config_with_provider_api_key_ref(pasted);
        let view = derive_effective_view(&config, None, None);
        let cell = &view.providers[0];
        assert_eq!(
            cell.credential_ref_scheme.as_deref(),
            Some("unknown"),
            "`{pasted}` is not an allowlisted ref scheme; it must project the fixed label"
        );
        let rendered = format!("{cell:?}");
        assert!(
            !rendered.contains(marker),
            "no byte run of `{pasted}` may reach the cell: {rendered}"
        );
    }
    // The allowlisted four still project, case-insensitively, and the
    // single-colon `env:VAR` spelling still reads as env.
    for (raw, expected) in [
        ("env://VAR", "env"),
        ("env:VAR", "env"),
        ("FILE:///etc/k", "file"),
        ("oauth://anthropic", "oauth"),
        ("literal:k", "literal"),
    ] {
        assert_eq!(credential_ref_scheme(Some(raw)).as_deref(), Some(expected));
    }
}

#[test]
fn absent_or_blank_credential_ref_projects_none() {
    for absent in [None, Some(""), Some("   ")] {
        assert_eq!(credential_ref_scheme(absent), None, "{absent:?}");
    }
}

/// Only `http`/`https` project. Every other scheme carries an OPAQUE host,
/// which the url crate does NOT percent-decode, so credential bytes would
/// ride straight through `host_str()`. This cannot lean on
/// `validate_base_url_scheme`: that runs only from the per-model factory
/// loop, while this projection walks every provider entry, so a provider no
/// model references arrives here unvalidated.
#[test]
fn non_http_scheme_projects_no_origin() {
    for raw in [
        "weird://h.example%40sk-live-LEAKED/v1",
        "gopher://user:sk-live-P@h.example/x",
        "ftp://h.example/v1",
        // These two have no `//` authority, so they would yield None even
        // without the gate -- pinned so a future reader does not mistake
        // that for scheme checking.
        "mailto:a@b.example",
        "unix:/var/run/x.sock",
    ] {
        assert_eq!(
            endpoint_origin(raw),
            None,
            "`{raw}` is not http(s); it must project nothing"
        );
        assert!(!format!("{:?}", endpoint_origin(raw)).contains("sk-live"));
    }
}

/// For a special scheme the authority ends at the first `/`, so a `/` inside
/// a credential truncates it and the parser reads the pre-`@` text as the
/// HOST. When the credential sits in the username position that puts it
/// straight into the projected origin.
#[test]
fn at_sign_the_parser_did_not_consume_projects_no_origin() {
    // Parses as host "sk-live-KEY", port 8080 -- the real host is demoted
    // into the path, so the origin would have BEEN the credential.
    assert_eq!(
        endpoint_origin("https://sk-live-KEY:8080/x@h.example/v1"),
        None
    );
    assert_eq!(
        endpoint_origin("https://user:8080/sk-live-REST@h.example/v1"),
        None
    );
    // A genuine userinfo authority still projects its real origin: the
    // parser consumed the `@`, so host_str() is the operator's host.
    assert_eq!(
        endpoint_origin("https://user:sk-live-X@h.example/v1").as_deref(),
        Some("https://h.example")
    );
    // No `@` at all is the ordinary case and is unaffected.
    assert_eq!(
        endpoint_origin("https://h.example:8443/v1").as_deref(),
        Some("https://h.example:8443")
    );
}

/// TWO `@` is a distinct vector from one, and defeats any guard that asks
/// "did the parse consume userinfo?": the parser takes userinfo from the
/// LAST `@` before the first `/`, so a benign leading `x@` vouches for a
/// secret that then lands in the host slot. Both of these projected
/// credential-derived origins before the authority-scoped predicate.
#[test]
fn a_second_at_sign_cannot_vouch_for_a_demoted_credential() {
    assert_eq!(
        endpoint_origin("https://x@sk-live-AB/CD@h.example/v1"),
        None
    );
    assert_eq!(
        endpoint_origin("https://x@sk-live-KEY:8080/y@h.example/v1"),
        None
    );
    // Neither may leak even a lowercased fragment of the credential.
    for raw in [
        "https://x@sk-live-AB/CD@h.example/v1",
        "https://x@sk-live-KEY:8080/y@h.example/v1",
    ] {
        assert!(!format!("{:?}", endpoint_origin(raw)).contains("sk-live"));
    }
}

/// The withholding is deliberately fail-SAFE rather than precise: an `@`
/// that is legitimately in a path or query also suppresses the origin,
/// which the panel renders as "derived at startup". Pinned so the
/// imprecision is a recorded decision rather than a surprise -- tightening
/// it must not reopen `a_second_at_sign_cannot_vouch_for_a_demoted_credential`.
#[test]
fn an_at_sign_past_the_authority_withholds_fail_safe() {
    assert_eq!(endpoint_origin("https://gw.example/v1?tenant=a@b"), None);
    assert_eq!(endpoint_origin("https://h.example/v1@beta"), None);
}

#[test]
fn anthropic_entry_carries_its_own_auth_token_and_kind() {
    let config = config_with_provider_base_url("https://upstream.example");
    let view = derive_effective_view(&config, None, None);
    let cell = &view.providers[0];
    assert_eq!(cell.id, "acme");
    assert_eq!(cell.kind, "anthropic-api");
    assert_eq!(cell.auth_token.as_deref(), Some("api-key"));
}

#[test]
fn openai_compat_has_no_auth_token() {
    // The variant carries no auth discriminant, so the projection reports
    // absence rather than inventing a spanning token.
    let config: Config = toml::from_str(
        r#"
version = 2

[providers.compat]
kind = "openai-compat"
api_key_ref = "env://COMPAT_KEY"
base_url = "https://compat.example/v1"
"#,
    )
    .expect("valid config");

    let view = derive_effective_view(&config, None, None);
    assert_eq!(view.providers[0].auth_token, None);
}

/// Every auth arm the build carries, asserted against the token its own
/// config vocabulary already uses. The `auth_token` match is exhaustive
/// with no wildcard, so a new variant breaks the BUILD -- but a typo'd or
/// swapped token in an existing arm compiles fine and would publish a
/// wrong mechanism name on the panel. Only these tests catch that, and
/// each is cfg-gated to the feature that compiles its arm.
fn only_provider(toml_text: &str) -> ProviderCell {
    let config: Config = toml::from_str(toml_text).expect("valid config");
    let view = derive_effective_view(&config, None, None);
    view.providers
        .into_iter()
        .next()
        .expect("one provider projected")
}

#[test]
fn anthropic_oauth_bearer_carries_its_own_token() {
    let cell = only_provider(
        r#"
version = 2

[providers.oauth]
kind = "anthropic-api"
api_key_ref = "oauth://anthropic"
auth_kind = "oauth-bearer"
"#,
    );
    assert_eq!(cell.kind, "anthropic-api");
    assert_eq!(cell.auth_token.as_deref(), Some("oauth-bearer"));
    assert_eq!(cell.credential_ref_scheme.as_deref(), Some("oauth"));
}

#[cfg(feature = "openai-responses")]
#[test]
fn openai_responses_carries_each_of_its_own_auth_tokens() {
    for (auth_kind, expected) in [
        ("api-key", "api-key"),
        ("chatgpt-oauth", "chatgpt-oauth"),
        ("bedrock-mantle", "bedrock-mantle"),
    ] {
        let cell = only_provider(&format!(
            r#"
version = 2

[providers.r]
kind = "openai-responses"
api_key_ref = "env://K"
auth_kind = "{auth_kind}"
"#
        ));
        assert_eq!(cell.kind, "openai-responses");
        assert_eq!(
            cell.auth_token.as_deref(),
            Some(expected),
            "auth_kind {auth_kind} must project its own token"
        );
    }
}

#[cfg(feature = "gemini")]
#[test]
fn gemini_carries_each_of_its_own_auth_modes() {
    for (auth_mode, expected) in [("api-key", "api-key"), ("cloud-code", "cloud-code")] {
        let cell = only_provider(&format!(
            r#"
version = 2

[providers.g]
kind = "gemini"
api_key_ref = "env://GEMINI_API_KEY"
auth_mode = "{auth_mode}"
"#
        ));
        assert_eq!(cell.kind, "gemini");
        assert_eq!(
            cell.auth_token.as_deref(),
            Some(expected),
            "auth_mode {auth_mode} must project its own token"
        );
    }
}

#[cfg(feature = "gemini")]
#[test]
fn gemini_cloud_code_reports_its_effective_host_not_the_api_key_default() {
    // An unset `base_url` on a cloud-code entry keeps the api-key public
    // default in config, but the lane never talks to that host: the
    // factory leaves the constructor's cloud-code default in place.
    let unset = only_provider(
        r#"
version = 2

[providers.g]
kind = "gemini"
api_key_ref = "oauth://antigravity"
auth_mode = "cloud-code"
"#,
    );
    assert_eq!(
        unset.endpoint_origin.as_deref(),
        endpoint_origin(routectl_providers::gemini::DAILY_BASE_URL).as_deref(),
    );

    // An explicit pin is reported verbatim -- never rewritten.
    let pinned = only_provider(&format!(
        r#"
version = 2

[providers.g]
kind = "gemini"
api_key_ref = "oauth://antigravity"
auth_mode = "cloud-code"
base_url = "{}"
"#,
        routectl_providers::gemini::PROD_BASE_URL
    ));
    assert_eq!(
        pinned.endpoint_origin.as_deref(),
        endpoint_origin(routectl_providers::gemini::PROD_BASE_URL).as_deref(),
    );

    // An api-key entry keeps reporting the public REST default.
    let api_key = only_provider(
        r#"
version = 2

[providers.g]
kind = "gemini"
api_key_ref = "env://GEMINI_API_KEY"
"#,
    );
    assert_eq!(
        api_key.endpoint_origin.as_deref(),
        endpoint_origin(&crate::config::default_gemini_base()).as_deref(),
    );
}

#[cfg(feature = "bedrock")]
#[test]
fn bedrock_carries_its_creds_kind_and_no_configured_endpoint() {
    let cell = only_provider(
        r#"
version = 2

[providers.b]
kind = "bedrock"
region = "us-east-1"

[providers.b.creds]
kind = "default-chain"
"#,
    );
    assert_eq!(cell.kind, "bedrock");
    assert_eq!(cell.auth_token.as_deref(), Some("default-chain"));
    // Bedrock carries no `base_url` at all -- the factory derives the host
    // from `region`. A pure derivation reports absence rather than
    // duplicating that factory logic.
    assert_eq!(cell.endpoint_origin, None);
}

#[test]
fn zero_rpm_limit_stays_distinct_from_unlimited() {
    // `Some(0)` seeds a zero-capacity bucket -- immediately rate-limited.
    // Collapsing it into `None` (unlimited) would invert the meaning.
    let limited: Config = toml::from_str(
        r#"
version = 2

[providers.zero]
kind = "anthropic-api"
api_key_ref = "env://K"
rpm_limit = 0

[providers.unlimited]
kind = "anthropic-api"
api_key_ref = "env://K"

[providers.capped]
kind = "anthropic-api"
api_key_ref = "env://K"
rpm_limit = 60
"#,
    )
    .expect("valid config");

    let view = derive_effective_view(&limited, None, None);
    let rpm = |id: &str| {
        view.providers
            .iter()
            .find(|p| p.id == id)
            .expect("provider projected")
            .rpm_limit
    };
    assert_eq!(rpm("zero"), Some(0));
    assert_eq!(rpm("unlimited"), None);
    assert_eq!(rpm("capped"), Some(60));
}

#[test]
fn providers_are_projected_in_provider_key_order() {
    let config: Config = toml::from_str(
        r#"
version = 2

[providers.zeta]
kind = "anthropic-api"
api_key_ref = "env://K"

[providers.alpha]
kind = "anthropic-api"
api_key_ref = "env://K"
"#,
    )
    .expect("valid config");

    let view = derive_effective_view(&config, None, None);
    let ids: Vec<&str> = view.providers.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["alpha", "zeta"]);
}

#[test]
fn capability_layer_empty_when_no_overrides() {
    // A config with no capability overrides yields an empty capability
    // layer -- rendered empty, never an error.
    let config = config_with_opus_model();
    let view = derive_effective_view(&config, None, None);
    assert!(view.capabilities.is_empty());
}

#[test]
fn derivation_source_is_pure_no_router_build() {
    // Structural guard: this module never calls into the secret-resolving /
    // provider-constructing build path. The pure `&Config` plus price-table
    // signature is the real invariant; this scan tripwires an accidental
    // reintroduction. The tests live in a sidecar, so the whole file is
    // production code.
    let production = include_str!("config_effective.rs");

    assert!(!production.contains("build_resolved_models"));
    assert!(!production.contains("build_provider"));
}

/// A config in the recorded shape of a real multi-seat deployment: a
/// five-seat pool of one provider kind, one standalone provider with its
/// own breaker, a few untuned providers, an explicit `[retry]` block with
/// both global timeouts set, and one class override. Models set no
/// first-content override, so the table holds provider rows only.
fn live_shape_config() -> Config {
    toml::from_str(
        r#"
version = 2

[retry]
max_attempts = 3
initial_backoff_ms = 250
backoff_multiplier = 2.0
jitter_ms = 50
request_timeout_ms = 1800000
stream_first_byte_timeout_ms = 600000

[retry.classes.bad-request]
fallback = false

[providers.seat-1]
kind = "anthropic-api"
api_key_ref = "env://SEAT_1"

[providers.seat-2]
kind = "anthropic-api"
api_key_ref = "env://SEAT_2"

[providers.seat-3]
kind = "anthropic-api"
api_key_ref = "env://SEAT_3"

[providers.seat-4]
kind = "anthropic-api"
api_key_ref = "env://SEAT_4"

[providers.seat-5]
kind = "anthropic-api"
api_key_ref = "env://SEAT_5"

[pools.seats]
members = ["seat-1", "seat-2", "seat-3", "seat-4", "seat-5"]

[providers.relay]
kind = "anthropic-api"
api_key_ref = "env://RELAY"
base_url = "https://relay.example/v1"
circuit_failures = 5
circuit_cooldown_ms = 30000

[providers.compat-a]
kind = "openai-compat"
base_url = "https://compat-a.example/v1"
api_key_ref = "env://COMPAT_A"

[providers.compat-b]
kind = "openai-compat"
base_url = "https://compat-b.example/v1"
api_key_ref = "env://COMPAT_B"

[models.big]
provider = "seats"
upstream = "claude-opus-4-8"

[models.side]
provider = "relay"
upstream = "claude-sonnet-4-6"

[models.cheap]
provider = "compat-a"
upstream = "u"
"#,
    )
    .expect("valid config")
}

fn resolved<T>(value: T, source: TuningSource) -> Resolved<T> {
    Resolved { value, source }
}

fn after(n: u32) -> BreakerThreshold {
    BreakerThreshold::After(std::num::NonZeroU32::new(n).expect("non-zero"))
}

/// The row an entry with no breaker or RPM key of its own resolves to under
/// the live-shape `[retry]` block.
fn untuned_row(target: &str) -> TuningRow {
    TuningRow {
        target: target.to_string(),
        kind: TuningTargetKind::Provider,
        tuning: ResolvedTuning {
            circuit_failures: resolved(after(5), TuningSource::ShippedDefault),
            circuit_cooldown_ms: resolved(60_000, TuningSource::ShippedDefault),
            rpm_limit: resolved(None, TuningSource::ShippedDefault),
            request_timeout_ms: resolved(Some(1_800_000), TuningSource::Global),
            stream_first_byte_timeout_ms: resolved(Some(600_000), TuningSource::Global),
        },
    }
}

/// The whole resolved-tuning table for the live config shape.
///
/// Against the behavior before resolved tuning existed, only the breaker
/// cells of untuned entries changed: every pool seat and untuned provider
/// went from breaker off to the shipped 5 failures / 60000 ms. The tuned
/// provider keeps its own 5 / 30000, both global timeouts still apply to
/// every row, and RPM stays unlimited everywhere.
#[test]
fn live_shape_config_resolves_to_the_expected_tuning_table() {
    let config = live_shape_config();

    let view = derive_effective_view(&config, None, None);

    let mut relay = untuned_row("relay");
    relay.tuning.circuit_failures = resolved(after(5), TuningSource::Provider);
    relay.tuning.circuit_cooldown_ms = resolved(30_000, TuningSource::Provider);
    let expected = vec![
        untuned_row("compat-a"),
        untuned_row("compat-b"),
        relay,
        untuned_row("seat-1"),
        untuned_row("seat-2"),
        untuned_row("seat-3"),
        untuned_row("seat-4"),
        untuned_row("seat-5"),
    ];
    assert_eq!(view.tuning, expected);
}

#[test]
fn a_pooled_model_first_byte_override_adds_one_row_per_seat_in_member_order() {
    let mut config = live_shape_config();
    config
        .models
        .get_mut("big")
        .expect("big model")
        .stream_first_byte_timeout_ms = Some(90_000);
    config.pools.get_mut("seats").expect("seats pool").rpm_limit = Some(20);

    let view = derive_effective_view(&config, None, None);

    let model_rows: Vec<&TuningRow> = view
        .tuning
        .iter()
        .filter(|row| row.kind == TuningTargetKind::Model)
        .collect();
    let targets: Vec<&str> = model_rows.iter().map(|row| row.target.as_str()).collect();
    assert_eq!(
        targets,
        [
            "big#seat-1",
            "big#seat-2",
            "big#seat-3",
            "big#seat-4",
            "big#seat-5"
        ]
    );
    for row in &model_rows {
        assert_eq!(
            row.tuning.stream_first_byte_timeout_ms,
            resolved(Some(90_000), TuningSource::Model),
            "{}",
            row.target
        );
        assert_eq!(
            row.tuning.rpm_limit,
            resolved(Some(20), TuningSource::Pool),
            "{}",
            row.target
        );
    }
    assert_eq!(view.tuning.last(), model_rows.last().copied());
}

/// A two-member pool where member `seat-a` sets its own breaker and RPM keys
/// and the pool sets different ones, a pooled model and a direct model each
/// carrying a first-content override.
fn member_override_config() -> Config {
    toml::from_str(
        r#"
version = 2

[providers.seat-a]
kind = "anthropic-api"
api_key_ref = "env://SEAT_A"
rpm_limit = 7
circuit_failures = 2

[providers.seat-b]
kind = "anthropic-api"
api_key_ref = "env://SEAT_B"

[providers.solo]
kind = "anthropic-api"
api_key_ref = "env://SOLO"
rpm_limit = 9

[pools.team]
members = ["seat-a", "seat-b"]
rpm_limit = 40
circuit_failures = 8

[models.pooled]
provider = "team"
upstream = "u"
stream_first_byte_timeout_ms = 45000

[models.direct]
provider = "solo"
upstream = "u"
stream_first_byte_timeout_ms = 50000
"#,
    )
    .expect("valid config")
}

/// Each seat row resolves through its member, as dispatch stamps it: the
/// member's own keys beat the pool's, the untuned member inherits the pool,
/// and the model's first-content override applies to both seats. A direct
/// model keeps one row under its bare nickname.
#[test]
fn a_pooled_model_row_per_seat_resolves_through_its_member() {
    let config = member_override_config();

    let view = derive_effective_view(&config, None, None);

    let model_row = |target: &str,
                     failures: Resolved<BreakerThreshold>,
                     rpm: Resolved<Option<u32>>,
                     first_byte_ms: u64| TuningRow {
        target: target.to_string(),
        kind: TuningTargetKind::Model,
        tuning: ResolvedTuning {
            circuit_failures: failures,
            circuit_cooldown_ms: resolved(60_000, TuningSource::ShippedDefault),
            rpm_limit: rpm,
            request_timeout_ms: resolved(Some(1_800_000), TuningSource::ShippedDefault),
            stream_first_byte_timeout_ms: resolved(Some(first_byte_ms), TuningSource::Model),
        },
    };
    let expected = vec![
        model_row(
            "direct",
            resolved(after(5), TuningSource::ShippedDefault),
            resolved(Some(9), TuningSource::Provider),
            50_000,
        ),
        model_row(
            "pooled#seat-a",
            resolved(after(2), TuningSource::Provider),
            resolved(Some(7), TuningSource::Provider),
            45_000,
        ),
        model_row(
            "pooled#seat-b",
            resolved(after(8), TuningSource::Pool),
            resolved(Some(40), TuningSource::Pool),
            45_000,
        ),
    ];
    let model_rows: Vec<TuningRow> = view
        .tuning
        .into_iter()
        .filter(|row| row.kind == TuningTargetKind::Model)
        .collect();
    assert_eq!(model_rows, expected);
}

#[test]
fn a_zero_model_first_byte_override_adds_no_row() {
    let mut config = live_shape_config();
    config
        .models
        .get_mut("big")
        .expect("big model")
        .stream_first_byte_timeout_ms = Some(0);

    let view = derive_effective_view(&config, None, None);

    assert!(
        view.tuning
            .iter()
            .all(|row| row.kind == TuningTargetKind::Provider)
    );
}

#[test]
fn a_pool_rpm_limit_leaves_member_provider_cells_unset_and_shows_on_their_tuning_rows() {
    let mut config = live_shape_config();
    config.pools.get_mut("seats").expect("seats pool").rpm_limit = Some(20);

    let view = derive_effective_view(&config, None, None);

    for cell in &view.providers {
        assert_eq!(cell.rpm_limit, None, "provider cell {}", cell.id);
    }
    for row in view
        .tuning
        .iter()
        .filter(|row| row.kind == TuningTargetKind::Provider)
    {
        let expected = if row.target.starts_with("seat-") {
            resolved(Some(20), TuningSource::Pool)
        } else {
            resolved(None, TuningSource::ShippedDefault)
        };
        assert_eq!(row.tuning.rpm_limit, expected, "tuning row {}", row.target);
    }
}

/// One config with a model per fill and window shape: a configured fill and
/// window on a pass-through egress, a configured fill on an Anthropic-shape
/// egress, an Anthropic-shape egress with neither, a pass-through egress
/// with neither, and an Anthropic-shape pool.
fn facts_config() -> Config {
    toml::from_str(
        r#"
version = 2

[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "env://ANTHROPIC_API_KEY"

[providers.compat]
kind = "openai-compat"
base_url = "https://compat.example/v1"
api_key_ref = "env://K"

[providers.seat-a]
kind = "anthropic-api"
api_key_ref = "env://A"

[pools.seats]
members = ["seat-a"]

[models.configured]
provider = "compat"
upstream = "glm-5"
context_window = 200000
max_output_tokens = 12000

[models.opus]
provider = "anthropic"
upstream = "claude-opus-4-8"

[models.opus-capped]
provider = "anthropic"
upstream = "claude-opus-4-8"
max_output_tokens = 16000

[models.plain]
provider = "compat"
upstream = "glm-5"

[models.pooled]
provider = "seats"
upstream = "claude-opus-4-8"
"#,
    )
    .expect("valid config")
}

fn model<'a>(view: &'a EffectiveView, nickname: &str) -> &'a ModelCell {
    view.models
        .iter()
        .find(|m| m.nickname == nickname)
        .unwrap_or_else(|| panic!("model {nickname} in the view"))
}

#[test]
fn model_window_and_fill_carry_their_source() {
    let view = derive_effective_view(&facts_config(), None, None);

    let default_fill = Some(FillCeiling {
        tokens: routectl_providers::anthropic_api::DEFAULT_MAX_OUTPUT_TOKENS,
        source: FillSource::Default,
    });
    let rows: [(&str, Option<u32>, Option<FillCeiling>); 5] = [
        (
            "configured on an egress that never fills",
            Some(200_000),
            None,
        ),
        (
            "opus-capped",
            None,
            Some(FillCeiling {
                tokens: 16_000,
                source: FillSource::ModelConfig,
            }),
        ),
        ("opus", None, default_fill),
        ("plain", None, None),
        ("pooled", None, default_fill),
    ];
    for (name, window, fill) in rows {
        let nickname = name.split(' ').next().unwrap_or(name);
        let cell = model(&view, nickname);
        assert_eq!(cell.context_window, window, "{nickname} window");
        assert_eq!(cell.fill, fill, "{nickname} fill");
    }
}

/// A pool's fill is read through its first resolvable member: Invoke and
/// Converse share the `bedrock` kind, so a pool may mix the two shapes, and
/// member order alone decides whether the pool's row reads the Invoke
/// baseline or no fill.
#[cfg(feature = "bedrock")]
#[test]
fn a_mixed_shape_bedrock_pool_reads_its_fill_through_the_first_member() {
    let config = |members: &str| -> Config {
        toml::from_str(&format!(
            r#"
version = 2

[providers.invoke]
kind = "bedrock"
region = "us-east-1"
[providers.invoke.creds]
kind = "default-chain"

[providers.converse]
kind = "bedrock"
region = "us-east-1"
api_shape = "converse"
[providers.converse.creds]
kind = "default-chain"

[pools.mixed]
members = {members}

[models.pooled]
provider = "mixed"
upstream = "anthropic.claude-opus-4-8"
"#
        ))
        .expect("valid config")
    };
    let default_fill = Some(FillCeiling {
        tokens: routectl_providers::anthropic_api::DEFAULT_MAX_OUTPUT_TOKENS,
        source: FillSource::Default,
    });
    let rows = [
        ("invoke first", r#"["invoke", "converse"]"#, default_fill),
        ("converse first", r#"["converse", "invoke"]"#, None),
    ];
    for (name, members, expected) in rows {
        let view = derive_effective_view(&config(members), None, None);

        assert_eq!(model(&view, "pooled").fill, expected, "{name}");
    }
}

#[test]
fn fill_source_tokens_are_fixed() {
    assert_eq!(FillSource::ModelConfig.as_str(), "model-config");
    assert_eq!(FillSource::Learned.as_str(), "learned");
    assert_eq!(FillSource::Default.as_str(), "default");
}

/// A learned ceiling lowers a fill only when it is below it; the source then
/// reads `learned`. A model whose egress has no fill stays unfilled.
#[test]
fn a_learned_ceiling_lowers_a_fill_only_when_below_it() {
    let base = FillCeiling {
        tokens: 16_000,
        source: FillSource::ModelConfig,
    };
    let rows = [
        ("no learned ceiling", None, base),
        ("learned above", Some(20_000), base),
        ("learned equal", Some(16_000), base),
        (
            "learned below",
            Some(8_192),
            FillCeiling {
                tokens: 8_192,
                source: FillSource::Learned,
            },
        ),
    ];
    for (name, learned, expected) in rows {
        assert_eq!(base.lowered_by(learned), expected, "{name}");
    }
}

fn dated_table(date: &str, input: f64) -> PriceTable {
    let json = serde_json::json!({
        "schema_version": 1,
        "generated_at": date,
        "providers": [{
            "id": "anthropic",
            "models": [{ "id": "claude-opus-4-8", "input": input, "output": 25.0 }],
        }],
    });
    crate::prices::parse_and_validate(&serde_json::to_vec(&json).expect("serializes"))
        .expect("valid table")
}

#[test]
fn model_prices_carry_each_field_source_and_the_view_carries_table_dates() {
    let mut config = facts_config();
    config.models.get_mut("opus").expect("opus").pricing = Some(PricingConfig {
        output_per_mtok: Some(30.0),
        ..PricingConfig::default()
    });
    let refreshed = dated_table("2026-11-05", 5.0);
    let shipped = dated_table("2026-10-01", 4.0);

    let view = derive_effective_view(&config, Some(&refreshed), Some(&shipped));

    let opus = model(&view, "opus");
    assert_eq!(
        opus.price_fields(),
        [
            (PriceDimension::Input, Some(5.0), PriceSource::Refreshed),
            (PriceDimension::Output, Some(30.0), PriceSource::ModelConfig),
            (PriceDimension::CacheRead, None, PriceSource::Unknown),
            (PriceDimension::CacheWrite(300), None, PriceSource::Unknown),
            (
                PriceDimension::CacheWrite(3600),
                Some(10.0),
                PriceSource::Derived
            ),
        ]
    );
    let refreshed_date = TableDate::parse("2026-11-05");
    let shipped_date = TableDate::parse("2026-10-01");
    assert_eq!(
        view.price_tables,
        PriceTableDates {
            refreshed: refreshed_date,
            shipped: shipped_date,
        }
    );
    assert_eq!(
        view.price_tables.date_for(PriceSource::Refreshed),
        refreshed_date
    );
    assert_eq!(
        view.price_tables.date_for(PriceSource::Shipped),
        shipped_date
    );
    for source in [
        PriceSource::ModelConfig,
        PriceSource::Registry,
        PriceSource::Derived,
        PriceSource::Unknown,
    ] {
        assert_eq!(view.price_tables.date_for(source), None, "{source:?}");
    }
}

/// A pool-backed model prices through its first member's entry name, the
/// key a usage row served through the pool bills under, so a `[registry]`
/// row scoped to that member applies; one scoped to the pool name does not
/// match a billed row and does not apply here either.
#[test]
fn a_pool_model_prices_through_its_first_member_entry() {
    let rows = [
        ("registry scoped to the first member", "seat-a", Some(7.0)),
        ("registry scoped to the pool name", "seats", None),
    ];
    for (name, scope, expected) in rows {
        let mut config = facts_config();
        config.registry.insert(
            "claude-opus-4-8".to_string(),
            crate::config::RegistryEntry {
                pricing: Some(PricingConfig {
                    input_per_mtok: Some(7.0),
                    ..PricingConfig::default()
                }),
                provider: Some(scope.to_string()),
            },
        );

        let view = derive_effective_view(&config, None, None);

        assert_eq!(
            model(&view, "pooled").pricing.input_per_mtok,
            expected,
            "{name}"
        );
    }
}

#[test]
fn a_model_no_source_prices_reads_unknown_on_every_field() {
    let view = derive_effective_view(&facts_config(), None, None);

    let plain = model(&view, "plain");
    let fields = plain.price_fields();
    assert_eq!(fields.len(), 5, "{fields:?}");
    for (dimension, value, source) in fields {
        assert_eq!(value, None, "{dimension:?}");
        assert_eq!(source, PriceSource::Unknown, "{dimension:?}");
    }
    assert_eq!(view.price_tables, PriceTableDates::default());
}

/// Every lifetime a source prices beyond 5m and 1h follows the five fixed
/// fields, shortest first, each with its own source; the 300 and 3600
/// lifetimes stay in their fixed rows whichever spelling priced them.
#[test]
fn price_fields_list_each_extra_priced_lifetime_after_the_fixed_five() {
    let mut config = facts_config();
    config.models.get_mut("opus").expect("opus").pricing = Some(PricingConfig {
        cache_write_5m_per_mtok: Some(3.75),
        cache_write_per_mtok: Some(
            [("1h", 6.0), ("30m", 4.5), ("24h", 9.0)]
                .map(|(key, rate)| (key.to_string(), rate))
                .into(),
        ),
        ..PricingConfig::default()
    });
    config.registry.insert(
        "glm-5".to_string(),
        crate::config::RegistryEntry {
            pricing: Some(PricingConfig {
                cache_write_per_mtok: Some([("90s".to_string(), 0.5)].into()),
                ..PricingConfig::default()
            }),
            provider: None,
        },
    );

    let view = derive_effective_view(&config, None, None);

    let rows = [
        (
            "model-config lifetimes",
            "opus",
            vec![
                (
                    PriceDimension::CacheWrite(300),
                    Some(3.75),
                    PriceSource::ModelConfig,
                ),
                (
                    PriceDimension::CacheWrite(3600),
                    Some(6.0),
                    PriceSource::ModelConfig,
                ),
                (
                    PriceDimension::CacheWrite(1800),
                    Some(4.5),
                    PriceSource::ModelConfig,
                ),
                (
                    PriceDimension::CacheWrite(86_400),
                    Some(9.0),
                    PriceSource::ModelConfig,
                ),
            ],
        ),
        (
            "a registry lifetime below 5m",
            "plain",
            vec![
                (PriceDimension::CacheWrite(300), None, PriceSource::Unknown),
                (PriceDimension::CacheWrite(3600), None, PriceSource::Unknown),
                (
                    PriceDimension::CacheWrite(90),
                    Some(0.5),
                    PriceSource::Registry,
                ),
            ],
        ),
    ];
    for (name, nickname, expected_writes) in rows {
        let fields = model(&view, nickname).price_fields();
        let fixed: Vec<_> = fields.iter().take(3).map(|(d, _, _)| *d).collect();
        assert_eq!(
            fixed,
            [
                PriceDimension::Input,
                PriceDimension::Output,
                PriceDimension::CacheRead
            ],
            "{name}"
        );
        assert_eq!(fields[3..], expected_writes[..], "{name}");
    }
}

#[test]
fn price_dimension_config_key_names_the_key_that_prices_it() {
    let rows = [
        ("input", PriceDimension::Input, "input_per_mtok"),
        ("output", PriceDimension::Output, "output_per_mtok"),
        (
            "cache read",
            PriceDimension::CacheRead,
            "cache_read_per_mtok",
        ),
        (
            "5m legacy key",
            PriceDimension::CacheWrite(300),
            "cache_write_5m_per_mtok",
        ),
        (
            "1h legacy key",
            PriceDimension::CacheWrite(3600),
            "cache_write_1h_per_mtok",
        ),
        (
            "30m map key",
            PriceDimension::CacheWrite(1800),
            "cache_write_per_mtok.30m",
        ),
        (
            "24h map key",
            PriceDimension::CacheWrite(86_400),
            "cache_write_per_mtok.1d",
        ),
        (
            "90s map key",
            PriceDimension::CacheWrite(90),
            "cache_write_per_mtok.90s",
        ),
    ];
    for (name, dimension, expected) in rows {
        assert_eq!(dimension.config_key(), expected, "{name}");
    }
}
