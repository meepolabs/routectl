//! The CLI's HTTP client for the local daemon: where to dial, how to dial it,
//! which credential to carry, and how much of an answer to read.
//!
//! Every input here (`[server] host`, the listener token reference, the proxy
//! environment) comes from the operator's own configuration and shell. The
//! policy is still strict, because what answers on the configured port is not
//! necessarily the daemon: the destination is a loopback IP literal or nothing,
//! the client never proxies and never follows a redirect, and a response body is
//! read only up to [`MAX_BODY_BYTES`].

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use routectl_router::Config;

/// Ceiling on a whole round trip to the daemon. Its control and status routes
/// answer from memory, so a slow answer means the daemon is saturated or the
/// port belongs to something else -- both of which the operator wants reported
/// rather than waited out.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Ceiling on a response body read through [`read_capped_body`]. Far above any
/// answer the daemon gives a CLI call, and low enough that whatever holds the
/// port cannot make the CLI buffer without bound.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Why a response body could not be read.
#[derive(Debug, thiserror::Error)]
pub enum BodyError {
    #[error("response body exceeds {limit} bytes")]
    TooLarge { limit: usize },
    #[error("reading the response body failed: {0}")]
    Read(#[source] reqwest::Error),
}

/// Build the HTTP client for a daemon call, with `timeout` bounding the whole
/// round trip.
///
/// Two settings are load-bearing, and neither is reqwest's default:
///
/// - **No proxy.** reqwest reads `HTTP_PROXY` / `ALL_PROXY` (and their
///   lowercase spellings) from the environment by default. A daemon call is a
///   loopback conversation with a process on this machine, so honoring an
///   operator's outbound web proxy would hand the listener token and the
///   operator's own target names to an unrelated hop -- silently, on any box
///   where those variables happen to be exported.
/// - **No redirects.** The `x-api-key` these calls send is NOT on reqwest's
///   cross-host strip list (which covers only `Authorization`, `Cookie`,
///   `Proxy-Authorization` and `WWW-Authenticate`), so a followed 3xx would
///   carry the credential and the request body to whatever host a `Location`
///   header names. The same reasoning and the same policy as every credentialed
///   client in `routectl-providers::http_client`; each caller maps the returned
///   3xx to a failure explicitly.
pub fn build_client(timeout: Duration) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(timeout)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

/// The URL of `path` on a daemon whose `[server] host` is `bind`, or `None`
/// when no loopback endpoint can be derived from it.
///
/// `[server] host` names what the daemon BINDS, which is not always something
/// to dial. Three cases:
///
/// - a LITERAL loopback bind is preserved exactly (any address in
///   `127.0.0.0/8`, `::1`, or an IPv4-mapped loopback). Preserved, not
///   normalized: the daemon bound that specific address, so rewriting
///   `127.0.0.5` to `127.0.0.1` would dial a port nothing is listening on;
/// - a WILDCARD bind (`0.0.0.0` / `::`) is reachable on every interface
///   INCLUDING loopback, so the loopback address of the matching family is a
///   destination the operator's own configuration justifies;
/// - a SPECIFIC non-loopback bind is REFUSED. It must not be rewritten to
///   loopback: a daemon bound only to `10.20.30.40` is not listening on
///   loopback at all, so the rewrite would send a credentialed request to
///   whatever OTHER process holds that port on this machine -- and it would
///   silently contradict the operator's configuration rather than reporting
///   that the daemon is unreachable as configured.
///
/// `localhost` is refused along with every other NAME, and that is deliberate
/// rather than an oversight. Accepting it would force the CLI to pick an
/// address FAMILY the operator never chose: the name resolves to `127.0.0.1`,
/// `::1`, or both depending on the resolver and the hosts file, and the daemon
/// bound whichever one it was handed. Guessing wrong aims a credentialed request
/// at a port that may belong to a different process, and resolving the name
/// instead would let a resolver answer decide where the credential goes. There
/// is no correct guess, so the operator states the literal -- which they already
/// had to give the daemon.
///
/// Everything else -- any hostname, an empty string, a URL, a `host:port` pair,
/// a malformed literal -- yields `None` for the same reason.
///
/// The authority is rendered through `SocketAddr`, so an IPv6 literal is
/// bracketed. An unbracketed `::1:8791` is not a parseable authority at all.
pub fn control_url(bind: &str, port: u16, path: &str) -> Option<String> {
    let host = bind.trim();
    if host.is_empty() {
        return None;
    }
    // A bracketed IPv6 literal as written in config, e.g. `[::1]`.
    let literal = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host);
    // Only a parseable IP LITERAL gets past here: a name (`localhost` included)
    // fails this parse and is refused.
    let ip: IpAddr = literal.parse().ok()?;
    let destination = if crate::server::is_loopback(&ip.to_string()) {
        // Already loopback: preserve the exact address the daemon bound.
        ip
    } else if ip.is_unspecified() {
        // A wildcard bind covers loopback too, so the matching family's
        // loopback address is where the daemon can be reached.
        match ip {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        }
    } else {
        // A specific non-loopback bind. Refuse rather than guess.
        return None;
    };
    Some(authority_url(SocketAddr::new(destination, port), path))
}

/// Render the URL of `path` at a resolved socket address. `SocketAddr`'s own
/// `Display` brackets an IPv6 literal, which is the whole reason the authority
/// is built through it rather than by string interpolation.
fn authority_url(addr: SocketAddr, path: &str) -> String {
    format!("http://{addr}{path}")
}

/// The first configured listener token, resolved through the shared secret
/// resolver, or `None` when the daemon runs token-less or the reference does
/// not resolve here.
///
/// Only the first is returned: the configured set is an accept-list, so any
/// member authenticates, and sending more than one would leak how many are
/// configured. A resolution failure is deliberately silent: the CLI has no
/// business diagnosing the operator's credential store, and the daemon's own
/// 401 is a clearer report than a guess from the client side.
pub async fn first_listener_token(config: &Config) -> Option<String> {
    use routectl_auth::{MemoryStore, SecretRef, SecretStore};

    let uri = config.server.auth.as_ref()?.tokens.first()?;
    let secret_ref = SecretRef::parse(uri).ok()?;
    MemoryStore::new().get(&secret_ref).await.ok()
}

/// Read `response`'s body, refusing one longer than [`MAX_BODY_BYTES`].
///
/// The body is read chunk by chunk and the cap is checked before each chunk is
/// kept, so an oversized answer costs at most one chunk past the cap rather than
/// the whole stream.
pub async fn read_capped_body(mut response: reqwest::Response) -> Result<Vec<u8>, BodyError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(BodyError::Read)? {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(BodyError::TooLarge {
                limit: MAX_BODY_BYTES,
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
#[path = "daemon_client_tests.rs"]
mod tests;
