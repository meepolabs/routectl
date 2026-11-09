//! Each request records the targets it passed over without an attempt, one
//! count per reason, so that per request
//! `skips + targets dialed == targets evaluated`, where a target is evaluated
//! when a request filter dropped it or the walk reached it. A target a filter
//! dropped counts under the filter's reason unless its own state names a
//! higher-priority one; the walk counts a target only when the gate refused
//! it before any attempt, never for a refusal on a same-target retry.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};
use routectl_core::{
    ChatRequest, EligibilityReason, Error, Message, MessageContent, Provider, RequestObservation,
    Role, SkipCounts, ToolDef, UpstreamReason,
};
use serde_json::json;

use super::super::credential_park_dispatch_tests::{ScriptedProvider, Step, Surface};
use super::super::{DispatchMeta, Router, RouterOptions};
use crate::config::{
    AliasValue, Config, OverrideEntry, ProviderEntry, ProviderRuntimePolicy, RetryPolicy,
};
use crate::resolved::ResolvedModel;
use crate::runtime_state::UnusableCause;

const NAMES: [&str; 5] = ["a", "b", "c", "d", "e"];
const PROVIDER_KIND: &str = "anthropic-api";
const SEARCH: &str = "web_search";

fn nickname(idx: usize) -> String {
    format!("m-{}", NAMES[idx])
}

fn provider_name(idx: usize) -> String {
    format!("p-{}", NAMES[idx])
}

fn rpm1_entry() -> ProviderEntry {
    ProviderEntry::anthropic_api("literal:k").with_runtime(ProviderRuntimePolicy {
        rpm_limit: Some(1),
        ..Default::default()
    })
}

/// A configured one-token window: every request overflows it.
const ONE_TOKEN_WINDOW: u32 = 1;

/// How one reconciliation row is arranged and what it must record.
struct Row {
    name: &'static str,
    surface: Surface,
    /// Chain length: targets `m-a` onward, each on its own provider.
    targets: usize,
    config: fn(&mut Config),
    /// Scripts for the targets that do not simply answer.
    scripts: &'static [(usize, &'static [Step])],
    /// Targets configured with a one-token context window.
    tiny_windows: &'static [usize],
    state: fn(&Router),
    tools: bool,
    skips: SkipCounts,
    dialed: usize,
    evaluated: u64,
    /// How the request ended: [`SERVED`] or the shape [`ended`] names.
    ends: &'static str,
}

const SERVED: &str = "served";

/// The request's end reduced to its variant and, for a local refusal, its
/// reason token; never a body.
fn ended(err: Option<&Error>) -> &'static str {
    match err {
        None => SERVED,
        Some(Error::NotImplemented(..)) => "not_implemented",
        Some(Error::LocallyUnavailable { reason, .. }) => reason.as_str(),
        Some(Error::CapabilityUnsupportedEverywhere { .. }) => "capability_unsupported_everywhere",
        Some(Error::Upstream { .. }) => "upstream",
        Some(_) => "other",
    }
}

struct Fixture {
    router: Router,
    providers: Vec<Arc<ScriptedProvider>>,
}

fn fixture(row: &Row) -> Fixture {
    let mut config = Config {
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 0,
            jitter_ms: 0,
            ..RetryPolicy::default()
        },
        ..Config::default()
    };
    for idx in 0..row.targets {
        config.providers.insert(
            provider_name(idx),
            ProviderEntry::anthropic_api("literal:k"),
        );
    }
    config.aliases.insert(
        "alias".into(),
        AliasValue::Chain((0..row.targets).map(nickname).collect()),
    );
    (row.config)(&mut config);
    let providers: Vec<Arc<ScriptedProvider>> = (0..row.targets)
        .map(|idx| {
            let script = row
                .scripts
                .iter()
                .find(|(target, _)| *target == idx)
                .map_or(&[Step::Ok][..], |(_, script)| *script);
            ScriptedProvider::new(&provider_name(idx), script)
        })
        .collect();
    let models: BTreeMap<String, Arc<ResolvedModel>> = providers
        .iter()
        .enumerate()
        .map(|(idx, scripted)| {
            let mut model = ResolvedModel::new(
                nickname(idx),
                provider_name(idx),
                Arc::clone(scripted) as Arc<dyn Provider>,
                format!("claude-{}", nickname(idx)),
            );
            if row.tiny_windows.contains(&idx) {
                model = model.with_context_window(ONE_TOKEN_WINDOW);
            }
            (nickname(idx), Arc::new(model))
        })
        .collect();
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    (row.state)(&router);
    Fixture { router, providers }
}

fn request(tools: bool, observation: &RequestObservation) -> ChatRequest {
    let mut req = ChatRequest {
        model: "alias".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("count these words please".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        ..Default::default()
    };
    if tools {
        req.tools = Some(vec![ToolDef::Other(json!({ "type": SEARCH }))]);
    }
    req.routectl_internal.observation = Some(observation.clone());
    req
}

/// Dispatch one request on `surface`, draining a stream, and return its meta
/// and error, if any.
async fn dispatch(
    router: &Router,
    surface: Surface,
    req: ChatRequest,
) -> (DispatchMeta, Option<Error>) {
    use futures::stream::StreamExt as _;
    match surface {
        Surface::Complete => {
            let out = router
                .complete_with_options(req, RouterOptions::new())
                .await;
            (out.meta, out.result.err())
        }
        Surface::Stream => {
            let out = router.stream_with_options(req, RouterOptions::new()).await;
            let err = match out.result {
                Ok(mut stream) => {
                    let mut err = None;
                    while let Some(item) = stream.next().await {
                        if let Err(e) = item {
                            err = Some(e);
                        }
                    }
                    err
                }
                Err(e) => Some(e),
            };
            (out.meta, err)
        }
        Surface::CountTokens => {
            let out = router.count_tokens_with_meta(req).await;
            (out.meta, out.result.err())
        }
    }
}

fn no_config(_: &mut Config) {}

fn no_state(_: &Router) {}

fn override_search_on_a(config: &mut Config) {
    config.capability.overrides.insert(
        provider_name(0),
        OverrideEntry {
            unsupported: vec![SEARCH.into()],
            force_supported: Vec::new(),
        },
    );
}

fn override_search_on_every_target(config: &mut Config) {
    for idx in 0..NAMES.len() {
        config.capability.overrides.insert(
            provider_name(idx),
            OverrideEntry {
                unsupported: vec![SEARCH.into()],
                force_supported: Vec::new(),
            },
        );
    }
}

fn enable_window_gate(config: &mut Config) {
    config.window_gate.enabled = true;
}

fn rpm1_on_d(config: &mut Config) {
    config.providers.insert(provider_name(3), rpm1_entry());
}

fn rpm1_on_a_with_one_retry(config: &mut Config) {
    config.providers.insert(provider_name(0), rpm1_entry());
    config.retry.max_attempts = 2;
}

fn rpm1_on_a(config: &mut Config) {
    config.providers.insert(provider_name(0), rpm1_entry());
}

fn one_retry(config: &mut Config) {
    config.retry.max_attempts = 2;
}

fn mark_unusable(router: &Router, idx: usize) {
    assert!(
        router.mark_credential_unusable_for_tests(&nickname(idx), UnusableCause::Revoked),
        "premise: {} has an account",
        nickname(idx),
    );
}

fn unusable_a(router: &Router) {
    mark_unusable(router, 0);
}

fn park(router: &Router, idx: usize) {
    assert!(
        router.park_credential_for_tests(
            &nickname(idx),
            UpstreamReason::CredentialExpired,
            Instant::now() + Duration::from_hours(1),
        ),
        "premise: {} has an account",
        nickname(idx),
    );
}

/// `m-a` unusable, `m-b` parked, `m-c` breaker open, `m-d` with its one
/// request of the minute already spent.
fn four_refusing_targets(router: &Router) {
    mark_unusable(router, 0);
    park(router, 1);
    assert!(router.force_open_breaker(&nickname(2), Duration::from_hours(1)));
    let (refusal, guard) = router.admit_dispatch(&nickname(3));
    assert!(refusal.is_none(), "premise: the bucket held one token");
    drop(guard);
}

fn breaker_open_on_a(router: &Router) {
    assert!(router.force_open_breaker(&nickname(0), Duration::from_hours(1)));
}

/// `m-a`'s one request of the minute already spent.
fn rpm_spent_on_a(router: &Router) {
    let (refusal, guard) = router.admit_dispatch(&nickname(0));
    assert!(refusal.is_none(), "premise: the bucket held one token");
    drop(guard);
}

fn unusable_a_parked_b(router: &Router) {
    mark_unusable(router, 0);
    park(router, 1);
}

const REJECT_503: &[Step] = &[Step::Reject(503, None)];
const REJECT_503_THEN_OK: &[Step] = &[Step::Reject(503, None), Step::Ok];

fn rows() -> Vec<Row> {
    vec![
        Row {
            name: "capability drop",
            surface: Surface::Complete,
            targets: 2,
            config: override_search_on_a,
            scripts: &[],
            tiny_windows: &[],
            state: no_state,
            tools: true,
            skips: SkipCounts {
                capability: 1,
                ..SkipCounts::default()
            },
            dialed: 1,
            evaluated: 2,
            ends: SERVED,
        },
        Row {
            name: "capability drop of a target that is also unusable",
            surface: Surface::Complete,
            targets: 2,
            config: override_search_on_a,
            scripts: &[],
            tiny_windows: &[],
            state: unusable_a,
            tools: true,
            skips: SkipCounts {
                credential_unusable: 1,
                ..SkipCounts::default()
            },
            dialed: 1,
            evaluated: 2,
            ends: SERVED,
        },
        Row {
            name: "window drop",
            surface: Surface::Complete,
            targets: 2,
            config: enable_window_gate,
            scripts: &[],
            tiny_windows: &[0],
            state: no_state,
            tools: false,
            skips: SkipCounts {
                context_window: 1,
                ..SkipCounts::default()
            },
            dialed: 1,
            evaluated: 2,
            ends: SERVED,
        },
        Row {
            name: "unusable, parked, open and rate-limited targets, then the fifth serves",
            surface: Surface::Stream,
            targets: 5,
            config: rpm1_on_d,
            scripts: &[],
            tiny_windows: &[],
            state: four_refusing_targets,
            tools: false,
            skips: SkipCounts {
                credential_unusable: 1,
                account_parked: 1,
                breaker_open: 1,
                rate_limited: 1,
                ..SkipCounts::default()
            },
            dialed: 1,
            evaluated: 5,
            ends: SERVED,
        },
        Row {
            name: "early success leaves the rest unevaluated",
            surface: Surface::Complete,
            targets: 2,
            config: no_config,
            scripts: &[],
            tiny_windows: &[],
            state: no_state,
            tools: false,
            skips: SkipCounts::default(),
            dialed: 1,
            evaluated: 1,
            ends: SERVED,
        },
        Row {
            name: "same-target retry without a limit dials again",
            surface: Surface::Complete,
            targets: 2,
            config: one_retry,
            scripts: &[(0, REJECT_503_THEN_OK)],
            tiny_windows: &[],
            state: no_state,
            tools: false,
            skips: SkipCounts::default(),
            dialed: 1,
            evaluated: 1,
            ends: SERVED,
        },
        Row {
            name: "same-target retry refused by the rate limit is not a skip",
            surface: Surface::Complete,
            targets: 2,
            config: rpm1_on_a_with_one_retry,
            scripts: &[(0, REJECT_503)],
            tiny_windows: &[],
            state: no_state,
            tools: false,
            skips: SkipCounts::default(),
            dialed: 2,
            evaluated: 2,
            ends: SERVED,
        },
        Row {
            name: "count_tokens walk",
            surface: Surface::CountTokens,
            targets: 2,
            config: no_config,
            scripts: &[],
            tiny_windows: &[],
            state: unusable_a,
            tools: false,
            skips: SkipCounts {
                credential_unusable: 1,
                ..SkipCounts::default()
            },
            dialed: 1,
            evaluated: 2,
            ends: SERVED,
        },
        Row {
            name: "all skipped",
            surface: Surface::Complete,
            targets: 2,
            config: no_config,
            scripts: &[],
            tiny_windows: &[],
            state: unusable_a_parked_b,
            tools: false,
            skips: SkipCounts {
                credential_unusable: 1,
                account_parked: 1,
                ..SkipCounts::default()
            },
            dialed: 0,
            evaluated: 2,
            ends: "credential_unusable",
        },
        Row {
            name: "count_tokens breaker gate refuses the first target and ends the walk",
            surface: Surface::CountTokens,
            targets: 2,
            config: no_config,
            scripts: &[],
            tiny_windows: &[],
            state: breaker_open_on_a,
            tools: false,
            skips: SkipCounts {
                breaker_open: 1,
                ..SkipCounts::default()
            },
            dialed: 0,
            evaluated: 1,
            ends: "breaker_open",
        },
        Row {
            name: "count_tokens RPM gate refuses the first target and ends the walk",
            surface: Surface::CountTokens,
            targets: 2,
            config: rpm1_on_a,
            scripts: &[],
            tiny_windows: &[],
            state: rpm_spent_on_a,
            tools: false,
            skips: SkipCounts {
                rate_limited: 1,
                ..SkipCounts::default()
            },
            dialed: 0,
            evaluated: 1,
            ends: "rate_limited",
        },
        Row {
            name: "capability override drops every target: NotImplemented",
            surface: Surface::Complete,
            targets: 2,
            config: override_search_on_every_target,
            scripts: &[],
            tiny_windows: &[],
            state: no_state,
            tools: true,
            skips: SkipCounts {
                capability: 2,
                ..SkipCounts::default()
            },
            dialed: 0,
            evaluated: 2,
            ends: "not_implemented",
        },
    ]
}

#[tokio::test]
async fn skips_and_dialed_targets_reconcile_with_the_targets_evaluated() {
    for row in rows() {
        // Arrange
        let name = row.name;
        let fx = fixture(&row);
        let observation = RequestObservation::new();

        // Act
        let (meta, err) = dispatch(&fx.router, row.surface, request(row.tools, &observation)).await;

        // Assert
        let skips = observation.snapshot().skips;
        let dialed = fx
            .providers
            .iter()
            .filter(|p| p.calls.load(Ordering::SeqCst) > 0)
            .count();
        assert_eq!(skips, row.skips, "{name}");
        assert_eq!(dialed, row.dialed, "{name}: targets dialed");
        assert_eq!(
            skips.total() + dialed as u64,
            row.evaluated,
            "{name}: skips + dialed == evaluated",
        );
        assert_eq!(ended(err.as_ref()), row.ends, "{name}: {err:?}");
        if dialed == 0 {
            assert_eq!(meta.attempt_count, 0, "{name}");
        }
    }
}

#[tokio::test]
async fn a_capability_refused_on_every_lane_keeps_its_capability_skips() {
    // Arrange
    let row = Row {
        name: "every lane refuses",
        surface: Surface::Complete,
        targets: 2,
        config: no_config,
        scripts: &[],
        tiny_windows: &[],
        state: no_state,
        tools: true,
        skips: SkipCounts::default(),
        dialed: 0,
        evaluated: 2,
        ends: "capability_unsupported_everywhere",
    };
    let fx = fixture(&row);
    for idx in 0..row.targets {
        let lane = format!("{}#claude-{}", provider_name(idx), nickname(idx));
        let observed = fx.router.learned_capabilities.observe(
            crate::learned_capability::LearnKey {
                state_key: &lane,
                feature_key_raw: SEARCH,
                provider_kind: PROVIDER_KIND,
            },
            crate::learned_capability::NegativeEvidence {
                tier: SignalTier::SelfIdentifying,
                phase: FailurePhase::F1,
                source: EvidenceSource::Live,
                evidence_class: None,
            },
            Instant::now(),
        );
        assert!(observed.is_some(), "premise: {lane} learned the negative");
    }
    let observation = RequestObservation::new();

    // Act
    let (meta, err) = dispatch(&fx.router, row.surface, request(true, &observation)).await;

    // Assert
    assert!(
        matches!(err, Some(Error::CapabilityUnsupportedEverywhere { .. })),
        "{err:?}"
    );
    assert_eq!(
        observation.snapshot().skips,
        SkipCounts {
            capability: 2,
            ..SkipCounts::default()
        }
    );
    assert_eq!(meta.attempt_count, 0);
    assert!(
        fx.providers
            .iter()
            .all(|p| p.calls.load(Ordering::SeqCst) == 0)
    );
}

#[test]
fn a_filter_reason_yields_only_to_a_higher_ranked_state_reason() {
    let rows = [
        (None, EligibilityReason::Capability),
        (
            Some(EligibilityReason::RateLimited),
            EligibilityReason::RateLimited,
        ),
    ];
    for (state, expected) in rows {
        assert_eq!(
            super::super::chain::higher_reason(EligibilityReason::Capability, state),
            expected,
            "{state:?}"
        );
    }
}
