//! Status-subtree-only middleware: an anti-DNS-rebinding `Host` allowlist and
//! a bounded-concurrency load-shed responder.
//!
//! Everything here scopes to `/status*` ONLY. The ingress `/v1/*` proxy lane
//! carries none of it: the proxy degrades last, never sheds, and does not
//! second-guess the `Host` header (an operator points arbitrary clients at
//! it). The status family is a bounded diagnostic read, so a burst of pollers
//! or a browser-driven DNS-rebinding probe must never compete with -- or leak
//! through -- the forwarding path.
//!
//! # The coupled status-timing numbers
//!
//! Five numbers bound how long a `/status*` request may occupy the process, and
//! none of them can be changed on its own. They are collected here once so a
//! reader of any one of them finds the whole derivation in one place; each
//! const's own comment states what it is and points back here.
//!
//! | number | where | what it bounds |
//! |---|---|---|
//! | [`STATUS_MAX_INFLIGHT`] = 4 | this module | admitted concurrent `/status*` requests, and equally concurrent blocking panel builders |
//! | [`QUERY_BUDGET_MS`] = 2000 | this module | one `/status/query` grouped aggregate |
//! | [`USAGE_BUDGET_MS`] = 1000 | this module | one whole `/status/usage` panel build |
//! | `BODY_READ_TIMEOUT` = 1000ms | `handlers::status::query` | how long that route waits for the client's request body |
//! | `TIMEOUT_MS` = 2000 / `QUERY_TIMEOUT_MS` = 3500 | `dash_00_state.js` | the browser's own aborts -- the GET one covers `/status` and `/status/usage`, the QUERY one covers `/status/query` only |
//!
//! **The sum identity.** On `/status/query` the body read and the panel
//! deadline are SERIAL in one handler (the handler awaits `to_bytes` under
//! `BODY_READ_TIMEOUT`, and only then runs the build whose deadline is
//! [`QUERY_BUDGET_MS`]), so the two BUDGETED stretches sum, and that sum is what
//! must stay inside the client's QUERY abort:
//!
//! ```text
//! BODY_READ_TIMEOUT + QUERY_BUDGET_MS = 1000 + 2000 = 3000 <= 3500 (QUERY_TIMEOUT_MS)
//! ```
//!
//! Raising either term alone inverts it: at 2000 + 2000 = 4000 the browser
//! aborts before the server can shed its own panel, turning a clean
//! `query_timeout` into a client-side timeout. Whoever changes one term must
//! re-derive the sum, and the third number (3500) lives in JavaScript, so no
//! compile-time assertion can carry this for you.
//!
//! **What the identity does NOT bound.** It covers the two BUDGETED stretches,
//! not end-to-end request time, so 3000 <= 3500 is necessary but not sufficient
//! for "the server always sheds before the browser aborts". Unbudgeted time sits
//! between and around them:
//!
//! - the wait for a builder capacity permit. Capacity is held through the
//!   blocking work and so survives cancellation, which is deliberate -- but it
//!   means a replacement request admitted after a client abort can wait on a
//!   detached builder before its own budget even starts. The query deadline is
//!   anchored inside the build for exactly this reason: it measures the QUERY,
//!   not the queueing.
//! - `spawn_blocking` worker-queue delay.
//! - opening the ledger, the `earliest_ts_start` anchor probe, the caller-side
//!   fold, and JSON serialization -- none reachable by the SQLite progress
//!   handler (see Occupancy below).
//!
//! So a saturated surface CAN exceed 3500ms and be client-aborted rather than
//! returning a clean `query_timeout`. That is a degradation, not a correctness
//! break: the panel is read-only and the client retries. Bounding it would take
//! an end-to-end deadline started after the body read and threaded through the
//! capacity wait, which is a separate change from sizing these budgets.
//!
//! **Occupancy.** The capacity unit is ADMITTED REQUESTS, and an admitted
//! request holds at most one blocking builder at a time (the `/status`
//! aggregate composes its panels sequentially), so worst-case occupancy is
//! [`STATUS_MAX_INFLIGHT`] builders each held for roughly
//! `max(QUERY_BUDGET_MS, USAGE_BUDGET_MS)`. Roughly, not exactly: the deadline
//! is checked from a SQLite progress handler every `PROGRESS_OPS` VM ops, and
//! opening the ledger, the `earliest_ts_start` anchor probe, the caller-side
//! fold and the JSON serialization all sit outside its reach, so a builder can
//! overshoot its budget by the cost of whichever of those it is in.
//!
//! On the `/status` aggregate -- and ONLY there -- the panel budgets are terms
//! of a sequential sum that must land inside the 2000ms GET abort.
//! [`QUERY_BUDGET_MS`] is not one of those terms: `/status/query` is its own
//! route with its own abort.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::server::is_loopback;

/// Ceiling on concurrent in-flight `/status*` requests across the WHOLE
/// subtree. The unit is ADMITTED HTTP REQUESTS, and it is equally the ceiling
/// on concurrent blocking panel builders -- see this module's own docs for that
/// equivalence and for the timing numbers it composes with. A concurrent
/// fan-out inside the aggregate would silently multiply the builder count by
/// four while this const stayed at its face value.
///
/// The builder half of the ceiling holds UNCONDITIONALLY, cancellation
/// included. This layer's permit lives in the response future, so a client that
/// aborts mid-request does release its admission permit -- but not its builder
/// capacity, which `handlers::status::guard_panel` moves into the
/// `spawn_blocking` closure itself (see `handlers::status::BuilderCapacity`).
/// A detached builder therefore keeps its permit until the blocking work ends,
/// and the next request is DELAYED behind it rather than admitted alongside it.
/// Delayed, never shed: shedding at that point would need a panel-level reason
/// code the wire does not carry.
///
/// Deliberately a hardcoded const, not a config knob: the status surface is a
/// fixed-cost diagnostic read, so a small shared cap keeps a poller burst from
/// monopolizing the blocking pool the panel builders run on. Excess sheds
/// immediately as a 503 (never queues).
pub const STATUS_MAX_INFLIGHT: usize = 4;

/// Wall-clock budget for ONE `/status/query` grouped aggregate, milliseconds.
/// Deliberately a hardcoded const, not a config knob: like
/// [`STATUS_MAX_INFLIGHT`] it bounds a fixed-cost diagnostic read rather than
/// expressing an operator preference. An overrun sheds as an unavailable panel
/// (`query_timeout`), never a 500.
///
/// Sized to bound a runaway scan without cutting a legitimate large-ledger read
/// short. A fine-grained GROUP BY is NOT sub-second at scale: on a million-row
/// window it measures around 1.0s unpriced-aggregate and around 1.2s with a
/// bucketed series, so a 1000ms budget shed real reads.
///
/// Raising it is not a local decision -- it is one term of the sum identity in
/// this module's own docs, and cannot move without the other.
pub const QUERY_BUDGET_MS: u64 = 2000;

/// Wall-clock budget for ONE `/status/usage` panel build, milliseconds --
/// covering the WHOLE collection (all four ledger reads on one connection),
/// not any single statement. Hardcoded for the same reason as
/// [`QUERY_BUDGET_MS`]: it bounds a fixed-cost diagnostic read, not an operator
/// preference. An overrun sheds the panel as unavailable (`query_timeout`),
/// never a 500.
///
/// Sized against the CLIENT's 2000ms per-GET abort: the `/status` aggregate
/// composes its panels sequentially, so this budget is one term of a sum that
/// must still land inside that abort, leaving room for the three cheap panels
/// and the response write. Raising it is not a local decision -- see this
/// module's own docs for the coupled numbers and the occupancy relationship
/// with [`STATUS_MAX_INFLIGHT`].
pub const USAGE_BUDGET_MS: u64 = 1000;

/// Wire schema version of the fixed transport-level envelopes this module
/// emits (the overload 503 and the forbidden-host 403). These are NOT panel
/// payloads -- they never reach a `Panel<T>` -- but they carry the same
/// `schema_version` baseline so a consumer reads one dialect across the
/// status surface.
const GATE_SCHEMA_VERSION: u32 = 1;

/// Resolved `Host` allowlist for the status subtree: the loopback literals
/// plus the exact address routectl actually bound. Under a wildcard bind
/// (`0.0.0.0` / `::`) it degrades to a bound-port check (see `Self::allows`).
/// Cheap to clone (an `Arc`), as the axum middleware state machinery clones it
/// per request.
#[derive(Clone)]
pub struct StatusHostAllowlist {
    inner: Arc<AllowlistInner>,
}

struct AllowlistInner {
    /// The bound host literal, e.g. `127.0.0.1` or (under `--unsafe-public`) a
    /// LAN address like `192.168.1.5`.
    bind_host: String,
    /// The bound `host:port`, e.g. `127.0.0.1:8080`.
    bind_host_port: String,
    /// The bound port, matched on its own under a wildcard bind.
    bind_port: u16,
    /// Whether the bind address is unspecified (`0.0.0.0` / `::`), which no
    /// client-supplied `Host` can name literally.
    is_wildcard: bool,
}

impl StatusHostAllowlist {
    /// Build from the address routectl bound. Loopback binds are covered by
    /// the loopback-literal check regardless; the stored bind address is what
    /// lets a deliberate `--unsafe-public` LAN bind stay reachable.
    pub fn new(bound: SocketAddr) -> Self {
        Self {
            inner: Arc::new(AllowlistInner {
                bind_host: bound.ip().to_string(),
                bind_host_port: bound.to_string(),
                bind_port: bound.port(),
                is_wildcard: bound.ip().is_unspecified(),
            }),
        }
    }

    /// A `Host` value is allowed when it is a loopback literal (with or
    /// without a port). Otherwise: under a wildcard bind (`0.0.0.0` / `::`) no
    /// client can name the unspecified address literally, so the guard
    /// degrades to a PORT check -- a Host is allowed iff its parsed port equals
    /// the bound port (a portless Host fails closed). This weaker anti-rebinding
    /// posture is acceptable because token auth now sits ABOVE this guard: the
    /// port check keeps a deliberate public bind reachable while the token, not
    /// the `Host`, carries the real access decision. Under a concrete
    /// (non-wildcard) bind the exact bound address is required, unchanged.
    fn allows(&self, host: &str) -> bool {
        if is_loopback_authority(host) {
            return true;
        }
        if self.inner.is_wildcard {
            return port_of(host) == Some(self.inner.bind_port);
        }
        host == self.inner.bind_host || host == self.inner.bind_host_port
    }
}

/// One HTTP `Host` authority, split into host and optional port, or `None` when
/// the authority is not well-formed.
///
/// THE single authority parser for this module. [`split_host_port`] and
/// [`port_of`] are thin projections of it, and that is the point: they used to be
/// two independent walks over the same grammar, which is how the port side came
/// to accept `[::1]evil:8080` (bracket contents discarded, trailing junk ignored)
/// while the host side rejected it. Under a WILDCARD bind the allowlist degrades
/// to a port-only check, so that port parse WAS the access decision -- a foreign
/// authority was admitted on a port match alone. One parser makes the two
/// answers agree by construction.
///
/// The accepted grammar, deliberately narrow:
///
///   * a nonempty host, either bracketed (`[...]`) or bare;
///   * a BRACKETED host must parse as an `Ipv6Addr` -- brackets delimit an IPv6
///     literal and nothing else, so a bracketed domain or IPv4 value is refused
///     rather than given a second spelling this parser treats as equivalent;
///   * a bracketed authority is followed by nothing or by `:` plus a `u16`;
///   * a bare authority's port, when present, is `:` plus a `u16`; a bare
///     multi-colon IPv6 literal (`::1`, `2001:db8::10`) has no port at all, so
///     its trailing group is never read as one;
///   * an authority carrying a userinfo `@` is rejected outright, before any host
///     parsing. That rule lives HERE rather than in one caller because every
///     consumer needs it: the wildcard port check reached `@:8080` without ever
///     consulting the loopback predicate that used to own the rule.
///
/// Everything else -- an empty authority or host, an unterminated `[`, an empty
/// bracket, text after `]`, a non-numeric or out-of-range port -- is `None`
/// rather than a partial host.
///
/// # Zone-scoped IPv6 is unsupported, and fails closed
///
/// A zone-scoped literal (`[fe80::1%eth0]`, RFC 6874's `%25`-escaped form
/// included) does NOT parse as an `Ipv6Addr`, so it is refused. That is
/// deliberate and not an omission to fix by widening the grammar:
///
///   * a zone index is meaningful only to the host that owns it -- it names a
///     local interface, not an endpoint -- so it cannot be compared against a
///     bound address the way every other authority here can;
///   * the addresses it scopes are link-local, which is neither loopback nor an
///     address routectl binds, so a zone-scoped authority would be refused by the
///     allowlist even if it parsed;
///   * accepting the syntax would mean deciding whether `%eth0` and `%2` name the
///     same interface, which is a question this predicate has no business
///     answering while a credential is on the line.
///
/// Failing closed costs nothing real: an operator reaching the daemon over a
/// link-local address states the loopback or bound literal instead.
fn parse_authority(authority: &str) -> Option<(&str, Option<u16>)> {
    /// Parse a written port: nonempty, all digits, and inside the `u16` range.
    fn port(raw: &str) -> Option<u16> {
        (!raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()))
            .then(|| raw.parse().ok())
            .flatten()
    }

    // Userinfo makes the reachable host ambiguous, and a legitimate `Host`
    // never carries it (RFC 7230 forbids it), so the whole shape is refused
    // rather than parsed.
    if authority.contains('@') {
        return None;
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let close = rest.find(']')?;
        let (host, after) = (&rest[..close], &rest[close + 1..]);
        // Brackets delimit an IPv6 literal and nothing else (RFC 3986), so the
        // contents must PARSE as one. A bracketed domain or IPv4 value is a shape
        // no conforming client sends, and accepting it would give a host a second
        // spelling this predicate treats as equivalent while a client would not:
        // `[localhost]` and `[127.0.0.1]` passed the loopback check here without
        // naming that host to any browser.
        host.parse::<std::net::Ipv6Addr>().ok()?;
        if after.is_empty() {
            return Some((host, None));
        }
        return Some((host, Some(port(after.strip_prefix(':')?)?)));
    }
    match authority.rsplit_once(':') {
        // A bare multi-colon literal is an address, not `host:port`.
        Some((head, _)) if head.contains(':') => Some((authority, None)),
        Some((host, raw)) => (!host.is_empty()).then_some(()).and_then(|()| {
            let parsed = port(raw)?;
            Some((host, Some(parsed)))
        }),
        None => (!authority.is_empty()).then_some((authority, None)),
    }
}

/// The bare host of an HTTP `Host` authority, or `None` when the authority is
/// not well-formed. A projection of [`parse_authority`]; see it for the grammar
/// and for why the two projections must not diverge.
fn split_host_port(authority: &str) -> Option<&str> {
    parse_authority(authority).map(|(host, _)| host)
}

/// The port written in an HTTP `Host` authority, or `None` when none is written
/// or the authority is not well-formed. A projection of [`parse_authority`], so
/// it cannot accept an authority whose host [`split_host_port`] rejects -- which
/// matters because under a wildcard bind this answer IS the access decision.
fn port_of(authority: &str) -> Option<u16> {
    parse_authority(authority).and_then(|(_, port)| port)
}

/// Whether an HTTP `Host` authority names a loopback endpoint: a WELL-FORMED
/// authority (per [`parse_authority`]) whose host satisfies the server's shared
/// loopback predicate (the full `127.0.0.0/8` range, `::1`, IPv4-mapped IPv6, and
/// `localhost`).
///
/// A malformed authority is not loopback, because it has no host to test --
/// userinfo, trailing junk after `]`, an empty bracket, a bad port. Those rules
/// live in the parser rather than here, so the port projection cannot disagree
/// with this one.
///
/// `pub(crate)` for a SECOND anti-rebinding consumer, not for general use: the
/// mutating control route (`handlers::control`) carries the same guard for the
/// same reason. Sharing the predicate rather than re-deriving it is what keeps
/// the two surfaces from disagreeing about what loopback means; a re-derivation
/// would miss exactly the details the parser's own docs enumerate.
pub(crate) fn is_loopback_authority(authority: &str) -> bool {
    split_host_port(authority).is_some_and(is_loopback)
}

/// Process-wide count of `/status*` requests rejected by the host guard for a
/// disallowed `Host`. A rejected request never reaches a panel handler, so -
/// like the shed counter - this is the only place a wrong-Host 403 is
/// observable.
static HOST_403_COUNT: AtomicU64 = AtomicU64::new(0);

/// Reject a request whose claimed authority falls outside the allowlist
/// (anti-DNS-rebinding). Applied ONLY to the status subtree -- `/v1/*` never
/// sees it.
///
/// Mirrors the mutating control route (`handlers::control`), which carries the
/// same three rules for the same reason. Every claim the request makes is
/// checked, and an unreadable claim fails closed:
///
///   * every `Host` header VALUE, not just the first -- a request may carry the
///     header twice, and which duplicate a downstream reader honors is not this
///     guard's call to assume;
///   * a `Host` value that is not valid UTF-8 -- PRESENT but unevaluable. The
///     client did claim an authority; this build cannot read it, and a claim that
///     cannot be evaluated has not been validated;
///   * the request URI's authority -- HTTP/2 puts `:authority` there and sends no
///     `Host` at all, so a Host-only guard sees nothing on an h2c request and the
///     absent-authority allowance becomes a bypass.
///
/// A request claiming NO authority anywhere is permitted: the rebinding vector is
/// a browser, which always sends one, while a hand-rolled origin-form client
/// legitimately omits it.
///
/// A rejection is counted and logged SAMPLED (1st + every Nth, reusing the shed
/// sampler) at warn, so an operator can tell a wrong-authority rejection apart
/// from a bad-token one. Only the running total is logged, never the claimed
/// value (it is attacker-controlled).
pub async fn host_guard(
    State(allowlist): State<StatusHostAllowlist>,
    req: Request,
    next: Next,
) -> Response {
    let host_claims_ok = req
        .headers()
        .get_all(header::HOST)
        .iter()
        .all(|value| value.to_str().is_ok_and(|host| allowlist.allows(host)));
    let uri_claim_ok = req
        .uri()
        .authority()
        .is_none_or(|authority| allowlist.allows(authority.as_str()));
    let claim_site = if host_claims_ok {
        if uri_claim_ok {
            return next.run(req).await;
        }
        ClaimSite::UriAuthority
    } else {
        ClaimSite::HostHeader
    };
    let host_403_total = HOST_403_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if should_log_shed(host_403_total) {
        tracing::warn!(
            target: SHED_TARGET,
            host_403_total,
            claim_site = claim_site.as_str(),
            "status surface rejected a request with a disallowed authority claim",
        );
    }
    forbidden_host()
}

/// Which claim a refusal fired on. A CLOSED set of two compile-time tokens, so
/// the log line can say where to look without carrying a byte of
/// caller-controlled input.
///
/// `pub(crate)` because the mutating control route refuses for the same reasons
/// through the same predicate and reports the same field: an operator
/// correlating a rejection across the two surfaces reads ONE vocabulary, and a
/// second copy of these tokens could drift from this one.
///
/// The refusal used to be described as a "disallowed Host header", which is wrong
/// two ways now: it also fires on a URI authority, where no header exists, and on
/// an unreadable value, where there is nothing to quote. An operator needs the
/// SITE to know where to look; they never need the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaimSite {
    /// One of the request's `Host` header values was disallowed or unreadable.
    HostHeader,
    /// The request URI's authority (HTTP/2 `:authority`) was disallowed.
    UriAuthority,
}

impl ClaimSite {
    /// The stable log token for this site.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::HostHeader => "host_header",
            Self::UriAuthority => "uri_authority",
        }
    }
}

fn forbidden_host() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "schema_version": GATE_SCHEMA_VERSION,
            "error": {
                "code": "forbidden_host",
                "message": "Host header not allowed for the status surface",
            },
        })),
    )
        .into_response()
}

/// `tracing` target for the status-gate shed observability.
const SHED_TARGET: &str = "routectl::status::gate";

/// Log the shed on the 1st event and every Nth thereafter, so a saturated
/// status surface never logs per shed. A fixed compile-time interval, not a
/// config knob -- the shed log is a coarse degradation signal, not a metric.
const SHED_LOG_SAMPLE_N: u64 = 64;

/// Process-wide count of shed `/status*` requests. The load-shed layer sheds
/// BEFORE a request reaches a panel handler, so a shed is invisible to the
/// per-panel `PanelCounters`; this tiny in-process counter is the only place
/// it is observable.
static STATUS_SHED_COUNT: AtomicU64 = AtomicU64::new(0);

/// Whether the shed at 1-based position `count` should be logged: the first
/// shed, then every `SHED_LOG_SAMPLE_N`th.
const fn should_log_shed(count: u64) -> bool {
    count == 1 || count.is_multiple_of(SHED_LOG_SAMPLE_N)
}

/// Map the load-shed overload error to a FIXED JSON 503. The status subtree's
/// inner service is infallible, so the only error the shed layer can surface
/// is `tower::load_shed::error::Overloaded`; every value maps to the same
/// body, carrying no request-specific detail.
///
/// The shed is counted here (the only observable shed site) and logged
/// SAMPLED -- 1st shed + every Nth -- at warn, since a saturated status
/// surface is genuine degradation. Only the running total is logged, never
/// the error or any request detail.
pub async fn handle_status_overload(_err: tower::BoxError) -> Response {
    let shed_total = STATUS_SHED_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if should_log_shed(shed_total) {
        tracing::warn!(
            target: SHED_TARGET,
            shed_total,
            "status surface overloaded; shed a request",
        );
    }
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "schema_version": GATE_SCHEMA_VERSION,
            "error": {
                "code": "overloaded",
                "message": "status service temporarily overloaded; retry shortly",
            },
        })),
    )
        .into_response()
}

/// Wrap a status (sub-)router with the load-shed stack, scoped to that router
/// only. Ordering matters (outermost first): the `HandleErrorLayer` catches
/// the shed error and renders the 503 (making the service infallible for
/// axum); `LoadShedLayer` wraps the concurrency limit so that when the cap is
/// saturated the shed fires IMMEDIATELY on `call` rather than queueing on
/// `poll_ready`; `GlobalConcurrencyLimitLayer` shares ONE semaphore across
/// every route the layer is cloned onto, so the cap is subtree-wide (a plain
/// `ConcurrencyLimitLayer` would mint a fresh per-route semaphore, yielding
/// `routes * cap` -- not the single `STATUS_MAX_INFLIGHT` this const names).
///
/// Shared by production wiring and the shed test so the two cannot drift.
pub fn apply_overload_layers<S>(router: axum::Router<S>) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router.layer(
        tower::ServiceBuilder::new()
            .layer(axum::error_handling::HandleErrorLayer::new(
                handle_status_overload,
            ))
            .layer(tower::load_shed::LoadShedLayer::new())
            .layer(tower::limit::GlobalConcurrencyLimitLayer::new(
                STATUS_MAX_INFLIGHT,
            )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request as HttpRequest;
    use axum::routing::get;
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    use tokio::sync::Semaphore;
    use tower::ServiceExt;

    fn allowlist(bound: &str) -> StatusHostAllowlist {
        StatusHostAllowlist::new(bound.parse().unwrap())
    }

    #[test]
    fn loopback_literals_are_allowed_with_or_without_port() {
        let al = allowlist("127.0.0.1:8787");
        for host in [
            "localhost",
            "localhost:8787",
            "127.0.0.1",
            "127.0.0.1:8787",
            "127.0.0.1:9999",
            "[::1]",
            "[::1]:8787",
        ] {
            assert!(al.allows(host), "loopback host must be allowed: {host}");
        }
    }

    #[test]
    fn bound_address_is_allowed_and_others_rejected() {
        let al = allowlist("192.168.1.5:8080");
        assert!(al.allows("192.168.1.5"));
        assert!(al.allows("192.168.1.5:8080"));
        // A different host -- the DNS-rebinding vector -- is rejected.
        assert!(!al.allows("evil.example.com"));
        assert!(!al.allows("evil.example.com:8080"));
        assert!(!al.allows("10.0.0.9:8080"));
    }

    #[test]
    fn wildcard_ipv4_bind_matches_on_port_only() {
        let al = allowlist("0.0.0.0:8080");
        // A wildcard bind is reachable from any client whose Host names the
        // bound port; the unspecified address itself matches nothing useful.
        assert!(al.allows("192.168.1.5:8080"));
        assert!(!al.allows("192.168.1.5:9999"));
        // A Host with no port fails closed under a wildcard bind.
        assert!(!al.allows("192.168.1.5"));
    }

    #[test]
    fn wildcard_ipv6_bind_matches_on_port_only() {
        let al = allowlist("[::]:8080");
        assert!(al.allows("[fe80::1]:8080"));
        assert!(!al.allows("[fe80::1]:9999"));
    }

    /// Bracket contents must be a real IPv6 literal. A bracketed domain or IPv4
    /// value is refused.
    ///
    /// Brackets exist in an authority for exactly one purpose -- to delimit an
    /// IPv6 literal whose colons would otherwise read as a port separator (RFC
    /// 3986). Accepting anything else inside them means the parser recognizes a
    /// shape no conforming client sends, and each accepted oddity is a second
    /// spelling of a host: `[localhost]` and `[127.0.0.1]` passed the loopback
    /// check here while a browser would not treat them as that host at all. One
    /// spelling per host is what keeps this predicate's answer the same answer a
    /// client's own resolution would give.
    #[test]
    fn bracket_contents_must_be_an_ipv6_literal() {
        for hostile in [
            // A bracketed domain, with and without a port.
            "[localhost]",
            "[localhost]:8787",
            "[evil.example]",
            "[evil.example]:8787",
            // A bracketed IPv4 literal: legal-looking, still not IPv6.
            "[127.0.0.1]",
            "[127.0.0.1]:8787",
            "[192.168.1.5]:8787",
            // Not an address at all.
            "[]",
            "[ ]",
            "[::1 ]",
        ] {
            assert!(
                split_host_port(hostile).is_none(),
                "`{hostile}` does not hold an IPv6 literal, so the brackets are \
                 not a delimiter this parser recognizes"
            );
            assert!(
                !is_loopback_authority(hostile),
                "`{hostile}` must not classify as loopback"
            );
        }
    }

    /// A ZONE-SCOPED IPv6 literal is refused, and that is the documented
    /// behavior rather than an accident of the parse.
    ///
    /// A zone index names a local interface, not an endpoint, so it cannot be
    /// compared against the address routectl bound -- and the link-local
    /// addresses it scopes are neither loopback nor bound, so such an authority
    /// would be refused by the allowlist even if the syntax were accepted.
    /// Pinned so a future widening of the grammar has to come here and argue
    /// with the reasoning rather than silently admit the shape.
    #[test]
    fn a_zone_scoped_ipv6_authority_fails_closed() {
        for scoped in [
            "[fe80::1%eth0]",
            "[fe80::1%eth0]:8787",
            // RFC 6874's percent-escaped spelling.
            "[fe80::1%25eth0]",
            // A numeric zone index.
            "[fe80::1%2]",
            // Loopback with a zone is still refused: the zone is what makes it
            // unanswerable, not the address.
            "[::1%eth0]",
        ] {
            assert!(
                split_host_port(scoped).is_none(),
                "`{scoped}` carries a zone index, which names a local interface \
                 rather than an endpoint, so it must fail closed"
            );
            assert!(
                !is_loopback_authority(scoped),
                "`{scoped}` must not classify as loopback"
            );
        }
        // Control: the same address WITHOUT a zone parses, so the refusals above
        // are attributable to the zone and not to the literal.
        assert_eq!(split_host_port("[fe80::1]"), Some("fe80::1"));
    }

    /// The paired positive: every real bracketed IPv6 literal still parses,
    /// including the IPv4-MAPPED form a dual-stack listener produces.
    #[test]
    fn bracketed_ipv6_literals_still_parse() {
        for (benign, host, port) in [
            ("[::1]", "::1", None),
            ("[::1]:8787", "::1", Some(8787)),
            ("[::ffff:127.0.0.1]", "::ffff:127.0.0.1", None),
            ("[::ffff:127.0.0.1]:8787", "::ffff:127.0.0.1", Some(8787)),
            ("[2001:db8::10]:8787", "2001:db8::10", Some(8787)),
            ("[::]", "::", None),
        ] {
            assert_eq!(
                parse_authority(benign),
                Some((host, port)),
                "`{benign}` is a well-formed bracketed IPv6 authority"
            );
        }
        // And the loopback ones still classify as loopback.
        for benign in ["[::1]", "[::1]:8787", "[::ffff:127.0.0.1]:8787"] {
            assert!(
                is_loopback_authority(benign),
                "`{benign}` names loopback and must still pass"
            );
        }
    }

    /// A WILDCARD allowlist must not salvage a port out of a malformed
    /// authority.
    ///
    /// Under a wildcard bind the allowlist degrades to a port-only check, so the
    /// port parse IS the access decision -- and it was reached through a separate
    /// parser that accepted shapes the host parser rejects. `[::1]evil:8080`
    /// yielded port 8080 (bracket contents discarded, trailing junk ignored),
    /// `:8080` yielded 8080 with NO host at all, and `[]:8080` yielded 8080 from
    /// an empty bracket. Each is a foreign or nonsense authority admitted on a
    /// port match alone.
    #[test]
    fn a_wildcard_allowlist_salvages_no_port_from_a_malformed_authority() {
        let al = allowlist("0.0.0.0:8080");
        for hostile in [
            // Trailing junk after a bracketed literal, with the bound port.
            "[::1]evil:8080",
            "[::1].evil:8080",
            "[127.0.0.1]evil:8080",
            // No host at all.
            ":8080",
            // Empty bracketed host.
            "[]:8080",
            "[]",
            // Userinfo shapes carrying the bound port.
            "@:8080",
            "user@evil.example:8080",
            // An unterminated bracket.
            "[::1:8080",
            // Empty authority.
            "",
        ] {
            assert!(
                !al.allows(hostile),
                "`{hostile}` is not a well-formed authority, so no port may be \
                 salvaged from it -- under a wildcard bind the port parse IS the \
                 access decision"
            );
        }
    }

    /// A port outside the u16 range is not a port. It previously read as
    /// `None`-by-overflow, which happened to fail closed; now it is refused by
    /// the parser itself, so the behavior is stated rather than incidental.
    #[test]
    fn an_out_of_range_port_is_not_a_port() {
        let al = allowlist("0.0.0.0:8080");
        for hostile in ["192.168.1.5:99999", "192.168.1.5:65536", "[::1]:99999"] {
            assert!(!al.allows(hostile), "`{hostile}` has no valid port");
        }
        assert_eq!(port_of("192.168.1.5:99999"), None);
        assert_eq!(port_of("192.168.1.5:65536"), None);
        // The boundary value is legal.
        assert_eq!(port_of("192.168.1.5:65535"), Some(65535));
    }

    /// `split_host_port` and `port_of` agree on what is well-formed: for every
    /// authority, either both answer or neither does.
    ///
    /// They were two independent parsers over the same grammar, which is how the
    /// port side came to accept `[::1]evil:8080` while the host side rejected it.
    /// This pins the shared contract rather than each half separately.
    #[test]
    fn the_host_and_port_parsers_agree_on_well_formedness() {
        for authority in [
            // Well-formed, with and without a port.
            "127.0.0.1",
            "127.0.0.1:8080",
            "localhost",
            "localhost:8080",
            "[::1]",
            "[::1]:8080",
            "::1",
            "2001:db8::10",
            "evil.example",
            "evil.example:8080",
            // Malformed in every way the grammar can be.
            "[::1]evil",
            "[::1]evil:8080",
            "[::1]:8080evil",
            "[]:8080",
            "[]",
            "[::1",
            ":8080",
            "",
            "127.0.0.1:evil",
            "127.0.0.1:99999",
        ] {
            let host = split_host_port(authority);
            let port = port_of(authority);
            // A well-formed authority always yields a host; a port only when one
            // is written. A malformed one yields neither.
            if host.is_none() {
                assert_eq!(
                    port, None,
                    "`{authority}` has no parseable host, so it must have no \
                     parseable port either -- a port salvaged from an \
                     unparseable authority is exactly the wildcard-bind bypass"
                );
            }
        }
    }

    #[test]
    fn loopback_literals_pass_under_wildcard_bind() {
        let al = allowlist("0.0.0.0:8080");
        for host in [
            "localhost",
            "localhost:8080",
            "127.0.0.1",
            "127.0.0.1:8080",
            "[::1]",
            "[::1]:8080",
        ] {
            assert!(
                al.allows(host),
                "loopback host must be allowed under a wildcard bind: {host}"
            );
        }
    }

    /// Everything AFTER a bracketed literal's `]` must be empty or a numeric
    /// `:port`, or the authority is rejected.
    ///
    /// This closes a bypass of the same family as the userinfo one:
    /// `split_host_port` returned the bracket contents and DISCARDED the rest, so
    /// `[::1]evil.example` yielded `::1` and read as loopback -- while a client
    /// resolving that authority goes wherever the trailing text names. Same for a
    /// junk port suffix (`[::1]:8791evil`), which is not a port at all.
    #[test]
    fn a_bracketed_authority_with_trailing_junk_is_never_loopback() {
        for hostile in [
            // Trailing text directly after the bracket.
            "[::1]evil",
            "[::1]evil.example",
            "[::1].evil",
            "[::1].evil.example",
            // A port-looking suffix that is not all digits.
            "[::1]:8791evil",
            "[::1]:80:evil",
            "[::1]:evil",
            "[::1]:",
            // Bracketed IPv4 carries the same shape.
            "[127.0.0.1]evil",
            "[127.0.0.1].evil",
            "[127.0.0.1]:8791evil",
            // An unterminated bracket is not an authority this guard accepts.
            "[::1",
        ] {
            assert!(
                !is_loopback_authority(hostile),
                "`{hostile}` carries text after the bracketed literal that is not \
                 a numeric port, so the host a client reaches is not the literal \
                 inside the brackets"
            );
        }
    }

    /// The paired positive: the two legitimate bracketed shapes still pass, so
    /// the rejection above is about the SUFFIX and not about brackets.
    #[test]
    fn well_formed_bracketed_loopback_authorities_still_pass() {
        for benign in ["[::1]", "[::1]:8787", "[::1]:0", "[::ffff:127.0.0.1]:8787"] {
            assert!(
                is_loopback_authority(benign),
                "`{benign}` is a well-formed bracketed loopback authority"
            );
        }
    }

    /// An authority carrying USERINFO is rejected outright, never parsed for a
    /// host.
    ///
    /// This closes a real bypass: `split_host_port` strips IPv6 brackets by
    /// finding the first `]`, so `[::1]@evil.example` yielded `::1` and
    /// classified as LOOPBACK -- while a browser resolving that authority
    /// connects to `evil.example`, with `[::1]` as a userinfo field the host
    /// never sees. The rebinding guard would have read the attacker's authority
    /// as its own.
    ///
    /// A legitimate `Host` header carries no userinfo (RFC 7230 forbids it), so
    /// rejecting the whole shape costs nothing and needs no per-position
    /// reasoning about where the `@` sits.
    #[test]
    fn an_authority_carrying_userinfo_is_never_loopback() {
        for hostile in [
            // The bracket-parse bypass, both with and without a port.
            "[::1]@evil.example",
            "[::1]@evil.example:8791",
            "[::1]:8791@evil.example",
            // The IPv4 shapes, for symmetry -- these already failed the host
            // check, and must keep failing for the explicit reason.
            "127.0.0.1@evil.example",
            "127.0.0.1:8791@evil.example",
            "localhost@evil.example",
            // Userinfo with a password field.
            "[::1]:pw@evil.example",
            "user:pw@127.0.0.1",
            // A trailing `@` is still userinfo syntax.
            "127.0.0.1@",
            "@127.0.0.1",
        ] {
            assert!(
                !is_loopback_authority(hostile),
                "`{hostile}` carries a userinfo separator, so the loopback-looking \
                 text in it is not necessarily the host a client reaches; the \
                 whole shape is refused rather than parsed"
            );
        }
    }

    /// The paired positive: every userinfo-free loopback authority still passes.
    /// Without this, rejecting on any `@` would be indistinguishable from
    /// rejecting everything, which would break the status surface and the
    /// control route together.
    #[test]
    fn userinfo_free_loopback_authorities_still_pass() {
        for benign in [
            "127.0.0.1",
            "127.0.0.1:8787",
            "127.0.0.5:8787",
            "localhost",
            "localhost:8787",
            "[::1]",
            "[::1]:8787",
            "::1",
        ] {
            assert!(
                is_loopback_authority(benign),
                "`{benign}` is a plain loopback authority and must still pass"
            );
        }
    }

    /// A non-loopback authority stays rejected, so the acceptance above is not a
    /// predicate that accepts everything.
    #[test]
    fn plain_non_loopback_authorities_stay_rejected() {
        for foreign in [
            "evil.example",
            "evil.example:8791",
            "10.20.30.40:8791",
            "[2001:db8::1]:8791",
        ] {
            assert!(
                !is_loopback_authority(foreign),
                "`{foreign}` must be rejected"
            );
        }
    }

    #[test]
    fn split_host_port_handles_ipv6_and_bare_hosts() {
        assert_eq!(split_host_port("[::1]:8787"), Some("::1"));
        assert_eq!(split_host_port("[::1]"), Some("::1"));
        assert_eq!(split_host_port("127.0.0.1:8787"), Some("127.0.0.1"));
        assert_eq!(split_host_port("localhost"), Some("localhost"));
        assert_eq!(split_host_port("example.com"), Some("example.com"));
        // A bare (unbracketed) multi-colon IPv6 literal has no port: its
        // trailing group must not be stripped. `::1` must survive whole so the
        // loopback check recognizes it.
        assert_eq!(split_host_port("::1"), Some("::1"));
        assert_eq!(split_host_port("2001:db8::10"), Some("2001:db8::10"));
    }

    /// A malformed authority yields NO host rather than a salvaged fragment.
    /// Returning a fragment is what let a trailing-junk authority read as its
    /// bracketed literal, so the parse now refuses instead of guessing.
    #[test]
    fn split_host_port_refuses_a_malformed_authority() {
        // Trailing text or a non-numeric port after a bracketed literal.
        assert_eq!(split_host_port("[::1]evil"), None);
        assert_eq!(split_host_port("[::1].evil"), None);
        assert_eq!(split_host_port("[::1]:8791evil"), None);
        assert_eq!(split_host_port("[::1]:"), None);
        assert_eq!(split_host_port("[127.0.0.1]evil"), None);
        // An unterminated bracket.
        assert_eq!(split_host_port("[::1"), None);
        // A non-numeric port on a bare host.
        assert_eq!(split_host_port("localhost:evil"), None);
        assert_eq!(split_host_port("127.0.0.1:evil"), None);
    }

    #[test]
    fn port_of_handles_ipv6_and_bare_hosts() {
        assert_eq!(port_of("[::1]:8787"), Some(8787));
        assert_eq!(port_of("[::1]"), None);
        assert_eq!(port_of("[fe80::1]:8080"), Some(8080));
        assert_eq!(port_of("127.0.0.1:8787"), Some(8787));
        assert_eq!(port_of("127.0.0.1"), None);
        assert_eq!(port_of("localhost:8080"), Some(8080));
        assert_eq!(port_of("example.com"), None);
        // Out-of-range ports do not parse as u16 -> fail closed.
        assert_eq!(port_of("example.com:99999"), None);
        // A bare (unbracketed) multi-colon IPv6 literal carries no port; its
        // final group must not be read as one.
        assert_eq!(port_of("2001:db8::10"), None);
        assert_eq!(port_of("::1"), None);
    }

    #[test]
    fn bare_ipv6_is_not_allowed_by_port_only_wildcard_match() {
        // Under a wildcard bind, the port-only degradation must NOT be fooled
        // by a bare IPv6 authority whose trailing group equals the bound port.
        // `2001:db8::10` is a non-loopback host with no port, so it fails the
        // loopback check AND the port check -> rejected.
        let al = allowlist("[::]:10");
        assert!(!al.allows("2001:db8::10"));
    }

    #[test]
    fn bare_ipv6_loopback_is_recognized() {
        // A bare `::1` (no brackets, no port) is loopback and must be allowed
        // regardless of the bind.
        assert!(is_loopback_authority("::1"));
        let al = allowlist("0.0.0.0:8080");
        assert!(al.allows("::1"));
    }

    #[derive(Clone)]
    struct HoldState {
        arrived: Arc<AtomicUsize>,
        release: Arc<Semaphore>,
    }

    async fn hold_handler(State(state): State<HoldState>) -> StatusCode {
        state.arrived.fetch_add(1, Ordering::SeqCst);
        // Park in-flight (holding a concurrency permit) until the test
        // releases us. A Semaphore permit is never lost to a wake-race,
        // unlike a `Notify`.
        let _permit = state.release.acquire().await.expect("release semaphore");
        StatusCode::OK
    }

    fn get_request() -> HttpRequest<Body> {
        HttpRequest::builder()
            .method("GET")
            .uri("/hold")
            .body(Body::empty())
            .unwrap()
    }

    /// Saturating the subtree-wide cap sheds the excess request immediately as
    /// the fixed JSON 503, without queueing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn load_shed_returns_fixed_503_when_cap_saturated() {
        let state = HoldState {
            arrived: Arc::new(AtomicUsize::new(0)),
            release: Arc::new(Semaphore::new(0)),
        };
        let router = apply_overload_layers(
            axum::Router::new()
                .route("/hold", get(hold_handler))
                .with_state(state.clone()),
        );

        // Fire exactly STATUS_MAX_INFLIGHT requests that park in-flight, each
        // holding a permit.
        let mut handles = Vec::new();
        for _ in 0..STATUS_MAX_INFLIGHT {
            let router = router.clone();
            handles.push(tokio::spawn(
                async move { router.oneshot(get_request()).await },
            ));
        }

        // Wait until every permit is held (each handler increments `arrived`
        // only AFTER its concurrency permit was acquired in `poll_ready`).
        let deadline = Instant::now() + Duration::from_secs(5);
        while state.arrived.load(Ordering::SeqCst) < STATUS_MAX_INFLIGHT {
            assert!(
                Instant::now() < deadline,
                "handlers never reached in-flight"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // The next request finds no permit -> shed -> fixed 503.
        let resp = router.clone().oneshot(get_request()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["schema_version"], GATE_SCHEMA_VERSION);
        assert_eq!(json["error"]["code"], "overloaded");
        assert!(json["error"]["message"].is_string());

        // Release the parked requests and confirm they complete cleanly (the
        // held permits were real, not a fluke of ordering).
        state.release.add_permits(STATUS_MAX_INFLIGHT);
        for handle in handles {
            let resp = handle.await.unwrap().unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
    }

    /// The status overload layers gate ONLY the status subtree. When the
    /// status cap is saturated (excess sheds a 503), a request routed OUTSIDE
    /// the subtree is untouched by the shared ConcurrencyLimit/LoadShed and
    /// proceeds normally -- the second half of the isolation contract the
    /// proxy lane depends on. Deterministic at the same tower layer the shed
    /// test uses: the status stack is held saturated by parked requests, and
    /// the non-status route carries no permit and no layer, so its 200 never
    /// races the status saturation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn non_status_route_proceeds_while_status_saturated() {
        let state = HoldState {
            arrived: Arc::new(AtomicUsize::new(0)),
            release: Arc::new(Semaphore::new(0)),
        };
        // The status subtree carries the overload layers; the non-status lane
        // is merged in WITHOUT them, mirroring `/v1/*` inheriting none of the
        // status gate.
        let status_subtree = apply_overload_layers(
            axum::Router::new()
                .route("/hold", get(hold_handler))
                .with_state(state.clone()),
        );
        let app = axum::Router::new()
            .route("/v1/passthrough", get(|| async { StatusCode::OK }))
            .merge(status_subtree);

        // Saturate the status cap with parked requests, each holding a permit.
        let mut handles = Vec::new();
        for _ in 0..STATUS_MAX_INFLIGHT {
            let app = app.clone();
            handles.push(tokio::spawn(
                async move { app.oneshot(get_request()).await },
            ));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while state.arrived.load(Ordering::SeqCst) < STATUS_MAX_INFLIGHT {
            assert!(
                Instant::now() < deadline,
                "status handlers never reached in-flight"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Confirm the status cap is genuinely saturated: an extra status
        // request sheds the fixed 503.
        let shed = app.clone().oneshot(get_request()).await.unwrap();
        assert_eq!(
            shed.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "status subtree must shed while its cap is saturated"
        );

        // The non-status lane is NOT gated by the status layers: it proceeds.
        let passthrough_req = HttpRequest::builder()
            .method("GET")
            .uri("/v1/passthrough")
            .body(Body::empty())
            .unwrap();
        let passthrough = app.clone().oneshot(passthrough_req).await.unwrap();
        assert_eq!(
            passthrough.status(),
            StatusCode::OK,
            "a non-status route must not be shed by the status cap"
        );

        // Release the parked status requests; they complete cleanly.
        state.release.add_permits(STATUS_MAX_INFLIGHT);
        for handle in handles {
            let resp = handle.await.unwrap().unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
    }

    /// Deterministic guard for the page-off-budget wiring: the dashboard page
    /// (`GET /`) is merged ALONGSIDE the load-shed-gated JSON subtree but
    /// OUTSIDE `apply_overload_layers`, so it never consumes the shared JSON
    /// shed budget. Built exactly as production wires it -- the saturable JSON
    /// stand-in under the overload layers, the REAL `page_router()` merged in
    /// off-budget. With the JSON cap held fully saturated by parked requests,
    /// the page route must still answer 200 while an extra JSON request sheds
    /// 503; the barrier makes this a race-free assertion rather than the soft
    /// probabilistic burst it replaces.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn page_route_proceeds_while_json_status_saturated() {
        use crate::handlers::status::page_router;

        let state = HoldState {
            arrived: Arc::new(AtomicUsize::new(0)),
            release: Arc::new(Semaphore::new(0)),
        };
        // The JSON subtree carries the overload layers; the page route is
        // merged in WITHOUT them, mirroring production's off-budget page lane.
        let json_subtree = apply_overload_layers(
            axum::Router::new()
                .route("/hold", get(hold_handler))
                .with_state(state.clone()),
        );
        let app = json_subtree.merge(page_router());

        // Saturate the JSON cap with parked requests, each holding a permit.
        let mut handles = Vec::new();
        for _ in 0..STATUS_MAX_INFLIGHT {
            let app = app.clone();
            handles.push(tokio::spawn(
                async move { app.oneshot(get_request()).await },
            ));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while state.arrived.load(Ordering::SeqCst) < STATUS_MAX_INFLIGHT {
            assert!(
                Instant::now() < deadline,
                "JSON handlers never reached in-flight"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Confirm the JSON cap is genuinely saturated: an extra JSON request
        // sheds the fixed 503.
        let shed = app.clone().oneshot(get_request()).await.unwrap();
        assert_eq!(
            shed.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "JSON subtree must shed while its cap is saturated"
        );

        // The page route is NOT gated by the JSON shed layers: it proceeds.
        let page_req = HttpRequest::builder()
            .method("GET")
            .uri("/")
            .body(Body::empty())
            .unwrap();
        let page = app.clone().oneshot(page_req).await.unwrap();
        assert_eq!(
            page.status(),
            StatusCode::OK,
            "GET / must not be shed by the JSON status cap"
        );

        // Release the parked JSON requests; they complete cleanly (the held
        // permits were real, not a fluke of ordering).
        state.release.add_permits(STATUS_MAX_INFLIGHT);
        for handle in handles {
            let resp = handle.await.unwrap().unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
    }

    /// The capacity UNIT: `STATUS_MAX_INFLIGHT` bounds admitted requests AND,
    /// because no handler fans one admitted request into several builders, the
    /// concurrent BLOCKING panel builders behind them. The `/status` aggregate
    /// is the only route that could break that equality -- it builds four
    /// panels -- so it is the one this pins.
    ///
    /// Same idiom as the shed tests above: park work in-flight, count arrivals,
    /// release, assert. The park lives one level deeper (inside `guard_panel`'s
    /// blocking closure, via the builder probe) because a builder count is
    /// invisible from the wire -- a fan-out answers 200 on every panel too.
    ///
    /// Far more requests than permits are fired, so the excess sheds and only
    /// admitted ones can build. With the aggregate awaiting its four builders
    /// sequentially the plateau is `STATUS_MAX_INFLIGHT`; a concurrent join
    /// parks `4 * STATUS_MAX_INFLIGHT`.
    ///
    /// Every assertion is deferred until AFTER `release`: a parked builder
    /// occupies a blocking worker that no unwind can reclaim, so a panic while
    /// the plateau is still held would hang the test binary at runtime shutdown
    /// instead of reporting the failure.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_aggregates_hold_at_most_max_inflight_blocking_builders() {
        use crate::handlers::status::builder_probe::{BUILDER_PROBE, BuilderProbe};
        use crate::handlers::status::{DaemonMeta, StatusState, status_router};
        use crate::server::AppState;
        use arc_swap::ArcSwap;
        use routectl_router::{Config, Router};
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!("version = {}\n", routectl_router::CURRENT_CONFIG_VERSION),
        )
        .unwrap();

        let router = Router::new(Arc::new(Config::default()));
        let (app, _writer_dir) = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
        // A config path is required for the doctor panel to reach a builder at
        // all, so all four panels are genuinely in play.
        let mut status = StatusState::from_app(&app, Some(config_path), DaemonMeta::for_test());
        status.usage_db_path = dir.path().join("absent-usage.db");
        let app = apply_overload_layers(status_router().with_state(Arc::new(status)));

        let probe = BuilderProbe::new();
        let mut handles = Vec::new();
        for _ in 0..(STATUS_MAX_INFLIGHT * 3) {
            let app = app.clone();
            let probe = Arc::clone(&probe);
            handles.push(tokio::spawn(BUILDER_PROBE.scope(probe, async move {
                let req = HttpRequest::builder()
                    .method("GET")
                    .uri("/status")
                    .body(Body::empty())
                    .unwrap();
                app.oneshot(req).await
            })));
        }

        // Wait for the plateau: every admitted request has parked one builder.
        let deadline = Instant::now() + Duration::from_secs(10);
        while probe.started() < STATUS_MAX_INFLIGHT && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // Read SUBMISSIONS, not arrivals. Submission is counted on the async
        // side the instant a handler decides to hand a builder to
        // `spawn_blocking`, so once the admitted requests have parked their
        // builders, every builder those requests will ever submit is already
        // counted. A fan-out submits all four of a request's builders before
        // any can be observed to start, so it registers here immediately --
        // no settle window, and nothing for a slow blocking pool to hide.
        let parked = probe.submitted();

        probe.release();
        let mut served = 0usize;
        for handle in handles {
            let resp = handle.await.unwrap().unwrap();
            assert!(
                matches!(
                    resp.status(),
                    StatusCode::OK | StatusCode::SERVICE_UNAVAILABLE
                ),
                "unexpected aggregate status {}",
                resp.status()
            );
            if resp.status() == StatusCode::OK {
                served += 1;
            }
        }

        assert_eq!(
            parked, STATUS_MAX_INFLIGHT,
            "an admitted request must hold exactly one blocking builder at a \
             time: {parked} concurrent builders under a cap of {STATUS_MAX_INFLIGHT}"
        );
        assert!(
            served >= STATUS_MAX_INFLIGHT,
            "the admitted requests must complete once released, not deadlock: {served} served"
        );
    }

    /// Build a status app whose four panels all reach a real builder, plus the
    /// temp dir keeping its config path alive. Shared by the cancellation tests
    /// so they exercise the same wiring the capacity test above does.
    fn cancellation_test_app() -> (axum::Router, tempfile::TempDir) {
        use crate::handlers::status::{DaemonMeta, StatusState, status_router};
        use crate::server::AppState;
        use arc_swap::ArcSwap;
        use routectl_router::{Config, Router};
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!("version = {}\n", routectl_router::CURRENT_CONFIG_VERSION),
        )
        .unwrap();

        let router = Router::new(Arc::new(Config::default()));
        let (app, _writer_dir) = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
        // A config path is required for the doctor panel to reach a builder at
        // all, so all four panels are genuinely in play.
        let mut status = StatusState::from_app(&app, Some(config_path), DaemonMeta::for_test());
        status.usage_db_path = dir.path().join("absent-usage.db");
        (
            apply_overload_layers(status_router().with_state(Arc::new(status))),
            dir,
        )
    }

    /// Poll `probe` until `ready` holds, using `deadline_secs` ONLY as a hang
    /// bound. Returns whether the condition was reached, so the caller can
    /// defer the assertion until after it has released the parked builders --
    /// asserting while builders sit on blocking workers would unwind into a
    /// runtime shutdown that cannot reclaim them, hanging the test binary
    /// instead of reporting the failure.
    async fn wait_for(
        probe: &Arc<crate::handlers::status::builder_probe::BuilderProbe>,
        deadline_secs: u64,
        ready: impl Fn(&crate::handlers::status::builder_probe::BuilderProbe) -> bool,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(deadline_secs);
        while Instant::now() < deadline {
            if ready(probe) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        ready(probe)
    }

    /// CANCELLATION, single-panel route: a client that aborts mid-request must
    /// not free builder capacity while the builder it already started is still
    /// running.
    ///
    /// The Tower gate's permit lives in the RESPONSE FUTURE, so aborting the
    /// request drops it and admission is immediately free again -- that half is
    /// correct and deliberate (the next request is DELAYED, not shed). What must
    /// NOT come back with it is the blocking-builder capacity, which is held by
    /// a `spawn_blocking` job the abort cannot reach.
    ///
    /// Deterministic by construction, with no settle window anywhere. All
    /// `STATUS_MAX_INFLIGHT` builders are parked on blocking workers first;
    /// their requests are then aborted; a fresh request is issued. Because every
    /// permit is owned by a parked builder, the fresh request can only reach the
    /// capacity gate and wait -- and `capacity_exhausted` is the positive event
    /// that says it did. That state cannot end without the test releasing, so
    /// the snapshot taken after it is stable and the timeouts are pure hang
    /// bounds. Against the pre-fix code the aborted requests' permits are gone
    /// with their futures, so the fresh request submits a builder immediately
    /// and `submitted` overshoots.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancelled_single_panel_requests_do_not_free_builder_capacity() {
        use crate::handlers::status::builder_probe::{BUILDER_PROBE, BuilderProbe};

        let (app, _dir) = cancellation_test_app();
        let probe = BuilderProbe::new();

        // Saturate: every admitted request parks its one builder on a blocking
        // worker, so all STATUS_MAX_INFLIGHT builder permits are held.
        let mut cancelled = Vec::new();
        for _ in 0..STATUS_MAX_INFLIGHT {
            let app = app.clone();
            let probe = Arc::clone(&probe);
            cancelled.push(tokio::spawn(BUILDER_PROBE.scope(probe, async move {
                let req = HttpRequest::builder()
                    .method("GET")
                    .uri("/status/health")
                    .body(Body::empty())
                    .unwrap();
                app.oneshot(req).await
            })));
        }
        let saturated = wait_for(&probe, 10, |p| p.started() >= STATUS_MAX_INFLIGHT).await;

        // Abort every request future. Each drops its Tower permit; each leaves
        // its builder running on a blocking worker.
        for handle in &cancelled {
            handle.abort();
        }
        for handle in cancelled {
            assert!(handle.await.is_err(), "request future must have aborted");
        }
        let submitted_before = probe.submitted();

        // A fresh request is admitted (capacity to ADMIT is free again) but must
        // find no builder permit.
        let follow_up = {
            let app = app.clone();
            let probe = Arc::clone(&probe);
            tokio::spawn(BUILDER_PROBE.scope(probe, async move {
                let req = HttpRequest::builder()
                    .method("GET")
                    .uri("/status/health")
                    .body(Body::empty())
                    .unwrap();
                app.oneshot(req).await
            }))
        };
        let delayed = wait_for(&probe, 10, |p| p.capacity_exhausted() >= 1).await;
        let submitted_while_detached = probe.submitted();

        // Release BEFORE asserting: a parked builder owns a blocking worker no
        // unwind can reclaim.
        probe.release();
        let served = follow_up.await.unwrap().unwrap();

        assert!(
            saturated,
            "the builders never parked; nothing was saturated"
        );
        assert!(
            delayed,
            "the follow-up request never hit the builder-capacity gate: the \
             cancelled requests' builder permits were freed by their futures dropping"
        );
        assert_eq!(
            submitted_while_detached, submitted_before,
            "no new blocking work may start while the detached builders hold \
             capacity: {submitted_while_detached} builders submitted, expected \
             {submitted_before}"
        );
        // Capacity recovers: once the detached builders finish, the delayed
        // request gets its permit and completes normally.
        assert_eq!(served.status(), StatusCode::OK);
        assert_eq!(
            probe.submitted(),
            submitted_before + 1,
            "the delayed builder must run once capacity is released"
        );
    }

    /// CANCELLATION, aggregate route: the same property via `GET /status`.
    ///
    /// Worth its own case because the aggregate is the route that composes four
    /// builders, so it is the one where a regression to a concurrent fan-out
    /// would break the permit accounting. The `uri` is the only difference from
    /// the single-panel case, which is the point: the permit lives in
    /// `guard_panel`, the chokepoint BOTH routes go through, so the aggregate
    /// inherits the property rather than restating it. That the aggregate's
    /// builders really took that path is what `submitted` measures -- it is only
    /// ever incremented inside `guard_panel`, so a synthetic stand-in could not
    /// move it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancelled_aggregate_requests_do_not_free_builder_capacity() {
        use crate::handlers::status::builder_probe::{BUILDER_PROBE, BuilderProbe};

        let (app, _dir) = cancellation_test_app();
        let probe = BuilderProbe::new();

        let mut cancelled = Vec::new();
        for _ in 0..STATUS_MAX_INFLIGHT {
            let app = app.clone();
            let probe = Arc::clone(&probe);
            cancelled.push(tokio::spawn(BUILDER_PROBE.scope(probe, async move {
                let req = HttpRequest::builder()
                    .method("GET")
                    .uri("/status")
                    .body(Body::empty())
                    .unwrap();
                app.oneshot(req).await
            })));
        }
        let saturated = wait_for(&probe, 10, |p| p.started() >= STATUS_MAX_INFLIGHT).await;

        for handle in &cancelled {
            handle.abort();
        }
        for handle in cancelled {
            assert!(handle.await.is_err(), "request future must have aborted");
        }
        // The aggregate is SEQUENTIAL, so each cancelled request had submitted
        // exactly its first builder -- the count is the saturation plateau, not
        // four times it.
        let submitted_before = probe.submitted();

        let follow_up = {
            let app = app.clone();
            let probe = Arc::clone(&probe);
            tokio::spawn(BUILDER_PROBE.scope(probe, async move {
                let req = HttpRequest::builder()
                    .method("GET")
                    .uri("/status")
                    .body(Body::empty())
                    .unwrap();
                app.oneshot(req).await
            }))
        };
        let delayed = wait_for(&probe, 10, |p| p.capacity_exhausted() >= 1).await;
        let submitted_while_detached = probe.submitted();

        probe.release();
        let served = follow_up.await.unwrap().unwrap();

        assert!(
            saturated,
            "the builders never parked; nothing was saturated"
        );
        assert_eq!(
            submitted_before, STATUS_MAX_INFLIGHT,
            "a sequential aggregate submits ONE builder per admitted request"
        );
        assert!(
            delayed,
            "the follow-up aggregate never hit the builder-capacity gate: the \
             cancelled requests' builder permits were freed by their futures dropping"
        );
        assert_eq!(
            submitted_while_detached, submitted_before,
            "no new blocking work may start while the detached builders hold \
             capacity: {submitted_while_detached} builders submitted, expected \
             {submitted_before}"
        );
        // Capacity recovers and the whole aggregate completes -- all four of its
        // builders ran through the same permit-owning path, which is also the
        // no-deadlock check (a sequential aggregate re-acquiring per panel must
        // not starve itself).
        assert_eq!(served.status(), StatusCode::OK);
        assert_eq!(
            probe.submitted(),
            submitted_before + 4,
            "the delayed aggregate must build all four panels once released"
        );
    }

    #[test]
    fn should_log_shed_samples_first_and_every_nth() {
        assert!(should_log_shed(1), "the first shed always logs");
        assert!(!should_log_shed(2));
        assert!(!should_log_shed(SHED_LOG_SAMPLE_N - 1));
        assert!(should_log_shed(SHED_LOG_SAMPLE_N), "every Nth logs");
        assert!(!should_log_shed(SHED_LOG_SAMPLE_N + 1));
        assert!(should_log_shed(2 * SHED_LOG_SAMPLE_N));
    }

    /// The shed path counts every shed but logs SAMPLED, and never logs the
    /// error or any request detail -- only the fixed message + running total.
    /// Driven on the default current-thread runtime so the thread-local
    /// capture sees the log emitted inline by `handle_status_overload`.
    #[tokio::test]
    async fn shed_log_is_sampled_and_never_leaks_error_detail() {
        use routectl_testkit::capture_lines;

        let secret = "/secret/path/token-sk-LEAKED";
        let fired = SHED_LOG_SAMPLE_N * 3;
        let ((), lines) = capture_lines(async {
            for _ in 0..fired {
                let err: tower::BoxError = secret.into();
                let resp = handle_status_overload(err).await;
                assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
            }
        })
        .await;

        // A window of 3N sheds yields a handful of sampled lines, never one
        // per shed -- the whole point of the sampling.
        assert!(!lines.is_empty(), "at least one sampled shed line expected");
        assert!(
            (lines.len() as u64) < fired,
            "shed log must be sampled: {} lines for {fired} sheds",
            lines.len()
        );
        for line in &lines {
            assert!(
                !line.contains("LEAKED") && !line.contains("/secret/"),
                "shed log leaked the raw error: {line}"
            );
            assert!(
                line.contains("status surface overloaded"),
                "unexpected shed line: {line}"
            );
        }
    }

    /// A guarded app over the loopback allowlist, for the authority-validation
    /// tests below.
    #[cfg(test)]
    fn guarded_app() -> axum::Router {
        use axum::middleware::from_fn_with_state;

        axum::Router::new()
            .route("/status", get(|| async { StatusCode::OK }))
            .layer(from_fn_with_state(allowlist("127.0.0.1:8787"), host_guard))
    }

    /// Drive one request at the guarded app, returning its status.
    #[cfg(test)]
    async fn guarded_status(request: HttpRequest<Body>) -> StatusCode {
        guarded_app()
            .oneshot(request)
            .await
            .expect("guard must respond")
            .status()
    }

    /// EVERY `Host` value is validated, not just the first.
    ///
    /// The status subtree read `HeaderMap::get`, which returns only the first
    /// value, so a request carrying the header twice had its second value
    /// unchecked. Which duplicate a downstream reader honors is not this guard's
    /// call to assume: all of them are claims and all must pass. Mirrors the
    /// mutating control route, which carries the same rule.
    /// Holds `status_host_403`: this test drives status refusals, which bump the
    /// process-global counter the sampled log reads. The key covers every test
    /// that MOVES that counter, not only those asserting on it.
    #[tokio::test]
    #[serial_test::serial(status_host_403)]
    async fn host_guard_validates_every_duplicate_host_value() {
        for (first, second) in [
            ("127.0.0.1:8787", "rebind.evil"),
            ("rebind.evil", "127.0.0.1:8787"),
            ("127.0.0.1:8787", "[::1]@evil.example"),
            ("127.0.0.1:8787", "[::1]evil.example"),
        ] {
            let request = HttpRequest::builder()
                .method("GET")
                .uri("/status")
                .header(header::HOST, first)
                .header(header::HOST, second)
                .body(Body::empty())
                .unwrap();

            assert_eq!(
                guarded_status(request).await,
                StatusCode::FORBIDDEN,
                "Host values ({first:?}, {second:?}) include a disallowed claim \
                 and must be rejected -- reading only the first leaves the other \
                 unchecked"
            );
        }
    }

    /// The paired positive: duplicated values that are ALL allowed are served, so
    /// the rejection above is about the VALUE rather than the duplication.
    #[tokio::test]
    async fn host_guard_serves_duplicate_allowed_hosts() {
        let request = HttpRequest::builder()
            .method("GET")
            .uri("/status")
            .header(header::HOST, "127.0.0.1:8787")
            .header(header::HOST, "127.0.0.1:8787")
            .body(Body::empty())
            .unwrap();

        assert_eq!(guarded_status(request).await, StatusCode::OK);
    }

    /// A PRESENT but non-UTF-8 `Host` fails CLOSED.
    ///
    /// Treating it as absent read a client's authority claim as no claim at all.
    /// The client did claim an authority; this build simply cannot read it, and a
    /// claim that cannot be evaluated has not been validated.
    /// Holds `status_host_403`: this test drives status refusals, which bump the
    /// process-global counter the sampled log reads. The key covers every test
    /// that MOVES that counter, not only those asserting on it.
    #[tokio::test]
    #[serial_test::serial(status_host_403)]
    async fn host_guard_fails_closed_on_a_non_utf8_host() {
        for raw in [&b"\xff\xfe"[..], &b"127.0.0.1\xff"[..], &b"\xc3"[..]] {
            let mut request = HttpRequest::builder()
                .method("GET")
                .uri("/status")
                .body(Body::empty())
                .unwrap();
            request.headers_mut().insert(
                header::HOST,
                axum::http::HeaderValue::from_bytes(raw)
                    .expect("non-UTF-8 bytes are still a legal header value"),
            );

            assert_eq!(
                guarded_status(request).await,
                StatusCode::FORBIDDEN,
                "a present but unreadable Host ({raw:?}) must fail closed"
            );
        }
    }

    /// An HTTP/2 request carries its authority on the request URI, with no `Host`
    /// header at all -- so a Host-only guard sees nothing and admits it. The
    /// status subtree is reachable over h2c on the same cleartext port as the
    /// control route, so it needs the same URI check.
    /// Holds `status_host_403`: this test drives status refusals, which bump the
    /// process-global counter the sampled log reads. The key covers every test
    /// that MOVES that counter, not only those asserting on it.
    #[tokio::test]
    #[serial_test::serial(status_host_403)]
    async fn host_guard_rejects_a_foreign_uri_authority() {
        for authority in ["rebind.evil", "rebind.evil:8787", "10.20.30.40:8787"] {
            let request = HttpRequest::builder()
                .method("GET")
                .uri(format!("http://{authority}/status"))
                .body(Body::empty())
                .unwrap();

            assert_eq!(
                guarded_status(request).await,
                StatusCode::FORBIDDEN,
                "URI authority `{authority}` is foreign and must be rejected: on \
                 HTTP/2 this is where the authority lives"
            );
        }
    }

    /// The paired positive for the URI check, plus the two shapes that must keep
    /// working: an allowed URI authority, and a genuinely authority-less
    /// origin-form request (which makes no claim to validate).
    #[tokio::test]
    async fn host_guard_serves_allowed_and_absent_authorities() {
        for authority in ["127.0.0.1:8787", "[::1]:8787", "localhost:8787"] {
            let request = HttpRequest::builder()
                .method("GET")
                .uri(format!("http://{authority}/status"))
                .body(Body::empty())
                .unwrap();

            assert_eq!(
                guarded_status(request).await,
                StatusCode::OK,
                "URI authority `{authority}` is allowed and must be served"
            );
        }

        let origin_form = HttpRequest::builder()
            .method("GET")
            .uri("/status")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            origin_form.uri().authority(),
            None,
            "premise: this fixture must really carry no authority"
        );
        assert_eq!(
            guarded_status(origin_form).await,
            StatusCode::OK,
            "an authority-less origin-form request makes no claim and is permitted"
        );
    }

    /// A foreign URI authority is rejected even when a benign `Host` sits beside
    /// it: satisfying the guard with the half an attacker does not need must not
    /// vouch for the half it does.
    /// Holds `status_host_403`: this test drives status refusals, which bump the
    /// process-global counter the sampled log reads. The key covers every test
    /// that MOVES that counter, not only those asserting on it.
    #[tokio::test]
    #[serial_test::serial(status_host_403)]
    async fn host_guard_rejects_a_foreign_authority_beside_an_allowed_host() {
        let request = HttpRequest::builder()
            .method("GET")
            .uri("http://rebind.evil/status")
            .header(header::HOST, "127.0.0.1:8787")
            .body(Body::empty())
            .unwrap();

        assert_eq!(guarded_status(request).await, StatusCode::FORBIDDEN);
    }

    /// The malformed-authority shapes are refused at the GUARD, not merely by the
    /// predicate's own unit tests -- including under a wildcard bind, where the
    /// port check is the access decision.
    /// Holds `status_host_403`: this test drives status refusals, which bump the
    /// process-global counter the sampled log reads. The key covers every test
    /// that MOVES that counter, not only those asserting on it.
    #[tokio::test]
    #[serial_test::serial(status_host_403)]
    async fn host_guard_rejects_malformed_authorities_under_a_wildcard_bind() {
        use axum::middleware::from_fn_with_state;

        let app = axum::Router::new()
            .route("/status", get(|| async { StatusCode::OK }))
            .layer(from_fn_with_state(allowlist("0.0.0.0:8787"), host_guard));

        for hostile in [
            "[::1]evil:8787",
            "[]:8787",
            ":8787",
            "@:8787",
            "[::1]:8787evil",
            "192.168.1.5:99999",
        ] {
            let request = HttpRequest::builder()
                .method("GET")
                .uri("/status")
                .header(header::HOST, hostile)
                .body(Body::empty())
                .unwrap();
            let status = app
                .clone()
                .oneshot(request)
                .await
                .expect("guard must respond")
                .status();

            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "`{hostile}` is malformed and must be refused even under a \
                 wildcard bind, where a salvaged port would BE the access decision"
            );
        }

        // Control: a well-formed foreign Host on the bound port is still allowed
        // under a wildcard bind (that is the documented degradation), so the
        // refusals above are about well-formedness rather than about the guard
        // refusing everything.
        let allowed = HttpRequest::builder()
            .method("GET")
            .uri("/status")
            .header(header::HOST, "192.168.1.5:8787")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(allowed).await.unwrap().status(),
            StatusCode::OK,
            "control: a well-formed authority on the bound port is allowed under \
             a wildcard bind"
        );
    }

    /// The sampled refusal line names the CLAIM SITE from a closed set, and still
    /// carries no caller value.
    ///
    /// The old message said "disallowed Host header", which is now wrong twice
    /// over: the refusal also fires on a URI authority (where there is no header)
    /// and on an unreadable value (where there is nothing to name). An operator
    /// reading it needs to know WHICH claim failed to know where to look, and a
    /// closed token gives them that without echoing attacker-controlled bytes.
    ///
    /// The 403 counter is process-GLOBAL and the log is sampled off it, so a
    /// single refusal is not guaranteed to emit. Each site is therefore driven a
    /// full sampling window so at least one of its lines must land, rather than
    /// asserting on a count a concurrent sibling can move.
    ///
    /// `#[serial(status_host_403)]` because the sampled line is derived from the
    /// process-global [`HOST_403_COUNT`]: a concurrent sibling driving refusals
    /// shifts this test's position in the sampling window, so every test that
    /// MOVES that counter holds the same key -- not only the ones asserting on it.
    #[tokio::test]
    #[serial_test::serial(status_host_403)]
    async fn the_refusal_line_names_the_claim_site_without_the_value() {
        use axum::middleware::from_fn_with_state;
        use routectl_testkit::capture_lines;

        let app = axum::Router::new()
            .route("/status", get(|| async { StatusCode::OK }))
            .layer(from_fn_with_state(allowlist("127.0.0.1:8787"), host_guard));

        // Each site, driven its own window, in its own capture.
        let window = SHED_LOG_SAMPLE_N + 1;
        for (site, build) in [("host_header", true), ("uri_authority", false)] {
            let ((), lines) = capture_lines(async {
                for _ in 0..window {
                    let request = if build {
                        HttpRequest::builder()
                            .method("GET")
                            .uri("/status")
                            .header(header::HOST, "evil-LEAKED.example:8787")
                            .body(Body::empty())
                            .unwrap()
                    } else {
                        HttpRequest::builder()
                            .method("GET")
                            .uri("http://evil-LEAKED.example/status")
                            .body(Body::empty())
                            .unwrap()
                    };
                    let resp = app.clone().oneshot(request).await.unwrap();
                    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
                }
            })
            .await;

            let joined = lines.join("\n");
            assert!(
                !lines.is_empty(),
                "a full sampling window must emit at least one {site} line"
            );
            assert!(
                joined.contains("disallowed authority claim"),
                "the message must describe an authority CLAIM, not a Host header: \
                 {joined}"
            );
            assert!(
                joined.contains(&format!("claim_site=\"{site}\"")),
                "a {site} refusal must name that site: {joined}"
            );
            assert!(
                !joined.contains("LEAKED") && !joined.contains("evil-"),
                "the refusal line leaked a caller-controlled value: {joined}"
            );
        }
    }

    /// The host guard counts every disallowed-`Host` 403 but logs SAMPLED, and
    /// never logs the raw (attacker-controlled) `Host` value -- only the fixed
    /// message + running total. Driven on the default current-thread runtime so
    /// the thread-local capture sees the warn emitted inline by the middleware.
    /// Holds the same `status_host_403` key as the sibling claim-site test: both
    /// drive the process-global 403 counter the sampler reads, so running them
    /// concurrently moves each other's window.
    #[tokio::test]
    #[serial_test::serial(status_host_403)]
    async fn host_guard_403_is_sampled_and_never_leaks_host() {
        use axum::middleware::from_fn_with_state;
        use routectl_testkit::capture_lines;

        let al = allowlist("192.168.1.5:8080");
        let app = axum::Router::new()
            .route("/status", get(|| async { StatusCode::OK }))
            .layer(from_fn_with_state(al, host_guard));

        let secret_host = "evil-LEAKED.example.com:8080";
        let fired = SHED_LOG_SAMPLE_N * 3;
        let ((), lines) = capture_lines(async {
            for _ in 0..fired {
                let req = HttpRequest::builder()
                    .method("GET")
                    .uri("/status")
                    .header(header::HOST, secret_host)
                    .body(Body::empty())
                    .unwrap();
                let resp = app.clone().oneshot(req).await.unwrap();
                assert_eq!(resp.status(), StatusCode::FORBIDDEN);
            }
        })
        .await;

        // A window of 3N rejections yields a handful of sampled lines, never
        // one per rejection.
        assert!(
            !lines.is_empty(),
            "at least one sampled host-403 line expected"
        );
        assert!(
            (lines.len() as u64) < fired,
            "host-403 log must be sampled: {} lines for {fired} rejections",
            lines.len()
        );
        for line in &lines {
            assert!(
                !line.contains("LEAKED") && !line.contains("evil-"),
                "host-403 log leaked the raw Host: {line}"
            );
            assert!(
                line.contains("disallowed authority claim"),
                "unexpected host-403 line: {line}"
            );
        }
    }
}
