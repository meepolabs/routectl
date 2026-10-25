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
/// `auth_kind` omitted: the chatgpt-oauth default.
#[cfg(feature = "openai-responses")]
const RESPONSES_CODEX: &str =
    "kind = \"openai-responses\"\napi_key_ref = \"literal:k\"\naccount_id_ref = \"literal:a\"\n";
#[cfg(feature = "openai-responses")]
const RESPONSES_LOOPBACK: &str = "kind = \"openai-responses\"\napi_key_ref = \"literal:k\"\n\
     auth_kind = \"api-key\"\nbase_url = \"http://127.0.0.1:18080/v1\"\n";
#[cfg(all(feature = "openai-responses", feature = "bedrock"))]
const RESPONSES_MANTLE: &str = "kind = \"openai-responses\"\napi_key_ref = \"\"\n\
     bedrock_mantle = { region = \"us-east-1\", creds = { kind = \"default-chain\" } }\n";
#[cfg(feature = "bedrock")]
const ANTHROPIC_MANTLE_BAD_REGION: &str = "kind = \"anthropic-api\"\napi_key_ref = \"\"\n\
     bedrock_mantle = { region = \"not-a-region\", creds = { kind = \"default-chain\" } }\n";
#[cfg(feature = "bedrock")]
const COMPAT_MANTLE_BAD_REGION: &str = "kind = \"openai-compat\"\napi_key_ref = \"\"\n\
     bedrock_mantle = { region = \"not-a-region\", creds = { kind = \"default-chain\" } }\n";
#[cfg(all(feature = "openai-responses", feature = "bedrock"))]
const RESPONSES_MANTLE_BAD_REGION: &str = "kind = \"openai-responses\"\napi_key_ref = \"\"\n\
     bedrock_mantle = { region = \"not-a-region\", creds = { kind = \"default-chain\" } }\n";
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
            expect: Some((
                "anthropic-api",
                "https://bedrock-mantle.us-west-2.api.aws/anthropic",
            )),
        },
        Row {
            name: "anthropic-api, Mantle sub-lane in a region that names no endpoint",
            provider_toml: Some(ANTHROPIC_MANTLE_BAD_REGION),
            forwarded: false,
            expect: None,
        },
        Row {
            name: "openai-compat, Mantle sub-lane",
            provider_toml: Some(COMPAT_MANTLE),
            forwarded: false,
            expect: Some((
                "openai-compat",
                "https://bedrock-mantle.us-west-2.api.aws/openai/v1",
            )),
        },
        Row {
            name: "openai-compat, Mantle sub-lane in a region that names no endpoint",
            provider_toml: Some(COMPAT_MANTLE_BAD_REGION),
            forwarded: false,
            expect: None,
        },
    ]);
    #[cfg(feature = "openai-responses")]
    rows.extend([
        Row {
            name: "openai-responses api-key, own credential",
            provider_toml: Some(RESPONSES_REMOTE),
            forwarded: false,
            expect: Some(("openai-responses", "https://api.openai.com/v1")),
        },
        Row {
            name: "openai-responses chatgpt-oauth, own credential",
            provider_toml: Some(RESPONSES_CODEX),
            forwarded: false,
            expect: Some(("openai-responses", "https://chatgpt.com/backend-api/codex")),
        },
        Row {
            name: "openai-responses, forwarded",
            provider_toml: Some(RESPONSES_REMOTE),
            forwarded: true,
            expect: None,
        },
        Row {
            name: "openai-responses, loopback",
            provider_toml: Some(RESPONSES_LOOPBACK),
            forwarded: false,
            expect: None,
        },
    ]);
    #[cfg(all(feature = "openai-responses", feature = "bedrock"))]
    rows.extend([
        Row {
            name: "openai-responses, Mantle sub-lane",
            provider_toml: Some(RESPONSES_MANTLE),
            forwarded: false,
            expect: Some((
                "openai-responses",
                "https://bedrock-mantle.us-east-1.api.aws/openai/v1",
            )),
        },
        Row {
            name: "openai-responses, Mantle sub-lane in a region that names no endpoint",
            provider_toml: Some(RESPONSES_MANTLE_BAD_REGION),
            forwarded: false,
            expect: None,
        },
    ]);
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

/// Mutation check: drop the anthropic Mantle arm of
/// `classifiable_egress_base_url` -> red on the anthropic Mantle row; replace
/// a Mantle arm's body with
/// `Some(Cow::Owned(mantle_anthropic_base(&m.region).unwrap_or_else(|_|
/// "https://bedrock-mantle.invalid".into())))` (that arm's own base function)
/// -> red on that sub-lane's bad-region row.
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
                 ..
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
        #[cfg_attr(
            not(any(feature = "bedrock", feature = "openai-responses")),
            allow(unused_mut)
        )]
        let mut premises = vec![
            (ANTHROPIC_LOOPBACK, "anthropic-api"),
            (COMPAT_LOOPBACK, "openai-compat"),
            (COMPAT_LOOPBACK_V4, "openai-compat"),
            (COMPAT_LOOPBACK_V6, "openai-compat"),
            (COMPAT_UNSPECIFIED, "openai-compat"),
        ];
        #[cfg(feature = "bedrock")]
        premises.extend([
            (ANTHROPIC_MANTLE_BAD_REGION, "anthropic-api"),
            (COMPAT_MANTLE_BAD_REGION, "openai-compat"),
            (BEDROCK_UNADMITTED_REGION, "bedrock"),
        ]);
        #[cfg(feature = "openai-responses")]
        premises.push((RESPONSES_LOOPBACK, "openai-responses"));
        #[cfg(all(feature = "openai-responses", feature = "bedrock"))]
        premises.push((RESPONSES_MANTLE_BAD_REGION, "openai-responses"));
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
/// Mutation check: drop the `matches_target` check in `plan_field_carry` ->
/// the reactive assertion reds (a plan is produced); in
/// `Router::preflight_lane_disagreement` -> the pre-flight assertion reds (the
/// planner reaches the verdict read per row and reports `not_eligible`).
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
    assert_eq!(
        preflight_decisions(&records),
        vec![(
            false,
            None,
            super::super::field_preflight::FIELD_PREFLIGHT_UNATTRIBUTABLE_TARGET
        )],
        "the pre-flight planner refuses the whole target on attribution, before \
         any row or verdict is read",
    );
}

/// Each pre-flight record as `(acted, field_path, reason)`.
fn preflight_decisions(
    records: &[super::super::FieldPreflight],
) -> Vec<(bool, Option<&'static str>, &'static str)> {
    records
        .iter()
        .map(|record| (record.acted, record.field_path, record.reason))
        .collect()
}

/// A request carrying the grounded effort row at `high`, beside the grounded
/// fields of [`grounded_request`].
#[cfg(feature = "bedrock")]
fn effort_request() -> routectl_core::ChatRequest {
    let mut req = grounded_request();
    req.reasoning = Some(routectl_core::ReasoningConfig {
        effort: Some("high".into()),
        ..Default::default()
    });
    req
}

/// A target expanded from a Mantle entry, read after a reload repointed that
/// entry to its direct sibling under the same name, and the converse (a direct
/// target on an entry now Mantle): the kind still agrees, the egress does not,
/// so no stage may apply the entry's scope or verdicts to the target. The
/// clear runs over a resident verdict on the sent level, so it would remove
/// one if the check were absent.
///
/// Mutation check: compare only the kind in place of `matches_target` in
/// `plan_field_carry` -> red on the Mantle-target row (a plan); in
/// `Router::preflight_lane_disagreement` -> red on both rows (`not_eligible`
/// per row on the Mantle-target row, `no_grounded_field` on the
/// direct-target row); in `clear_accepted_effort` -> red on the Mantle-target
/// row (a cleared event).
#[cfg(feature = "bedrock")]
#[test]
fn a_mantle_target_on_an_entry_repointed_to_its_direct_sibling_plans_nothing() {
    use routectl_core::wire_check::EgressKind;
    let cases = [
        (
            "Mantle target, entry now direct",
            ANTHROPIC_MANTLE,
            EgressKind::AnthropicApiMantle,
            ANTHROPIC_REMOTE,
        ),
        (
            "direct target, entry now Mantle",
            ANTHROPIC_REMOTE,
            EgressKind::AnthropicApi,
            ANTHROPIC_MANTLE,
        ),
    ];
    for (name, expanded_from, expanded_egress, current) in cases {
        a_target_on_a_repointed_entry_plans_nothing(name, expanded_from, expanded_egress, current);
    }
}

/// One row of [`a_mantle_target_on_an_entry_repointed_to_its_direct_sibling_plans_nothing`].
#[cfg(feature = "bedrock")]
fn a_target_on_a_repointed_entry_plans_nothing(
    name: &str,
    expanded_from: &str,
    expanded_egress: routectl_core::wire_check::EgressKind,
    current: &str,
) {
    // Arrange
    let stale_target = target_on(&router_on(expanded_from));
    assert_eq!(
        stale_target.provider_kind,
        Some("anthropic-api"),
        "{name}: premise"
    );
    assert_eq!(
        stale_target.egress_kind,
        Some(expanded_egress),
        "{name}: premise"
    );
    let router = router_on(current);
    assert!(
        attributable_field_lane(&router.config, PROVIDER, false).is_some(),
        "{name}: premise: the current entry is itself attributable",
    );
    let lane = stale_target.learned_lane.clone().expect("a lane");
    super::super::probe_test_support::plant_acting_negative(
        &router,
        &lane,
        "output_config.effort.high",
        "anthropic-api",
    );
    let req = effort_request();

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
    let cleared = router.clear_accepted_effort(&stale_target, &req, &[]);

    // Assert
    assert!(
        reactive.is_none(),
        "{name}: the reactive arm admits no plan"
    );
    assert_eq!(
        preflight_decisions(&records),
        vec![(
            false,
            None,
            super::super::field_preflight::FIELD_PREFLIGHT_UNATTRIBUTABLE_TARGET
        )],
        "{name}: the pre-flight refuses the whole target on attribution",
    );
    assert!(
        cleared.is_none(),
        "{name}: the ranked clear removes nothing"
    );
}

/// A Mantle entry whose region names no endpoint is unattributable and plans
/// nothing, while the same entry in a canonical region is attributable: the
/// refusal is the region's, not the sub-lane's.
///
/// The `lane.is_none()` assertion carries the mutation below. The plan
/// assertion holds either way while the sub-lane's allowlist is empty, and
/// becomes discriminating once a row is admitted there.
///
/// Mutation check: replace the anthropic Mantle arm's body with
/// `Some(Cow::Owned(mantle_anthropic_base(&m.region).unwrap_or_else(|_|
/// "https://bedrock-mantle.invalid".into())))` -> red on the attribution
/// assertion.
#[cfg(feature = "bedrock")]
#[test]
fn a_mantle_entry_in_a_region_that_names_no_endpoint_plans_nothing() {
    // Arrange
    let router = router_on(ANTHROPIC_MANTLE_BAD_REGION);
    let target = target_on(&router);
    assert!(
        attributable_field_lane(&router_on(ANTHROPIC_MANTLE).config, PROVIDER, false).is_some(),
        "control: the canonical region is attributable",
    );

    // Act
    let lane = attributable_field_lane(&router.config, PROVIDER, false);
    let plan = router.plan_field_carry(
        &target,
        &grounded_request(),
        super::FieldSettlementMode::Settling,
        std::time::Instant::now(),
    );

    // Assert
    assert!(lane.is_none(), "no host, no attribution");
    assert!(plan.is_none(), "and nothing is planned");
}

/// A target that names its provider kind but no egress resolves no row and
/// plans nothing, on an entry whose own lane would plan: the scope cannot tell
/// which sub-lane the target sends with, so it fails closed.
///
/// Mutation check: drop the egress comparison from
/// `AttributableLane::matches_target` -> red (a plan, and pre-flight records
/// past attribution).
#[test]
fn a_target_with_a_kind_and_no_egress_plans_nothing() {
    // Arrange
    let router = router_on(ANTHROPIC_REMOTE);
    let mut target = target_on(&router);
    let req = grounded_request();
    let plan = |target: &super::super::DispatchTarget| {
        router.plan_field_carry(
            target,
            &req,
            super::FieldSettlementMode::NonSettling,
            std::time::Instant::now(),
        )
    };
    assert!(
        plan(&target).is_some(),
        "control: the egress-bearing target plans"
    );
    target.egress_kind = None;
    assert_eq!(target.provider_kind, Some("anthropic-api"), "premise");

    // Act
    let reactive = plan(&target);
    let (_planned, records, _plan) = router.plan_field_preflight(
        &req,
        &target,
        super::super::class_observe::DispatchSurface::Complete,
    );

    // Assert
    assert!(reactive.is_none(), "the reactive arm plans nothing");
    assert!(
        !records.is_empty(),
        "premise: the planner recorded a decision"
    );
    assert!(
        records.iter().all(|record| !record.acted
            && record.reason
                == super::super::field_preflight::FIELD_PREFLIGHT_UNATTRIBUTABLE_TARGET),
        "the pre-flight acts on nothing: {:?}",
        records.iter().map(|r| r.reason).collect::<Vec<_>>(),
    );
}

/// An attributable Mantle anthropic lane on a claude upstream is
/// probe-eligible, which stays inert only while no grounded row is
/// count-validated: a probe captures count-validated rows alone. A grounded
/// row turning count-validated makes Mantle probes live, and this pin is
/// where that surfaces.
///
/// Mutation check: set `count_tokens_validates: true` on one grounded row ->
/// red on the second assertion.
#[cfg(feature = "bedrock")]
#[test]
fn a_mantle_anthropic_lane_is_probe_eligible_and_no_grounded_row_is_count_validated() {
    let router = router_on(ANTHROPIC_MANTLE);

    let kind = router.probe_lane_kind(PROVIDER, false, "claude-sonnet-4-5");

    assert_eq!(kind, Some("anthropic-api"));
    let validated: Vec<&str> = super::FieldRepairTable::grounded_only()
        .rows()
        .iter()
        .filter(|row| row.count_tokens_validates)
        .map(|row| row.path)
        .collect();
    assert!(validated.is_empty(), "{validated:?}");
}
