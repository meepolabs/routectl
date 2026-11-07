//! Shared helpers for the Bedrock mantle lanes.
//!
//! The URL builders are pure and dependency-free: they derive the
//! mantle host and per-vocabulary bases from a region string alone, so
//! both the Anthropic and OpenAI lanes agree on one source of truth and
//! never carry a manually configured base URL. Signing reuses the
//! parameterized Bedrock SigV4 signer under the `bedrock-mantle` scope.

/// AWS SigV4 service scope for the Bedrock mantle lanes.
pub const MANTLE_SERVICE: &str = "bedrock-mantle";

/// Mantle host for `region`, without a trailing slash, refusing a
/// non-canonical region.
///
/// Shape: `https://bedrock-mantle.<region>.<partition dual-stack suffix>`
/// (`api.aws` outside China). Path-free so lane bases can append their own
/// vocabulary segment.
pub fn mantle_host(region: &str) -> routectl_core::Result<String> {
    let region = crate::aws_region::require_aws_region(region)?;
    Ok(format!(
        "https://bedrock-mantle.{}.{}",
        region.as_str(),
        region.partition().dual_stack_dns_suffix()
    ))
}

/// Anthropic-vocabulary base URL for `region`, without a trailing slash.
///
/// The Anthropic client appends `/v1/messages`, so this base ends at
/// `/anthropic`.
pub fn mantle_anthropic_base(region: &str) -> routectl_core::Result<String> {
    Ok(format!("{}/anthropic", mantle_host(region)?))
}

/// OpenAI-vocabulary base URL for `region`, without a trailing slash.
///
/// Shape: `<host>/openai/v1`.
pub fn mantle_openai_base(region: &str) -> routectl_core::Result<String> {
    Ok(format!("{}/openai/v1", mantle_host(region)?))
}

/// Bedrock mantle authentication shared by the mantle egress lanes.
///
/// Present (a provider config's `mantle` field is `Some`) selects the
/// mantle lane: the request body is serialized to bytes and
/// SigV4/bearer-signed under the `bedrock-mantle` scope before egress,
/// and the lane's first-party header plumbing (`x-api-key`, Claude-Code,
/// or codex identity) is bypassed. `region` is the single source of truth
/// for both the derived endpoint host and the SigV4 signing scope;
/// `creds` carries the resolved AWS credential shape (bearer key or SigV4
/// provider). Shared home for the Anthropic and OpenAI mantle lanes.
#[cfg(feature = "bedrock")]
#[derive(Clone)]
pub struct MantleAuth {
    /// AWS region driving the derived host and the SigV4 signing scope.
    pub region: String,
    /// Resolved credential shape (bearer key or SigV4 provider).
    pub creds: crate::bedrock::auth::ResolvedCreds,
}

#[cfg(feature = "bedrock")]
impl MantleAuth {
    /// Observability discriminator for the credential shape:
    /// `"bearer"` for a Bedrock console API key, `"sigv4"` for a signed
    /// AWS credential. Never carries any secret material.
    pub(crate) const fn auth_mode(&self) -> &'static str {
        match self.creds {
            crate::bedrock::auth::ResolvedCreds::Bearer { .. } => "bearer",
            crate::bedrock::auth::ResolvedCreds::Sigv4 { .. } => "sigv4",
        }
    }
}

#[cfg(feature = "bedrock")]
impl std::fmt::Debug for MantleAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The region is non-secret. `creds` carries credential material,
        // so surface only its shape discriminator (never the key or the
        // provider), mirroring the redacting Debug on `BedrockCreds`.
        f.debug_struct("MantleAuth")
            .field("region", &self.region)
            .field("auth_mode", &self.auth_mode())
            .finish()
    }
}

/// Whether a client-supplied image or file URL may be forwarded to a Mantle
/// upstream, which fetches it server-side under the operator's AWS identity.
///
/// Accepts only `https` and `data` URLs. Both the raw layout (a lowercase
/// `https://` or `data:` prefix, no ASCII whitespace or control character)
/// and `url::Url` must agree: the upstream's parser is unpublished, and
/// `Url::parse` silently strips tabs, newlines and edge spaces that another
/// parser may read differently. Non-ASCII bytes refuse too, so one URL has
/// one raw layout rather than a raw and a normalized one. The empty string
/// refuses.
#[cfg(feature = "bedrock")]
pub(crate) fn is_forwardable_media_url(url: &str) -> bool {
    let scheme = if url.starts_with("https://") {
        "https"
    } else if url.starts_with("data:") {
        "data"
    } else {
        return false;
    };
    if !url.is_ascii()
        || url
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
    {
        return false;
    }
    url::Url::parse(url).is_ok_and(|parsed| parsed.scheme() == scheme)
}

/// Object members the media-URL walker never descends into: tool
/// definitions and their JSON schemas. Mantle does not fetch from tool
/// definitions today; if it enables image generation, the image generation
/// tool's `input_image_mask.image_url` is the member to revisit.
#[cfg(feature = "bedrock")]
const MEDIA_WALK_SKIPPED_MEMBERS: [&str; 3] = ["tools", "input_schema", "parameters"];

/// `source.type` values whose payload is not a URL, so a `url` member beside
/// them is not one the upstream fetches.
#[cfg(feature = "bedrock")]
const NON_URL_SOURCE_KINDS: [&str; 4] = ["base64", "text", "file", "content"];

/// Longest object key rendered verbatim in a refusal path.
#[cfg(feature = "bedrock")]
const MAX_RENDERED_KEY_LEN: usize = 64;

/// Refuse a Mantle request value that carries a media URL the upstream
/// would fetch under the operator's AWS identity, unless it passes
/// [`is_forwardable_media_url`]. Lane-neutral: it reads no lane's schema,
/// only member names, so the Responses, anthropic-api and openai-compat
/// Mantle lanes share it.
///
/// It descends every object and array under `root` and checks:
/// - every member whose key ends in `_url`: it must be a string, except that
///   `image_url` may also be an object whose `url` is a string;
/// - a member named `source` holding an object, by its `url`, when its
///   `type` is `url`, or when it carries a `url` member and its `type` is
///   not a non-URL kind (`base64`, `text`, `file`, `content`).
///
/// A matched member that is null, empty or of another shape, or a `url`
/// source missing its `url`, refuses. A bare `url` member elsewhere is not
/// checked: citation annotations and search results carry one the upstream
/// does not fetch.
///
/// It never descends into string values (function-call `arguments` are
/// JSON-encoded strings), any member named `tools`, `input_schema` or
/// `parameters`, or the `input` member of an object whose `type` is
/// `tool_use` (client-authored tool arguments on Anthropic shapes). Server
/// tool inputs (`server_tool_use`, `mcp_tool_use`) are walked.
///
/// `path` names `root` in the refusal message. The message names the
/// field, its position and the accepted scheme class, never the URL; an
/// object key that is not a short ASCII identifier renders as `<key>`
/// because client keys are client bytes.
#[cfg(feature = "bedrock")]
pub(crate) fn refuse_unforwardable_media_urls(
    provider_id: &str,
    root: &serde_json::Value,
    path: &str,
) -> routectl_core::Result<()> {
    match find_unforwardable_media_url(root, path) {
        None => Ok(()),
        Some((field, at)) => Err(routectl_core::Error::normalize_request(
            provider_id,
            format!(
                "`{field}` at {at} is not an https or data URL; bedrock-mantle fetches media \
                 URLs itself and accepts only https and data"
            ),
            routectl_core::RefusalReason::Unrepresentable,
        )),
    }
}

/// The first refused media member under `value`, as its rendered field name
/// and the rendered path of the object holding it.
#[cfg(feature = "bedrock")]
fn find_unforwardable_media_url<'v>(
    value: &'v serde_json::Value,
    path: &str,
) -> Option<(&'v str, String)> {
    use serde_json::Value;

    match value {
        Value::Array(items) => items
            .iter()
            .enumerate()
            .find_map(|(i, item)| find_unforwardable_media_url(item, &format!("{path}[{i}]"))),
        Value::Object(members) => {
            if let Some(field) = refused_media_member(members) {
                return Some((field, path.to_owned()));
            }
            let is_tool_use = members.get("type").and_then(Value::as_str) == Some("tool_use");
            members
                .iter()
                .filter(|(key, _)| !MEDIA_WALK_SKIPPED_MEMBERS.contains(&key.as_str()))
                .filter(|(key, _)| !(is_tool_use && key.as_str() == "input"))
                .find_map(|(key, child)| {
                    find_unforwardable_media_url(child, &format!("{path}.{}", render_key(key)))
                })
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => None,
    }
}

/// The rendered name of the media member of this object that refuses, if
/// any.
#[cfg(feature = "bedrock")]
fn refused_media_member(members: &serde_json::Map<String, serde_json::Value>) -> Option<&str> {
    let refused_url_member = members
        .iter()
        .filter(|(key, _)| key.ends_with("_url"))
        .find(|(key, value)| !url_member_passes(key, value));
    if let Some((key, _)) = refused_url_member {
        return Some(render_key(key));
    }
    let source = members.get("source").filter(|s| s.is_object())?;
    if is_url_source(source) && !passes_media_url_check(source.get("url")) {
        return Some("source");
    }
    None
}

/// Whether a member whose key ends in `_url` carries a forwardable URL.
#[cfg(feature = "bedrock")]
fn url_member_passes(key: &str, value: &serde_json::Value) -> bool {
    if key == "image_url" && value.is_object() {
        return passes_media_url_check(value.get("url"));
    }
    passes_media_url_check(Some(value))
}

/// Whether a `source` object names a URL the upstream fetches.
#[cfg(feature = "bedrock")]
fn is_url_source(source: &serde_json::Value) -> bool {
    match source.get("type").and_then(serde_json::Value::as_str) {
        Some("url") => true,
        Some(kind) if NON_URL_SOURCE_KINDS.contains(&kind) => false,
        _ => source.get("url").is_some(),
    }
}

#[cfg(feature = "bedrock")]
fn passes_media_url_check(url: Option<&serde_json::Value>) -> bool {
    url.and_then(serde_json::Value::as_str)
        .is_some_and(is_forwardable_media_url)
}

#[cfg(feature = "bedrock")]
fn render_key(key: &str) -> &str {
    let is_identifier = !key.is_empty()
        && key.len() <= MAX_RENDERED_KEY_LEN
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if is_identifier { key } else { "<key>" }
}

/// Sign `req` in place for a mantle lane under the `bedrock-mantle` SigV4
/// scope.
///
/// Delegates to the parameterized Bedrock signer, handling both credential
/// shapes: `Bearer` attaches `Authorization: Bearer <key>`, `Sigv4` signs
/// the request and merges the AWS auth headers.
#[cfg(feature = "bedrock")]
pub async fn sign(
    req: &mut reqwest::Request,
    creds: &crate::bedrock::auth::ResolvedCreds,
    region: &str,
) -> routectl_core::Result<()> {
    crate::bedrock::signing::apply_with_service(req, creds, region, MANTLE_SERVICE).await
}

/// Probe a mantle lane by resolving its credential, mirroring the Bedrock
/// provider's probe posture.
///
/// The mantle endpoint authenticates with SigV4/bearer, not the
/// first-party `x-api-key`, and exposes no free models-list surface, so a
/// reachability probe must NOT dial the inference host. Instead it checks
/// the credential is live: a `Bearer` key is a static secret (trivially
/// reachable), a `Sigv4` provider re-provides its chain (catching an
/// expired SSO / broken profile after startup). Reason strings are fixed
/// literals so no profile name, ARN, or SDK detail leaks into an
/// operator-facing message.
#[cfg(feature = "bedrock")]
pub async fn probe(creds: &crate::bedrock::auth::ResolvedCreds) -> routectl_core::ProbeOutcome {
    use aws_credential_types::provider::ProvideCredentials;

    use crate::bedrock::auth::ResolvedCreds;

    match creds {
        ResolvedCreds::Bearer { .. } => routectl_core::ProbeOutcome::Reachable,
        ResolvedCreds::Sigv4 { provider } => {
            match tokio::time::timeout(crate::probe::PROBE_TIMEOUT, provider.provide_credentials())
                .await
            {
                Ok(Ok(_)) => routectl_core::ProbeOutcome::Reachable,
                Ok(Err(e)) => {
                    // Log the real SDK error server-side (an expired SSO,
                    // missing profile, and unreachable IMDS look identical
                    // otherwise), but keep a fixed literal in the outcome so
                    // no profile name, ARN, or SDK detail leaks to the
                    // operator surface -- mirrors `bedrock::auth::resolve`.
                    tracing::warn!(error = %e, "mantle credential resolution failed");
                    routectl_core::ProbeOutcome::AuthFailed("credential resolution failed".into())
                }
                Err(_) => routectl_core::ProbeOutcome::Unreachable(
                    "credential resolution timed out".into(),
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{mantle_anthropic_base, mantle_host, mantle_openai_base};

    #[test]
    fn host_has_expected_shape_and_no_trailing_slash() {
        let host = mantle_host("us-east-1").unwrap();
        assert_eq!(host, "https://bedrock-mantle.us-east-1.api.aws");
        assert!(!host.ends_with('/'));
    }

    #[test]
    fn anthropic_base_ends_at_anthropic_segment() {
        let base = mantle_anthropic_base("eu-west-1").unwrap();
        assert_eq!(base, "https://bedrock-mantle.eu-west-1.api.aws/anthropic");
        assert!(!base.ends_with('/'));
    }

    #[test]
    fn openai_base_has_v1_suffix() {
        let base = mantle_openai_base("ap-southeast-2").unwrap();
        assert_eq!(
            base,
            "https://bedrock-mantle.ap-southeast-2.api.aws/openai/v1"
        );
        assert!(!base.ends_with('/'));
    }

    #[test]
    fn host_follows_the_region_partition_dual_stack_suffix() {
        let cases = [
            ("us-west-2", "bedrock-mantle.us-west-2.api.aws"),
            ("us-gov-west-1", "bedrock-mantle.us-gov-west-1.api.aws"),
            (
                "cn-north-1",
                "bedrock-mantle.cn-north-1.api.amazonwebservices.com.cn",
            ),
        ];

        for (region, host) in cases {
            let base = mantle_host(region).unwrap();
            let parsed = reqwest::Url::parse(&base).unwrap();

            assert_eq!(parsed.scheme(), "https", "{region}");
            assert_eq!(parsed.host_str(), Some(host), "{region}");
            assert_eq!(parsed.port(), None, "{region}");
        }
    }

    #[test]
    fn every_builder_refuses_a_host_altering_region() {
        let hostile = [
            "x@127.0.0.1:9/",
            "us-west-2.evil.example/",
            "evil.example#",
            "evil.example?",
            "us-west-2 ",
            "US-WEST-2",
            "",
        ];

        for region in hostile {
            let results = [
                mantle_host(region),
                mantle_anthropic_base(region),
                mantle_openai_base(region),
            ];

            for result in results {
                match result {
                    Err(routectl_core::Error::Config(msg)) => {
                        assert!(msg.contains("region"), "{region:?}: {msg}");
                    }
                    other => panic!("{region:?} must be refused, got {other:?}"),
                }
            }
        }
    }

    #[cfg(feature = "bedrock")]
    #[test]
    fn forwardable_media_url_accepts_only_https_and_data() {
        let accepted = [
            ("https", "https://example.com/a.png"),
            ("https with query", "https://example.com/a.png?sig=abc&x=1"),
            ("data image", "data:image/png;base64,AAAA"),
            ("data pdf", "data:application/pdf;base64,AAAA"),
        ];
        let refused = [
            ("empty", ""),
            ("s3", "s3://b/k"),
            ("http", "http://x"),
            ("file", "file:///etc/passwd"),
            ("gs", "gs://b/o"),
            ("javascript", "javascript:x"),
            ("uppercase scheme", "HTTPS://x"),
            ("leading space", " https://example.com/a.png"),
            ("trailing newline", "https://example.com/a.png\n"),
            ("embedded tab", "https://exam\tple.com/a.png"),
            ("single slash", "https:/x"),
            ("empty host", "https://"),
            ("control not whitespace", "https://x/\u{1}"),
            ("delete", "https://example.com/\u{7f}"),
            ("non-ascii host", "https://example\u{3002}com/a.png"),
        ];

        for (name, url) in accepted {
            assert!(
                super::is_forwardable_media_url(url),
                "{name} must be accepted"
            );
        }
        for (name, url) in refused {
            assert!(
                !super::is_forwardable_media_url(url),
                "{name} must be refused"
            );
        }
    }

    #[cfg(feature = "bedrock")]
    mod media_walk {
        use serde_json::{Value, json};

        use super::super::refuse_unforwardable_media_urls;

        const S3: &str = "s3://planted-bucket-zq7/secret-key-path-k3.png";
        const HTTPS: &str = "https://example.com/a.png";

        fn refusal_message(root: &Value) -> Option<String> {
            match refuse_unforwardable_media_urls("mantle:walk", root, "root") {
                Ok(()) => None,
                Err(routectl_core::Error::NormalizeRequest(
                    _,
                    msg,
                    routectl_core::RefusalReason::Unrepresentable,
                )) => Some(msg),
                Err(other) => panic!("unexpected error kind: {other:?}"),
            }
        }

        #[test]
        fn refuses_each_fetched_url_shape_and_names_its_path() {
            let rows: Vec<(&str, Value, &str)> = vec![
                (
                    "image_url string",
                    json!({"type": "input_image", "image_url": S3}),
                    "`image_url` at root ",
                ),
                (
                    "image_url object url",
                    json!({"type": "image_url", "image_url": {"url": S3}}),
                    "`image_url` at root ",
                ),
                (
                    "image_url object without url",
                    json!({"image_url": {"detail": "auto"}}),
                    "`image_url` at root ",
                ),
                (
                    "image_url null",
                    json!({"image_url": null}),
                    "`image_url` at root ",
                ),
                (
                    "image_url empty",
                    json!({"image_url": ""}),
                    "`image_url` at root ",
                ),
                (
                    "image_url number",
                    json!({"image_url": 7}),
                    "`image_url` at root ",
                ),
                (
                    "file_url string",
                    json!({"file_url": S3}),
                    "`file_url` at root ",
                ),
                (
                    "file_url object",
                    json!({"file_url": {"url": HTTPS}}),
                    "`file_url` at root ",
                ),
                (
                    "source url",
                    json!({"type": "image", "source": {"type": "url", "url": S3}}),
                    "`source` at root ",
                ),
                (
                    "source url missing url",
                    json!({"source": {"type": "url"}}),
                    "`source` at root ",
                ),
                (
                    "object under array under object",
                    json!({"output": {"type": "computer_screenshot", "image_url": S3}}),
                    "`image_url` at root.output ",
                ),
                (
                    "nested two levels in arrays",
                    json!([{"content": [{"image_url": {"url": S3}}]}]),
                    "`image_url` at root[0].content[0] ",
                ),
                (
                    "non-identifier key renders as placeholder",
                    json!({S3: {"image_url": S3}}),
                    "`image_url` at root.<key> ",
                ),
                (
                    "input of a non tool_use object is walked",
                    json!({"type": "tool_result", "input": {"image_url": S3}}),
                    "`image_url` at root.input ",
                ),
                (
                    "parameters-like key outside the skip set is walked",
                    json!({"params": {"image_url": S3}}),
                    "`image_url` at root.params ",
                ),
                (
                    "file_url null",
                    json!({"file_url": null}),
                    "`file_url` at root ",
                ),
                (
                    "any _url member string s3",
                    json!({"type": "video", "video_url": S3}),
                    "`video_url` at root ",
                ),
                (
                    "non-identifier _url key renders as placeholder",
                    json!({"planted-bucket-zq7_url": S3}),
                    "`<key>` at root ",
                ),
                (
                    "source with url and no type",
                    json!({"type": "image", "source": {"url": S3}}),
                    "`source` at root ",
                ),
                (
                    "source with url and an unknown type",
                    json!({"source": {"type": "s3_location", "url": S3}}),
                    "`source` at root ",
                ),
            ];

            for (row, root, expected) in rows {
                let msg = refusal_message(&root).unwrap_or_else(|| panic!("{row}: must refuse"));
                assert!(msg.contains(expected), "{row}: {msg}");
                assert!(msg.contains("https or data"), "{row}: {msg}");
                assert!(
                    !msg.contains("planted-bucket-zq7") && !msg.contains("secret-key-path-k3"),
                    "{row}: message must not echo the URL: {msg}"
                );
            }
        }

        #[test]
        fn never_descends_into_strings_tool_definitions_or_tool_use_input() {
            let rows: Vec<(&str, Value)> = vec![
                (
                    "string argument",
                    json!({"type": "function_call", "arguments": json!({"image_url": S3}).to_string()}),
                ),
                (
                    "tools",
                    json!({"tools": [{"type": "function", "image_url": S3}]}),
                ),
                (
                    "input_schema",
                    json!({"input_schema": {"properties": {"image_url": {"default": S3}}, "image_url": S3}}),
                ),
                (
                    "parameters",
                    json!({"parameters": {"image_url": S3, "source": {"type": "url", "url": S3}}}),
                ),
                (
                    "tool_use input",
                    json!({"type": "tool_use", "id": "t1", "input": {"image_url": S3, "file_url": S3}}),
                ),
            ];

            for (row, root) in rows {
                assert_eq!(refusal_message(&root), None, "{row}: must not be walked");
            }
        }

        #[test]
        fn forwards_https_data_and_non_fetched_shapes() {
            let rows: Vec<(&str, Value)> = vec![
                ("image_url https", json!({"image_url": HTTPS})),
                (
                    "image_url object data",
                    json!({"image_url": {"url": "data:image/png;base64,AAAA"}}),
                ),
                ("file_url https", json!({"file_url": HTTPS})),
                (
                    "source url https",
                    json!({"source": {"type": "url", "url": HTTPS}}),
                ),
                (
                    "source base64",
                    json!({"source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}),
                ),
                ("source string", json!({"source": S3})),
                (
                    "source base64 beside a url member",
                    json!({"source": {"type": "base64", "data": "AAAA", "url": S3}}),
                ),
                (
                    "source file beside a url member",
                    json!({"source": {"type": "file", "file_id": "f1", "url": S3}}),
                ),
                (
                    "bare url member",
                    json!({"type": "url_citation", "url": S3, "title": "t"}),
                ),
                (
                    "url-like key without the underscore suffix",
                    json!({"imageurl": S3, "urls": [S3]}),
                ),
                (
                    "plain text mentioning s3",
                    json!([{"type": "input_text", "text": S3}]),
                ),
                ("scalars", json!([null, true, 1, "x"])),
            ];

            for (row, root) in rows {
                assert_eq!(refusal_message(&root), None, "{row}: must pass");
            }
        }
    }

    #[cfg(feature = "bedrock")]
    mod signing {
        use reqwest::header::AUTHORIZATION;

        use super::super::sign;
        use crate::bedrock::BedrockCreds;
        use crate::bedrock::auth::resolve;
        #[tokio::test]
        async fn sigv4_credential_scope_uses_mantle_service() {
            let resolved = resolve(
                &BedrockCreds::Static {
                    access_key: "testkey-sign-xyz".into(),
                    secret_key: "test-secret-key".into(),
                    session_token: None,
                },
                "us-west-2",
            )
            .await
            .unwrap();

            let client = reqwest::Client::new();
            let mut req = client
                .post("https://bedrock-mantle.us-west-2.api.aws/anthropic/v1/messages")
                .body("{}")
                .build()
                .unwrap();

            sign(&mut req, &resolved, "us-west-2").await.unwrap();

            let auth = req
                .headers()
                .get(AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .expect("Authorization header set");
            assert!(
                auth.starts_with("AWS4-HMAC-SHA256 "),
                "expected SigV4 prefix, got: {auth}"
            );
            assert!(
                auth.contains("/us-west-2/bedrock-mantle/aws4_request"),
                "missing mantle service scope, got: {auth}"
            );
        }

        #[tokio::test]
        async fn bearer_path_attaches_authorization_header() {
            let resolved = resolve(
                &BedrockCreds::BearerKey {
                    key: "mantle-api-key-xyz".into(),
                },
                "us-west-2",
            )
            .await
            .unwrap();

            let client = reqwest::Client::new();
            let mut req = client
                .post("https://bedrock-mantle.us-west-2.api.aws/anthropic/v1/messages")
                .body("{}")
                .build()
                .unwrap();

            sign(&mut req, &resolved, "us-west-2").await.unwrap();
            assert_eq!(
                req.headers()
                    .get(AUTHORIZATION)
                    .and_then(|v| v.to_str().ok()),
                Some("Bearer mantle-api-key-xyz")
            );
        }
    }

    /// `MantleAuth` Debug surfaces only the non-secret region and the auth
    /// shape discriminator -- never the bearer key or the SigV4 provider.
    #[cfg(feature = "bedrock")]
    #[tokio::test]
    async fn mantle_auth_debug_redacts_credential_material() {
        use super::MantleAuth;
        use crate::bedrock::BedrockCreds;
        use crate::bedrock::auth::resolve;

        let creds = resolve(
            &BedrockCreds::BearerKey {
                key: "super-secret-bearer-value".into(),
            },
            "eu-west-1",
        )
        .await
        .unwrap();
        let auth = MantleAuth {
            region: "eu-west-1".into(),
            creds,
        };

        let rendered = format!("{auth:?}");
        assert!(
            rendered.contains("eu-west-1"),
            "region is non-secret and must render: {rendered}"
        );
        assert!(
            rendered.contains("bearer"),
            "auth-mode discriminator must render: {rendered}"
        );
        assert!(
            !rendered.contains("super-secret-bearer-value"),
            "credential material must never render in Debug: {rendered}"
        );
    }
}
