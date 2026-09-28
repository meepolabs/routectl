//! Cross-lane sweep: the Claude Code billing/attribution block never reaches
//! any enabled egress's serialized wire body, from either canonical system
//! surface, while a legitimate system sentinel beside it always does.
//!
//! Drives each provider's real `normalize_request`, so the assertion is on
//! the bytes each lane would ship, not on a typed intermediate. One request
//! value is reused across every lane, which also pins that no lane's filter
//! mutates the caller's canonical request: a fallback attempt onto the next
//! lane receives the original and applies that lane's own policy.
//!
//! The two sentinels share no substring with each other or with any routectl
//! default, so a lane that dropped all system content fails the legitimate
//! half, and a lane that forwarded everything fails the fingerprint half.

#![cfg(all(
    feature = "openai-compat",
    feature = "anthropic-api",
    feature = "bedrock",
    feature = "openai-responses",
    feature = "gemini"
))]

use routectl_core::{
    ChatRequest, Message, MessageContent, Provider, RequestProvenance, Role, SystemBlock,
    SystemContent,
};
use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider};
use routectl_providers::bedrock::{
    BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth::ResolvedCreds,
};
use routectl_providers::gemini::{GeminiConfig, GeminiProvider};
use routectl_providers::openai_compat::{
    HistoryReasoning, OpenAiCompatConfig, OpenAiCompatProvider, ReasoningDialect,
};
use routectl_providers::openai_responses::{OpenAiResponsesConfig, OpenAiResponsesProvider};

const FINGERPRINT: &str = "x-anthropic-billing-header: cc_version=9.9.9; cch=sweepfp7";
const FINGERPRINT_TELL: &str = "sweepfp7";
const LEGITIMATE: &str = "sweep-legit-sentinel-3q";

/// Where the fingerprint rides on the canonical request.
#[derive(Debug, Clone, Copy)]
enum Source {
    TopLevelSystem,
    SystemRoleMessage,
}

/// Lanes whose `Role::System` handling is not yet covered, keyed by lane and
/// source. Content-pinned: [`every_excluded_pair_really_leaks_today`] fails
/// the moment an entry's lane stops leaking, so a fixed lane cannot keep its
/// exemption.
const KNOWN_GAPS: &[(&str, &str)] = &[];

fn is_known_gap(lane: &str, source: Source) -> bool {
    KNOWN_GAPS
        .iter()
        .any(|(l, s)| *l == lane && *s == format!("{source:?}"))
}

fn system_message(text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::System,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn user_message(text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn block(text: &str) -> SystemBlock {
    SystemBlock {
        kind: "text".into(),
        text: text.into(),
        cache_control: None,
        citations: None,
    }
}

/// A request carrying the fingerprint and the legitimate sentinel on ONE
/// surface, with nothing on the other.
fn request_for(source: Source) -> ChatRequest {
    let mut req = ChatRequest {
        model: "sweep-model".into(),
        max_tokens: Some(64),
        ..Default::default()
    };
    match source {
        Source::TopLevelSystem => {
            req.system = Some(SystemContent::Blocks(vec![
                block(FINGERPRINT),
                block(LEGITIMATE),
            ]));
            req.messages = vec![user_message("hi")].into();
        }
        Source::SystemRoleMessage => {
            req.messages = vec![
                system_message(FINGERPRINT),
                system_message(LEGITIMATE),
                user_message("hi"),
            ]
            .into();
        }
    }
    req
}

fn bedrock(shape: BedrockApiShape) -> BedrockProvider {
    let cfg = BedrockConfig {
        id: "bedrock-sweep".into(),
        region: "us-east-1".into(),
        model_id: "anthropic.claude-sonnet-4-5".into(),
        api_shape: shape,
        creds: BedrockCreds::BearerKey {
            key: "test-key".into(),
        },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        allowed_betas: Vec::new(),
        allowed_body_fields: Vec::new(),
        additional_model_request_fields: None,
        adaptive_thinking: None,
    };
    let resolved = ResolvedCreds::Bearer {
        key: "test-key".into(),
    };
    BedrockProvider::new(cfg, resolved).expect("canonical region")
}

/// Every enabled egress, by its telemetry lane name.
fn lanes() -> Vec<(&'static str, Box<dyn Provider>)> {
    vec![
        (
            "openai-compat",
            Box::new(OpenAiCompatProvider::new(OpenAiCompatConfig {
                id: "openai-compat-sweep".into(),
                base_url: "https://api.example.com/v1".into(),
                api_key: "test-key".into(),
                header_extras: Vec::new(),
                payload_extras: None,
                reasoning_dialect: ReasoningDialect::OpenAi,
                history_reasoning: HistoryReasoning::Auto,
                user_agent: None,
                strict_translation: false,
                disable_stream_include_usage: false,
                mantle: None,
            })),
        ),
        (
            "anthropic",
            Box::new(AnthropicApiProvider::new(AnthropicApiConfig::new(
                "anthropic-sweep",
                "test-key",
            ))),
        ),
        (
            "bedrock-converse",
            Box::new(bedrock(BedrockApiShape::Converse)),
        ),
        ("bedrock-invoke", Box::new(bedrock(BedrockApiShape::Invoke))),
        (
            "openai-responses",
            Box::new(OpenAiResponsesProvider::new(OpenAiResponsesConfig::new(
                "openai-responses-sweep",
                "test-key",
            ))),
        ),
        (
            "gemini",
            Box::new(GeminiProvider::new(GeminiConfig::new(
                "gemini-sweep",
                "test-key",
            ))),
        ),
    ]
}

fn wire(provider: &dyn Provider, req: &ChatRequest) -> String {
    let body = provider
        .normalize_request(req)
        .unwrap_or_else(|e| panic!("{} must normalize: {e}", provider.id()));
    serde_json::to_string(&body).expect("body renders")
}

#[test]
fn no_enabled_egress_ships_the_fingerprint_and_every_one_ships_the_legitimate_sentinel() {
    for source in [Source::TopLevelSystem, Source::SystemRoleMessage] {
        // Arrange
        let req = request_for(source);
        let before = serde_json::to_value(&req).expect("request renders");

        for (lane, provider) in lanes() {
            if is_known_gap(lane, source) {
                continue;
            }

            // Act
            let body = wire(provider.as_ref(), &req);

            // Assert
            assert!(
                !body.contains(FINGERPRINT_TELL),
                "{lane} shipped the client fingerprint from {source:?}: {body}"
            );
            assert!(
                body.contains(LEGITIMATE),
                "{lane} lost legitimate system content from {source:?}: {body}"
            );
        }

        assert_eq!(
            serde_json::to_value(&req).expect("request renders"),
            before,
            "an egress mutated the caller's canonical request ({source:?})"
        );
    }
}

/// Positive control on the sweep's fixture: with no filter in the way, both
/// sentinels are present in what every lane receives. A sweep whose fixture
/// carried no fingerprint would pass the absence half vacuously.
#[test]
fn the_sweep_fixture_carries_both_sentinels_on_each_source() {
    for source in [Source::TopLevelSystem, Source::SystemRoleMessage] {
        let rendered = serde_json::to_string(&request_for(source)).expect("renders");
        assert!(rendered.contains(FINGERPRINT_TELL), "{source:?}");
        assert!(rendered.contains(LEGITIMATE), "{source:?}");
    }
}

/// The sweep covers every lane the crate builds, so a lane added without
/// joining it is red rather than silently unswept.
#[test]
fn the_sweep_names_every_enabled_egress() {
    let names: Vec<&str> = lanes().into_iter().map(|(lane, _)| lane).collect();
    assert_eq!(
        names,
        vec![
            "openai-compat",
            "anthropic",
            "bedrock-converse",
            "bedrock-invoke",
            "openai-responses",
            "gemini",
        ]
    );
}

/// Every exclusion is a live gap: the excluded lane really does ship the
/// fingerprint from the excluded source today. When that lane is fixed this
/// fails, and the entry must be deleted so the sweep covers the pair.
#[test]
fn every_excluded_pair_really_leaks_today() {
    for (lane_name, source_name) in KNOWN_GAPS {
        let source = match *source_name {
            "TopLevelSystem" => Source::TopLevelSystem,
            "SystemRoleMessage" => Source::SystemRoleMessage,
            other => panic!("unknown source {other}"),
        };
        let (_, provider) = lanes()
            .into_iter()
            .find(|(lane, _)| lane == lane_name)
            .unwrap_or_else(|| panic!("{lane_name} is not an enabled egress"));
        let body = wire(provider.as_ref(), &request_for(source));
        assert!(
            body.contains(FINGERPRINT_TELL),
            "{lane_name} no longer ships the fingerprint from {source_name}; remove its \
             KNOWN_GAPS entry so the sweep covers it"
        );
    }
}

// ---------------------------------------------------------------------------
// Top-level `metadata` from the ingress sweep
// ---------------------------------------------------------------------------

const METADATA_TELL: &str = "sweep-meta-5v";

/// Lanes that forward the ingress `metadata` block by design: the
/// anthropic-api egress speaks to the party the block is addressed to.
/// Content-pinned by [`first_party_lanes_really_forward_ingress_metadata`].
const METADATA_FIRST_PARTY: &[&str] = &["anthropic"];

fn request_with_ingress_metadata() -> ChatRequest {
    let mut req = request_for(Source::TopLevelSystem);
    req.provider_extras = Some(serde_json::json!({
        "metadata": {"user_id": METADATA_TELL, "account_uuid": METADATA_TELL}
    }));
    req
}

#[test]
fn no_third_party_egress_ships_ingress_metadata() {
    // Arrange
    let req = request_with_ingress_metadata();

    for (lane, provider) in lanes() {
        if METADATA_FIRST_PARTY.contains(&lane) {
            continue;
        }

        // Act
        let body = wire(provider.as_ref(), &req);

        // Assert
        assert!(
            !body.contains(METADATA_TELL),
            "{lane} shipped the client metadata block: {body}"
        );
        assert!(
            body.contains(LEGITIMATE),
            "{lane} lost legitimate system content: {body}"
        );
    }
}

/// Positive control for the sweep above: the fixture's metadata does reach a
/// wire when no withhold applies, so its absence elsewhere is evidence.
#[test]
fn first_party_lanes_really_forward_ingress_metadata() {
    for lane_name in METADATA_FIRST_PARTY {
        let (_, provider) = lanes()
            .into_iter()
            .find(|(lane, _)| lane == lane_name)
            .unwrap_or_else(|| panic!("{lane_name} is not an enabled egress"));
        let body = wire(provider.as_ref(), &request_with_ingress_metadata());
        assert!(
            body.contains(METADATA_TELL),
            "{lane_name} no longer forwards ingress metadata; drop its first-party entry"
        );
    }
}

// ---------------------------------------------------------------------------
// Credential-bearing ingress extras
// ---------------------------------------------------------------------------

const CREDENTIAL_TELL: &str = "sweep-mcp-tok-4r";

/// Lanes that forward a swept `mcp_servers` entry from either ingress today.
/// The Gemini source boundary does not cover them; they are recorded here so
/// the sweep states its reach instead of implying it. Content-pinned in both
/// directions by
/// [`every_lane_outside_the_credential_boundary_really_forwards_it_today`].
const CREDENTIAL_FORWARDING_LANES: &[&str] = &["anthropic", "bedrock-converse", "bedrock-invoke"];

const INGRESS_PROVENANCES: &[RequestProvenance] = &[
    RequestProvenance::AnthropicIngress,
    RequestProvenance::OpenaiIngress,
];

/// The shape an Anthropic Messages client sends for a remote MCP server: the
/// server's bearer rides inside the swept extras.
fn request_with_ingress_credential(provenance: RequestProvenance) -> ChatRequest {
    let mut req = request_for(Source::TopLevelSystem);
    req.provider_extras = Some(serde_json::json!({
        "mcp_servers": [{
            "type": "url",
            "url": "https://mcp.example.com/sse",
            "name": "sweep",
            "authorization_token": CREDENTIAL_TELL
        }]
    }));
    req.routectl_internal.provenance = provenance;
    req
}

#[test]
fn no_egress_inside_the_boundary_ships_an_ingress_credential() {
    for &provenance in INGRESS_PROVENANCES {
        // Arrange
        let req = request_with_ingress_credential(provenance);

        for (lane, provider) in lanes() {
            if CREDENTIAL_FORWARDING_LANES.contains(&lane) {
                continue;
            }

            // Act
            let body = wire(provider.as_ref(), &req);

            // Assert
            assert!(
                !body.contains(CREDENTIAL_TELL),
                "{lane} shipped an ingress credential from {provenance:?}: {body}"
            );
            assert!(
                body.contains(LEGITIMATE),
                "{lane} lost legitimate system content from {provenance:?}: {body}"
            );
        }
    }
}

/// Positive control for the Gemini arm of the sweep above: the same fixture
/// from a trusted library caller does reach the Gemini wire, so its absence
/// on the ingress provenances is the boundary and not a fixture that carries
/// nothing.
#[test]
fn a_library_caller_credential_reaches_the_gemini_wire() {
    let (_, gemini) = lanes()
        .into_iter()
        .find(|(lane, _)| *lane == "gemini")
        .expect("gemini is an enabled egress");
    let body = wire(
        gemini.as_ref(),
        &request_with_ingress_credential(RequestProvenance::Library),
    );
    assert!(body.contains(CREDENTIAL_TELL), "{body}");
}

#[test]
fn every_lane_outside_the_credential_boundary_really_forwards_it_today() {
    for (lane, provider) in lanes() {
        for &provenance in INGRESS_PROVENANCES {
            let body = wire(
                provider.as_ref(),
                &request_with_ingress_credential(provenance),
            );
            assert_eq!(
                body.contains(CREDENTIAL_TELL),
                CREDENTIAL_FORWARDING_LANES.contains(&lane),
                "{lane} from {provenance:?} no longer matches its CREDENTIAL_FORWARDING_LANES \
                 entry; update the register so the sweep covers what it claims"
            );
        }
    }
}
