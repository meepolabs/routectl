use super::*;
use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result};

use crate::catalog::Source;
use crate::catalog_overlay::CatalogOverlay;
use crate::class_policy::ClassPolicy;
use crate::config::Config;
use crate::factory::apply_catalog_overlay;
use crate::override_registry::{OverrideProvenance, OverrideVerdict};
use crate::resolved::ResolvedModel;

struct StubProvider {
    id: String,
}

#[async_trait]
impl Provider for StubProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(&self.id, "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        unreachable!()
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        unreachable!()
    }
}

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
fn model_row_matches_apply_catalog_overlay_stamp() {
    // Arrange: same (provider_kind, upstream) fed to both paths.
    let config = config_with_opus_model();
    let overlay = CatalogOverlay::default();

    let provider: Arc<dyn Provider> = Arc::new(StubProvider { id: "stub".into() });
    let mut resolved = BTreeMap::new();
    resolved.insert(
        "opus".to_string(),
        Arc::new(ResolvedModel::new(
            "opus",
            "anthropic",
            provider,
            "claude-opus-4-8",
        )),
    );

    // Act
    let stamped = apply_catalog_overlay(resolved, &config, &overlay);
    let view = derive_effective_view(&config, &overlay);

    // Assert: the derivation reproduces the stamped merge exactly.
    let derived = view.models.iter().find(|m| m.nickname == "opus").unwrap();
    assert_eq!(derived.row, stamped["opus"].effective_row);
    assert!(matches!(
        derived.row,
        EffectiveRow::Present {
            source: Source::Baked,
            ..
        }
    ));
}

#[test]
fn overlay_user_cell_tags_user_source() {
    // Arrange: a user overlay cell for the opus selector.
    let config = config_with_opus_model();
    let overlay: CatalogOverlay = serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "revision": 1,
        "cells": {
            "anthropic-api:claude-opus-4-8": {
                "source": "user",
                "verified_at": "2026-07-01",
                "rm": 0.05
            }
        }
    }))
    .expect("valid overlay");

    // Act
    let view = derive_effective_view(&config, &overlay);

    // Assert
    let derived = view.models.iter().find(|m| m.nickname == "opus").unwrap();
    match &derived.row {
        EffectiveRow::Present { source, row, .. } => {
            assert_eq!(*source, Source::User);
            assert_eq!(row.rm, 0.05);
        }
        other => panic!("expected Present/User, got {other:?}"),
    }
}

#[test]
fn disable_overlay_cell_yields_disabled_row() {
    let config = config_with_opus_model();
    let overlay: CatalogOverlay = serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "revision": 1,
        "cells": { "anthropic-api:claude-opus-4-8": null }
    }))
    .expect("valid overlay");

    let view = derive_effective_view(&config, &overlay);
    let derived = view.models.iter().find(|m| m.nickname == "opus").unwrap();
    assert_eq!(derived.row, EffectiveRow::Disabled);
}

#[test]
fn unpriced_selector_yields_missing_row() {
    // A model whose provider is unknown resolves to an empty
    // provider_kind, which matches no baked cell -- the same fallback the
    // chain-build merge uses -- so neither layer has a row.
    let mut config = config_with_opus_model();
    config.models.get_mut("opus").unwrap().provider = "ghost".to_string();

    let view = derive_effective_view(&config, &CatalogOverlay::default());
    let derived = view.models.iter().find(|m| m.nickname == "opus").unwrap();
    assert_eq!(derived.provider_kind, "");
    assert_eq!(derived.row, EffectiveRow::Missing);
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
    let view = derive_effective_view(&config, &CatalogOverlay::default());

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
    let view = derive_effective_view(&config, &CatalogOverlay::default());

    // Assert: the capability layer carries the cell, tagged with the
    // Override provenance (the "override" token of the routing filter's
    // source contract) and the route-away verdict.
    let cell = view
        .capabilities
        .iter()
        .find(|c| c.target_spec == "anthropic" && c.capability_key == "web_search")
        .expect("seeded override must surface as a capability cell");
    assert_eq!(cell.verdict, OverrideVerdict::RouteAway);
    assert_eq!(cell.provenance, OverrideProvenance::Override);
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
    let view = derive_effective_view(&config, &CatalogOverlay::default());

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
    let view = derive_effective_view(&config, &CatalogOverlay::default());
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
    let view = derive_effective_view(&config, &CatalogOverlay::default());
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
    let view = derive_effective_view(&config, &CatalogOverlay::default());
    assert_eq!(view.providers[0].endpoint_origin, None);
    assert!(!format!("{:?}", view.providers).contains("sk-live-LEAKED"));
}

#[test]
fn credential_ref_scheme_carries_the_scheme_only() {
    let config = config_with_provider_base_url("https://upstream.example");
    let view = derive_effective_view(&config, &CatalogOverlay::default());
    let cell = &view.providers[0];
    assert_eq!(cell.credential_ref_scheme.as_deref(), Some("env"));
    assert!(
        !format!("{cell:?}").contains("ACME_KEY"),
        "the ref body must not surface"
    );
}

/// An unrecognized ref is a bare value someone pasted into the field, not a
/// ref with an exotic scheme -- and a bare value IS secret material, so the
/// text before its first colon is a key id, not a scheme.
#[test]
fn unrecognized_credential_ref_publishes_none_of_its_bytes() {
    for pasted in [
        "AKIAEXAMPLE:wJalrXUtnFEMI",
        "sk-live-abc:def",
        "sk-live-no-colon-at-all",
        ":sk-live-leading",
    ] {
        assert_eq!(
            credential_ref_scheme(Some(pasted)),
            None,
            "`{pasted}` is not an allowlisted ref scheme; it must project nothing"
        );
    }
    // The allowlisted four still project, case-insensitively.
    for (raw, expected) in [
        ("env://VAR", "env"),
        ("FILE:///etc/k", "file"),
        ("oauth://anthropic", "oauth"),
        ("literal:k", "literal"),
    ] {
        assert_eq!(credential_ref_scheme(Some(raw)).as_deref(), Some(expected));
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
    let view = derive_effective_view(&config, &CatalogOverlay::default());
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

    let view = derive_effective_view(&config, &CatalogOverlay::default());
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
    let view = derive_effective_view(&config, &CatalogOverlay::default());
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

    let view = derive_effective_view(&limited, &CatalogOverlay::default());
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

    let view = derive_effective_view(&config, &CatalogOverlay::default());
    let ids: Vec<&str> = view.providers.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["alpha", "zeta"]);
}

#[test]
fn capability_layer_empty_when_no_overrides() {
    // A config with no capability overrides yields an empty capability
    // layer -- rendered empty, never an error.
    let config = config_with_opus_model();
    let view = derive_effective_view(&config, &CatalogOverlay::default());
    assert!(view.capabilities.is_empty());
}

#[test]
fn derivation_source_is_pure_no_router_build() {
    // Structural guard: this module never calls into the secret-resolving /
    // provider-constructing build path. The pure `(&Config, &CatalogOverlay)`
    // signature is the real invariant; this scan tripwires an accidental
    // reintroduction. The tests live in a sidecar, so the whole file is
    // production code.
    let production = include_str!("config_effective.rs");

    assert!(!production.contains("build_resolved_models"));
    assert!(!production.contains("build_provider"));
}
