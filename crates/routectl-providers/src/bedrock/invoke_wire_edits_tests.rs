//! The Bedrock Invoke seam declares its own body edits on the attempt's wire
//! tap: the `stream` strip, the `model` field moving into the URL, the
//! top-level cache marker lowered onto a content block, and the body
//! `anthropic_version` Bedrock requires.

use routectl_core::ChatRequest;
use routectl_core::wire_check::{EditKind, EditSite, Leg, WireTap};
use serde_json::json;

use super::normalize_request;
use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};

fn cfg() -> BedrockConfig {
    BedrockConfig {
        id: "invoke-wire-edits".into(),
        region: "us-east-1".into(),
        model_id: "us.anthropic.claude-opus-5-5".into(),
        api_shape: BedrockApiShape::Invoke,
        creds: BedrockCreds::BearerKey { key: "k".into() },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        additional_model_request_fields: None,
    }
}

fn request(extra: serde_json::Value) -> ChatRequest {
    let mut body = json!({
        "model": "claude-x",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
    });
    if let (Some(obj), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
        obj.extend(extra.clone());
    }
    serde_json::from_value(body).expect("fixture is a canonical request")
}

fn site_edits(req: ChatRequest, site: EditSite) -> Vec<(EditKind, String)> {
    let mut req = req;
    let tap = WireTap::default();
    req.routectl_internal.wire_tap = Some(tap.clone());
    normalize_request(&cfg(), &req).expect("assembles");
    tap.take_request()
        .edits(Leg::Request)
        .iter()
        .filter(|edit| edit.site() == site)
        .map(|edit| (edit.kind(), edit.path().to_string()))
        .collect()
}

fn owned(rows: &[(EditKind, &str)]) -> Vec<(EditKind, String)> {
    rows.iter()
        .map(|(kind, path)| (*kind, (*path).to_string()))
        .collect()
}

struct Row {
    name: &'static str,
    site: EditSite,
    firing: fn() -> ChatRequest,
    idle: fn() -> ChatRequest,
    expected: &'static [(EditKind, &'static str)],
}

const ROWS: &[Row] = &[
    Row {
        name: "client stream flag stripped",
        site: EditSite::StreamStrip,
        firing: || request(json!({"stream": true})),
        idle: || request(json!({})),
        expected: &[(EditKind::Remove, "/stream")],
    },
    Row {
        name: "top-level cache marker lowered onto the last block",
        site: EditSite::CacheMarkerLowering,
        firing: || request(json!({"cache_control": {"type": "ephemeral"}})),
        idle: || request(json!({})),
        expected: &[
            (EditKind::Remove, "/cache_control"),
            (EditKind::Add, "/messages/0/content/0/cache_control"),
        ],
    },
    Row {
        name: "top-level cache marker on a string turn rewrites that turn",
        site: EditSite::CacheMarkerLowering,
        firing: || {
            let mut req = request(json!({"cache_control": {"type": "ephemeral"}}));
            req.messages = vec![routectl_core::test_utils::user_msg("hi")].into();
            req
        },
        idle: || {
            let mut req = request(json!({}));
            req.messages = vec![routectl_core::test_utils::user_msg("hi")].into();
            req
        },
        expected: &[
            (EditKind::Remove, "/cache_control"),
            (EditKind::Rewrite, "/messages/0/content"),
        ],
    },
    Row {
        name: "client anthropic_version replaced with the Bedrock value",
        site: EditSite::RequiredFieldDefault,
        firing: || request(json!({"provider_extras": {"anthropic_version": "2023-06-01"}})),
        idle: || request(json!({"provider_extras": {"anthropic_version": "bedrock-2023-05-31"}})),
        expected: &[(EditKind::Rewrite, "/anthropic_version")],
    },
];

#[test]
fn each_invoke_site_declares_its_edit_only_when_it_fires() {
    for row in ROWS {
        let fired = site_edits((row.firing)(), row.site);
        let quiet = site_edits((row.idle)(), row.site);

        assert_eq!(fired, owned(row.expected), "{}: firing", row.name);
        assert!(quiet.is_empty(), "{}: idle {quiet:?}", row.name);
    }
}

#[test]
fn the_model_field_is_declared_moved_into_the_url() {
    let edits = site_edits(request(json!({})), EditSite::UrlFieldRelocate);

    assert_eq!(edits, owned(&[(EditKind::Remove, "/model")]));
}

#[test]
fn an_absent_anthropic_version_is_declared_added() {
    let edits = site_edits(request(json!({})), EditSite::RequiredFieldDefault);

    assert_eq!(edits, owned(&[(EditKind::Add, "/anthropic_version")]));
}

#[test]
fn an_untapped_invoke_body_matches_a_tapped_one() {
    let req = request(json!({"stream": true, "cache_control": {"type": "ephemeral"}}));
    let mut tapped_req = req.clone();
    tapped_req.routectl_internal.wire_tap = Some(WireTap::default());

    let untapped = normalize_request(&cfg(), &req).expect("assembles");
    let tapped = normalize_request(&cfg(), &tapped_req).expect("assembles");

    assert_eq!(untapped, tapped);
}

#[test]
fn a_lowering_refused_for_a_breakpoint_rule_declares_no_lowering_edit() {
    let mut req = request(json!({
        "cache_control": {"type": "ephemeral", "ttl": "5m"},
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "hi"},
            {"type": "some_future_block", "cache_control": {"type": "ephemeral", "ttl": "1h"}}
        ]}],
    }));
    let tap = WireTap::default();
    req.routectl_internal.wire_tap = Some(tap.clone());

    let result = normalize_request(&cfg(), &req);

    let err = result.expect_err("the lowered arrangement breaks the ttl order");
    assert_eq!(
        err.refusal_reason(),
        Some(routectl_core::RefusalReason::CacheTtlOrder)
    );
    let lowering: Vec<_> = tap
        .take_request()
        .edits(Leg::Request)
        .iter()
        .filter(|edit| edit.site() == EditSite::CacheMarkerLowering)
        .map(|edit| (edit.kind(), edit.path().to_string()))
        .collect();
    assert!(lowering.is_empty(), "refused request declared {lowering:?}");
}
