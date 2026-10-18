//! Tests for the typed local refusal reason: which variants carry one
//! through `Error::refusal_reason`, that the reason stays out of the
//! client-facing Display text, and that Debug renders it as its token.

use super::Error;
use crate::{RefusalReason, UpstreamReason};

fn json_error() -> Error {
    Error::Json(serde_json::from_str::<serde_json::Value>("{").unwrap_err())
}

#[test]
fn refusal_reason_is_carried_by_local_refusals_only() {
    let rows: [(&str, Error, Option<RefusalReason>); 15] = [
        (
            "validation",
            Error::Validation("bad".into(), RefusalReason::FieldType),
            Some(RefusalReason::FieldType),
        ),
        (
            "local refusal",
            Error::LocalRefusal {
                provider: "p".into(),
                detail: "d".into(),
                reason: RefusalReason::CloakRelocation,
            },
            Some(RefusalReason::CloakRelocation),
        ),
        ("json", json_error(), Some(RefusalReason::Unparseable)),
        (
            "normalize request",
            Error::NormalizeRequest("p".into(), "m".into()),
            None,
        ),
        (
            "normalize response",
            Error::NormalizeResponse("p".into(), "m".into()),
            None,
        ),
        ("upstream", Error::upstream("p", 400, "bad"), None),
        ("unknown provider", Error::UnknownProvider("p".into()), None),
        ("unknown alias", Error::UnknownAlias("a".into()), None),
        ("auth", Error::Auth("denied".into()), None),
        (
            "credential",
            Error::Credential {
                detail: "expired".into(),
                reason: UpstreamReason::CredentialExpired,
                parked_until_ms: None,
            },
            None,
        ),
        ("config", Error::Config("bad".into()), None),
        ("internal", Error::Internal("boom".into()), None),
        ("streaming", Error::Streaming("cut".into()), None),
        ("io", Error::Io(std::io::Error::other("disk")), None),
        (
            "not implemented",
            Error::NotImplemented("p".into(), "count_tokens".into()),
            None,
        ),
    ];

    for (name, err, expected) in rows {
        assert_eq!(err.refusal_reason(), expected, "{name}");
    }
}

#[test]
fn refusal_reason_stays_out_of_the_display_text() {
    let rows = [
        (
            "validation",
            Error::Validation(
                "cache_control: 5 breakpoints exceeds maximum of 4".into(),
                RefusalReason::CacheBreakpointCap,
            ),
            "validation: cache_control: 5 breakpoints exceeds maximum of 4",
        ),
        (
            "local refusal",
            Error::LocalRefusal {
                provider: "seat".into(),
                detail: "system prompt cannot be relocated".into(),
                reason: RefusalReason::CloakRelocation,
            },
            "provider `seat`: refused before egress: system prompt cannot be relocated",
        ),
    ];

    for (name, err, expected) in rows {
        assert_eq!(err.to_string(), expected, "{name}");
    }
}

#[test]
fn local_refusal_debug_renders_the_reason_token() {
    let err = Error::LocalRefusal {
        provider: "seat".into(),
        detail: "d".into(),
        reason: RefusalReason::CloakRelocation,
    };

    let rendered = format!("{err:?}");

    assert!(
        rendered.contains("reason: \"cloak-relocation\""),
        "{rendered}"
    );
}
