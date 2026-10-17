//! The closed credential-reason table, driven through the shared read: every
//! recorded capture row recognizes its reason, and nothing short of that
//! exact evidence does.

use super::{CarriedBody, UpstreamErrorRead};
use reqwest::header::HeaderMap;
use routectl_core::UpstreamReason::{self, CredentialExpired, CredentialInvalid};
use serde_json::Value;

pub const NATIVE_CAPTURE: &str =
    include_str!("../tests/fixtures/bedrock_native_credential_capture.json");
pub const MANTLE_CAPTURE: &str =
    include_str!("../tests/fixtures/bedrock_mantle_credential_capture.json");
pub const IAM_DENY_403: &str =
    include_str!("../tests/fixtures/bedrock/error_403_access_denied.json");

/// One recorded response from a capture fixture.
pub struct Captured {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: String,
}

impl Captured {
    /// The read as an AWS-backed (Bedrock or Mantle) seat sees it.
    pub fn read(&self) -> UpstreamErrorRead {
        self.read_on_seat(true)
    }

    /// The read as a seat that is (`true`) or is not AWS-backed sees it.
    pub fn read_on_seat(&self, aws_backed: bool) -> UpstreamErrorRead {
        UpstreamErrorRead::from_parts(self.status, &self.headers, self.body.clone(), false)
            .aws_backed(aws_backed)
    }
}

fn responses(fixture: &str) -> serde_json::Map<String, Value> {
    let doc: Value = serde_json::from_str(fixture).expect("capture fixture is JSON");
    doc["responses"]
        .as_object()
        .expect("capture fixture has a responses object")
        .clone()
}

/// The response recorded under `key`, with its `x-amzn-errortype` header.
pub fn captured(fixture: &str, key: &str) -> Captured {
    let row = responses(fixture)
        .remove(key)
        .unwrap_or_else(|| panic!("no capture row {key}"));
    let mut headers = HeaderMap::new();
    if let Some(value) = row["x-amzn-errortype"].as_str() {
        headers.insert("x-amzn-errortype", value.parse().expect("header value"));
    }
    Captured {
        status: u16::try_from(row["status"].as_u64().expect("status")).expect("u16 status"),
        headers,
        body: row["body"].as_str().expect("body").to_string(),
    }
}

pub fn headers_with_type(value: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("x-amzn-errortype", value.parse().expect("header value"));
    headers
}

fn reason_of(read: UpstreamErrorRead) -> Option<UpstreamReason> {
    read.into_error("p", CarriedBody::Original, None)
        .upstream_reason()
}

/// Every capture row and the reason it is evidence of.
pub const CAPTURE_ROWS: [(&str, &str, UpstreamReason); 12] = [
    (NATIVE_CAPTURE, "expired-invoke", CredentialExpired),
    (NATIVE_CAPTURE, "expired-converse", CredentialExpired),
    (NATIVE_CAPTURE, "invalid-invoke", CredentialInvalid),
    (NATIVE_CAPTURE, "invalid-converse", CredentialInvalid),
    (MANTLE_CAPTURE, "mantle-chat-expired", CredentialExpired),
    (
        MANTLE_CAPTURE,
        "mantle-responses-expired",
        CredentialExpired,
    ),
    (
        MANTLE_CAPTURE,
        "mantle-anthropic-expired",
        CredentialExpired,
    ),
    (
        MANTLE_CAPTURE,
        "mantle-chat-expired-security-token",
        CredentialExpired,
    ),
    (
        MANTLE_CAPTURE,
        "mantle-responses-expired-security-token",
        CredentialExpired,
    ),
    (
        MANTLE_CAPTURE,
        "mantle-anthropic-expired-security-token",
        CredentialExpired,
    ),
    (MANTLE_CAPTURE, "mantle-chat-invalid", CredentialInvalid),
    (
        MANTLE_CAPTURE,
        "mantle-anthropic-invalid",
        CredentialInvalid,
    ),
];

#[test]
fn every_capture_row_recognizes_its_reason() {
    for (fixture, key, want) in CAPTURE_ROWS {
        let got = reason_of(captured(fixture, key).read());

        assert_eq!(got, Some(want), "{key}");
    }
}

#[test]
fn the_row_table_covers_every_recorded_response() {
    let mut recorded: Vec<String> = [NATIVE_CAPTURE, MANTLE_CAPTURE]
        .into_iter()
        .flat_map(|fixture| responses(fixture).into_iter().map(|(key, _)| key))
        .collect();
    let mut rows: Vec<String> = CAPTURE_ROWS
        .iter()
        .map(|(_, key, _)| (*key).to_string())
        .collect();
    recorded.sort();
    rows.sort();

    assert_eq!(rows, recorded);
}

#[test]
fn a_capture_body_truncated_at_the_cap_is_not_evidence() {
    let row = captured(NATIVE_CAPTURE, "expired-invoke");

    let got = reason_of(
        UpstreamErrorRead::from_parts(row.status, &row.headers, row.body, true).aws_backed(true),
    );

    assert_eq!(got, None);
}

#[test]
fn evidence_short_of_a_recorded_row_yields_no_reason() {
    let deny = "AccessDeniedException:http://internal.amazon.com/coral/com.amazon.coral.service/";
    let none = HeaderMap::new();
    let cases: [(&str, u16, HeaderMap, &str); 20] = [
        ("status 401 alone", 401, none.clone(), ""),
        ("status 403 alone", 403, none.clone(), ""),
        (
            "bare AccessDeniedException, IAM deny body",
            403,
            headers_with_type(deny),
            IAM_DENY_403,
        ),
        (
            "bare AccessDeniedException, no message",
            403,
            headers_with_type(deny),
            "{}",
        ),
        (
            "expired message without the AccessDeniedException type",
            403,
            none.clone(),
            r#"{"Message":"Bearer Token has expired"}"#,
        ),
        (
            "expired message under another AWS type",
            403,
            headers_with_type("ValidationException"),
            r#"{"Message":"Bearer Token has expired"}"#,
        ),
        (
            "native expired message on a 401",
            401,
            headers_with_type(deny),
            r#"{"Message":"Bearer Token has expired"}"#,
        ),
        (
            "native message with trailing text",
            403,
            headers_with_type(deny),
            r#"{"Message":"Bearer Token has expired."}"#,
        ),
        (
            "permission_error carrying a recorded message",
            403,
            none.clone(),
            r#"{"type":"error","error":{"type":"permission_error","message":"Invalid bearer token"}}"#,
        ),
        (
            "anthropic type with an openai code (conflicting vocabularies)",
            401,
            none.clone(),
            r#"{"error":{"type":"authentication_error","code":"invalid_api_key","message":"Invalid bearer token"}}"#,
        ),
        (
            "openai code with a foreign type (conflicting vocabularies)",
            401,
            none.clone(),
            r#"{"error":{"type":"authentication_error_x","code":"invalid_api_key","message":"Invalid bearer token"}}"#,
        ),
        (
            "openai type without the invalid_api_key code",
            401,
            none.clone(),
            r#"{"error":{"type":"permission_denied_error","message":"Invalid bearer token"}}"#,
        ),
        (
            "recorded mantle message on a 403",
            403,
            none.clone(),
            r#"{"error":{"type":"authentication_error","message":"Invalid bearer token"}}"#,
        ),
        (
            "the invalid security token message (also an opt-in-region answer)",
            401,
            none.clone(),
            r#"{"error":{"code":"invalid_api_key","message":"The security token included in the request is invalid"}}"#,
        ),
        (
            "a reason token inside an unrecognized mantle body",
            401,
            none.clone(),
            r#"{"error":{"code":"invalid_api_key","message":"credential-expired"}}"#,
        ),
        (
            "a reason token inside an unrecognized native body",
            403,
            headers_with_type(deny),
            r#"{"Message":"credential-invalid"}"#,
        ),
        (
            "signature prefix in another case",
            401,
            none.clone(),
            r#"{"error":{"type":"authentication_error","message":"signature expired: 20261016T163056Z"}}"#,
        ),
        (
            "signature prefix not at the start",
            401,
            none.clone(),
            r#"{"error":{"type":"authentication_error","message":"x Signature expired: 20261016T163056Z"}}"#,
        ),
        (
            "signature phrase without the prefix delimiter",
            401,
            none.clone(),
            r#"{"error":{"type":"authentication_error","message":"Signature expired"}}"#,
        ),
        (
            "native message under both message keys, disagreeing",
            403,
            headers_with_type(deny),
            r#"{"Message":"Bearer Token has expired","message":"Authentication failed: Please make sure your API Key is valid."}"#,
        ),
    ];
    for (name, status, headers, body) in cases {
        let got = reason_of(
            UpstreamErrorRead::from_parts(status, &headers, body, false).aws_backed(true),
        );

        assert_eq!(got, None, "{name}");
    }
}

#[test]
fn every_table_row_cites_a_capture_that_recognizes_its_reason() {
    for row in &super::REASON_TABLE {
        let (file, key) = row.evidence;
        let fixture = match file {
            "bedrock_native_credential_capture.json" => NATIVE_CAPTURE,
            "bedrock_mantle_credential_capture.json" => MANTLE_CAPTURE,
            other => panic!("row cites an uncommitted fixture {other}"),
        };

        let got = reason_of(captured(fixture, key).read());

        assert_eq!(got, Some(row.reason), "{file}#{key}");
    }
}

/// A 403 row for the carried-body check: the `x-amzn-errortype` header
/// value, the body, and the message of the envelope it must be carried in,
/// or `None` when it is not AWS-shaped and keeps the plain-text carry.
pub type Carry403Row = (
    &'static str,
    Option<&'static str>,
    &'static str,
    Option<&'static str>,
);

/// The plain-text carry a non-AWS 403 keeps: no parse, so the ingress shows
/// the bare status.
pub const PLAIN_403_TEXT: &str = "bedrock access denied";

pub const CARRY_403_ROWS: [Carry403Row; 5] = [
    (
        "html proxy page, no header",
        None,
        "<html>Forbidden</html>",
        None,
    ),
    (
        "json proxy body, no type or action",
        None,
        r#"{"message":"Forbidden"}"#,
        None,
    ),
    ("empty body, no header", None, "", None),
    (
        "type from the header",
        Some("AccessDeniedException:http://internal.amazon.com/coral/"),
        "<html>Forbidden</html>",
        Some("bedrock access denied (AccessDeniedException)"),
    ),
    (
        "IAM action, no type",
        None,
        r#"{"message":"User: arn:aws:iam::123456789012:role/App is not authorized to perform: bedrock:InvokeModel on resource: x"}"#,
        Some("bedrock access denied: missing IAM action bedrock:InvokeModel"),
    ),
];

/// The read for a [`CARRY_403_ROWS`] row.
pub fn carry_403_read(header: Option<&str>, body: &str) -> UpstreamErrorRead {
    let headers = header.map_or_else(HeaderMap::new, headers_with_type);
    UpstreamErrorRead::from_parts(403, &headers, body, false)
}

/// Assert `err` carries what `row` expects: the built envelope for an
/// AWS-shaped 403, else the plain text with no parse.
pub fn assert_403_carry(lane: &str, row: &Carry403Row, err: &routectl_core::Error) {
    let (name, _, _, want) = row;
    let routectl_core::Error::Upstream { body, .. } = err else {
        panic!("{lane} {name}: expected Error::Upstream, got {err:?}");
    };
    match want {
        Some(message) => {
            let carried = err
                .parsed_body()
                .and_then(routectl_core::ParsedErrorBody::error_message);
            assert_eq!(carried, Some(*message), "{lane} {name}");
        }
        None => {
            assert!(
                err.parsed_body().is_none(),
                "{lane} {name}: plain carry has no parse"
            );
            assert_eq!(body, PLAIN_403_TEXT, "{lane} {name}");
        }
    }
}

/// A 403 whose message names a principal: the body, and the scrubbed
/// message its envelope must carry.
pub type Principal403Row = (&'static str, &'static str, &'static str);

pub const PRINCIPAL_403_ROWS: [Principal403Row; 5] = [
    (
        "nested envelope without a type naming the principal",
        r#"{"error":{"message":"User: arn:aws:iam::123456789012:user/alice is not authorized to perform: bedrock:InvokeModel on resource: arn:aws:bedrock:us-east-1:123456789012:foundation-model/x"}}"#,
        "bedrock access denied: missing IAM action bedrock:InvokeModel",
    ),
    (
        "first-party envelope naming an assumed role",
        r#"{"error":{"type":"permission_denied_error","message":"User: arn:aws:sts::123456789012:assumed-role/R/s is not authorized to perform: bedrock-mantle:CreateInference on resource: x"}}"#,
        "bedrock access denied (permission_denied_error): missing IAM action bedrock-mantle:CreateInference",
    ),
    (
        "first-party envelope naming only an account id",
        r#"{"type":"error","error":{"type":"permission_error","message":"account 123456789012 is not enabled"}}"#,
        "bedrock access denied (permission_error)",
    ),
    (
        "markers spelled with JSON escapes",
        r#"{"error":{"type":"permission_denied_error","message":"\u0055ser: \u0061rn:aws:iam::12345\u0036789012:user/alice is not authorized"}}"#,
        "bedrock access denied (permission_denied_error)",
    ),
    (
        "principal named by the Principal field with an ARN",
        r#"{"error":{"type":"permission_denied_error","message":"Principal: arn:aws-us-gov:iam::123456789012:role/R is not authorized"}}"#,
        "bedrock access denied (permission_denied_error)",
    ),
];

/// A first-party 403 naming no principal: carried as read, message intact.
pub const FIRST_PARTY_403: &str = r#"{"type":"error","error":{"type":"permission_error","message":"you do not have permission"}}"#;

pub fn has_account_id_run(text: &str) -> bool {
    text.as_bytes()
        .windows(12)
        .any(|w| w.iter().all(u8::is_ascii_digit))
}

/// Assert no surface of `text` names a principal.
pub fn assert_no_principal(surface: &str, text: &str) {
    assert!(!text.contains("arn:"), "{surface} leaked an ARN: {text}");
    assert!(
        !text.contains("Principal:"),
        "{surface} leaked the principal: {text}"
    );
    assert!(
        !has_account_id_run(text),
        "{surface} leaked an account id: {text}"
    );
    assert!(
        !text.contains("User:"),
        "{surface} leaked the principal: {text}"
    );
}

/// Assert a principal-bearing 403 was carried as the scrubbed envelope and
/// that neither the carried body nor any captured log field names the
/// principal.
pub fn assert_principal_scrubbed(
    lane: &str,
    row: &Principal403Row,
    err: &routectl_core::Error,
    events: &[routectl_testkit::CapturedEvent],
) {
    let (name, _, want) = row;
    let routectl_core::Error::Upstream { body, .. } = err else {
        panic!("{lane} {name}: expected Error::Upstream, got {err:?}");
    };
    let carried = err
        .parsed_body()
        .and_then(routectl_core::ParsedErrorBody::error_message);
    assert_eq!(carried, Some(*want), "{lane} {name}");
    assert_no_principal(&format!("{lane} {name} carried body"), body);
    let logged: Vec<&str> = events
        .iter()
        .flat_map(|e| e.fields.iter().map(|(_, v)| v.as_str()))
        .collect();
    assert!(
        events.iter().any(|e| e.level == tracing::Level::WARN),
        "{lane} {name}: the failure WARN fired"
    );
    for value in logged {
        assert_no_principal(&format!("{lane} {name} log field"), value);
    }
}

#[test]
fn an_aws_403_envelope_never_carries_the_lane_message() {
    let row = &PRINCIPAL_403_ROWS[0];
    let read = UpstreamErrorRead::from_parts(403, &HeaderMap::new(), row.1, false);
    let lane_message = read.text().to_string();

    let carried = read.aws_carried(lane_message);

    let CarriedBody::Built(body) = carried else {
        panic!("an AWS-shaped 403 is carried as the built envelope");
    };
    let message = body
        .parsed()
        .and_then(routectl_core::ParsedErrorBody::error_message);
    assert_eq!(message, Some(row.2));
    assert_no_principal("carried envelope", body.text());
}

#[test]
fn a_capture_row_on_a_seat_that_is_not_aws_backed_yields_no_reason() {
    for (fixture, key, _) in CAPTURE_ROWS {
        let got = reason_of(captured(fixture, key).read_on_seat(false));

        assert_eq!(got, None, "{key}");
    }
}

#[test]
fn an_unbounded_nested_code_on_a_403_never_reaches_the_error() {
    let body = r#"{"error":{"type":"permission_denied_error","code":"arn:aws:iam::123456789012:user/alice","message":"denied"}}"#;
    for aws_backed in [false, true] {
        let read = UpstreamErrorRead::from_parts(403, &HeaderMap::new(), body, false)
            .aws_backed(aws_backed);

        let err = read.into_error("p", CarriedBody::Original, None);

        let routectl_core::Error::Upstream {
            upstream_type,
            upstream_code,
            ..
        } = &err
        else {
            panic!("expected Error::Upstream");
        };
        assert_eq!(upstream_code.as_deref(), None, "aws_backed={aws_backed}");
        assert_eq!(
            upstream_type.as_deref(),
            Some("permission_denied_error"),
            "aws_backed={aws_backed}: a bounded type survives"
        );
    }
}

#[test]
fn a_first_party_403_keeps_its_nested_tokens() {
    let read = UpstreamErrorRead::from_parts(403, &HeaderMap::new(), FIRST_PARTY_403, false);

    let (upstream_type, _) = read.tokens();

    assert_eq!(upstream_type.as_deref(), Some("permission_error"));
}

#[test]
fn a_principal_in_any_string_leaf_needs_the_scrub_on_a_non_aws_seat() {
    let cases = [
        (
            "string-valued error",
            r#"{"error":"User: arn:aws:iam::123456789012:user/alice"}"#,
            true,
        ),
        (
            "ARN in error.param",
            r#"{"error":{"type":"permission_denied_error","message":"denied","param":"arn:aws:iam::123456789012:user/alice"}}"#,
            true,
        ),
        (
            "Principal ARN in a sibling of error",
            r#"{"error":{"type":"permission_error","message":"denied"},"detail":{"why":"Principal: arn:aws:iam::123456789012:role/R"}}"#,
            true,
        ),
        (
            "escaped ARN inside an array leaf",
            r#"{"error":{"type":"permission_error","message":"denied","hints":["\u0061rn:aws:iam::123456789012:role/R"]}}"#,
            true,
        ),
        (
            "assumed-role path after an arn",
            r#"{"error":{"type":"permission_error","message":"arn:aws:sts::123456789012:assumed-role/R/s"}}"#,
            true,
        ),
        (
            "account id after the word account",
            r#"{"error":{"type":"permission_error","message":"account 123456789012 is not enabled"}}"#,
            true,
        ),
        (
            "first-party 403 naming no principal",
            FIRST_PARTY_403,
            false,
        ),
    ];
    for (name, body, want) in cases {
        let read = UpstreamErrorRead::from_parts(403, &HeaderMap::new(), body, false);

        assert_eq!(read.needs_access_denied_scrub(), want, "{name}");
    }
}

/// 403 bodies on a non-AWS seat that name no AWS principal, though they hold
/// loose markers: each is carried as read, its message intact.
pub const LOOSE_MARKER_403_ROWS: [(&str, &str); 6] = [
    (
        "arn:aws inside a longer word",
        r#"{"error":{"type":"permission_error","message":"the setting yarn:aws:plugin is not allowed"}}"#,
    ),
    (
        "moderation 403 echoing the flagged input",
        r#"{"error":{"code":403,"message":"Your input was flagged","metadata":{"reasons":["harassment"],"flagged_input":"User: call me at 447700900123 ...","provider_name":"x","model_slug":"y"}}}"#,
    ),
    (
        "anthropic 403 with a request id sibling",
        r#"{"type":"error","error":{"type":"permission_error","message":"Your API key does not have permission to use the specified resource."},"request_id":"req_011CQ123456789012345678"}"#,
    ),
    (
        "a word ending in arn:",
        r#"{"error":{"type":"permission_error","message":"learn: this key cannot call the model; warn: retry later"}}"#,
    ),
    (
        "a millisecond timestamp",
        r#"{"error":{"type":"permission_error","message":"denied at 1760671234567"}}"#,
    ),
    (
        "an arn of another scheme",
        r#"{"error":{"type":"permission_error","message":"urn:uuid:x or arn:partner:thing is not allowed"}}"#,
    ),
];

#[test]
fn a_403_with_only_loose_markers_on_a_non_aws_seat_needs_no_scrub() {
    for (name, body) in LOOSE_MARKER_403_ROWS {
        let read = UpstreamErrorRead::from_parts(403, &HeaderMap::new(), body, false);

        assert!(!read.needs_access_denied_scrub(), "{name}");
    }
}
