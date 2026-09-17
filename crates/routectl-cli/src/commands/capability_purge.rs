//! `routectl capability purge <target> <capability>` -- ask the running daemon
//! to drop one learned-capability entry.
//!
//! # Why this goes through the daemon
//!
//! A learned negative lives in the daemon's in-memory registry, and its
//! persisted form lives in a SQLite ledger the daemon holds open. So this
//! command NEVER touches the ledger: editing the file directly would leave the
//! live verdict steering routing until the next restart, and a read-write open
//! against the daemon's own database is how a healthy ledger loses its
//! write-ahead sidecars. The daemon owns the state; this command is a client.
//!
//! # Persistence is the override config, not this command
//!
//! A purge removes ONE observation. Traffic that provokes the same rejection
//! teaches it again, which is correct -- an observation is not a decision. An
//! operator who wants the decision to stick writes it in
//! `[capability.overrides]` (see CONFIGURATION.md, "Capability intelligence"),
//! and this command points them there rather than offering a second, competing
//! runtime store for the same intent.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use routectl_router::Config;
use serde_json::json;

/// Path of the daemon's purge route.
const PURGE_PATH: &str = "/control/capability/purge";

/// Ceiling on the whole round trip. The daemon's work is a map removal and a
/// non-blocking enqueue, so a slow answer means the daemon is saturated or the
/// port belongs to something else -- both of which the operator wants reported
/// rather than waited out.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Where an operator sets a DURABLE decision, quoted verbatim in the success
/// output. One spelling, so the command and the docs cannot drift on the name.
const OVERRIDE_TABLE: &str = "[capability.overrides]";

/// Ceiling on the rendered refusal code. A real code is a short closed-set
/// token; the cap only bounds what an impostor on that port can push into the
/// operator's terminal through a diagnostic line.
const MAX_REFUSAL_CODE_CHARS: usize = 64;

/// Run one purge against the configured daemon and return the process exit
/// code: `0` on a purge or a clean no-op, non-zero on any failure to reach or
/// be understood by the daemon.
///
/// Every failure path writes to stderr and returns non-zero WITHOUT touching
/// local state -- there is no local state to leave half-written, which is the
/// point of routing through the daemon. A failure is never reported as a purge.
pub async fn run(config: &Config, target: &str, capability: &str) -> i32 {
    // Derive the destination BEFORE resolving any credential: an underivable
    // one must refuse locally rather than put a listener token on a socket
    // whose address nobody chose.
    let Some(url) = control_url(&config.server.host, config.server.port) else {
        eprintln!(
            "error: could not derive a loopback control address from the \
             configured `[server] host`"
        );
        eprintln!(
            "       the control surface is loopback-only; `[server] host` must \
             be an explicit loopback literal (`127.0.0.1` or `::1`) or a \
             wildcard bind (`0.0.0.0` or `::`)"
        );
        eprintln!(
            "       a hostname -- `localhost` included -- is not accepted: it \
             names no address family, and resolving it would let a resolver \
             decide where this request goes"
        );
        return 1;
    };
    let client = match build_client() {
        Ok(client) => client,
        Err(e) => {
            eprintln!("error: could not build an HTTP client: {e}");
            return 1;
        }
    };

    let mut request = client.post(&url).json(&json!({
        "state_key": target,
        "capability_key": capability,
    }));
    // The daemon's control route sits behind the SAME listener auth as
    // `/v1/*`, so a token-configured daemon needs a credential here. Only the
    // first configured token is sent: the set is an accept-list, so any member
    // authenticates, and sending more than one would leak how many are
    // configured. The value is a secret REFERENCE in config; resolving it is
    // the caller's business, not this command's -- so an unresolvable
    // reference simply produces an unauthenticated request and the daemon's own
    // 401 is what the operator sees.
    if let Some(token) = first_listener_token(config).await {
        request = request.header("x-api-key", token);
    }

    let response = match request.send().await {
        Ok(response) => response,
        Err(e) => {
            eprintln!("error: could not reach the routectl daemon at {url}: {e}");
            eprintln!("       is `routectl serve` running on that address?");
            return 1;
        }
    };

    let status = response.status();
    // A 3xx arm ahead of the success gate, for the DIAGNOSTIC only. The
    // classification is already correct without it -- `is_success()` is
    // 200..300, so a 3xx takes the failure path either way, and that is the
    // property the status test pins (it drives 3xx responses carrying a valid
    // purge-success body, which the defective `>= 400` shape would report as a
    // purge). What this arm adds is the right message: a redirect means
    // something other than the daemon answered on that port, and telling the
    // operator that beats rendering an absent `error.code` as `unknown`.
    if status.is_redirection() {
        eprintln!("error: the control address answered with a redirect (status {status})");
        eprintln!("       redirects are not followed; is another service on that port?");
        return 1;
    }
    let body: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        // The daemon's refusal codes are a closed set, so the code is the only
        // actionable detail there is -- but this string arrives from whatever
        // answered on that port, so it is untrusted bytes on a path straight to
        // a terminal and is sanitized and capped before it is rendered. The
        // message is NOT echoed at all: it is fixed prose the daemon owns, and
        // reprinting it would make this command's output depend on the daemon's
        // wording as well as widen the untrusted surface.
        let raw_code = body["error"]["code"].as_str().unwrap_or("unknown");
        let code = render_refusal_code(raw_code);
        eprintln!("error: the daemon refused the purge (status {status}, code {code})");
        // One actionable line for the outcomes an operator can DO something
        // about, matched on the raw token before sanitization (the sanitized
        // form is for display only). Every other code, and any code this build
        // does not know, falls through with the line above alone -- guessing at
        // an unknown refusal would be worse than saying nothing.
        //
        // The three below all leave the entry EXACTLY as it was, which is what
        // makes retrying safe and worth telling the operator.
        match raw_code {
            DURABILITY_FAILED_CODE => {
                eprintln!(
                    "       the clear could not be persisted, so nothing was removed and the \
                     entry is unchanged."
                );
                eprintln!("       check the daemon's usage-database health, then run this again.");
            }
            PURGE_BUSY_CODE => {
                eprintln!(
                    "       another purge of this same key is in flight; nothing was changed."
                );
                eprintln!("       wait for it to finish, then run this again if still needed.");
            }
            PURGE_SUPERSEDED_CODE => {
                eprintln!(
                    "       the entry changed while this ran, so nothing was removed -- the \
                     daemon refused to delete a newer observation."
                );
                eprintln!("       check the current state, then run this again if still needed.");
            }
            PURGE_STALE_CODE => {
                eprintln!(
                    "       the daemon reloaded its configuration while this ran; nothing was \
                     changed."
                );
                eprintln!("       run this again to act on the reloaded configuration.");
            }
            _ => {}
        }
        return 1;
    }

    match body["purged"].as_bool() {
        Some(true) => {
            println!("purged learned capability `{capability}` on `{target}`");
            println!(
                "note: live traffic can teach this again. To make the decision \
                 durable, set it under {OVERRIDE_TABLE} in your config."
            );
            0
        }
        Some(false) => {
            println!("nothing to purge: no learned entry for `{capability}` on `{target}`");
            0
        }
        // A success status with no `purged` field means the daemon answered
        // something this build does not understand -- report it rather than
        // guessing, so an operator never reads a purge that may not have
        // happened.
        None => {
            eprintln!("error: the daemon returned an unrecognized purge response");
            1
        }
    }
}

/// Build the HTTP client for a control call.
///
/// Two settings are load-bearing, and neither is reqwest's default:
///
/// - **No proxy.** reqwest reads `HTTP_PROXY` / `ALL_PROXY` (and their
///   lowercase spellings) from the environment by default. A control call is a
///   loopback conversation with a process on this machine, so honoring an
///   operator's outbound web proxy would hand the listener token and the
///   operator's own target names to an unrelated hop -- silently, on any box
///   where those variables happen to be exported.
/// - **No redirects.** The `x-api-key` this command sends is NOT on reqwest's
///   cross-host strip list (which covers only `Authorization`, `Cookie`,
///   `Proxy-Authorization` and `WWW-Authenticate`), so a followed 3xx would
///   carry the credential and the request body to whatever host a `Location`
///   header names. The same reasoning and the same policy as every credentialed
///   client in `routectl-providers::http_client`; the caller maps the returned
///   3xx to a failure explicitly.
fn build_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

/// The control-route URL for a daemon whose `[server] host` is `bind`, or
/// `None` when no loopback endpoint can be derived from it.
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
///   that the control surface is unreachable as configured.
///
/// `localhost` is refused along with every other NAME, and that is deliberate
/// rather than an oversight. Accepting it would force this command to pick an
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
fn control_url(bind: &str, port: u16) -> Option<String> {
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
    Some(authority_url(SocketAddr::new(destination, port)))
}

/// Render the control URL for a resolved socket address. `SocketAddr`'s own
/// `Display` brackets an IPv6 literal, which is the whole reason the authority
/// is built through it rather than by string interpolation.
fn authority_url(addr: SocketAddr) -> String {
    format!("http://{addr}{PURGE_PATH}")
}

/// The daemon refusal codes this command renders guidance for.
///
/// Mirrored rather than imported so the CLI keeps rendering a WIRE vocabulary
/// (this command also talks to a daemon of a different build), and pinned equal
/// to the route's own constants by a test -- so the mirror cannot drift silently
/// while the guidance quietly stops matching.
const DURABILITY_FAILED_CODE: &str = "durability_failed";
const PURGE_BUSY_CODE: &str = "purge_busy";
const PURGE_STALE_CODE: &str = "purge_stale";
const PURGE_SUPERSEDED_CODE: &str = "purge_superseded";

/// Render an untrusted refusal code safe for a terminal line.
///
/// The code arrives as a JSON string from whatever answered on the control
/// port, so it is untrusted input on a path straight to the operator's screen.
/// Rendering it raw forges output: a newline fabricates a whole extra line that
/// reads as routectl's own diagnostics ("error: purged 47 entries"), and an ANSI
/// CSI sequence repaints or clears the screen. Routed through the same shared
/// filter every log surface in this workspace uses, with a tighter cap because
/// a legitimate code is a short closed-set token.
fn render_refusal_code(raw: &str) -> String {
    routectl_core::sanitize_for_log_with_cap(raw, MAX_REFUSAL_CODE_CHARS)
}

/// The first configured listener token, resolved through the shared secret
/// resolver, or `None` when the daemon runs token-less or the reference does
/// not resolve here.
///
/// A resolution failure is deliberately silent: this command has no business
/// diagnosing the operator's credential store, and the daemon's own 401 is a
/// clearer report than a guess from the client side.
async fn first_listener_token(config: &Config) -> Option<String> {
    use routectl_auth::{MemoryStore, SecretRef, SecretStore};

    let uri = config.server.auth.as_ref()?.tokens.first()?;
    let secret_ref = SecretRef::parse(uri).ok()?;
    MemoryStore::new().get(&secret_ref).await.ok()
}

#[cfg(test)]
#[path = "capability_purge_tests.rs"]
mod tests;
