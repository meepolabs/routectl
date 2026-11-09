//! Both Bedrock seams withhold the client's `metadata` block and ship the
//! operator's own value in its place, and each traces the `anthropic_beta`
//! set it ships once per request.

use std::sync::Arc;

use routectl_core::ChatRequest;
use routectl_core::wire_check::{EditKind, EditSite, Leg, WireTap};
use serde_json::{Value, json};

use super::{BedrockApiShape, BedrockConfig, BedrockCreds};

const SHAPES: [BedrockApiShape; 2] = [BedrockApiShape::Invoke, BedrockApiShape::Converse];

fn cfg(shape: BedrockApiShape) -> BedrockConfig {
    BedrockConfig {
        id: "bedrock-metadata".into(),
        region: "us-east-1".into(),
        model_id: "us.anthropic.claude-opus-5-5".into(),
        api_shape: shape,
        creds: BedrockCreds::BearerKey { key: "k".into() },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        additional_model_request_fields: None,
    }
}

fn request() -> ChatRequest {
    serde_json::from_value(json!({
        "model": "claude-x",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
    }))
    .expect("fixture is a canonical request")
}

/// A request as the dispatch layer leaves it: `provider_extras` holds the
/// operator layer deep-merged over the client's sweep, and the operator layer
/// alone is recorded on the internal carrier.
fn dispatched(client: Option<Value>, operator: Option<Value>) -> ChatRequest {
    let mut req = request();
    let mut merged = client.clone().unwrap_or_else(|| json!({}));
    if let (Some(merged), Some(Value::Object(op))) = (merged.as_object_mut(), &operator) {
        for (k, v) in op {
            match (merged.get_mut(k), v) {
                (Some(Value::Object(existing)), Value::Object(layer)) => {
                    existing.extend(layer.clone());
                }
                _ => {
                    merged.insert(k.clone(), v.clone());
                }
            }
        }
    }
    if client.is_some() || operator.is_some() {
        req.provider_extras = Some(merged);
    }
    req.routectl_internal.operator_payload_extras = operator.map(Arc::new);
    req
}

fn normalize(cfg: &BedrockConfig, req: &ChatRequest) -> Value {
    match cfg.api_shape {
        BedrockApiShape::Invoke => super::invoke::normalize_request(cfg, req),
        BedrockApiShape::Converse => super::converse::normalize_request(cfg, req),
    }
    .expect("assembles")
}

/// The object `metadata` ships in: the body on Invoke, the field bag on
/// Converse.
fn shipped_metadata(shape: BedrockApiShape, body: &Value) -> Option<Value> {
    let carrier = match shape {
        BedrockApiShape::Invoke => Some(body),
        BedrockApiShape::Converse => body.get("additionalModelRequestFields"),
    };
    carrier.and_then(|c| c.get("metadata")).cloned()
}

#[test]
fn operator_payload_metadata_survives_and_client_metadata_is_stripped() {
    struct Row {
        name: &'static str,
        client: Value,
        payload: Option<Value>,
        config: Option<Value>,
    }
    let rows = [
        Row {
            name: "operator payload_extras",
            client: json!({"metadata": {"user_id": "u-client-1"}}),
            payload: Some(json!({"metadata": {"trace": "op-trace"}})),
            config: None,
        },
        Row {
            name: "additional_model_request_fields only",
            client: json!({"metadata": {"user_id": "u-client-1"}}),
            payload: None,
            config: Some(json!({"metadata": {"trace": "op-trace"}})),
        },
        Row {
            name: "payload_extras wins over additional_model_request_fields",
            client: json!({"metadata": {"user_id": "u-client-1"}}),
            payload: Some(json!({"metadata": {"trace": "op-trace"}})),
            config: Some(json!({"metadata": {"trace": "config-trace"}})),
        },
    ];
    for shape in SHAPES {
        for row in &rows {
            let mut cfg = cfg(shape);
            cfg.additional_model_request_fields = row.config.clone();
            let req = dispatched(Some(row.client.clone()), row.payload.clone());

            let body = normalize(&cfg, &req);

            assert_eq!(
                shipped_metadata(shape, &body),
                Some(json!({"trace": "op-trace"})),
                "{shape:?} / {}: {body}",
                row.name
            );
            let serialized = body.to_string();
            assert!(
                !serialized.contains("user_id") && !serialized.contains("u-client-1"),
                "{shape:?} / {}: the client fingerprint reached the upstream: {serialized}",
                row.name
            );
        }
    }
}

#[test]
fn client_metadata_alone_is_withheld_on_both_seams() {
    for shape in SHAPES {
        let req = dispatched(Some(json!({"metadata": {"user_id": "u-client-1"}})), None);

        let body = normalize(&cfg(shape), &req);

        assert_eq!(shipped_metadata(shape, &body), None, "{shape:?}: {body}");
    }
}

fn converse_fingerprint_count() -> u64 {
    crate::translation_drop_metrics::translation_policy_action_snapshot()
        .into_iter()
        .find(|e| e.lane == "bedrock-converse" && e.policy_class == "client_fingerprint_stripped")
        .map_or(0, |e| e.action_count)
}

#[test]
fn converse_fingerprint_counts_only_when_client_metadata_differs() {
    let operator = json!({"metadata": {"trace": "op-trace"}});
    let rows = [
        ("client sent the operator's own value", operator.clone(), 0),
        (
            "client sent its own member",
            json!({"metadata": {"user_id": "u-client-1"}}),
            1,
        ),
    ];
    for (name, client, want) in rows {
        let req = dispatched(Some(client), Some(operator.clone()));
        let before = converse_fingerprint_count();

        normalize(&cfg(BedrockApiShape::Converse), &req);

        assert_eq!(converse_fingerprint_count() - before, want, "{name}");
    }
}

fn metadata_strip_edits(shape: BedrockApiShape, mut req: ChatRequest) -> Vec<(EditKind, String)> {
    let tap = WireTap::default();
    req.routectl_internal.wire_tap = Some(tap.clone());
    normalize(&cfg(shape), &req);
    tap.take_request()
        .edits(Leg::Request)
        .iter()
        .filter(|e| {
            e.site() == EditSite::ClientFingerprintStrip && e.path().starts_with("/metadata")
        })
        .map(|e| (e.kind(), e.path().to_string()))
        .collect()
}

/// One declaration row: name, client extras, operator extras, the Remove
/// path expected (if any).
type DeclareRow = (
    &'static str,
    Option<Value>,
    Option<Value>,
    Option<&'static str>,
);

#[test]
fn a_withheld_client_metadata_is_declared_removed_on_both_seams() {
    let operator = json!({"metadata": {"trace": "op-trace"}});
    let rows: [DeclareRow; 4] = [
        (
            "client only: the whole block",
            Some(json!({"metadata": {"user_id": "u-client-1"}})),
            None,
            Some("/metadata"),
        ),
        (
            "client member beside the operator's: that member",
            Some(json!({"metadata": {"user_id": "u-client-1"}})),
            Some(operator.clone()),
            Some("/metadata/*"),
        ),
        (
            "client sent the operator's own value: nothing",
            Some(operator.clone()),
            Some(operator.clone()),
            None,
        ),
        ("no metadata at all: nothing", None, None, None),
    ];
    for shape in SHAPES {
        for (name, client, op, want) in &rows {
            let edits = metadata_strip_edits(shape, dispatched(client.clone(), op.clone()));

            let want: Vec<(EditKind, String)> = want
                .iter()
                .map(|p| (EditKind::Remove, (*p).to_string()))
                .collect();
            assert_eq!(edits, want, "{shape:?} / {name}");
        }
    }
}

const BETA_TRACE: &str = "bedrock final anthropic_beta set";

#[test]
fn final_beta_set_traced_once_per_request() {
    for shape in SHAPES {
        let mut cfg = cfg(shape);
        cfg.anthropic_beta = vec!["context-1m-2025-08-07".into()];
        let mut req = request();
        req.anthropic_beta = vec!["client-flag\u{1b}[31m".into(), "withheld-flag".into()];
        req.routectl_internal.withheld_betas = Arc::from(vec!["withheld-flag".to_string()]);

        let events = routectl_testkit::capture_events(|| {
            normalize(&cfg, &req);
        });

        let traces: Vec<_> = events.iter().filter(|e| e.message == BETA_TRACE).collect();
        assert_eq!(traces.len(), 1, "{shape:?}: {events:?}");
        let trace = traces[0];
        assert_eq!(trace.level, tracing::Level::TRACE, "{shape:?}");
        assert_eq!(trace.field("count"), Some("2"), "{shape:?}");
        let flags = trace.field("flags").expect("flags field");
        assert_eq!(flags, "context-1m-2025-08-07,client-flag?[31m", "{shape:?}");
        assert!(flags.is_ascii() && !flags.contains('\u{1b}'), "{shape:?}");
    }
}
