//! The access-denied rendering shared by the client message and the DEBUG
//! body line: which AWS type and reason it names, and the strings the
//! wire-watch refusal rules match on.

use super::{AccessDeniedDetail, access_denied_message, sanitized_debug_body};
use routectl_core::UpstreamReason::{self, CredentialExpired, CredentialInvalid};

const ACTION: &str = "bedrock:InvokeModel";

fn detail(aws_type: Option<&str>, reason: Option<UpstreamReason>) -> AccessDeniedDetail {
    AccessDeniedDetail {
        aws_type: aws_type.map(str::to_string),
        reason,
    }
}

#[test]
fn access_denied_message_renders_type_reason_and_action_segments() {
    let cases: [(&str, AccessDeniedDetail, Option<&str>, &str); 8] = [
        (
            "nothing named",
            detail(None, None),
            None,
            "bedrock access denied",
        ),
        (
            "action only",
            detail(None, None),
            Some(ACTION),
            "bedrock access denied: missing IAM action bedrock:InvokeModel",
        ),
        (
            "bare type",
            detail(Some("AccessDeniedException"), None),
            None,
            "bedrock access denied (AccessDeniedException)",
        ),
        (
            "namespaced type is stripped",
            detail(Some("com.amazonaws.bedrock#AccessDeniedException"), None),
            None,
            "bedrock access denied (AccessDeniedException)",
        ),
        (
            "type and expired reason",
            detail(Some("AccessDeniedException"), Some(CredentialExpired)),
            None,
            "bedrock access denied (AccessDeniedException): credential-expired",
        ),
        (
            "type and invalid reason",
            detail(Some("AccessDeniedException"), Some(CredentialInvalid)),
            None,
            "bedrock access denied (AccessDeniedException): credential-invalid",
        ),
        (
            "every segment, in order",
            detail(Some("AccessDeniedException"), Some(CredentialExpired)),
            Some(ACTION),
            "bedrock access denied (AccessDeniedException): credential-expired: missing IAM action bedrock:InvokeModel",
        ),
        (
            "reason without a type",
            detail(None, Some(CredentialInvalid)),
            None,
            "bedrock access denied: credential-invalid",
        ),
    ];
    for (name, detail, action, want) in cases {
        let got = access_denied_message(&detail, action.map(str::to_string));

        assert_eq!(got, want, "{name}");
    }
}

#[test]
fn an_unbounded_type_omits_the_type_segment() {
    let oversize = "A".repeat(129);
    let cases: [(&str, &str); 6] = [
        ("empty", ""),
        ("space", "Access Denied"),
        ("slash", "role/AppRole"),
        ("arn", "arn:aws:iam::123456789012:role/AppRole"),
        ("namespace with nothing after it", "com.amazonaws.bedrock#"),
        ("oversize", oversize.as_str()),
    ];
    for (name, aws_type) in cases {
        let got = access_denied_message(
            &detail(Some(aws_type), Some(CredentialExpired)),
            Some(ACTION.into()),
        );

        assert_eq!(
            got,
            "bedrock access denied: credential-expired: missing IAM action bedrock:InvokeModel",
            "{name}"
        );
    }
}

#[test]
fn the_debug_body_renders_the_same_detail_as_the_client_message() {
    let body = "User: arn:aws:iam::123456789012:role/App is not authorized to perform: bedrock:InvokeModel on resource: x";
    let named = detail(Some("AccessDeniedException"), Some(CredentialInvalid));

    let got = sanitized_debug_body(403, body, &named);

    assert_eq!(got, access_denied_message(&named, Some(ACTION.into())));
}

/// The refusal-rule strings the wire-watch tests key on, as the native lane
/// renders them for the committed captures.
mod weld {
    use crate::upstream_error::UpstreamErrorRead;
    use crate::upstream_error::reason_tests::{
        IAM_DENY_403, NATIVE_CAPTURE, captured, headers_with_type,
    };
    use serde_json::Value;

    const DETAIL_STRINGS: &str = include_str!("../tests/fixtures/bedrock_403_detail_strings.json");

    fn debug_body(read: &UpstreamErrorRead) -> String {
        super::sanitized_debug_body(read.status(), read.text(), &read.access_denied_detail())
    }

    #[test]
    fn detail_strings_fixture_is_what_the_native_lane_renders() {
        let deny = headers_with_type("AccessDeniedException:urn:example:ignored-suffix");
        let rendered = [
            (
                "expired",
                debug_body(&captured(NATIVE_CAPTURE, "expired-invoke").read()),
            ),
            (
                "invalid",
                debug_body(&captured(NATIVE_CAPTURE, "invalid-converse").read()),
            ),
            (
                "model_access",
                debug_body(&UpstreamErrorRead::from_parts(
                    403,
                    &deny,
                    IAM_DENY_403,
                    false,
                )),
            ),
            (
                "untyped",
                debug_body(&UpstreamErrorRead::from_parts(
                    403,
                    &reqwest::header::HeaderMap::new(),
                    "",
                    false,
                )),
            ),
        ];
        let fixture: Value = serde_json::from_str(DETAIL_STRINGS).expect("fixture is JSON");
        let strings = fixture["strings"].as_object().expect("strings object");

        assert_eq!(
            strings.len(),
            rendered.len(),
            "one fixture string per rendered row"
        );
        for (key, got) in rendered {
            assert_eq!(strings[key].as_str(), Some(got.as_str()), "{key}");
        }
    }
}

#[test]
fn an_account_id_shaped_type_omits_the_type_segment() {
    const ACCOUNT_ID: &str = "123456789012";
    let cases = [
        ("bare account id", ACCOUNT_ID.to_string()),
        ("account id after a name", format!("Denied-{ACCOUNT_ID}")),
        ("namespaced", format!("com.amazonaws#{ACCOUNT_ID}Exception")),
    ];
    for (name, aws_type) in cases {
        let got = access_denied_message(&detail(Some(&aws_type), None), None);

        assert_eq!(got, "bedrock access denied", "{name}");
    }
    let eleven = access_denied_message(&detail(Some("Code12345678901"), None), None);
    assert_eq!(
        eleven, "bedrock access denied (Code12345678901)",
        "11 digits is not an account id"
    );
}
