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
    let mut builder = Client::builder()
        // Defense-in-depth: every real provider endpoint enforces
        // TLS 1.2+, but pinning here closes any path where reqwest's
        // default would negotiate down to an older protocol against
        // a misconfigured proxy or an in-the-middle box. Cheap, no
        // operational impact.
        .min_tls_version(reqwest::tls::Version::TLS_1_2);
    if let Some(ua) = user_agent {
        builder = builder.user_agent(ua);
    }
    builder
        .build()
        .expect("reqwest::Client::build failed (TLS init?); fatal at startup")
}

/// Header names that `extra_headers` is NOT allowed to set, because
/// doing so would silently bypass the provider's auth contract or
/// corrupt the wire body shape. Compared case-insensitively against
/// the user-supplied key.
///
/// - `authorization` and `x-api-key` are the auth carriers themselves.
/// - `host` is request-routing; overriding it would let TOML pin a
///   different upstream and confuse SigV4 / virtual-host aware servers.
/// - `content-type` is set by reqwest's `.json()` to
///   `application/json`. Overriding it (e.g. to `text/plain`) would
///   make Anthropic / OpenAI reject the body with a vague 400 that
///   looks like an auth or schema error.
/// - `content-length` is computed by reqwest from the serialized body;
///   a TOML override desyncs the wire contract.
const RESERVED_EXTRA_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    "host",
    "content-type",
    "content-length",
];

/// True if the given header name is reserved for routectl's own
/// management and must not be set via user `extra_headers`.
pub fn is_reserved_extra_header(name: &str) -> bool {
    let lc = name.to_ascii_lowercase();
    RESERVED_EXTRA_HEADERS.contains(&lc.as_str())
}
