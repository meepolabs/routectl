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
pub const STREAM_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(5);

/// Connect (TCP + TLS handshake) timeout for every shared client.
/// Caps only the initial connection (not per-read). A hung connect to
/// an unreachable upstream would otherwise stall paths not wrapped by
/// the router request-timeout. 10s is generous for a public handshake
/// and short enough to fail a black-holed connect fast.
pub const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Upper bound on a non-streaming response (success or error) body that
/// any provider will buffer into memory. The streaming path already caps
/// individual eventstream frames; this closes the analogous gap on the
/// one-shot `complete()` / error-body reads, where a lying or hostile
/// upstream could otherwise stream an unbounded body and exhaust memory.
///
/// Hardcoded like the sibling timeouts above -- deliberately not a config
/// knob. `server.max_body_bytes` is a different, ingress-side concern (how
/// large a request routectl accepts); this is the egress-side ceiling on
/// what an upstream may return. 16 MiB is far above any legitimate
/// completion or error envelope and small enough to bound a single
/// buffered read.
pub const MAX_RESPONSE_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Read a response body into memory, refusing to buffer more than `cap`
/// bytes. Returns `(bytes, hit_cap)` where `bytes` is the prefix read
/// (always `<= cap`) and `hit_cap` is true when the body was rejected or
/// truncated at the cap.
///
/// Two independent guards, because `Content-Length` cannot be trusted:
///
/// 1. **Fast-reject** -- an honest `Content-Length` over `cap` lets us
///    bail before reading a single body byte (`bytes` is empty).
/// 2. **Mid-transfer** -- a running byte total checked after every chunk,
///    aborting the moment it crosses `cap`. This is the adversarial case:
///    a chunked transfer (no `Content-Length`) or a proxy whose header
///    understates the real size would slip past the fast-reject, so the
///    running total is the real ceiling. The prefix is truncated to `cap`.
///
/// The body is read once into a single buffer; callers derive
/// `serde_json::from_slice` / `String::from_utf8_lossy` from that buffer
/// rather than re-reading. `cap` is a parameter so tests can inject a
/// small ceiling. Kept `pub` (crate-visible) so every provider egress
/// shares one implementation.
pub async fn read_body_capped(
    mut resp: reqwest::Response,
    cap: usize,
) -> reqwest::Result<(Vec<u8>, bool)> {
    if let Some(len) = resp.content_length()
        && len > cap as u64
    {
        return Ok((Vec::new(), true));
    }
    let mut body = Vec::new();
    // Peak transient allocation per iteration is bounded, not just the
    // accumulated `body`. `resp.chunk()` yields one hyper HTTP/1 Data
    // frame, and hyper slices each frame out of its read buffer, whose
    // size the adaptive read strategy caps at DEFAULT_MAX_BUFFER_SIZE
    // (~408 KiB) regardless of the declared chunk size on the wire. So a
    // single hostile chunked frame claiming, say, 4 MiB is delivered as a
    // sequence of <=408 KiB `Bytes`, not one giant allocation -- the loop
    // trips the cap after a bounded number of small frames rather than
    // buffering the whole frame first. The `chunk[..remaining]` truncation
    // then bounds the accumulated `body` itself to `cap`.
    while let Some(chunk) = resp.chunk().await? {
        let remaining = cap - body.len();
        if chunk.len() > remaining {
            // Stop at the cap boundary inside this chunk: buffer only up to
            // the ceiling so peak memory never exceeds `cap`, even when a
            // single chunk alone would cross it.
            body.extend_from_slice(&chunk[..remaining]);
            return Ok((body, true));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, false))
}

/// Fixed, client-safe message for a response body that exceeded the
/// buffering cap ([`MAX_RESPONSE_BODY_BYTES`]). Never echoes any upstream
/// bytes -- every provider egress collapses a capped body (client message
/// and the upstream-failure WARN excerpt alike) to this single string.
///
/// Hoisted here so all five providers share one implementation. Depends
/// only on the unconditionally-compiled `MAX_RESPONSE_BODY_BYTES`, never
/// on feature-gated code, so it is safe in a lean single-feature build.
pub fn body_cap_exceeded_message() -> String {
    format!("response body exceeded {MAX_RESPONSE_BODY_BYTES}-byte cap")
}

/// Emit exactly one WARN when a response-body read trips the cap. `path`
/// distinguishes the call site (`complete_success_body` | `error_body` |
/// `success_body` | `count_tokens_success_body`); `content_length` is the
/// upstream-advertised size when it sent one (`None` for a chunked/absent
/// -length response).
///
/// Shared by every provider so the field set
/// (`provider`, `status`, `body_cap_bytes`, `content_length`,
/// `body_truncated`, `path`) cannot drift.
pub fn warn_body_cap(provider: &str, status: u16, content_length: Option<u64>, path: &str) {
    tracing::warn!(
        provider = %provider,
        status,
        body_cap_bytes = MAX_RESPONSE_BODY_BYTES,
        content_length = ?content_length,
        body_truncated = true,
        path,
        "upstream response body exceeded cap; truncated",
    );
}

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

/// Build a `reqwest::Client` for one-shot reachability probes: same TLS
/// floor and connect timeout as [`build`], but with redirect-following
/// DISABLED. A probe must be EXACTLY one request -- following a
/// `Location` header would turn a single GET into multiple hops and let
/// a hostile endpoint steer the probe to an unintended host (SSRF).
///
/// Returns `Result` rather than panicking (unlike [`build`]) so a probe
/// on a machine with a broken TLS store degrades to a typed `Unreachable`
/// outcome instead of aborting `doctor`. The mantle provider-construction
/// caller, by contrast, intentionally `.expect()`s this result: a client
/// that cannot be built at startup is fatal there, matching [`build`]'s
/// contract -- the `Result` exists for the degradable probe path, not to
/// make provider construction fallible.
#[cfg(any(
    feature = "openai-compat",
    feature = "anthropic-api",
    feature = "openai-responses"
))]
pub fn build_no_redirect(user_agent: Option<&str>) -> reqwest::Result<Client> {
    common_builder(user_agent)
        .redirect(reqwest::redirect::Policy::none())
        .build()
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
        .read_timeout(STREAM_READ_TIMEOUT)
        // Connect-only cap: a hung TCP/TLS handshake to a black-holed
        // upstream would otherwise stall indefinitely on paths not
        // wrapped by the router request-timeout. See CONNECT_TIMEOUT.
        .connect_timeout(CONNECT_TIMEOUT);
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
        AUTH_HEADERS, CONNECT_TIMEOUT, MANAGED_HEADERS, MAX_RESPONSE_BODY_BYTES,
        STREAM_READ_TIMEOUT, apply_header_extras, insert_header, is_auth_header, is_managed_header,
        read_body_capped,
    };
    use reqwest::header::HeaderMap;

    #[test]
    fn stream_read_timeout_is_generous_idle_cap() {
        assert_eq!(
            STREAM_READ_TIMEOUT,
            std::time::Duration::from_mins(5),
            "streaming idle read timeout must be 300s",
        );
    }

    #[test]
    fn connect_timeout_is_short_handshake_cap() {
        assert_eq!(
            CONNECT_TIMEOUT,
            std::time::Duration::from_secs(10),
            "connect (TCP + TLS) timeout must be 10s",
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

    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Spawn a one-shot raw TCP server that replies with a chunked
    /// (no `Content-Length`) body of `total` bytes split into `chunk_size`
    /// pieces, then returns the base URL to GET. wiremock always sets an
    /// honest `Content-Length` -- which the fast-reject guard would
    /// short-circuit -- so a chunked upstream is the only way to drive the
    /// mid-transfer running-total guard against a real socket.
    async fn spawn_chunked_server(total: usize, chunk_size: usize) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let _ = socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\n\
                      Content-Type: application/octet-stream\r\n\
                      Transfer-Encoding: chunked\r\n\
                      \r\n",
                )
                .await;
            let mut sent = 0usize;
            while sent < total {
                let this = chunk_size.min(total - sent);
                let _ = socket.write_all(format!("{this:x}\r\n").as_bytes()).await;
                let _ = socket.write_all(&vec![b'a'; this]).await;
                let _ = socket.write_all(b"\r\n").await;
                sent += this;
            }
            let _ = socket.write_all(b"0\r\n\r\n").await;
            let _ = socket.flush().await;
        });
        format!("http://{addr}")
    }

    #[test]
    fn max_response_body_cap_is_16_mib() {
        assert_eq!(MAX_RESPONSE_BODY_BYTES, 16 * 1024 * 1024);
    }

    #[tokio::test]
    async fn read_body_capped_fast_rejects_honest_content_length_over_cap() {
        // wiremock computes an honest Content-Length from the body, so a
        // body over the cap is rejected by the header check before a single
        // body byte is streamed -- `bytes` comes back empty.
        let cap = 100;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'a'; cap * 4]))
            .mount(&server)
            .await;
        let resp = reqwest::get(server.uri()).await.unwrap();

        let (bytes, hit_cap) = read_body_capped(resp, cap).await.unwrap();

        assert!(hit_cap, "honest Content-Length over cap must trip hit_cap");
        assert!(
            bytes.is_empty(),
            "fast-reject must not read the body: got {} bytes",
            bytes.len()
        );
    }

    #[tokio::test]
    async fn read_body_capped_returns_under_cap_body_intact() {
        let cap = 1024;
        let body = vec![b'x'; 256];
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
            .mount(&server)
            .await;
        let resp = reqwest::get(server.uri()).await.unwrap();

        let (bytes, hit_cap) = read_body_capped(resp, cap).await.unwrap();

        assert!(!hit_cap, "an under-cap body must not trip hit_cap");
        assert_eq!(bytes, body, "under-cap body must be returned intact");
    }

    #[tokio::test]
    async fn read_body_capped_trips_mid_transfer_on_chunked_body_over_cap() {
        // A chunked upstream sends no Content-Length, so the fast-reject
        // cannot see the size -- the running-total guard must catch it and
        // truncate the prefix to the cap. This is the "content-length lie":
        // an absent/understated length only the mid-transfer check defends.
        let cap = 512;
        let url = spawn_chunked_server(cap * 8, 128).await;
        let resp = reqwest::get(url).await.unwrap();

        let (bytes, hit_cap) = read_body_capped(resp, cap).await.unwrap();

        assert!(hit_cap, "a chunked body over cap must trip mid-transfer");
        assert!(
            bytes.len() <= cap,
            "prefix must be truncated to cap: got {} > {cap}",
            bytes.len()
        );
    }

    #[tokio::test]
    async fn read_body_capped_reads_chunked_body_under_cap_fully() {
        // The streaming path (no Content-Length) reads every chunk when the
        // running total stays under the cap.
        let cap = 4096;
        let total = 900;
        let url = spawn_chunked_server(total, 128).await;
        let resp = reqwest::get(url).await.unwrap();

        let (bytes, hit_cap) = read_body_capped(resp, cap).await.unwrap();

        assert!(!hit_cap, "an under-cap chunked body must not trip hit_cap");
        assert_eq!(bytes.len(), total, "all chunks must be read");
    }

    #[tokio::test]
    async fn read_body_capped_bounds_peak_at_cap_when_one_chunk_straddles_it() {
        // A single chunk larger than the cap must be truncated to exactly
        // `cap` -- peak buffered bytes never exceed the ceiling even when
        // one chunk alone would cross it.
        let cap = 500;
        let url = spawn_chunked_server(cap * 3, cap * 3).await;
        let resp = reqwest::get(url).await.unwrap();

        let (bytes, hit_cap) = read_body_capped(resp, cap).await.unwrap();

        assert!(hit_cap, "an over-cap single chunk must trip mid-transfer");
        assert_eq!(bytes.len(), cap, "prefix must be bounded to exactly cap");
    }

    /// Empirical proof that a single oversized HTTP/1.1 chunked frame does
    /// NOT materialize as one giant `Bytes` from `resp.chunk()`. An
    /// upstream declares one 4 MiB wire chunk; hyper's HTTP/1 read buffer
    /// (adaptive strategy, capped at DEFAULT_MAX_BUFFER_SIZE ~= 408 KiB for
    /// the pinned hyper + reqwest, which do not override http1_max_buf_size)
    /// slices that frame into a sequence of small `Bytes`. This is the fact
    /// the `read_body_capped` loop relies on: transient per-iteration
    /// allocation stays far below the 16 MiB cap regardless of the wire
    /// chunk size, so the cap check trips before any large buffer forms.
    #[tokio::test]
    async fn single_wire_chunk_is_yielded_as_bounded_frames() {
        // One 4 MiB declared chunk, sent as a single wire chunk.
        let total = 4 * 1024 * 1024;
        let url = spawn_chunked_server(total, total).await;
        let mut resp = reqwest::get(url).await.unwrap();

        let mut seen = 0usize;
        let mut max_frame = 0usize;
        while let Some(chunk) = resp.chunk().await.unwrap() {
            seen += chunk.len();
            max_frame = max_frame.max(chunk.len());
        }

        assert_eq!(seen, total, "the whole body must be delivered");
        // The largest single frame must sit well under the whole wire chunk
        // and far below the 16 MiB response cap. 512 KiB gives headroom over
        // the ~408 KiB hyper read-buffer ceiling without admitting a
        // multi-megabyte single allocation.
        assert!(
            max_frame <= 512 * 1024,
            "a single chunk() frame must stay below the read-buffer bound: \
             got {max_frame} bytes from a {total}-byte wire chunk",
        );
        assert!(
            max_frame < MAX_RESPONSE_BODY_BYTES,
            "per-frame allocation must be far below the response cap",
        );
    }
}
