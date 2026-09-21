//! The OUTGOING `anthropic-beta` header a background probe sends must match the
//! one the admitted request sent, byte for byte.
//!
//! Asserted on CAPTURED WIRE HEADERS, not on a canonical `ChatRequest` and not
//! on a normalizer's JSON body. The header is composed inside the egress's
//! `build_headers` from three inputs it treats DIFFERENTLY:
//!
//!   - `req.anthropic_beta` (client/ingress) is filtered through the provider's
//!     `allowed_betas` allowlist;
//!   - `routectl_internal.operator_betas` (the operator floor) bypasses that
//!     allowlist unconditionally;
//!   - the pinned Claude-Code floor is added only for a request the egress
//!     classifies NON-CC.
//!
//! So a probe that merged its two captured sources would smuggle a filtered
//! client flag past the allowlist, and one that lost the originating
//! Claude-Code classification would receive a floor the admitted request
//! suppressed. Either way its answer is still attributed to the field under
//! test, and only a real header capture can tell.
//!
//! # Why two stages
//!
//! The router refuses to activate a probe for a LOOPBACK base URL (a rejection
//! from one is not attributable), and wiremock binds loopback -- so the router
//! cannot both activate and dial a mock. Stage one therefore runs the real
//! router on a remote-looking base URL behind a capturing provider, yielding the
//! two canonical requests the production code actually built. Stage two sends
//! each of those, unmodified, through a REAL `AnthropicApiProvider` aimed at
//! wiremock and captures the headers it emits. Neither stage re-derives a
//! header: the bytes compared are the ones the egress produced from the
//! requests the router produced.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_core::{
    ChatRequest, ChatResponse, Error, Message, MessageContent, Provider, Result, Role, TokenCount,
    Usage,
};
use routectl_providers::anthropic_api::{
    AnthropicApiConfig, AnthropicApiProvider, AuthKind, CloakConfig, CloakMode,
};
use routectl_router::{
    AliasValue, Config, ModelEntry, ProviderEntry, ResolvedModel, Router, RouterOptions,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// The closed-table display value every probe in this suite asks about.
const GROUNDED_DISPLAY: &str = "summarized";

/// A remote-looking base URL, so the router's attributability gate admits the
/// lane. No request is ever sent here -- the capturing provider below answers
/// in-process, and the wire stage uses wiremock.
const REMOTE_BASE: &str = "https://api.anthropic.com";

type Captured = Arc<parking_lot::Mutex<Vec<ChatRequest>>>;

/// Answers in-process while KEEPING every request it is handed, so the test can
/// read the exact canonical bodies the router built for the admitted dispatch
/// and for the probe.
struct CapturingProvider {
    complete_seen: Captured,
    count_seen: Captured,
}

#[async_trait::async_trait]
impl Provider for CapturingProvider {
    fn id(&self) -> &'static str {
        "p1"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p1", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.complete_seen.lock().push(req);
        Ok(ChatResponse {
            model: "claude-sonnet-4-5".to_string(),
            usage: Some(Usage::default()),
            ..Default::default()
        })
    }
    async fn stream(
        &self,
        _: ChatRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<routectl_core::ChatChunk>>> {
        Err(Error::upstream("p1", 500, "unused"))
    }
    async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        self.count_seen.lock().push(req);
        Ok(TokenCount {
            input_tokens: 7,
            extras: serde_json::Map::new(),
        })
    }
}

/// Records the `anthropic-beta` header of every request that reaches it.
struct HeaderCapture {
    seen: Arc<parking_lot::Mutex<Vec<String>>>,
}

impl Respond for HeaderCapture {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        self.seen.lock().push(
            req.headers
                .get("anthropic-beta")
                .map(|v| v.to_str().unwrap_or_default().to_string())
                .unwrap_or_default(),
        );
        ResponseTemplate::new(200).set_body_json(serde_json::json!({ "input_tokens": 7 }))
    }
}

/// STAGE ONE: run the real router and return `(admitted_per_target, probe)` --
/// the two canonical requests production built.
async fn canonical_requests(
    allowed_betas: &[&str],
    model_beta: Option<&str>,
    client_betas: &[&str],
    cloak_auto: bool,
    cc_session: bool,
) -> (ChatRequest, ChatRequest) {
    let complete_seen: Captured = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let count_seen: Captured = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let provider = Arc::new(CapturingProvider {
        complete_seen: Arc::clone(&complete_seen),
        count_seen: Arc::clone(&count_seen),
    });

    let mut config = Config::default();
    let mut entry = ProviderEntry::anthropic_api("literal:sk-ant-test");
    if let ProviderEntry::AnthropicApi {
        base_url,
        allowed_betas: allow,
        auth_kind,
        cloak,
        ..
    } = &mut entry
    {
        *base_url = REMOTE_BASE.to_string();
        *allow = allowed_betas.iter().map(|s| (*s).to_string()).collect();
        if cloak_auto {
            *auth_kind = AuthKind::OauthBearer;
            cloak.mode = CloakMode::Auto;
        }
    }
    config.providers.insert("p1".to_string(), entry);

    let mut model = ModelEntry::new("p1", "claude-sonnet-4-5");
    let mut extras = BTreeMap::new();
    if let Some(beta) = model_beta {
        extras.insert("anthropic-beta".to_string(), beta.to_string());
        model = model.with_header_extras(extras.clone());
    }
    config.models.insert("m1".to_string(), model);
    config
        .aliases
        .insert("default".to_string(), AliasValue::Single("m1".to_string()));

    let mut router = Router::new(Arc::new(config));
    let mut models = BTreeMap::new();
    let mut resolved = ResolvedModel::new("m1", "p1", provider, "claude-sonnet-4-5");
    if model_beta.is_some() {
        // On the RESOLVED model, which is what the dispatch overlay reads when
        // it composes `operator_betas`.
        resolved = resolved.with_header_extras(extras);
    }
    models.insert("m1".to_string(), Arc::new(resolved));
    router.install_resolved_models(models);

    let _ = router
        .complete_with_options(
            admitted_request(client_betas, cc_session),
            RouterOptions::default(),
        )
        .await;
    assert_eq!(
        router.probe_scheduler_snapshot().activations_total,
        1,
        "premise: the admitted request must have activated exactly one lane"
    );
    assert_eq!(
        router.run_due_probes().await,
        1,
        "premise: the probe must run, or there is no probe request to compare"
    );

    let admitted = complete_seen
        .lock()
        .last()
        .cloned()
        .expect("the admitted dispatch must have reached the provider");
    let probe = count_seen
        .lock()
        .last()
        .cloned()
        .expect("the probe must have reached the provider");
    (admitted, probe)
}

/// STAGE TWO: send `req` through a REAL Anthropic egress at `server` and return
/// the `anthropic-beta` header it actually emitted.
async fn wire_header(
    server: &MockServer,
    seen: &Arc<parking_lot::Mutex<Vec<String>>>,
    allowed_betas: &[&str],
    model_beta: Option<&str>,
    cloak_auto: bool,
    req: ChatRequest,
) -> String {
    let cfg = AnthropicApiConfig {
        id: "p1".into(),
        auth: Arc::new(routectl_core::StaticToken::new("sk-ant-test")),
        base_url: server.uri(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: if cloak_auto {
            AuthKind::OauthBearer
        } else {
            AuthKind::ApiKey
        },
        header_extras: model_beta
            .map(|b| vec![("anthropic-beta".to_string(), b.to_string())])
            .unwrap_or_default(),
        user_agent: None,
        allowed_betas: allowed_betas.iter().map(|s| (*s).to_string()).collect(),
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig {
            mode: if cloak_auto {
                CloakMode::Auto
            } else {
                CloakMode::Never
            },
            ..CloakConfig::default()
        },
        use_forwarded_bearer: false,
        #[cfg(feature = "bedrock")]
        mantle: None,
    };
    let before = seen.lock().len();
    let _ = AnthropicApiProvider::new(cfg).count_tokens(req).await;
    let seen = seen.lock();
    assert!(
        seen.len() > before,
        "the egress must have reached the mock, or no header was captured"
    );
    seen[before].clone()
}

/// An admitted request grounding the closed-table field, carrying
/// `client_betas` on the ingress carrier and optionally a CC session capture.
fn admitted_request(client_betas: &[&str], cc_session: bool) -> ChatRequest {
    let mut req = ChatRequest {
        model: "m1".to_string(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text("hi".to_string()),
            refusal: None,
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        anthropic_beta: client_betas.iter().map(|s| (*s).to_string()).collect(),
        max_tokens: Some(2048),
        ..Default::default()
    };
    req.routectl_internal.anthropic_thinking_display = Some(GROUNDED_DISPLAY.to_string());
    if cc_session {
        req.routectl_internal.claude_code_headers =
            vec![("x-claude-code-session-id".to_string(), "sess-1".to_string())];
    }
    req
}

/// Both stages: the admitted request's emitted header and the probe's.
async fn admitted_and_probe_headers(
    allowed_betas: &[&str],
    model_beta: Option<&str>,
    client_betas: &[&str],
    cloak_auto: bool,
    cc_session: bool,
) -> (String, String) {
    let (admitted_req, probe_req) = canonical_requests(
        allowed_betas,
        model_beta,
        client_betas,
        cloak_auto,
        cc_session,
    )
    .await;

    let server = MockServer::start().await;
    let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(HeaderCapture {
            seen: Arc::clone(&seen),
        })
        .mount(&server)
        .await;

    let admitted = wire_header(
        &server,
        &seen,
        allowed_betas,
        model_beta,
        cloak_auto,
        admitted_req,
    )
    .await;
    let probe = wire_header(
        &server,
        &seen,
        allowed_betas,
        model_beta,
        cloak_auto,
        probe_req,
    )
    .await;
    (admitted, probe)
}

#[tokio::test]
async fn the_probe_beta_header_equals_the_admitted_one_with_and_without_an_allowlist() {
    // The allowlist is NON-EMPTY, which is what makes this falsifiable: with an
    // empty one every client flag passes through and merging the two captured
    // sources would be indistinguishable from keeping them apart.
    //
    // `kept-beta-2025-01-01` is allowed; `denied-beta-2025-01-01` is not, so the
    // egress filters it out. The operator floor bypasses the allowlist.
    //
    // A probe that reapplied a UNION to `operator_betas` would smuggle the
    // DENIED flag past the allowlist -- its header would be strictly wider than
    // the admitted one, and this equality would fail.
    let (admitted, probe) = admitted_and_probe_headers(
        &["kept-beta-2025-01-01", "operator-beta-2025-01-01"],
        Some("operator-beta-2025-01-01"),
        &["kept-beta-2025-01-01", "denied-beta-2025-01-01"],
        false,
        false,
    )
    .await;

    assert_eq!(
        probe, admitted,
        "the probe's outgoing beta header must equal the admitted request's"
    );
    assert!(
        !probe.contains("denied-beta-2025-01-01"),
        "a filtered client flag must not reappear through the operator carrier: {probe}"
    );
    // Not vacuous: an allowed client flag AND the operator floor are both
    // present, so the equality above is over a non-empty header.
    assert!(
        admitted.contains("kept-beta-2025-01-01") && admitted.contains("operator-beta-2025-01-01"),
        "premise: the admitted header must carry both an allowed client flag \
         and the operator floor: {admitted}"
    );

    // The same equality on the CLOAK LANE, both classifications. Folded in here
    // rather than standing as its own test: measured, a standalone version went
    // red on exactly the mutations this test already catches, so it
    // discriminated nothing on its own. Kept as a CASE because the cloak lane is
    // a distinct input -- `auth_kind` and `CloakMode` both differ -- and a
    // divergence appearing only there would otherwise go unmeasured.
    for cc_session in [true, false] {
        let (admitted, probe) =
            admitted_and_probe_headers(&[], None, &["ctx-beta-2025-01-01"], true, cc_session).await;
        assert_eq!(
            probe, admitted,
            "probe and admitted headers must match on the cloak lane \
             (cc_session={cc_session})"
        );
    }
}

/// Stage one only: the CLASSIFICATION the router captures must reach the probe
/// request, for genuine-CC traffic and for non-CC traffic alike.
///
/// This is the router half of the Claude-Code weld. The egress half -- that a
/// request carrying this classification receives (or is denied) the pinned
/// Claude Code beta floor -- is pinned in the providers crate, because the
/// floor's own gate requires the EXACT `api.anthropic.com` host and so cannot
/// fire against any mock: a wire-stage assertion here would be measuring a
/// floor that never applied, in either arm, and would pass on a build that had
/// lost the classification entirely.
///
/// What CAN be measured here is the link those two halves share: the bit the
/// router derived from the admitted request, on the request the probe actually
/// sends. Deleting the router's capture makes both assertions below fail.
#[tokio::test]
async fn the_router_carries_the_admitted_claude_code_classification_onto_the_probe() {
    // Genuine CC: the admitted request presents a session capture.
    let (admitted, probe) =
        canonical_requests(&[], None, &["ctx-beta-2025-01-01"], true, true).await;
    assert!(
        routectl_core::identity::anthropic::has_claude_code_session(
            &admitted.routectl_internal.claude_code_headers
        ),
        "premise: the admitted request must present a genuine CC session"
    );
    assert!(
        probe.routectl_internal.claude_code_headers.is_empty(),
        "the probe must carry NO raw session capture -- only the classification"
    );
    assert_eq!(
        probe.routectl_internal.originating_claude_code_session,
        Some(true),
        "the probe must carry the admitted request's genuine-CC classification"
    );
    assert!(
        probe.routectl_internal.background_probe,
        "and must be marked as routectl's own traffic, so the client census \
         does not count it"
    );

    // NON-CC POSITIVE CONTROL, same lane, one difference: no session capture.
    // Without this the assertion above would pass on a build that hardcoded
    // `Some(true)`.
    let (admitted, probe) =
        canonical_requests(&[], None, &["ctx-beta-2025-01-01"], true, false).await;
    assert!(
        !routectl_core::identity::anthropic::has_claude_code_session(
            &admitted.routectl_internal.claude_code_headers
        ),
        "premise: the control must NOT present a session"
    );
    assert_eq!(
        probe.routectl_internal.originating_claude_code_session,
        Some(false),
        "a probe for non-CC traffic must carry the non-CC classification"
    );
}
