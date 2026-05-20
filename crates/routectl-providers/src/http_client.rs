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
const AUTH_HEADERS: &[&str] = &["authorization", "x-api-key", "anthropic-version"];

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

/// True if the given header name carries provider auth. Case-insensitive.
pub fn is_auth_header(name: &str) -> bool {
    let lc = name.to_ascii_lowercase();
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

#[cfg(test)]
mod tests {
    use super::{
        is_auth_header, is_managed_header, is_reserved_extra_header, AUTH_HEADERS, MANAGED_HEADERS,
    };

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
        for name in ["anthropic-beta", "content-type", "host", "x-request-id"] {
            assert!(!is_auth_header(name), "{name:?} must NOT classify as auth");
        }
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
