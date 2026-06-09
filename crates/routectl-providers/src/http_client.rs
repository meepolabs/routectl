//! Shared `reqwest::Client` factory.
//!
//! Centralizes a few decisions that every provider would otherwise repeat:
//! User-Agent override, sensible defaults, and (in the future) connection
//! pool tuning, TLS roots, etc. Per-request headers (auth, content-type,
//! anthropic-beta, etc.) are NOT applied here -- those vary per call site
//! and per request, so they stay in each provider's `build_headers`-style
//! method.
//!
//! Why this lives in `routectl-providers` rather than a shared util crate:
//! every consumer is a provider, the surface is small, and pulling
//! `reqwest` into `routectl-core` would invert the dep direction.

use reqwest::Client;

/// Idle read timeout applied to every shared client. reqwest's
/// `read_timeout` is per-read: the timer resets after each successful
/// read, so this caps the gap BETWEEN bytes/chunks, not the total
/// stream duration. That distinction matters -- a total `timeout` would
/// kill long extended-thinking streams that legitimately run for
/// minutes. This is purely a leak safety net: if an upstream stops
/// sending but keeps the TCP connection open mid-stream, the spawned
/// render task would otherwise block forever on the next read. 300s is
/// far longer than any legitimate inter-byte gap (thinking streams emit
/// periodic deltas/keepalives well inside this window), so a healthy
/// stream never trips it. Not configurable in v1: a single safe default
/// is enough and a knob would invite operators to set it too tight.
///
/// Separate concern from any first-byte timeout: first-byte covers the
/// initial response delay before the stream opens; this covers a hang
/// once bytes have started flowing.
pub(crate) const STREAM_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Build a `reqwest::Client` with the given optional User-Agent.
///
/// `None` keeps reqwest's default UA. Use this anywhere a provider
/// needs a stock client; tests can override later via wiremock.
///
/// Panics: practically never. `reqwest::ClientBuilder::build()` only
/// fails on TLS-init pathologies (missing system cert store, etc.) --
/// failures we can't recover from at provider-construct time anyway.
/// Promoting this to `Result<Client>` would force every provider's
/// `new()` to be fallible without giving callers anything useful to
/// do at the failure site.
pub fn build(user_agent: Option<&str>) -> Client {
    common_builder(user_agent)
        .build()
        .expect("reqwest::Client::build failed (TLS init?); fatal at startup")
}

/// Build a `reqwest::Client` with an attached cookie provider. Used by
/// the openai-responses provider to pin Cloudflare cookies across
/// requests against `chatgpt.com/backend-api/codex` (mirrors codex
/// CLI's `with_chatgpt_cloudflare_cookie_store`). The jar is shared
/// via Arc so the caller can persist it on shutdown.
#[cfg(feature = "openai-responses")]
pub fn build_with_cookie_provider<S>(user_agent: Option<&str>, jar: std::sync::Arc<S>) -> Client
where
    S: reqwest::cookie::CookieStore + 'static,
{
    common_builder(user_agent)
        .cookie_provider(jar)
        .build()
        .expect("reqwest::Client::build failed (TLS init?); fatal at startup")
}

/// Shared builder body: TLS-1.2 floor + optional UA. Centralized so
/// `build` and `build_with_cookie_provider` cannot drift on the TLS /
/// proxy / etc. defaults.
fn common_builder(user_agent: Option<&str>) -> reqwest::ClientBuilder {
    let mut builder = Client::builder()
        // Defense-in-depth: every real provider endpoint enforces
        // TLS 1.2+, but pinning here closes any path where reqwest's
        // default would negotiate down to an older protocol against
        // a misconfigured proxy or an in-the-middle box. Cheap, no
        // operational impact.
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        // Per-read idle timeout (resets after each successful read);
        // a mid-stream hang where the upstream stops sending but holds
        // the TCP connection open would otherwise leak the render task.
        // NOT a total-duration cap -- safe for long thinking streams.
        // See STREAM_READ_TIMEOUT.
        .read_timeout(STREAM_READ_TIMEOUT);
    if let Some(ua) = user_agent {
        builder = builder.user_agent(ua);
    }
    builder
}

/// Header names that carry the provider's auth secret. An entry in
/// `extra_headers` matching one of these would silently bypass the
/// provider's auth contract (the operator could ship a different
/// Bearer or x-api-key value than the one resolved from the secret
/// store). Compared case-insensitively against the user-supplied key.
///
/// `anthropic-version` is included because Anthropic-API egresses fix
/// the version at provider construction time (operator config) and
/// allowing an `extra_headers["anthropic-version"]` override would
/// desync from the body-schema versioning the egress assumes.
///
/// `chatgpt-account-id` is included because the openai-responses
/// ChatgptOauth egress derives it from the resolved account ref and
/// sets it as part of the auth pair. A `header_extras` entry of the
/// same name would collide with the auth-derived value, so it is
/// reserved.
///
/// Note: the `x-amz-` prefix is handled by `is_auth_header` directly
/// (not stored in this slice) because it is a prefix match rather than
/// an exact match. Any `x-amz-*` header supplied via `header_extras`
/// would desync the AWS SigV4 signature computed over the request
/// before these headers are added, so the entire prefix is reserved.
const AUTH_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    "anthropic-version",
    "chatgpt-account-id",
];

/// Header names that routectl owns the value of for wire-shape
/// correctness, but are NOT auth carriers. An operator setting one of
/// these in `header_extras` would silently lose to routectl's dynamic
/// composition (or worse, emit twice on the wire). Compared
/// case-insensitively against the user-supplied key.
///
/// - `host` is request-routing; overriding it would let TOML pin a
///   different upstream and confuse SigV4 / virtual-host aware servers.
/// - `content-type` is set by reqwest's `.json()` to
///   `application/json`. Overriding it (e.g. to `text/plain`) would
///   make Anthropic / OpenAI reject the body with a vague 400 that
///   looks like an auth or schema error.
/// - `content-length` is computed by reqwest from the serialized body;
///   a TOML override desyncs the wire contract.
///
/// v0.6.0 removed `anthropic-beta` from this list. The Anthropic
/// ingress lifts the inbound `anthropic-beta` HTTP header into
/// `req.anthropic_beta`; the router's dispatch-layer compose merges
/// the three sources (ingress + provider header_extras + model
/// header_extras) into one comma-joined value. Operators now own the
/// per-provider and per-model `anthropic-beta` slots via
/// `header_extras`.
const MANAGED_HEADERS: &[&str] = &["host", "content-type", "content-length"];

/// True if the given header name carries provider auth or belongs to
/// the AWS SigV4 signing envelope. Case-insensitive.
///
/// In addition to the exact-match names in `AUTH_HEADERS`, any header
/// with the `x-amz-` prefix is treated as auth-reserved. On the Bedrock
/// path, SigV4 signs a fixed set of `x-amz-*` headers (date, security
/// token, etc.) at request-build time. An operator-supplied
/// `x-amz-*` header_extra injected after signing but before send would
/// not appear in the signed string, making the signature invalid. The
/// WARN+skip path already used for the exact-match names applies here
/// too, keeping both the SigV4 path and the BearerKey path safe.
pub fn is_auth_header(name: &str) -> bool {
    let lc = name.to_ascii_lowercase();
    if lc.starts_with("x-amz-") {
        return true;
    }
    AUTH_HEADERS.contains(&lc.as_str())
}

/// True if the given header name is dynamically composed by routectl
/// (NOT an auth carrier). Case-insensitive.
pub fn is_managed_header(name: &str) -> bool {
    let lc = name.to_ascii_lowercase();
    MANAGED_HEADERS.contains(&lc.as_str())
}

/// Resolve the effective per-request `header_extras` source for an
/// egress's `build_headers`. When the router is in the loop it pre-
/// composes provider + model `header_extras` into
/// `ChatRequest.routectl_internal.header_extras`; the egress reads
/// from that to give model-level headers a path to the wire. Library
/// consumers that construct a `ChatRequest` directly leave the
/// carrier `None` and the egress falls back to its own
/// `cfg_header_extras` snapshot.
///
/// Returns an owned vec because callers iterate it once and the
/// allocation is single-digit-entries.
pub fn effective_header_extras(
    cfg_header_extras: &[(String, String)],
    req_override: Option<&std::collections::BTreeMap<String, String>>,
) -> Vec<(String, String)> {
    match req_override {
        Some(m) => m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        None => cfg_header_extras.to_vec(),
    }
}

/// True if the given header name is reserved for routectl's own
/// management and must not be set via user `extra_headers`. Union of
/// [`is_auth_header`] and [`is_managed_header`]. Callers that need to
/// distinguish the two reasons (so they can emit different log
/// messages) should call the split predicates directly.
///
/// In-tree callers all use the split predicates; this union is kept
/// for external library consumers that need the legacy single-check
/// shape.
#[allow(dead_code)]
pub fn is_reserved_extra_header(name: &str) -> bool {
    is_auth_header(name) || is_managed_header(name)
}

/// Insert a header name+value into a `HeaderMap`, replacing any
/// existing entry with the same (case-insensitive) name. Skips the
/// entry with a WARN if either the name or value cannot be parsed
/// into the http-crate types -- an invalid value would otherwise
/// poison `RequestBuilder::headers()`'s merge.
///
/// This is the single header-insert policy for every provider:
/// malformed names/values are logged at WARN and skipped rather than
/// failing the whole request. A single bad `header_extras` entry must
/// not take down an otherwise-valid request, and silently swallowing
/// it would hide operator config mistakes.
pub fn insert_header(
    map: &mut reqwest::header::HeaderMap,
    provider_id: &str,
    name: &str,
    value: &str,
) {
    let header_name = match reqwest::header::HeaderName::from_bytes(name.as_bytes()) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(
                provider = %provider_id,
                header = %name,
                error = %e,
                "skipping malformed header name",
            );
            return;
        }
    };
    let header_value = match reqwest::header::HeaderValue::from_str(value) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                provider = %provider_id,
                header = %name,
                error = %e,
                "skipping malformed header value",
            );
            return;
        }
    };
    map.insert(header_name, header_value);
}

/// Merge an egress's effective `header_extras` into `header_map`,
/// applying the shared skip policy every provider needs:
///
/// - **auth-reserved** ([`is_auth_header`]): skip with WARN -- letting
///   one through would bypass the provider's auth contract.
/// - **routectl-managed** ([`is_managed_header`]) or any name in
///   `list_valued`: skip with DEBUG -- routectl composes these
///   dynamically, so an operator value would lose or double-emit.
/// - everything else: insert via [`insert_header`] (WARN+skip on
///   malformed names/values).
///
/// `list_valued` carries the per-provider names that routectl composes
/// itself even though they are not in the global managed list. The
/// anthropic-api egress passes `&["anthropic-beta"]` (composed from the
/// ingress + provider + model union); the other providers pass `&[]`.
/// Names are compared case-insensitively.
///
/// Callers build a `HeaderMap`, call this once, then attach it to the
/// request (`rb.headers(map)` or `request.headers_mut()`). Centralizing
/// the loop keeps the auth/managed skip policy from drifting across the
/// four providers that share it.
pub fn apply_header_extras(
    header_map: &mut reqwest::header::HeaderMap,
    extras: &[(String, String)],
    provider_id: &str,
    list_valued: &[&str],
) {
    for (k, v) in extras {
        if is_auth_header(k) {
            tracing::warn!(
                provider = %provider_id,
                header = %k,
                "ignoring auth-reserved header from header_extras (would bypass provider auth)"
            );
            continue;
        }
        let is_list_valued = list_valued.iter().any(|n| k.eq_ignore_ascii_case(n));
        if is_list_valued || is_managed_header(k) {
            tracing::debug!(
                provider = %provider_id,
                header = %k,
                "dropping managed header from header_extras; composed dynamically by routectl"
            );
            continue;
        }
        insert_header(header_map, provider_id, k, v);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_header_extras, insert_header, is_auth_header, is_managed_header,
        is_reserved_extra_header, AUTH_HEADERS, MANAGED_HEADERS, STREAM_READ_TIMEOUT,
    };
    use reqwest::header::HeaderMap;

    #[test]
    fn stream_read_timeout_is_generous_idle_cap() {
        assert_eq!(
            STREAM_READ_TIMEOUT,
            std::time::Duration::from_secs(300),
            "streaming idle read timeout must be 300s",
        );
    }

    #[test]
    fn build_applies_without_panicking_with_read_timeout() {
        let _client = super::build(Some("test-ua"));
    }

    #[test]
    fn insert_header_inserts_valid_pair() {
        let mut map = HeaderMap::new();
        insert_header(&mut map, "p", "x-custom", "value");
        assert_eq!(map.get("x-custom").unwrap(), "value");
    }

    #[test]
    fn insert_header_replaces_existing_same_name() {
        let mut map = HeaderMap::new();
        insert_header(&mut map, "p", "x-custom", "first");
        insert_header(&mut map, "p", "x-custom", "second");
        // insert (not append) -> exactly one value, the latest.
        assert_eq!(map.get_all("x-custom").iter().count(), 1);
        assert_eq!(map.get("x-custom").unwrap(), "second");
    }

    #[test]
    fn insert_header_skips_malformed_name_without_panic() {
        let mut map = HeaderMap::new();
        // A space is illegal in a header name; WARN+skip, no insert.
        insert_header(&mut map, "p", "bad name", "value");
        assert!(map.is_empty());
    }

    #[test]
    fn insert_header_skips_malformed_value_without_panic() {
        let mut map = HeaderMap::new();
        // A newline is illegal in a header value; WARN+skip, no insert.
        insert_header(&mut map, "p", "x-custom", "bad\nvalue");
        assert!(map.is_empty());
    }

    #[test]
    fn apply_header_extras_inserts_plain_headers() {
        let mut map = HeaderMap::new();
        let extras = vec![("x-foo".to_string(), "1".to_string())];
        apply_header_extras(&mut map, &extras, "p", &[]);
        assert_eq!(map.get("x-foo").unwrap(), "1");
    }

    #[test]
    fn apply_header_extras_skips_auth_reserved() {
        let mut map = HeaderMap::new();
        let extras = vec![
            ("authorization".to_string(), "Bearer x".to_string()),
            ("x-api-key".to_string(), "k".to_string()),
            ("x-amz-date".to_string(), "20260101".to_string()),
            ("x-foo".to_string(), "1".to_string()),
        ];
        apply_header_extras(&mut map, &extras, "p", &[]);
        assert!(map.get("authorization").is_none());
        assert!(map.get("x-api-key").is_none());
        assert!(map.get("x-amz-date").is_none());
        // Non-reserved entry still lands.
        assert_eq!(map.get("x-foo").unwrap(), "1");
    }

    #[test]
    fn apply_header_extras_skips_managed() {
        let mut map = HeaderMap::new();
        let extras = vec![
            ("content-type".to_string(), "text/plain".to_string()),
            ("host".to_string(), "evil".to_string()),
        ];
        apply_header_extras(&mut map, &extras, "p", &[]);
        assert!(map.is_empty());
    }

    #[test]
    fn apply_header_extras_skips_list_valued_names() {
        let mut map = HeaderMap::new();
        let extras = vec![
            ("anthropic-beta".to_string(), "ctx-1m".to_string()),
            ("x-foo".to_string(), "1".to_string()),
        ];
        // anthropic-beta is list-valued (composed by routectl) -> skip;
        // x-foo is plain -> insert.
        apply_header_extras(&mut map, &extras, "p", &["anthropic-beta"]);
        assert!(map.get("anthropic-beta").is_none());
        assert_eq!(map.get("x-foo").unwrap(), "1");
    }

    #[test]
    fn apply_header_extras_list_valued_is_case_insensitive() {
        let mut map = HeaderMap::new();
        let extras = vec![("Anthropic-Beta".to_string(), "ctx-1m".to_string())];
        apply_header_extras(&mut map, &extras, "p", &["anthropic-beta"]);
        assert!(map.get("anthropic-beta").is_none());
    }

    #[test]
    fn apply_header_extras_empty_list_valued_keeps_anthropic_beta() {
        // With list_valued = &[], anthropic-beta is just a plain header
        // (the non-anthropic providers don't compose it themselves).
        let mut map = HeaderMap::new();
        let extras = vec![("anthropic-beta".to_string(), "ctx-1m".to_string())];
        apply_header_extras(&mut map, &extras, "p", &[]);
        assert_eq!(map.get("anthropic-beta").unwrap(), "ctx-1m");
    }

    #[test]
    fn is_auth_header_matches_auth_names() {
        for name in ["authorization", "Authorization", "AUTHORIZATION"] {
            assert!(is_auth_header(name), "{name:?} should classify as auth");
        }
        for name in ["x-api-key", "X-Api-Key", "X-API-KEY"] {
            assert!(is_auth_header(name), "{name:?} should classify as auth");
        }
        for name in [
            "anthropic-version",
            "Anthropic-Version",
            "ANTHROPIC-VERSION",
        ] {
            assert!(is_auth_header(name), "{name:?} should classify as auth");
        }
        for name in [
            "chatgpt-account-id",
            "ChatGPT-Account-Id",
            "CHATGPT-ACCOUNT-ID",
        ] {
            assert!(is_auth_header(name), "{name:?} should classify as auth");
        }
        for name in ["anthropic-beta", "content-type", "host", "x-request-id"] {
            assert!(!is_auth_header(name), "{name:?} must NOT classify as auth");
        }
    }

    /// Any header with an `x-amz-` prefix is auth-reserved on the
    /// Bedrock path because SigV4 signs the request before these
    /// headers are added. An extra `x-amz-*` injected after signing
    /// would not appear in the signed string, invalidating the
    /// signature.
    #[test]
    fn is_auth_header_treats_x_amz_prefix_as_reserved() {
        for name in [
            "x-amz-date",
            "X-Amz-Date",
            "X-AMZ-DATE",
            "x-amz-security-token",
            "x-amz-content-sha256",
            "x-amz-target",
        ] {
            assert!(
                is_auth_header(name),
                "{name:?} with x-amz- prefix must classify as auth-reserved"
            );
        }
        // Sanity: a non-x-amz header is not affected.
        assert!(!is_auth_header("x-custom-header"));
    }

    #[test]
    fn is_managed_header_does_not_contain_anthropic_beta() {
        // v0.6.0 removed `anthropic-beta` from the managed list.
        // Operators now own the per-provider and per-model values
        // via `header_extras`; the router's dispatch-layer compose
        // unions inbound HTTP header + provider + model into one
        // comma-joined header.
        assert!(
            !is_managed_header("anthropic-beta"),
            "anthropic-beta MUST NOT classify as managed in v0.6.0+",
        );
        assert!(
            !is_managed_header("Anthropic-Beta"),
            "case-insensitive: Anthropic-Beta MUST NOT be managed",
        );
    }

    #[test]
    fn is_managed_header_matches_managed_names() {
        for name in ["host", "Host", "HOST"] {
            assert!(
                is_managed_header(name),
                "{name:?} should classify as managed"
            );
        }
        for name in ["content-type", "Content-Type", "CONTENT-TYPE"] {
            assert!(
                is_managed_header(name),
                "{name:?} should classify as managed"
            );
        }
        for name in ["content-length", "Content-Length"] {
            assert!(
                is_managed_header(name),
                "{name:?} should classify as managed"
            );
        }
        for name in [
            "authorization",
            "x-api-key",
            "anthropic-version",
            "x-request-id",
        ] {
            assert!(
                !is_managed_header(name),
                "{name:?} must NOT classify as managed"
            );
        }
    }

    #[test]
    fn is_reserved_extra_header_unions_both() {
        // Every member of either slice flows through the union.
        for &h in AUTH_HEADERS.iter().chain(MANAGED_HEADERS.iter()) {
            assert!(
                is_reserved_extra_header(h),
                "{h:?} should classify as reserved"
            );
        }
        // A non-reserved header is not part of the union.
        assert!(!is_reserved_extra_header("x-request-id"));
        assert!(!is_reserved_extra_header("user-agent"));
    }

    #[test]
    fn is_auth_and_managed_are_disjoint() {
        // No header should be classified as BOTH auth and managed --
        // the WARN/DEBUG branch in caller code depends on this, and a
        // future addition that lands in both lists would double-log.
        for &h in AUTH_HEADERS {
            assert!(
                !MANAGED_HEADERS.contains(&h),
                "header {h:?} appears in both AUTH and MANAGED lists",
            );
        }
        for &h in MANAGED_HEADERS {
            assert!(
                !AUTH_HEADERS.contains(&h),
                "header {h:?} appears in both AUTH and MANAGED lists",
            );
        }
    }
}
