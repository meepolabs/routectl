//! Tests for the typed upstream reason carried on `Error::Upstream`: it
//! survives the request-id rebuild in either order, reads `None` off the
//! variant, keeps the boxed tail unallocated when nothing is carried, and
//! renders as its token in `Debug`.

use super::Error;
use crate::UpstreamReason;

const REASONS: [UpstreamReason; 2] = [
    UpstreamReason::CredentialExpired,
    UpstreamReason::CredentialInvalid,
];

fn upstream_with_parse() -> Error {
    Error::upstream("prov", 401, r#"{"error":{"type":"authentication_error"}}"#)
}

fn assert_carries_all(err: &Error, reason: UpstreamReason) {
    assert_eq!(err.upstream_reason(), Some(reason));
    assert_eq!(err.upstream_request_id(), Some("req-1"));
    let parsed = err.parsed_body().expect("parse survives");
    assert_eq!(parsed.error_type(), Some("authentication_error"));
}

#[test]
fn reason_survives_a_later_request_id() {
    for reason in REASONS {
        let err = upstream_with_parse()
            .with_upstream_reason(Some(reason))
            .with_upstream_request_id(Some("req-1".to_string()));

        assert_carries_all(&err, reason);
    }
}

#[test]
fn request_id_survives_a_later_reason() {
    for reason in REASONS {
        let err = upstream_with_parse()
            .with_upstream_request_id(Some("req-1".to_string()))
            .with_upstream_reason(Some(reason));

        assert_carries_all(&err, reason);
    }
}

#[test]
fn upstream_reason_is_none_when_unset() {
    let err = upstream_with_parse().with_upstream_request_id(Some("req-1".to_string()));

    assert_eq!(err.upstream_reason(), None);
}

#[test]
fn with_upstream_reason_is_a_noop_off_the_upstream_variant() {
    let err = Error::Validation("bad".to_string())
        .with_upstream_reason(Some(UpstreamReason::CredentialExpired));

    assert_eq!(err.upstream_reason(), None);
    assert_eq!(format!("{err:?}"), "Validation(\"bad\")");
}

#[test]
fn extras_stay_unallocated_when_nothing_is_carried() {
    let err = Error::upstream("prov", 500, "boom")
        .with_upstream_request_id(None)
        .with_upstream_reason(None);

    let Error::Upstream { extras, .. } = &err else {
        panic!("expected Error::Upstream, got {err:?}");
    };
    assert!(extras.0.is_none());
}

#[test]
fn upstream_debug_renders_the_reason_token_only_when_carried() {
    for reason in REASONS {
        let err = upstream_with_parse().with_upstream_reason(Some(reason));

        let rendered = format!("{err:?}");

        let field = format!("upstream_reason: Some({:?})", reason.as_str());
        assert!(rendered.contains(&field), "{rendered}");
    }

    let rendered = format!("{:?}", upstream_with_parse());

    assert!(rendered.contains("upstream_reason: None"), "{rendered}");
    for reason in REASONS {
        assert!(!rendered.contains(reason.as_str()), "{rendered}");
    }
}

fn credential(reason: UpstreamReason, parked_until_ms: Option<u64>) -> Error {
    Error::Credential {
        detail: "oauth refresh failed for p: refresh token expired".to_string(),
        reason,
        parked_until_ms,
    }
}

#[test]
fn a_credential_error_exposes_its_reason_through_the_one_accessor() {
    for reason in REASONS {
        for parked_until_ms in [None, Some(1_700_000_000_000)] {
            let err = credential(reason, parked_until_ms);

            assert_eq!(err.upstream_reason(), Some(reason), "{parked_until_ms:?}");
        }
    }
}

#[test]
fn a_credential_error_displays_like_an_auth_error() {
    let err = credential(UpstreamReason::CredentialInvalid, None);
    let auth = Error::Auth("oauth refresh failed for p: refresh token expired".to_string());

    assert_eq!(err.to_string(), auth.to_string());
}

#[test]
fn a_credential_error_debug_renders_the_reason_token_and_deadline() {
    let err = credential(UpstreamReason::CredentialExpired, Some(42));

    let rendered = format!("{err:?}");

    assert!(rendered.starts_with("Credential {"), "{rendered}");
    assert!(
        rendered.contains("reason: \"credential-expired\""),
        "{rendered}"
    );
    assert!(rendered.contains("parked_until_ms: Some(42)"), "{rendered}");
}
