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
//!
//! # PER-LANE REDIRECT POLICY
//!
//! Every egress client attaches at least one header that reqwest's
//! default redirect policy (`limited(10)`) does NOT strip on a
//! cross-host hop -- `x-api-key`, `x-goog-api-key`, `chatgpt-account-id`,
//! the Claude-Code identity headers, or (on the signed lanes) the
//! `x-amz-*` SigV4 envelope. reqwest only drops `AUTHORIZATION`,
//! `COOKIE`, `PROXY_AUTHORIZATION` and `WWW_AUTHENTICATE` on a host
//! change, which covers none of those. So every credentialed lane in
//! this crate builds with [`build_no_redirect`] (or, for the
//! cookie-backed lane, [`build_with_cookie_provider`], which also pins
//! `Policy::none()`):
//!
//! | Lane | Sensitive headers | Policy |
//! |---|---|---|
//! | anthropic-api first-party + mantle | `x-api-key`, Claude-Code identity | no-redirect |
//! | openai-compat first-party + mantle | `Authorization: Bearer` | no-redirect |
//! | openai-responses ChatGPT-OAuth / ApiKey / mantle | `Authorization`, `chatgpt-account-id` | no-redirect (cookie-backed variant included) |
//! | gemini (direct + Cloud Code) | `x-goog-api-key`, `Authorization: Bearer` | no-redirect |
//! | bedrock (native Invoke/Converse) | `x-amz-*` SigV4 envelope, `Authorization: Bearer` | no-redirect |
//! | probe (`doctor` reachability check) | none carried, but a probe must be exactly one hop | no-redirect |
//! | `catalog import` source fetch (in `routectl-cli`, not this crate) | none carried | no-redirect |
//!
//! The last row is the one client outside this crate: it builds its own
//! `reqwest::Client` at the CLI fetch boundary (so `routectl-router` stays
//! reqwest-free) and is genuinely uncredentialed -- two fixed public
//! catalog URLs, no auth headers. It still pins `Policy::none()` so no
//! workspace client inherits the stock policy silently, and its fetch path
//! maps the returned 3xx to a named source failure.
//!
//! There is no evidence in the docs, fixtures, or wire-quirk notes that
//! any of these lanes depends on following a same-host (or any) 3xx --
//! checked `docs/WIRE-GOTCHAS.md`, `docs/PROVIDER-QUIRKS.md`, and
//! `docs/CONFIGURATION.md` (which documents the mantle lanes' no-redirect
//! posture but records no first-party redirect dependency). With
//! redirects disabled, reqwest hands back the 3xx response itself rather
//! than an error, so every lane's status handling adds an explicit arm
//! for the 300..400 range (checked BEFORE the success/parse path) that
//! maps a 3xx to an upstream server-fault error -- the same uniform,
//! simpler choice as the no-redirect policy itself, applied all the way
//! through to the client-visible error class. There is currently no lane
//! that needs the stock redirect-following policy; [`common_builder`] is
//! the shared TLS/timeout/UA/proxy base every lane-specific builder wraps.
//!
//! # PROXY POLICY
//!
//! Every builder takes the base URL its client will dial and fixes the
//! proxy policy once, at construction, from that URL's parsed host:
//!
//! - a loopback host -- the exact name `localhost`, an IPv4 literal in
//!   `127.0.0.0/8`, native `::1`, or IPv4-mapped loopback -- is dialed
//!   directly (`ClientBuilder::no_proxy`) over `http` and `https` alike.
//!   The target is a process on this machine; routing it through
//!   `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` would hand its credentials
//!   and prompt content to an unrelated hop, in the clear for `http`. The
//!   classification uses the parsed host variant, never its text, so a DNS
//!   name such as `127.example.test` or `localhost.` is NOT loopback, and
//!   neither is the IPv4-compatible form (`::127.0.0.1`), which is not
//!   native `::1` and may follow an ordinary IPv6 route. The router's
//!   cleartext admission check classifies hosts the same way.
//! - any other parsed `http` or `https` host keeps reqwest's stock
//!   system-proxy discovery (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`,
//!   `NO_PROXY`, platform settings), so an operator's outbound proxy keeps
//!   working for real upstreams. Router validation refuses non-loopback
//!   cleartext `http` in config; a library caller that builds a provider
//!   directly against one gets the same proxy behavior as a stock client.
//! - a base URL that does not parse, has no host, or names another scheme
//!   is dialed directly. It then fails normal request-URL validation; it
//!   never gains a proxy.
//!
//! reqwest's proxy policy is client-wide (an explicit `Proxy` would switch
//! off system discovery entirely), and each client here serves one fixed
//! base authority, so construction time is the exact decision point.

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

/// Fixed message for the upstream error every lane surfaces when a
/// response comes back 3xx. Every credentialed lane disables
/// redirect-following (see the module docs above), so a 3xx is never a
/// hop to chase -- it is an upstream fault. Shared literal so the wording
/// cannot drift between lanes.
pub const REDIRECT_NOT_FOLLOWED_MESSAGE: &str = "upstream redirected; redirects are not followed";

/// Map a 3xx response to an upstream error that classifies like a server
/// fault (`FailureClass::ServerError`: retryable, fallbackable, breaker-
/// debited) instead of the unclassified `Unknown` a raw 3xx status would
/// otherwise produce. Every lane's status gate must call this for
/// `300..400` BEFORE its success/parse path -- a 3xx response body is not
/// JSON in the shape either path expects, and a raw 3xx echoed to the
/// client would carry no `Location` the client can use.
pub fn redirect_not_followed_error(provider_id: &str) -> routectl_core::Error {
    routectl_core::Error::upstream(provider_id, 502, REDIRECT_NOT_FOLLOWED_MESSAGE)
}

/// Status assumed for an error reported inside an HTTP-200 body when the
/// body carries no usable HTTP error status of its own.
#[cfg(any(feature = "openai-compat", feature = "gemini"))]
pub const IN_BAND_ERROR_DEFAULT_STATUS: u16 = 502;

/// Derive the HTTP status for an error object delivered inside a successful
/// response body from its `code` member. Only an integer in `400..=599` is
/// trusted; anything else (absent, string, success or redirect code,
/// out-of-range number) yields [`IN_BAND_ERROR_DEFAULT_STATUS`]. The ingress
/// maps any status it cannot render to 502, so an unclamped value would make
/// the status the client sees disagree with the one the router classified.
#[cfg(any(feature = "openai-compat", feature = "gemini"))]
pub fn in_band_error_status(code: Option<&serde_json::Value>) -> u16 {
    code.and_then(serde_json::Value::as_u64)
        .and_then(|n| u16::try_from(n).ok())
        .filter(|s| (400..=599).contains(s))
        .unwrap_or(IN_BAND_ERROR_DEFAULT_STATUS)
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

/// Build a `reqwest::Client` for requests against `base_url`: same TLS
/// floor, connect timeout, and proxy policy (see the module docs) as every
/// other client in this module, with redirect-following DISABLED. A probe must be EXACTLY one
/// request -- following a `Location` header would turn a single GET
/// into multiple hops and let a hostile endpoint steer the probe to an
/// unintended host (SSRF).
///
/// Returns `Result` rather than panicking so a probe on a machine with a
/// broken TLS store degrades to a typed `Unreachable` outcome instead of
/// aborting `doctor`. Provider-construction callers, by contrast,
/// intentionally `.expect()` this result: a client that cannot be built
/// at startup is fatal there -- the `Result` exists for the degradable
/// probe path, not to make provider construction fallible.
#[cfg(any(
    feature = "openai-compat",
    feature = "anthropic-api",
    feature = "openai-responses",
    feature = "gemini"
))]
pub fn build_no_redirect(user_agent: Option<&str>, base_url: &str) -> reqwest::Result<Client> {
    common_builder(user_agent, base_url)
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

/// Build a `reqwest::Client` with an attached cookie provider. Used by
/// the openai-responses provider to pin Cloudflare cookies across
/// requests against `chatgpt.com/backend-api/codex` (mirrors codex
/// CLI's `with_chatgpt_cloudflare_cookie_store`). The jar is shared
/// via Arc so the caller can persist it on shutdown.
///
/// Redirect-following is disabled for the same reason as
/// [`build_no_redirect`]: this lane's `Authorization` and
/// `chatgpt-account-id` headers are not both covered by reqwest's
/// default cross-host strip list, so a followed 3xx could carry
/// `chatgpt-account-id` to an unintended host. The cookie jar itself
/// stays domain-scoped regardless (the underlying `cookie_store` crate
/// only attaches a cookie to requests matching its recorded domain), so
/// disabling redirects here is purely about the header pair.
#[cfg(feature = "openai-responses")]
pub fn build_with_cookie_provider<S>(
    user_agent: Option<&str>,
    base_url: &str,
    jar: std::sync::Arc<S>,
) -> Client
where
    S: reqwest::cookie::CookieStore + 'static,
{
    common_builder(user_agent, base_url)
        .cookie_provider(jar)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("reqwest::Client::build failed (TLS init?); fatal at startup")
}

/// True when a client dialing `base_url` may use system proxy discovery:
/// a parsed `http` / `https` URL whose host is not loopback. See the module
/// docs, "PROXY POLICY".
fn uses_system_proxy(base_url: &str) -> bool {
    reqwest::Url::parse(base_url).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some_and(|h| !is_loopback_host(h))
    })
}

/// Loopback test over a parsed URL's `host_str()`. The URL parser has
/// already normalized the host: every IPv4 spelling is canonical dotted
/// form, an IPv6 literal is bracketed, and a domain is lowercased. So a
/// host that parses as an IP address here was an IP literal in the URL,
/// and anything else is a DNS name, of which only `localhost` is loopback.
fn is_loopback_host(host: &str) -> bool {
    use std::net::{Ipv4Addr, Ipv6Addr};
    if let Some(v6) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        return v6.parse::<Ipv6Addr>().is_ok_and(|ip| {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        });
    }
    host.parse::<Ipv4Addr>()
        .map_or(host == "localhost", |ip| ip.is_loopback())
}

/// Shared builder body: TLS-1.2 floor, timeouts, optional UA, and the
/// host-keyed proxy policy for `base_url`. Centralized so
/// `build_no_redirect` and `build_with_cookie_provider` cannot drift on
/// the TLS / proxy / etc. defaults.
fn common_builder(user_agent: Option<&str>, base_url: &str) -> reqwest::ClientBuilder {
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
    if !uses_system_proxy(base_url) {
        builder = builder.no_proxy();
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
#[path = "http_client_tests.rs"]
mod tests;
