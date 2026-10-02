//! Egress tests for the body fields Bedrock cannot represent on either
//! carrier, driven through each carrier's `normalize_request` so the
//! assertion reads the body that is actually serialized.

use std::sync::Arc;

use routectl_core::{ChatRequest, Message, MessageContent, RequestProvenance, Role};
use serde_json::{Value, json};

use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds, converse, invoke};

const SENTINEL_TOKEN: &str = "mcp-auth-sentinel-7Qz";
const ADJACENT_FIELD: &str = "top_k";

#[derive(Debug, Clone, Copy)]
enum Carrier {
    Invoke,
    Converse,
}

#[derive(Debug, Clone, Copy)]
enum Allowlist {
    Empty,
    ListsMcpServers,
}

/// Where the field enters the request before Bedrock assembly.
#[derive(Debug, Clone, Copy)]
enum Seam {
    /// The Anthropic ingress's forward-compat sweep of the client body.
    ClientBody,
    /// Operator `payload_extras` the router layered into `provider_extras`.
    ProviderExtras,
    /// Provider config `additional_model_request_fields`.
    OperatorExtras,
}

const CARRIERS: [Carrier; 2] = [Carrier::Invoke, Carrier::Converse];
const ALLOWLISTS: [Allowlist; 2] = [Allowlist::Empty, Allowlist::ListsMcpServers];
const SEAMS: [Seam; 3] = [Seam::ClientBody, Seam::ProviderExtras, Seam::OperatorExtras];

fn mcp_servers_value() -> Value {
    json!([{
        "type": "url",
        "url": "https://mcp.example.com/sse",
        "name": "probe",
        "authorization_token": SENTINEL_TOKEN
    }])
}

fn extras_with_mcp_servers_and_adjacent() -> Value {
    json!({ "mcp_servers": mcp_servers_value(), ADJACENT_FIELD: 40 })
}

fn allowed_body_fields(allowlist: Allowlist) -> Vec<String> {
    match allowlist {
        Allowlist::Empty => Vec::new(),
        Allowlist::ListsMcpServers => [
            "anthropic_version",
            "anthropic_beta",
            "max_tokens",
            "messages",
            "system",
            "tools",
            "tool_choice",
            "thinking",
            "output_config",
            "cache_control",
            "metadata",
            ADJACENT_FIELD,
            "mcp_servers",
        ]
        .into_iter()
        .map(String::from)
        .collect(),
    }
}

fn cfg(carrier: Carrier, allowlist: Allowlist, seam: Seam) -> BedrockConfig {
    BedrockConfig {
        id: "bedrock:unrepresentable".into(),
        region: "us-west-2".into(),
        model_id: "anthropic.claude-sonnet-4-5".into(),
        api_shape: match carrier {
            Carrier::Invoke => BedrockApiShape::Invoke,
            Carrier::Converse => BedrockApiShape::Converse,
        },
        creds: BedrockCreds::BearerKey { key: "test".into() },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        allowed_betas: Vec::new(),
        allowed_body_fields: allowed_body_fields(allowlist),
        additional_model_request_fields: match seam {
            Seam::OperatorExtras => Some(extras_with_mcp_servers_and_adjacent()),
            Seam::ClientBody | Seam::ProviderExtras => None,
        },
        adaptive_thinking: None,
    }
}

fn request(seam: Seam) -> ChatRequest {
    let mut req = ChatRequest {
        model: "anthropic.claude-sonnet-4-5".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        max_tokens: Some(64),
        ..Default::default()
    };
    match seam {
        Seam::ClientBody => {
            req.provider_extras = Some(extras_with_mcp_servers_and_adjacent());
            req.routectl_internal.provenance = RequestProvenance::AnthropicIngress;
        }
        Seam::ProviderExtras => {
            let operator = extras_with_mcp_servers_and_adjacent();
            req.provider_extras = Some(operator.clone());
            req.routectl_internal.operator_payload_extras = Some(Arc::new(operator));
        }
        Seam::OperatorExtras => {}
    }
    req
}

/// The object the carrier forwards the long-tail fields in: the whole body
/// on Invoke, the `additionalModelRequestFields` bag on Converse.
fn forwarded_fields(carrier: Carrier, body: &Value) -> &Value {
    match carrier {
        Carrier::Invoke => body,
        Carrier::Converse => &body["additionalModelRequestFields"],
    }
}

fn egress_body(carrier: Carrier, allowlist: Allowlist, seam: Seam) -> Value {
    let cfg = cfg(carrier, allowlist, seam);
    let req = request(seam);
    match carrier {
        Carrier::Invoke => invoke::normalize_request(&cfg, &req),
        Carrier::Converse => converse::normalize_request(&cfg, &req),
    }
    .unwrap_or_else(|e| panic!("{carrier:?}/{allowlist:?}/{seam:?} must normalize: {e}"))
}

fn cells() -> impl Iterator<Item = (Carrier, Allowlist, Seam)> {
    CARRIERS.into_iter().flat_map(|carrier| {
        ALLOWLISTS.into_iter().flat_map(move |allowlist| {
            SEAMS
                .into_iter()
                .map(move |seam| (carrier, allowlist, seam))
        })
    })
}

#[test]
fn mcp_servers_never_reaches_either_carrier_from_any_seam_under_any_allowlist() {
    // Arrange
    let mut leaks = Vec::new();

    // Act
    for (carrier, allowlist, seam) in cells() {
        let body = egress_body(carrier, allowlist, seam);
        let wire = serde_json::to_string(&body).expect("body serializes");
        if forwarded_fields(carrier, &body)
            .get("mcp_servers")
            .is_some()
            || wire.contains(SENTINEL_TOKEN)
        {
            leaks.push(format!("{carrier:?}/{allowlist:?}/{seam:?}: {wire}"));
        }
    }

    // Assert
    assert!(
        leaks.is_empty(),
        "mcp_servers reached {} egress cell(s):\n{}",
        leaks.len(),
        leaks.join("\n")
    );
}

/// Positive control for the matrix above: every seam really delivers its
/// fields to the carrier, so the absence of `mcp_servers` is the drop and
/// not a fixture that never reached the wire.
#[test]
fn an_adjacent_allowed_field_from_the_same_seam_survives_on_both_carriers() {
    // Arrange
    let mut lost = Vec::new();

    // Act
    for (carrier, allowlist, seam) in cells() {
        let body = egress_body(carrier, allowlist, seam);
        if forwarded_fields(carrier, &body).get(ADJACENT_FIELD) != Some(&json!(40)) {
            lost.push(format!("{carrier:?}/{allowlist:?}/{seam:?}: {body}"));
        }
    }

    // Assert
    assert!(
        lost.is_empty(),
        "the adjacent field was lost:\n{}",
        lost.join("\n")
    );
}

fn is_drop_event(event: &routectl_testkit::CapturedEvent) -> bool {
    event.message.contains("cannot represent")
}

#[test]
fn the_mcp_servers_drop_logs_the_field_name_and_never_the_connector_token() {
    // Arrange
    let mut problems = Vec::new();

    // Act
    for (carrier, allowlist, seam) in cells() {
        let cell = format!("{carrier:?}/{allowlist:?}/{seam:?}");
        let events = routectl_testkit::capture_events(|| {
            let _ = egress_body(carrier, allowlist, seam);
        });
        if events.iter().any(|e| {
            e.message.contains(SENTINEL_TOKEN)
                || e.fields.iter().any(|(_, v)| v.contains(SENTINEL_TOKEN))
        }) {
            problems.push(format!("{cell}: a log event carried the connector token"));
        }
        let drops: Vec<_> = events.iter().filter(|e| is_drop_event(e)).collect();
        let well_formed = drops.len() == 1
            && drops[0].level == tracing::Level::DEBUG
            && drops[0].field("field") == Some("mcp_servers")
            && drops[0].field("provider") == Some("bedrock:unrepresentable")
            && drops[0].field("surface").is_some();
        if !well_formed {
            problems.push(format!(
                "{cell}: expected one DEBUG drop event, got {drops:?}"
            ));
        }
    }

    // Assert
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn a_body_without_mcp_servers_logs_no_drop() {
    for carrier in CARRIERS {
        // Arrange
        let cfg = cfg(carrier, Allowlist::Empty, Seam::ClientBody);
        let req = request(Seam::OperatorExtras);

        // Act
        let events = routectl_testkit::capture_events(|| {
            let _ = match carrier {
                Carrier::Invoke => invoke::normalize_request(&cfg, &req),
                Carrier::Converse => converse::normalize_request(&cfg, &req),
            };
        });

        // Assert
        assert!(
            !events.iter().any(is_drop_event),
            "{carrier:?}: nothing was dropped, so nothing is owed a drop log: {events:?}"
        );
    }
}
