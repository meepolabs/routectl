//! The scope table read through the shared upstream read: the recorded bare
//! 429 is recognized on the Anthropic OAuth lane, and the clauses no wire
//! row can isolate are pinned here directly.

use reqwest::header::HeaderMap;
use routectl_core::ScopeRow;
use serde_json::Value;

use super::ScopeLane;
use crate::upstream_error::{CarriedBody, UpstreamErrorRead};

pub const BARE_429_CAPTURE: &str =
    include_str!("../tests/fixtures/anthropic_api/bare_429_capture.json");

/// One recorded response from the bare-429 capture.
pub struct Recorded {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Recorded {
    pub fn header_map(&self) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in &self.headers {
            map.append(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).expect("header name"),
                value.parse().expect("header value"),
            );
        }
        map
    }
}

/// The recorded bare 429.
pub fn bare_429() -> Recorded {
    let doc: Value = serde_json::from_str(BARE_429_CAPTURE).expect("capture fixture is JSON");
    let row = &doc["responses"]["oauth-bare-429"];
    let headers = row["headers"]
        .as_array()
        .expect("headers list")
        .iter()
        .map(|pair| {
            (
                pair[0].as_str().expect("header name").to_string(),
                pair[1].as_str().expect("header value").to_string(),
            )
        })
        .collect();
    Recorded {
        status: u16::try_from(row["status"].as_u64().expect("status")).expect("u16 status"),
        headers,
        body: row["body"].as_str().expect("body").to_string(),
    }
}

fn scope_row_of(read: UpstreamErrorRead) -> Option<ScopeRow> {
    read.into_error("p", CarriedBody::Original, None)
        .scope_row()
}

fn read(status: u16, headers: &HeaderMap, body: &str, hit_cap: bool) -> UpstreamErrorRead {
    UpstreamErrorRead::from_parts(status, headers, body.to_string(), hit_cap)
}

#[test]
fn fixture_carries_the_recorded_fields() {
    let recorded = bare_429();
    let body: Value = serde_json::from_str(&recorded.body).expect("body is JSON");

    assert_eq!(recorded.status, 429);
    assert!(
        recorded.headers.is_empty(),
        "no reset or rate-limit header was recorded"
    );
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "rate_limit_error");
    assert_eq!(body["error"]["message"], "Error");
    let mut top: Vec<&str> = body
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    top.sort_unstable();
    assert_eq!(top, ["error", "request_id", "type"]);
    let mut inner: Vec<&str> = body["error"]
        .as_object()
        .expect("error object")
        .keys()
        .map(String::as_str)
        .collect();
    inner.sort_unstable();
    assert_eq!(inner, ["message", "type"]);
}

#[test]
fn recorded_bare_429_on_the_oauth_lane_is_recognized() {
    let recorded = bare_429();

    let row = scope_row_of(
        read(
            recorded.status,
            &recorded.header_map(),
            &recorded.body,
            false,
        )
        .scope_lane(Some(ScopeLane::AnthropicOAuth)),
    );

    assert_eq!(row, Some(ScopeRow::AnthropicBareRateLimit));
}

#[test]
fn envelope_without_request_id_is_recognized() {
    let body = r#"{"type":"error","error":{"type":"rate_limit_error","message":"Error"}}"#;

    let row = scope_row_of(
        read(429, &HeaderMap::new(), body, false).scope_lane(Some(ScopeLane::AnthropicOAuth)),
    );

    assert_eq!(row, Some(ScopeRow::AnthropicBareRateLimit));
}

/// The response-body cap trips only on a body far larger than the parse cap,
/// so no wire row can separate this clause; a lane that hands the read a
/// flagged in-cap body is the only way to reach it.
#[test]
fn recorded_body_flagged_as_capped_is_not_recognized() {
    let recorded = bare_429();

    let row = scope_row_of(
        read(
            recorded.status,
            &recorded.header_map(),
            &recorded.body,
            true,
        )
        .scope_lane(Some(ScopeLane::AnthropicOAuth)),
    );

    assert_eq!(row, None);
}

#[test]
fn near_miss_envelopes_are_not_recognized() {
    let rows = [
        (
            "request_id null",
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"Error"},"request_id":null}"#,
        ),
        (
            "top-level type not error",
            r#"{"type":"failure","error":{"type":"rate_limit_error","message":"Error"}}"#,
        ),
        (
            "top-level type missing",
            r#"{"error":{"type":"rate_limit_error","message":"Error"}}"#,
        ),
        (
            "duplicate top-level type",
            r#"{"type":"error","type":"error","error":{"type":"rate_limit_error","message":"Error"}}"#,
        ),
        (
            "duplicate error type",
            r#"{"type":"error","error":{"type":"api_error","type":"rate_limit_error","message":"Error"}}"#,
        ),
        (
            "duplicate error object",
            r#"{"type":"error","error":{"type":"api_error","message":"x"},"error":{"type":"rate_limit_error","message":"Error"}}"#,
        ),
        (
            "message case variant",
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"error"}}"#,
        ),
        (
            "message padded",
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"Error "}}"#,
        ),
        ("not json", "Error"),
    ];
    for (name, body) in rows {
        let row = scope_row_of(
            read(429, &HeaderMap::new(), body, false).scope_lane(Some(ScopeLane::AnthropicOAuth)),
        );

        assert_eq!(row, None, "{name}");
    }
}

#[test]
fn recorded_bare_429_with_no_lane_is_not_recognized() {
    let recorded = bare_429();

    let row = scope_row_of(read(
        recorded.status,
        &recorded.header_map(),
        &recorded.body,
        false,
    ));

    assert_eq!(row, None);
}

/// The shared parse keeps the last of a repeated key, so it reads a
/// duplicated `message` as the bare row; only the strict parse sees the
/// repeat, and the row is rejected.
#[test]
fn duplicate_message_key_is_rejected_though_the_shared_parse_reads_it_as_bare() {
    let body = r#"{"type":"error","error":{"type":"rate_limit_error","message":"Rate limited","message":"Error"}}"#;
    let read =
        read(429, &HeaderMap::new(), body, false).scope_lane(Some(ScopeLane::AnthropicOAuth));
    let parsed = read
        .parsed()
        .expect("the shared parse accepts a repeated key");
    assert_eq!(parsed.error_type(), Some("rate_limit_error"));
    assert_eq!(parsed.error_message(), Some("Error"));

    let row = scope_row_of(read);

    assert_eq!(row, None);
}
