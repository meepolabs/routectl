//! A request routectl refuses on its own counts once on
//! `rc_request_refused_local_total` under its lane, whichever router exit it
//! leaves through; an upstream rejection and a credential refusal do not
//! count.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::{BoxStream, StreamExt};
use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};
use routectl_core::wire_check::{IngressDialect, IngressWire};
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, ChunkChoice, ChunkDelta, Error, Provider, Result, ToolDef,
};
use serde_json::{Value, json};

use crate::config::{AliasValue, Config, CredentialSource, ProviderEntry};
use crate::learned_capability::{EntryVerdict, ExportedEntry};
use crate::resolved::ResolvedModel;
use crate::router::Router;

const ALIAS: &str = "a";
const NICKNAME: &str = "m";
const PROVIDER: &str = "p";
const UPSTREAM: &str = "upstream";
const ANTHROPIC_LANE: &str = "anthropic->anthropic-api";

/// A provider whose every call fails with one fixed error.
struct FailingProvider {
    error: fn() -> Error,
}

#[async_trait::async_trait]
impl Provider for FailingProvider {
    fn id(&self) -> &'static str {
        "failing"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("failing", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Err((self.error)())
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err((self.error)())
    }
}

/// A provider that answers every call successfully.
struct AnsweringProvider;

#[async_trait::async_trait]
impl Provider for AnsweringProvider {
    fn id(&self) -> &'static str {
        "answering"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("answering", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Ok(ChatResponse::default())
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let chunk = ChatChunk {
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    content: Some("ok".into()),
                    ..Default::default()
                },
                finish_reason: None,
                matched_stop_sequence: None,
            }],
            ..Default::default()
        };
        Ok(futures::stream::iter([Ok(chunk)]).boxed())
    }
}

/// A router whose alias names one anthropic-api model served by `provider`,
/// with no retries and the given `[server] strict_translation`.
fn router_over(entry: ProviderEntry, provider: Arc<dyn Provider>, strict: bool) -> Router {
    let mut config = Config::default();
    config.server.strict_translation = strict;
    config.retry.max_attempts = 1;
    config.providers.insert(PROVIDER.into(), entry);
    config
        .aliases
        .insert(ALIAS.into(), AliasValue::Single(NICKNAME.into()));
    let mut router = Router::new(Arc::new(config));
    let model = ResolvedModel::new(NICKNAME, PROVIDER, provider, UPSTREAM);
    router.install_resolved_models(BTreeMap::from([(NICKNAME.to_string(), Arc::new(model))]));
    router
}

fn failing_router(error: fn() -> Error) -> Router {
    router_over(
        ProviderEntry::anthropic_api("literal:k"),
        Arc::new(FailingProvider { error }),
        false,
    )
}

/// A request that arrived through the Anthropic ingress.
fn anthropic_request() -> ChatRequest {
    let mut req = ChatRequest {
        model: ALIAS.into(),
        ..Default::default()
    };
    req.routectl_internal.ingress_wire = Some(IngressWire {
        dialect: IngressDialect::Anthropic,
        client_version: None,
        bytes: br#"{"model":"a","messages":[]}"#.as_slice().into(),
    });
    req
}

/// Expected `(lane, count)` local-refusal keys.
type Refusals = &'static [(&'static str, u64)];

/// One refusal-counting case: its name, the error the provider fails with,
/// and the keys the dispatch must leave.
type RefusalCase = (&'static str, fn() -> Error, Refusals);

/// `(lane, count)` for every local-refusal key the router holds.
fn refusals(router: &Router) -> Vec<(&'static str, u64)> {
    router
        .wire_check_snapshot()
        .local_refusals
        .iter()
        .map(|c| (c.lane.label(), c.reading.count))
        .collect()
}

#[derive(Clone, Copy, Debug)]
enum Surface {
    Complete,
    Stream,
}

const SURFACES: [Surface; 2] = [Surface::Complete, Surface::Stream];

/// Dispatch `req` on `surface` and return the error the client receives.
async fn dispatch_err(router: &Router, surface: Surface, req: ChatRequest) -> Error {
    match surface {
        Surface::Complete => Box::pin(router.complete(req))
            .await
            .expect_err("the dispatch fails"),
        Surface::Stream => Box::pin(router.stream(req))
            .await
            .err()
            .expect("the stream fails to open"),
    }
}

fn egress_normalize_refusal() -> Error {
    Error::normalize_request("failing", "request cannot be expressed on this egress")
}

fn egress_local_refusal() -> Error {
    Error::LocalRefusal {
        provider: "failing".into(),
        detail: "request shape refused before egress".into(),
        reason: routectl_core::RefusalReason::CloakRelocation,
    }
}

fn upstream_bad_request() -> Error {
    Error::upstream("failing", 400, "upstream rejected the request")
}

#[tokio::test]
async fn an_egress_refusal_counts_once_for_its_lane_and_an_upstream_400_does_not() {
    let cases: [RefusalCase; 3] = [
        (
            "normalize_request",
            egress_normalize_refusal,
            &[(ANTHROPIC_LANE, 1)],
        ),
        (
            "local_refusal",
            egress_local_refusal,
            &[(ANTHROPIC_LANE, 1)],
        ),
        ("upstream_400", upstream_bad_request, &[]),
    ];

    for (name, error, expected) in cases {
        for surface in SURFACES {
            let router = failing_router(error);

            dispatch_err(&router, surface, anthropic_request()).await;

            assert_eq!(refusals(&router), expected, "{name} on {surface:?}");
        }
    }
}

#[tokio::test]
async fn an_egress_refusal_without_an_ingress_body_has_no_lane_and_is_not_counted() {
    let router = failing_router(egress_local_refusal);
    let req = ChatRequest {
        model: ALIAS.into(),
        ..Default::default()
    };

    dispatch_err(&router, Surface::Complete, req).await;

    assert!(refusals(&router).is_empty());
}

fn acting_advisor_negative() -> ExportedEntry {
    let base = Instant::now();
    ExportedEntry {
        provider_kind: "anthropic-api".into(),
        state_key: format!("{PROVIDER}#{UPSTREAM}"),
        feature_key: "advisor".into(),
        verdict: EntryVerdict::Negative,
        signal: SignalTier::SelfIdentifying,
        observations: 1,
        first_seen: base,
        last_seen: base,
        expires_at: base + Duration::from_hours(48),
        evidence_class: None,
        phase: FailurePhase::F1,
        source: EvidenceSource::Live,
        in_flight: false,
        consecutive_failed_probes: 0,
    }
}

fn advisor_request() -> ChatRequest {
    ChatRequest {
        tools: Some(vec![ToolDef::Other(
            json!({"type": "advisor", "name": "advisor"}),
        )]),
        ..anthropic_request()
    }
}

#[tokio::test]
async fn a_strict_strip_refusal_counts_exactly_once() {
    for surface in SURFACES {
        let router = router_over(
            ProviderEntry::anthropic_api("literal:k"),
            Arc::new(AnsweringProvider),
            true,
        );
        router
            .learned_capabilities
            .import_entries(vec![acting_advisor_negative()]);

        let err = dispatch_err(&router, surface, advisor_request()).await;

        assert!(matches!(err, Error::Validation(..)), "{surface:?}: {err:?}");
        assert_eq!(router.metrics.strip_strict_rejected_total(), 1);
        assert_eq!(refusals(&router), [(ANTHROPIC_LANE, 1)], "{surface:?}");
    }
}

#[tokio::test]
async fn a_tool_pairing_refusal_carries_its_reason() {
    for surface in SURFACES {
        let router = router_over(
            ProviderEntry::anthropic_api("literal:k"),
            Arc::new(AnsweringProvider),
            false,
        );
        let mut req = anthropic_request();
        req.messages = vec![routectl_core::Message {
            role: routectl_core::Role::Tool,
            content: routectl_core::MessageContent::Text("r".into()),
            tool_call_id: Some("call_missing".into()),
            refusal: None,
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_calls: None,
        }]
        .into();

        let err = dispatch_err(&router, surface, req).await;

        assert_eq!(
            err.refusal_reason(),
            Some(routectl_core::RefusalReason::ToolPairing),
            "{surface:?}: {err:?}"
        );
    }
}

#[tokio::test]
async fn a_missing_forwarded_bearer_refusal_is_not_counted() {
    for surface in SURFACES {
        let router = router_over(
            ProviderEntry::anthropic_api("literal:k")
                .with_credential_source(CredentialSource::Forwarded),
            Arc::new(AnsweringProvider),
            false,
        );

        let err = dispatch_err(&router, surface, anthropic_request()).await;

        assert!(
            err.to_string().contains("missing_forwarded_bearer"),
            "{surface:?}: {err}"
        );
        assert_eq!(
            err.refusal_reason(),
            Some(routectl_core::RefusalReason::ForwardedBearerMissing),
            "{surface:?}"
        );
        assert!(refusals(&router).is_empty(), "{surface:?}");
    }
}
