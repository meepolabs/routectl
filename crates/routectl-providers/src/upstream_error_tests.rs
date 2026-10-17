use super::UpstreamErrorRead;
use reqwest::header::HeaderMap;

#[test]
fn retry_after_is_gated_on_rate_limit_status() {
    let mut headers = HeaderMap::new();
    headers.insert("retry-after", "30".parse().unwrap());
    let cases = [(429, true), (503, true), (400, false), (500, false)];
    for (status, want) in cases {
        let read = UpstreamErrorRead::from_parts(status, &headers, "", false);

        assert_eq!(read.retry_after().is_some(), want, "status {status}");
    }
}

#[cfg(feature = "gemini")]
#[test]
fn status_lift_reads_gemini_status_and_stringifies_code() {
    let v =
        serde_json::json!({"error": {"code": 429, "status": "RESOURCE_EXHAUSTED", "type": "x"}});

    let got = super::lift_nested_status_tokens(&v);

    assert_eq!(got, (Some("RESOURCE_EXHAUSTED".into()), Some("429".into())));
}

#[cfg(any(
    feature = "anthropic-api",
    feature = "bedrock",
    feature = "openai-compat",
    feature = "openai-responses"
))]
mod tokens {
    use super::super::{UpstreamErrorRead, lift_nested_error_tokens};
    use reqwest::header::HeaderMap;
    use serde_json::json;

    pub(super) const HEADER: &str = "x-amzn-errortype";

    type TokenRow<'a> = (
        &'a str,
        &'a HeaderMap,
        &'a str,
        Option<&'a str>,
        Option<&'a str>,
    );

    pub(super) fn headers_with_type(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(HEADER, value.parse().unwrap());
        headers
    }

    fn tokens(headers: &HeaderMap, body: &str) -> (Option<String>, Option<String>) {
        UpstreamErrorRead::from_parts(400, headers, body, false).tokens()
    }

    #[test]
    fn nested_lift_stringifies_numeric_code() {
        let v = json!({"error": {"type": "invalid_request_error", "code": 400}});

        let got = lift_nested_error_tokens(&v);

        assert_eq!(
            got,
            (Some("invalid_request_error".into()), Some("400".into()))
        );
    }

    #[test]
    fn token_precedence_rows() {
        let header = headers_with_type("ValidationException:http://internal.amazon.com/coral/");
        let no_header = HeaderMap::new();
        let cases: [TokenRow<'_>; 5] = [
            (
                "native envelope lifts nested type and code",
                &no_header,
                r#"{"error":{"type":"invalid_request_error","code":"bad"}}"#,
                Some("invalid_request_error"),
                Some("bad"),
            ),
            (
                "flat body lifts namespaced __type and code",
                &no_header,
                r#"{"__type":"com.amazonaws.bedrock#ThrottlingException","code":"T"}"#,
                Some("ThrottlingException"),
                Some("T"),
            ),
            (
                "header fills type when the flat body has none",
                &header,
                r#"{"message":"bad"}"#,
                Some("ValidationException"),
                None,
            ),
            (
                "body __type wins over the header",
                &header,
                r#"{"__type":"AccessDeniedException","message":"no"}"#,
                Some("AccessDeniedException"),
                None,
            ),
            (
                "header fills type on a native envelope without one",
                &header,
                r#"{"error":{"code":"bad"}}"#,
                Some("ValidationException"),
                Some("bad"),
            ),
        ];
        for (name, headers, body, want_type, want_code) in cases {
            let (got_type, got_code) = tokens(headers, body);

            assert_eq!(got_type.as_deref(), want_type, "{name}");
            assert_eq!(got_code.as_deref(), want_code, "{name}");
        }
    }

    #[test]
    fn unusable_header_lifts_no_type() {
        let mut headers = HeaderMap::new();
        headers.append(HEADER, "ValidationException".parse().unwrap());
        headers.append(HEADER, "ThrottlingException".parse().unwrap());

        let (got_type, _) = tokens(&headers, r#"{"message":"bad"}"#);

        assert_eq!(got_type, None);
    }
}

#[cfg(feature = "bedrock")]
#[test]
fn unusable_header_reason_reported_only_when_the_body_has_no_type() {
    use tokens::{HEADER, headers_with_type};
    let mut ambiguous = HeaderMap::new();
    ambiguous.append(HEADER, "ValidationException".parse().unwrap());
    ambiguous.append(HEADER, "ValidationException".parse().unwrap());
    let mut conflict = HeaderMap::new();
    conflict.append(HEADER, "ValidationException".parse().unwrap());
    conflict.append(HEADER, "ThrottlingException".parse().unwrap());
    let flat = r#"{"message":"bad"}"#;
    let typed = r#"{"__type":"ValidationException","message":"bad"}"#;
    let native = r#"{"error":{"type":"invalid_request_error"}}"#;
    let cases: [(&str, HeaderMap, &str, Option<&str>); 7] = [
        (
            "missing header, untyped body",
            HeaderMap::new(),
            flat,
            Some("missing"),
        ),
        (
            "empty header value",
            headers_with_type(""),
            flat,
            Some("invalid"),
        ),
        (
            "duplicate header",
            ambiguous.clone(),
            flat,
            Some("ambiguous"),
        ),
        ("conflicting header", conflict, flat, Some("conflict")),
        (
            "header lifted",
            headers_with_type("ValidationException"),
            flat,
            None,
        ),
        ("flat body typed itself", HeaderMap::new(), typed, None),
        ("native envelope typed itself", ambiguous, native, None),
    ];
    for (name, headers, body, want) in cases {
        let read = UpstreamErrorRead::from_parts(400, &headers, body, false);

        assert_eq!(read.unusable_error_type_header_reason(), want, "{name}");
    }
}

#[cfg(feature = "anthropic-api")]
mod success_body {
    use super::super::map_success_body;
    use routectl_core::Error;

    #[test]
    fn over_cap_maps_to_502_with_the_fixed_message() {
        let err = map_success_body("prov", 200, b"partial-json", true)
            .expect_err("a capped 2xx body must be an error");

        match err {
            Error::Upstream {
                provider,
                status,
                body,
                ..
            } => {
                assert_eq!(provider, "prov");
                assert_eq!(status, 502, "unreadable 2xx must classify as 502");
                assert_eq!(body, crate::http_client::body_cap_exceeded_message());
            }
            other => panic!("expected Error::Upstream, got {other:?}"),
        }
    }

    #[test]
    fn under_cap_parses_json() {
        let value = map_success_body("prov", 200, br#"{"ok":true,"n":7}"#, false)
            .expect("valid json must parse");

        assert_eq!(value["ok"], serde_json::json!(true));
        assert_eq!(value["n"], serde_json::json!(7));
    }

    #[test]
    fn under_cap_invalid_json_keeps_the_success_status() {
        let err =
            map_success_body("prov", 200, b"not json", false).expect_err("invalid json must error");

        match err {
            Error::Upstream { status, .. } => assert_eq!(status, 200),
            other => panic!("expected Error::Upstream, got {other:?}"),
        }
    }
}
