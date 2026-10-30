//! Calibration lane identity across the Router: one lane per
//! `(provider_kind, wire upstream)`, shared by every nickname and every seat
//! that sends that upstream, read the same way by the window gate and the
//! opening lookup, and admitted by one predicate on the warm rebuild and the
//! hot-reload carry-over.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::SystemTime;

use futures::stream::BoxStream;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Message, MessageContent, Provider, Result, Role,
};

use super::super::{DispatchMeta, DispatchTarget, Router};
use crate::calibration::{CalibrationLedgerReader, CalibrationLedgerRow};
use crate::catalog::{CatalogRow, EffectiveRow, Source};
use crate::config::{AliasValue, Config, CredentialSource, PoolEntry, ProviderEntry};
use crate::resolved::ResolvedModel;
use crate::seat_pool::SeatTarget;

struct StubProvider;

#[async_trait::async_trait]
impl Provider for StubProvider {
    fn id(&self) -> &'static str {
        "stub"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(routectl_core::Error::normalize_response("stub", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        unreachable!("calibration lane tests never dispatch")
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        unreachable!("calibration lane tests never dispatch")
    }
}

const ANTHROPIC: &str = "anthropic-api";
const OPENAI: &str = "openai-compat";
const OPUS: &str = "claude-opus-5-5";

fn stub() -> Arc<dyn Provider> {
    Arc::new(StubProvider)
}

fn own_anthropic() -> ProviderEntry {
    ProviderEntry::anthropic_api("env://K")
}

fn forwarded_anthropic() -> ProviderEntry {
    ProviderEntry::anthropic_api("").with_credential_source(CredentialSource::Forwarded)
}

fn own_compat() -> ProviderEntry {
    ProviderEntry::openai_compat("https://example.invalid/v1", "env://K")
}

/// A resolved model whose catalog row confirms `window` tokens, or carries no
/// window when `None`.
fn model(nickname: &str, provider: &str, upstream: &str, window: Option<u32>) -> ResolvedModel {
    let mut row = CatalogRow::sentinel();
    row.max_context_tokens = window;
    ResolvedModel::new(nickname, provider, stub(), upstream).with_effective_row(
        EffectiveRow::Present {
            row,
            source: Source::Baked,
            verified_at: "seed".to_string(),
        },
    )
}

/// A two-seat pool model: the model's `provider` names the pool, which is not
/// a `[providers]` entry, and each seat names its own member entry.
fn pooled(nickname: &str, pool: &str, members: &[&str], upstream: &str) -> ResolvedModel {
    let seats: Arc<[SeatTarget]> = members
        .iter()
        .map(|member| SeatTarget::new((*member).to_string(), stub(), None, false))
        .collect::<Vec<_>>()
        .into();
    model(nickname, pool, upstream, None).with_seats(seats)
}

fn router(config: Config, models: Vec<ResolvedModel>) -> Router {
    let mut router = Router::new(Arc::new(config));
    let table: BTreeMap<String, Arc<ResolvedModel>> = models
        .into_iter()
        .map(|m| (m.nickname.clone(), Arc::new(m)))
        .collect();
    router.install_resolved_models(table);
    router
}

fn config_with(providers: &[(&str, ProviderEntry)]) -> Config {
    let mut config = Config::default();
    for (name, entry) in providers {
        config.providers.insert((*name).to_string(), entry.clone());
    }
    config
}

/// A request large enough that the windows derived from its estimate sit far
/// from the estimator's own granularity.
fn request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("calibration-lane-fixture ".repeat(2_000)),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        ..Default::default()
    }
}

/// A window the raw estimate fits inside the gate's margin and an estimate
/// corrected by 2x clearly overflows.
fn tight_window(raw: u64) -> u32 {
    u32::try_from(raw * 4 / 3 + 4).expect("fixture estimate fits u32")
}

fn roomy_window(raw: u64) -> u32 {
    u32::try_from(raw * 8).expect("fixture estimate fits u32")
}

/// Record what a served dispatch of `target` for `requested` would record:
/// the provider kind and wire upstream the dispatch metadata carries, at a
/// 2x under-count, from three cohorts.
fn record_served(router: &Router, target: &DispatchTarget, requested: &str, raw: u64) {
    let mut meta = DispatchMeta::for_alias(requested);
    meta.mark_target(target, requested);
    for i in 0..9 {
        router.record_calibration_sample(
            meta.served_provider_kind.as_deref(),
            meta.served_upstream.as_deref(),
            Some(&format!("caller-{}", i % 3)),
            raw,
            raw * 2,
            SystemTime::now(),
        );
    }
}

fn feed(router: &Router, kind: &str, upstream: &str) {
    for i in 0..9 {
        router.record_calibration_sample(
            Some(kind),
            Some(upstream),
            Some(&format!("caller-{}", i % 3)),
            1_000,
            2_000,
            SystemTime::now(),
        );
    }
}

fn head(router: &Router, model: &str) -> DispatchTarget {
    router
        .dispatch_chain(model, None)
        .expect("chain resolves")
        .swap_remove(0)
}

fn kept_nicknames(chain: &[DispatchTarget]) -> Vec<String> {
    chain
        .iter()
        .map(|t| t.nickname.clone().unwrap_or_default())
        .collect()
}

fn has_lane(router: &Router, kind: &str, upstream: &str) -> bool {
    router
        .calibration_lanes()
        .contains(&(kind.to_string(), upstream.to_string()))
}

// ------------------------------------------------------------ shared lane

#[test]
fn two_nicknames_on_one_upstream_share_one_lane() {
    // Arrange: a direct and a passthrough entry, both anthropic-api, each
    // serving the same upstream under its own nickname.
    let req = request("pt-opus-5-5");
    let raw = crate::context_trim::estimate_total_tokens(&req);
    let mut config = config_with(&[
        ("anthropic-direct", own_anthropic()),
        ("passthrough", own_anthropic()),
    ]);
    config.window_gate.enabled = true;
    let router = router(
        config,
        vec![
            model("anthropic-opus-5-5", "anthropic-direct", OPUS, None),
            model("pt-opus-5-5", "passthrough", OPUS, Some(tight_window(raw))),
            model(
                "roomy",
                "anthropic-direct",
                "claude-roomy",
                Some(roomy_window(raw)),
            ),
        ],
    );
    let gate_chain = || vec![head(&router, "pt-opus-5-5"), head(&router, "roomy")];
    let before = router.filter_chain_by_window(gate_chain(), &req);
    assert_eq!(
        kept_nicknames(&before),
        vec!["pt-opus-5-5", "roomy"],
        "control: the raw estimate fits the passthrough window"
    );

    // Act: evidence recorded by serves through the DIRECT nickname only.
    record_served(
        &router,
        &head(&router, "anthropic-opus-5-5"),
        "anthropic-opus-5-5",
        raw,
    );

    // Assert: the passthrough nickname's gate and opening lookup both read it.
    let kept = router.filter_chain_by_window(gate_chain(), &req);
    assert_eq!(kept_nicknames(&kept), vec!["roomy"]);
    let lane = router.opening_lane("pt-opus-5-5").expect("lane resolves");
    assert_eq!(
        router.calibrated_estimate(lane.provider_kind, &lane.upstream_model, raw),
        Some(raw * 2)
    );
    assert_eq!(router.calibration_lanes().len(), 1, "one lane, not two");
}

#[test]
fn a_sample_through_any_pool_seat_lands_in_the_lane_the_opening_lookup_reads() {
    // Arrange
    let config = {
        let mut config = config_with(&[("seat-a", own_anthropic()), ("seat-b", own_anthropic())]);
        config.pools.insert(
            "pool".into(),
            PoolEntry::new(vec!["seat-a".into(), "seat-b".into()]),
        );
        config
    };
    let router = router(
        config,
        vec![pooled("opus", "pool", &["seat-a", "seat-b"], OPUS)],
    );
    let chain = router.dispatch_chain("opus", None).expect("chain resolves");
    assert_eq!(chain.len(), 2, "premise: one target per seat");

    // Act: the SECOND seat serves.
    record_served(&router, &chain[1], "opus", 1_000);

    // Assert
    let lane = router.opening_lane("opus").expect("pooled lane resolves");
    assert_eq!(
        router.calibrated_estimate(lane.provider_kind, &lane.upstream_model, 1_000),
        Some(2_000)
    );
}

#[test]
fn a_forwarded_target_reads_and_writes_the_requested_model_at_every_site() {
    // Arrange: an unlisted client model routed through `default` to a
    // forwarded target whose configured upstream differs from it.
    let requested = "client-model-x";
    let req = request(requested);
    let raw = crate::context_trim::estimate_total_tokens(&req);
    let mut config = config_with(&[("fwd", forwarded_anthropic()), ("own", own_anthropic())]);
    config.window_gate.enabled = true;
    config
        .aliases
        .insert("default".into(), AliasValue::Single("fwd-opus".into()));
    let router = router(
        config,
        vec![
            model("fwd-opus", "fwd", "claude-config", Some(tight_window(raw))),
            model("roomy", "own", "claude-roomy", Some(roomy_window(raw))),
        ],
    );
    let fwd = head(&router, requested);

    // Act
    let mut meta = DispatchMeta::for_alias(requested);
    meta.mark_target(&fwd, requested);
    feed(&router, ANTHROPIC, requested);
    let kept = router.filter_chain_by_window(vec![fwd, head(&router, "roomy")], &req);
    let lane = router.opening_lane(requested).expect("lane resolves");

    // Assert: the write, the gate and the opening lookup all name the
    // requested model, never the configured upstream or the nickname.
    assert_eq!(meta.served_upstream.as_deref(), Some(requested));
    assert_eq!(kept_nicknames(&kept), vec!["roomy"]);
    assert_eq!(lane.upstream_model, requested);
}

#[test]
fn the_same_upstream_under_two_provider_kinds_is_two_lanes() {
    // Arrange
    let router = router(
        config_with(&[("claude", own_anthropic()), ("compat", own_compat())]),
        vec![
            model("via-anthropic", "claude", OPUS, None),
            model("via-compat", "compat", OPUS, None),
        ],
    );

    // Act
    feed(&router, ANTHROPIC, OPUS);

    // Assert
    assert_eq!(
        router.calibrated_estimate(ANTHROPIC, OPUS, 1_000),
        Some(2_000)
    );
    assert_eq!(router.calibrated_estimate(OPENAI, OPUS, 1_000), None);
}

// ------------------------------------------------------------- carry-over

#[test]
fn a_reload_renaming_a_nickname_keeps_its_lane() {
    // Arrange
    let config = config_with(&[("claude", own_anthropic())]);
    let before = router(
        config.clone(),
        vec![model("anthropic-opus-5-5", "claude", OPUS, None)],
    );
    feed(&before, ANTHROPIC, OPUS);
    let mut after = router(config, vec![model("opus-renamed", "claude", OPUS, None)]);

    // Act
    after.carry_over_calibration_from(&before);

    // Assert
    assert_eq!(
        after.calibrated_estimate(ANTHROPIC, OPUS, 1_000),
        Some(2_000)
    );
}

#[test]
fn a_remapped_nickname_reads_the_cold_lane_and_an_unserved_upstream_is_pruned() {
    // Arrange: nickname X moves from u1 to u2. In the control a second
    // nickname keeps serving u1, so its lane must survive.
    let config = config_with(&[("claude", own_anthropic())]);
    let cases: [(&str, Vec<ResolvedModel>, bool); 2] = [
        (
            "nothing serves u1",
            vec![model("x", "claude", "u2", None)],
            false,
        ),
        (
            "y still serves u1",
            vec![
                model("x", "claude", "u2", None),
                model("y", "claude", "u1", None),
            ],
            true,
        ),
    ];
    for (case, models, u1_survives) in cases {
        let before = router(config.clone(), vec![model("x", "claude", "u1", None)]);
        feed(&before, ANTHROPIC, "u1");
        let mut after = router(config.clone(), models);

        // Act
        after.carry_over_calibration_from(&before);

        // Assert
        let lane = after.opening_lane("x").expect("x resolves");
        assert_eq!(lane.upstream_model, "u2", "{case}");
        assert_eq!(
            after.calibrated_estimate(lane.provider_kind, &lane.upstream_model, 1_000),
            None,
            "{case}: the remapped nickname reads the cold u2 lane"
        );
        assert_eq!(has_lane(&after, ANTHROPIC, "u1"), u1_survives, "{case}");
    }
}

#[test]
fn a_forwarded_lane_does_not_survive_a_reload_and_an_own_lane_does() {
    // The client requests a model equal to the target's configured upstream,
    // so only the credential source tells the two rows apart.
    let cases = [
        ("own credential", own_anthropic(), true),
        ("forwarded credential", forwarded_anthropic(), false),
    ];
    for (case, entry, survives) in cases {
        // Arrange
        let config = config_with(&[("entry", entry)]);
        let models = || vec![model(OPUS, "entry", OPUS, None)];
        let before = router(config.clone(), models());
        record_served(&before, &head(&before, OPUS), OPUS, 1_000);
        assert!(
            has_lane(&before, ANTHROPIC, OPUS),
            "{case}: premise, recorded live"
        );
        let mut after = router(config, models());

        // Act
        after.carry_over_calibration_from(&before);

        // Assert
        assert_eq!(has_lane(&after, ANTHROPIC, OPUS), survives, "{case}");
    }
}

#[test]
fn a_pooled_lane_survives_a_reload() {
    // Arrange
    let config = {
        let mut config = config_with(&[("seat-a", own_anthropic()), ("seat-b", own_anthropic())]);
        config.pools.insert(
            "pool".into(),
            PoolEntry::new(vec!["seat-a".into(), "seat-b".into()]),
        );
        config
    };
    let models = || vec![pooled("opus", "pool", &["seat-a", "seat-b"], OPUS)];
    let before = router(config.clone(), models());
    feed(&before, ANTHROPIC, OPUS);
    let mut after = router(config, models());

    // Act
    after.carry_over_calibration_from(&before);

    // Assert
    assert_eq!(
        after.calibrated_estimate(ANTHROPIC, OPUS, 1_000),
        Some(2_000)
    );
}

// ------------------------------------------------------------ warm rebuild

struct RowsReader(Vec<CalibrationLedgerRow>);

impl CalibrationLedgerReader for RowsReader {
    fn read_calibration_samples(&self, _: SystemTime, _: usize) -> Vec<CalibrationLedgerRow> {
        self.0.clone()
    }
}

#[test]
fn the_warm_rebuild_admits_exactly_the_lanes_an_own_credential_target_serves() {
    // Arrange: one resolved table covering a standalone own-credential model,
    // a pooled one, an openai-compat one, and a forwarded one whose nickname
    // and upstream equal the model a client would request.
    let mut config = config_with(&[
        ("direct", own_anthropic()),
        ("seat-a", own_anthropic()),
        ("seat-b", own_anthropic()),
        ("compat", own_compat()),
        ("fwd", forwarded_anthropic()),
    ]);
    config.pools.insert(
        "pool".into(),
        PoolEntry::new(vec!["seat-a".into(), "seat-b".into()]),
    );
    let router = router(
        config,
        vec![
            model("anthropic-opus-5-5", "direct", OPUS, None),
            pooled("sonnet", "pool", &["seat-a", "seat-b"], "claude-sonnet-5"),
            model("glm", "compat", "glm-4.6", None),
            model("claude-fwd", "fwd", "claude-fwd", None),
        ],
    );
    // (case, provider kind, upstream, admitted)
    let cases = [
        ("standalone own lane", ANTHROPIC, OPUS, true),
        ("pooled own lane", ANTHROPIC, "claude-sonnet-5", true),
        ("served upstream on another kind", OPENAI, OPUS, false),
        (
            "upstream nothing serves",
            ANTHROPIC,
            "claude-retired",
            false,
        ),
        ("forwarded lane", ANTHROPIC, "claude-fwd", false),
    ];
    let now = SystemTime::now();
    let rows: Vec<CalibrationLedgerRow> = cases
        .iter()
        .flat_map(|(_, kind, upstream, _)| {
            (0..9).map(move |i| {
                CalibrationLedgerRow::new(
                    now,
                    (*kind).to_string(),
                    (*upstream).to_string(),
                    Some(format!("caller-{}", i % 3)),
                    1_000,
                    1_500,
                )
            })
        })
        .collect();

    // Act
    let summary = router.rebuild_calibration_from_ledger(&RowsReader(rows), now, 1_000);

    // Assert
    for (case, kind, upstream, admitted) in cases {
        assert_eq!(has_lane(&router, kind, upstream), admitted, "{case}");
    }
    let lanes: BTreeSet<(String, String)> = router.calibration_lanes().into_iter().collect();
    assert_eq!(lanes.len(), 2);
    assert_eq!(summary.accepted, 18);
    assert_eq!(summary.lanes_calibrated, 2);
}
