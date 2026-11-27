//! Every Bedrock surface (Invoke, Converse, and the mantle Anthropic and
//! openai-compat lanes) withholds the client's `metadata` block and ships the operator's own
//! value in its place, and each Bedrock seam traces the `anthropic_beta` set
//! it ships once per request.

use std::sync::Arc;

use routectl_core::wire_check::{EditKind, EditSite, Leg, WireTap};
use routectl_core::{ChatRequest, RequestProvenance};
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
    for shape in SHAPES {
        let req = dispatched(
            Some(json!({"metadata": {"user_id": "u-client-1"}})),
            Some(json!({"metadata": {"trace": "op-trace"}})),
        );

        let body = normalize(&cfg(shape), &req);

        assert_eq!(
            shipped_metadata(shape, &body),
            Some(json!({"trace": "op-trace"})),
            "{shape:?}: {body}"
        );
        let serialized = body.to_string();
        assert!(
            !serialized.contains("user_id") && !serialized.contains("u-client-1"),
            "{shape:?}: the client fingerprint reached the upstream: {serialized}"
        );
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

/// A surface whose body leaves for AWS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Surface {
    Invoke,
    Converse,
    MantleAnthropic,
    MantleOpenaiCompat,
}

/// The surfaces that send the Anthropic body shape and its `metadata` block.
const ANTHROPIC_SHAPED: [Surface; 3] =
    [Surface::Invoke, Surface::Converse, Surface::MantleAnthropic];

fn mantle_openai_compat() -> crate::openai_compat::OpenAiCompatProvider {
    crate::openai_compat::OpenAiCompatProvider::new(crate::openai_compat::OpenAiCompatConfig {
        mantle: Some(crate::mantle::MantleAuth {
            region: "us-west-2".into(),
            creds: super::auth::ResolvedCreds::Bearer {
                key: "mantle-bearer-key".into(),
            },
        }),
        ..crate::openai_compat::OpenAiCompatConfig::new(
            "bedrock-mantle-compat",
            "https://bedrock-mantle.us-west-2.api.aws/openai/v1",
            "first-party-key",
        )
    })
}

/// The body `surface` ships.
fn normalize_on(surface: Surface, req: &ChatRequest) -> Value {
    let shape = match surface {
        Surface::Invoke => BedrockApiShape::Invoke,
        Surface::Converse => BedrockApiShape::Converse,
        Surface::MantleAnthropic => {
            return crate::anthropic_api::request::normalize(
                "bedrock-mantle",
                req,
                false,
                None,
                false,
                true,
                true,
            )
            .expect("assembles");
        }
        Surface::MantleOpenaiCompat => {
            return routectl_core::Provider::normalize_request(&mantle_openai_compat(), req)
                .expect("assembles");
        }
    };
    normalize(&cfg(shape), req)
}

fn shipped_metadata_on(surface: Surface, body: &Value) -> Option<Value> {
    match surface {
        Surface::Invoke | Surface::MantleAnthropic | Surface::MantleOpenaiCompat => {
            body.get("metadata").cloned()
        }
        Surface::Converse => shipped_metadata(BedrockApiShape::Converse, body),
    }
}

/// An Anthropic-ingress request as an attempt reaches the egress: `merged`
/// is the attempt's `provider_extras` after every dispatch-time merge, strip
/// and revert, and `operator` the operator layer the router recorded. As the
/// ingress does, the canonical `user` mirrors a string `metadata.user_id`.
fn attempt(merged: Value, operator: Option<Value>) -> ChatRequest {
    let mut req = request();
    req.user = merged
        .pointer("/metadata/user_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    req.provider_extras = Some(merged);
    req.routectl_internal.operator_payload_extras = operator.map(Arc::new);
    req.routectl_internal.provenance = RequestProvenance::AnthropicIngress;
    req
}

/// Every client-supplied value below carries this marker.
const CLIENT_MARK: &str = "u-client";

struct MetadataRow {
    name: &'static str,
    merged: Value,
    payload: Option<Value>,
    /// What an Anthropic-shaped surface ships: the operator members the
    /// attempt still carries.
    want: Option<Value>,
    /// What openai-compat ships: the operator's recorded value whole, for a
    /// key the attempt still carries.
    openai_compat_want: Option<Value>,
}

fn metadata_rows() -> Vec<MetadataRow> {
    let row = |name, merged, payload, want, openai_compat_want| MetadataRow {
        name,
        merged,
        payload,
        want,
        openai_compat_want,
    };
    vec![
        row(
            "client member beside an operator member",
            json!({"metadata": {"user_id": "u-client-1", "trace": "op-trace"}}),
            Some(json!({"metadata": {"trace": "op-trace"}})),
            Some(json!({"trace": "op-trace"})),
            Some(json!({"trace": "op-trace"})),
        ),
        row(
            "client object alone",
            json!({"metadata": {"user_id": "u-client-1", "account_uuid": "u-client-2"}}),
            None,
            None,
            None,
        ),
        row(
            "operator scalar over a client object",
            json!({"metadata": "op-tag"}),
            Some(json!({"metadata": "op-tag"})),
            Some(json!("op-tag")),
            Some(json!("op-tag")),
        ),
        row(
            "operator scalar, the slot holds the client's object",
            json!({"metadata": {"user_id": "u-client-1"}}),
            Some(json!({"metadata": "op-tag"})),
            None,
            Some(json!("op-tag")),
        ),
        row(
            "nested members: only the operator's nested member ships",
            json!({"metadata": {
                "user_id": "u-client-1",
                "tags": {"user": "u-client-3", "env": "prod"}
            }}),
            Some(json!({"metadata": {"tags": {"env": "prod"}}})),
            Some(json!({"tags": {"env": "prod"}})),
            Some(json!({"tags": {"env": "prod"}})),
        ),
        row(
            "operator empty object",
            json!({"metadata": {"user_id": "u-client-1"}}),
            Some(json!({"metadata": {}})),
            Some(json!({})),
            Some(json!({})),
        ),
        row(
            "revert of the only operator member",
            json!({"metadata": {"user_id": "u-client-1"}}),
            Some(json!({"metadata": {"trace": "op-trace"}})),
            None,
            Some(json!({"trace": "op-trace"})),
        ),
        row(
            "revert of one of two operator members",
            json!({"metadata": {"user_id": "u-client-1", "trace": "op-trace"}}),
            Some(json!({"metadata": {"trace": "op-trace", "team": "op-team"}})),
            Some(json!({"trace": "op-trace"})),
            Some(json!({"trace": "op-trace", "team": "op-team"})),
        ),
        row(
            "revert restored the client's value of an operator member",
            json!({"metadata": {"team": "u-client-team", "trace": "op-trace"}}),
            Some(json!({"metadata": {"trace": "op-trace", "team": "op-team"}})),
            Some(json!({"trace": "op-trace"})),
            Some(json!({"trace": "op-trace", "team": "op-team"})),
        ),
        row(
            "merged extras lack the key: nothing from payload_extras",
            json!({}),
            Some(json!({"metadata": {"trace": "op-trace"}})),
            None,
            None,
        ),
    ]
}

#[test]
fn only_operator_metadata_the_attempt_still_carries_reaches_any_anthropic_shaped_bedrock_surface() {
    let mut failures = Vec::new();
    for surface in ANTHROPIC_SHAPED {
        for row in metadata_rows() {
            let req = attempt(row.merged.clone(), row.payload.clone());

            let body = normalize_on(surface, &req);

            let got = shipped_metadata_on(surface, &body);
            let serialized = body.to_string();
            if got != row.want {
                failures.push(format!("{surface:?} / {}: shipped {got:?}", row.name));
            }
            if serialized.contains(CLIENT_MARK) || serialized.contains("user_id") {
                failures.push(format!(
                    "{surface:?} / {}: client member in {serialized}",
                    row.name
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn no_client_identity_reaches_the_mantle_openai_compat_lane() {
    let mut failures = Vec::new();
    for row in metadata_rows() {
        let req = attempt(row.merged.clone(), row.payload.clone());

        let body = normalize_on(Surface::MantleOpenaiCompat, &req);

        let serialized = body.to_string();
        if body.get("user").is_some()
            || serialized.contains(CLIENT_MARK)
            || serialized.contains("user_id")
        {
            failures.push(format!("{}: client identity in {serialized}", row.name));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn the_mantle_openai_compat_lane_ships_the_operator_metadata_whole_while_the_attempt_carries_the_key()
 {
    let mut failures = Vec::new();
    for row in metadata_rows() {
        let req = attempt(row.merged.clone(), row.payload.clone());

        let body = normalize_on(Surface::MantleOpenaiCompat, &req);

        let got = shipped_metadata_on(Surface::MantleOpenaiCompat, &body);
        if got != row.openai_compat_want {
            failures.push(format!("{}: shipped {got:?}", row.name));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Positive control for the mantle absence pin: the same assembly off the
/// mantle lane forwards the client's block, so the fixture does reach the
/// body when nothing withholds it.
#[test]
fn the_first_party_anthropic_lane_still_carries_the_client_metadata_fixture() {
    let req = attempt(json!({"metadata": {"user_id": "u-client-1"}}), None);

    let body = crate::anthropic_api::request::normalize("p", &req, false, None, false, true, false)
        .expect("assembles");

    assert_eq!(body["metadata"], json!({"user_id": "u-client-1"}), "{body}");
}

fn metadata_strip_edits_on(surface: Surface, mut req: ChatRequest) -> Vec<(EditKind, String)> {
    let tap = WireTap::default();
    req.routectl_internal.wire_tap = Some(tap.clone());
    normalize_on(surface, &req);
    tap.take_request()
        .edits(Leg::Request)
        .iter()
        .filter(|e| {
            e.site() == EditSite::ClientFingerprintStrip
                && (e.path().starts_with("/metadata") || e.path() == "/user")
        })
        .map(|e| (e.kind(), e.path().to_string()))
        .collect()
}

struct DeclaredRow {
    name: &'static str,
    merged: Value,
    operator: Option<Value>,
    /// The fingerprint removals an Anthropic-shaped surface declares.
    metadata_paths: &'static [&'static str],
    /// The fingerprint removals openai-compat declares: its `metadata`
    /// withhold sits under the managed-extras site, so only the canonical
    /// `user` the ingress mirrored from `metadata.user_id` shows here.
    openai_compat_paths: &'static [&'static str],
}

fn declared_rows() -> [DeclaredRow; 4] {
    [
        DeclaredRow {
            name: "client only: the whole block",
            merged: json!({"metadata": {"user_id": "u-client-1"}}),
            operator: None,
            metadata_paths: &["/metadata"],
            openai_compat_paths: &["/user"],
        },
        DeclaredRow {
            name: "nested client member beside the operator's nested member",
            merged: json!({"metadata": {"tags": {"user": "u-client-3", "env": "prod"}}}),
            operator: Some(json!({"metadata": {"tags": {"env": "prod"}}})),
            metadata_paths: &["/metadata/*/*"],
            openai_compat_paths: &[],
        },
        DeclaredRow {
            name: "top-level and nested client members",
            merged: json!({"metadata": {"user_id": "u-client-1", "tags": {"user": "u-client-3", "env": "prod"}}}),
            operator: Some(json!({"metadata": {"tags": {"env": "prod"}}})),
            metadata_paths: &["/metadata/*", "/metadata/*/*"],
            openai_compat_paths: &["/user"],
        },
        DeclaredRow {
            name: "the operator's own value: nothing",
            merged: json!({"metadata": {"trace": "op-trace"}}),
            operator: Some(json!({"metadata": {"trace": "op-trace"}})),
            metadata_paths: &[],
            openai_compat_paths: &[],
        },
    ]
}

fn removals(paths: &[&str]) -> Vec<(EditKind, String)> {
    paths
        .iter()
        .map(|p| (EditKind::Remove, (*p).to_string()))
        .collect()
}

#[test]
fn each_withheld_client_member_is_declared_at_its_depth_on_every_anthropic_shaped_surface() {
    for surface in ANTHROPIC_SHAPED {
        for row in declared_rows() {
            let req = attempt(row.merged.clone(), row.operator.clone());

            let edits = metadata_strip_edits_on(surface, req);

            assert_eq!(
                edits,
                removals(row.metadata_paths),
                "{surface:?} / {}",
                row.name
            );
        }
    }
}

#[test]
fn the_mantle_openai_compat_lane_declares_the_withheld_client_user_at_user() {
    for row in declared_rows() {
        let req = attempt(row.merged.clone(), row.operator.clone());

        let edits = metadata_strip_edits_on(Surface::MantleOpenaiCompat, req);

        assert_eq!(edits, removals(row.openai_compat_paths), "{}", row.name);
    }
}

const BETA_TRACE: &str = "bedrock final anthropic_beta set";

#[test]
fn final_beta_set_traced_once_per_request() {
    for shape in SHAPES {
        let cfg = cfg(shape);
        let mut req = request();
        req.anthropic_beta = vec![
            "context-1m-2025-08-07".into(),
            "client-flag\u{1b}[31m".into(),
            "withheld-flag".into(),
        ];
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
