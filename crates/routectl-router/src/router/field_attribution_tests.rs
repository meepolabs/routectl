//! The per-kind attributability decision every field stage shares, driven over
//! each kind field repair admits and each refusal it owns.

use super::{AttributableLane, attributable_field_lane};
use crate::config::Config;

const PROVIDER: &str = "p0";

const ANTHROPIC_REMOTE: &str = "kind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\n";
const ANTHROPIC_LOOPBACK: &str = "kind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\nbase_url = \"http://127.0.0.1:18080\"\n";
const COMPAT_REMOTE: &str = "kind = \"openai-compat\"\nbase_url = \"https://example.test/v1\"\napi_key_ref = \"literal:k\"\n";
const COMPAT_LOOPBACK: &str = "kind = \"openai-compat\"\nbase_url = \"http://localhost:11434/v1\"\napi_key_ref = \"literal:k\"\n";
const COMPAT_LOOPBACK_V4: &str = "kind = \"openai-compat\"\nbase_url = \"http://127.0.0.1:8000/v1\"\napi_key_ref = \"literal:k\"\n";
const COMPAT_LOOPBACK_V6: &str =
    "kind = \"openai-compat\"\nbase_url = \"http://[::1]:8000/v1\"\napi_key_ref = \"literal:k\"\n";
const COMPAT_UNSPECIFIED: &str = "kind = \"openai-compat\"\nbase_url = \"http://0.0.0.0:8000/v1\"\napi_key_ref = \"literal:k\"\n";
#[cfg(feature = "bedrock")]
const BEDROCK_NATIVE: &str =
    "kind = \"bedrock\"\nregion = \"us-west-2\"\ncreds = { kind = \"default-chain\" }\n";
#[cfg(feature = "bedrock")]
const BEDROCK_UNADMITTED_REGION: &str =
    "kind = \"bedrock\"\nregion = \"not-a-region\"\ncreds = { kind = \"default-chain\" }\n";
#[cfg(feature = "bedrock")]
const ANTHROPIC_MANTLE: &str = "kind = \"anthropic-api\"\napi_key_ref = \"\"\n\
     bedrock_mantle = { region = \"us-west-2\", creds = { kind = \"default-chain\" } }\n";
#[cfg(feature = "bedrock")]
const COMPAT_MANTLE: &str = "kind = \"openai-compat\"\napi_key_ref = \"\"\n\
     bedrock_mantle = { region = \"us-west-2\", creds = { kind = \"default-chain\" } }\n";
#[cfg(feature = "openai-responses")]
const RESPONSES_REMOTE: &str =
    "kind = \"openai-responses\"\napi_key_ref = \"literal:k\"\nauth_kind = \"api-key\"\n";
#[cfg(feature = "gemini")]
const GEMINI_REMOTE: &str = "kind = \"gemini\"\napi_key_ref = \"literal:k\"\n";

/// One row: the `[providers.p0]` body (`None` = no entry), the caller's
/// forwarded fact, and the expected `(kind, base_url)` or refusal.
struct Row {
    name: &'static str,
    provider_toml: Option<&'static str>,
    forwarded: bool,
    expect: Option<(&'static str, &'static str)>,
}

fn rows() -> Vec<Row> {
    #[cfg_attr(
        not(any(feature = "bedrock", feature = "openai-responses", feature = "gemini")),
        allow(unused_mut)
    )]
    let mut rows = vec![
        Row {
            name: "anthropic-api, own credential",
            provider_toml: Some(ANTHROPIC_REMOTE),
            forwarded: false,
            expect: Some(("anthropic-api", "https://api.anthropic.com")),
        },
        Row {
            name: "anthropic-api, forwarded",
            provider_toml: Some(ANTHROPIC_REMOTE),
            forwarded: true,
            expect: None,
        },
        Row {
            name: "anthropic-api, loopback base",
            provider_toml: Some(ANTHROPIC_LOOPBACK),
            forwarded: false,
            expect: None,
        },
        Row {
            name: "openai-compat, own credential",
            provider_toml: Some(COMPAT_REMOTE),
            forwarded: false,
            expect: Some(("openai-compat", "https://example.test/v1")),
        },
        Row {
            name: "openai-compat, forwarded",
            provider_toml: Some(COMPAT_REMOTE),
            forwarded: true,
            expect: None,
        },
        Row {
            name: "openai-compat, loopback base",
            provider_toml: Some(COMPAT_LOOPBACK),
            forwarded: false,
            expect: None,
        },
        Row {
            name: "openai-compat, IPv4 loopback literal",
            provider_toml: Some(COMPAT_LOOPBACK_V4),
            forwarded: false,
            expect: None,
        },
        Row {
            name: "openai-compat, IPv6 loopback literal",
            provider_toml: Some(COMPAT_LOOPBACK_V6),
            forwarded: false,
            expect: None,
        },
        Row {
            name: "openai-compat, unspecified address",
            provider_toml: Some(COMPAT_UNSPECIFIED),
            forwarded: false,
            expect: None,
        },
        Row {
            name: "missing entry",
            provider_toml: None,
            forwarded: false,
            expect: None,
        },
    ];
    #[cfg(feature = "bedrock")]
    rows.extend([
        Row {
            name: "bedrock, own credential",
            provider_toml: Some(BEDROCK_NATIVE),
            forwarded: false,
            expect: Some(("bedrock", "https://bedrock-runtime.us-west-2.amazonaws.com")),
        },
        Row {
            name: "bedrock, forwarded",
            provider_toml: Some(BEDROCK_NATIVE),
            forwarded: true,
            expect: None,
        },
        Row {
            name: "bedrock, a region that names no endpoint",
            provider_toml: Some(BEDROCK_UNADMITTED_REGION),
            forwarded: false,
            expect: None,
        },
        Row {
            name: "anthropic-api, Mantle sub-lane",
            provider_toml: Some(ANTHROPIC_MANTLE),
            forwarded: false,
            expect: None,
        },
        Row {
            name: "openai-compat, Mantle sub-lane",
            provider_toml: Some(COMPAT_MANTLE),
            forwarded: false,
            expect: None,
        },
    ]);
    #[cfg(feature = "openai-responses")]
    rows.push(Row {
        name: "openai-responses, own credential",
        provider_toml: Some(RESPONSES_REMOTE),
        forwarded: false,
        expect: None,
    });
    #[cfg(feature = "gemini")]
    rows.push(Row {
        name: "gemini, own credential",
        provider_toml: Some(GEMINI_REMOTE),
        forwarded: false,
        expect: None,
    });
    rows
}

fn config_for(provider_toml: Option<&str>) -> Config {
    provider_toml.map_or_else(Config::default, |body| {
        toml::from_str(&format!("\n[providers.{PROVIDER}]\n{body}")).expect("valid test toml")
    })
}

#[test]
fn the_attributability_decision_matches_the_table_for_every_kind() {
    for row in rows() {
        // Arrange
        let config = config_for(row.provider_toml);

        // Act
        let verdict = attributable_field_lane(&config, PROVIDER, row.forwarded);

        // Assert
        let observed = verdict.as_ref().map(
            |AttributableLane {
                 provider_kind,
                 base_url,
             }| (*provider_kind, base_url.as_ref()),
        );
        assert_eq!(observed, row.expect, "{}", row.name);
    }
}

/// A refusing row is evidence only if its entry parsed into the kind it
/// names: a body that silently deserialized as another kind would pass the
/// table above for the wrong reason.
#[test]
fn each_refusing_row_parses_into_the_kind_it_names() {
    let premises: Vec<(&str, &str)> = {
        #[cfg_attr(not(feature = "bedrock"), allow(unused_mut))]
        let mut premises = vec![
            (ANTHROPIC_LOOPBACK, "anthropic-api"),
            (COMPAT_LOOPBACK, "openai-compat"),
            (COMPAT_LOOPBACK_V4, "openai-compat"),
            (COMPAT_LOOPBACK_V6, "openai-compat"),
            (COMPAT_UNSPECIFIED, "openai-compat"),
        ];
        #[cfg(feature = "bedrock")]
        premises.extend([
            (ANTHROPIC_MANTLE, "anthropic-api"),
            (COMPAT_MANTLE, "openai-compat"),
            (BEDROCK_UNADMITTED_REGION, "bedrock"),
        ]);
        premises
    };
    for (provider_toml, kind) in premises {
        let config = config_for(Some(provider_toml));
        let entry = config.providers.get(PROVIDER).expect("the entry parsed");
        assert_eq!(entry.kind_str(), kind, "premise: {provider_toml}");
    }
}

/// A provider the planning stages never dial.
struct UnusedProvider;

#[async_trait::async_trait]
impl routectl_core::Provider for UnusedProvider {
    fn id(&self) -> &'static str {
        "unused"
    }
    fn normalize_request(
        &self,
        _: &routectl_core::ChatRequest,
    ) -> routectl_core::Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(
        &self,
        _: serde_json::Value,
    ) -> routectl_core::Result<routectl_core::ChatResponse> {
        Err(routectl_core::Error::normalize_response("unused", "unused"))
    }
    async fn complete(
        &self,
        _: routectl_core::ChatRequest,
    ) -> routectl_core::Result<routectl_core::ChatResponse> {
        Err(routectl_core::Error::upstream("unused", 500, "unused"))
    }
    async fn stream(
        &self,
        _: routectl_core::ChatRequest,
    ) -> routectl_core::Result<
        futures::stream::BoxStream<'static, routectl_core::Result<routectl_core::ChatChunk>>,
    > {
        Err(routectl_core::Error::upstream("unused", 500, "unused"))
    }
}

/// A one-model router on `provider_toml`, with the model resolved so chain
/// expansion yields its target.
fn router_on(provider_toml: &str) -> super::super::Router {
    let mut router =
        super::super::Router::new(std::sync::Arc::new(config_for(Some(provider_toml))))
            .with_capability_writes_assumed_durable_for_tests();
    let provider: std::sync::Arc<dyn routectl_core::Provider> = std::sync::Arc::new(UnusedProvider);
    router.install_resolved_models(std::collections::BTreeMap::from([(
        "m0".to_string(),
        std::sync::Arc::new(crate::resolved::ResolvedModel::new(
            "m0", PROVIDER, provider, "wire-0",
        )),
    )]));
    router
}

/// The dispatch target for `m0`, through `router`'s own chain expansion.
fn target_on(router: &super::super::Router) -> super::super::DispatchTarget {
    let model = router
        .resolved_models
        .values()
        .next()
        .map(std::sync::Arc::clone)
        .expect("one model");
    router
        .expand_chain_to_targets(vec![model], None)
        .pop()
        .expect("one target")
}

/// A request carrying the grounded closed-table field.
fn grounded_request() -> routectl_core::ChatRequest {
    let mut req = routectl_core::ChatRequest {
        model: "m0".into(),
        max_tokens: Some(64),
        reasoning: Some(routectl_core::ReasoningConfig {
            enabled: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    req.routectl_internal.anthropic_thinking_display = Some("summarized".into());
    req
}

/// A target expanded while the entry was one admitted kind, read after a reload
/// re-kinded that entry to another admitted kind: both stages must refuse, since
/// the verdict key would carry a kind the entry no longer has.
///
/// Mutation check: remove the `lane.provider_kind != provider_kind` comparison
/// in `plan_field_carry` -> the reactive assertion reds (a plan is produced);
/// remove it in `preflight_identity` -> the pre-flight assertion reds (the
/// planner reaches the verdict read and reports `not_eligible`).
#[test]
fn a_target_of_an_old_kind_on_a_re_kinded_entry_plans_nothing() {
    // Arrange
    let stale_target = target_on(&router_on(ANTHROPIC_REMOTE));
    assert_eq!(stale_target.provider_kind, Some("anthropic-api"), "premise");
    let router = router_on(COMPAT_REMOTE);
    assert!(
        attributable_field_lane(&router.config, PROVIDER, false).is_some(),
        "premise: the current entry is itself attributable",
    );
    let req = grounded_request();

    // Act
    let reactive = router.plan_field_carry(
        &stale_target,
        &req,
        super::FieldSettlementMode::Settling,
        std::time::Instant::now(),
    );
    let (_planned, records, _plan) = router.plan_field_preflight(
        &req,
        &stale_target,
        super::super::class_observe::DispatchSurface::Complete,
    );

    // Assert
    assert!(reactive.is_none(), "the reactive arm admits no plan");
    let decisions: Vec<(bool, &str)> = records.iter().map(|r| (r.acted, r.reason)).collect();
    assert_eq!(
        decisions,
        vec![(
            false,
            super::super::field_preflight::FIELD_PREFLIGHT_UNATTRIBUTABLE_TARGET
        )],
        "the pre-flight planner refuses on attribution, before any verdict read",
    );
}
