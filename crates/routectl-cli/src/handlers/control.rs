//! `POST /control/capability/purge` -- the daemon's one mutating control
//! surface: remove a single resident learned-capability entry and persist the
//! `cleared` settlement that keeps the warm rebuild from resurrecting it.
//!
//! # Why the daemon has to do this
//!
//! A learned negative lives in the running process's registry. A CLI that
//! edited the ledger directly would remove nothing from the live registry, so
//! the verdict would keep steering routing until the next restart -- and it
//! would write to a SQLite file the daemon holds open, which this repo has
//! learned the hard way is a way to break a healthy database. So the removal
//! happens where the state is, and the CLI is a client of this route.
//!
//! # Threat model: the daemon's existing one, unchanged
//!
//! This route carries NO credential scheme of its own. It sits on the same
//! axum server, the same socket, and the same listener-auth layer as `/v1/*`:
//! gated by `[server.auth].tokens` when tokens are configured, token-less on a
//! loopback dev bind exactly like the inference routes. Three checks narrow it
//! beyond them, none of them authentication, and each answering a DIFFERENT
//! question -- stating them separately because no one of them covers another:
//!
//! - a non-loopback PEER is refused outright, which the inference routes do not
//!   do. A mutating control surface has no legitimate remote caller even on an
//!   `--unsafe-public` bind, where the operator opted into exposing inference,
//!   not administration.
//! - an authority that does not name a loopback endpoint is refused --
//!   ANTI-DNS-REBINDING, the same guard and predicate the status subtree
//!   carries. This is the check the content-type gate cannot substitute for: an
//!   attacker who controls a hostname can point it at 127.0.0.1, and a page on
//!   `http://rebind.evil` is then SAME-ORIGIN with the daemon -- it needs no
//!   preflight, its content-type is unconstrained, and its peer really is
//!   loopback. Only the authority it claims still carries the attacker's name.
//!   BOTH places an authority can arrive are validated: the `Host` header
//!   (HTTP/1) and the request URI (HTTP/2 puts `:authority` there and sends no
//!   `Host`, so a Host-only guard would admit an h2c request outright). Only a
//!   request carrying neither -- origin-form from a hand-rolled client, making
//!   no authority claim at all -- is permitted.
//! - a non-JSON `content-type` is refused, using the same predicate the
//!   inference routes use. This buys exactly one property: a browser's SIMPLE
//!   cross-origin request cannot reach the mutation. A page on any origin can
//!   `fetch` a loopback URL with `text/plain` and no preflight, and while the
//!   response would be hidden from the script, the MUTATION would already have
//!   happened. Requiring JSON forces a preflight this daemon never answers. It
//!   says nothing about a same-origin (rebound) request -- that is the `Host`
//!   check's job.
//!
//! # Scope
//!
//! Learned entries only. The purge drops one observation the daemon made; it
//! never edits an operator override or invents a catalog prior. Persistence
//! across relearning is the capability override configuration's job (see
//! CONFIGURATION.md, "Capability intelligence"), and this route deliberately
//! does not offer a competing runtime store -- a second place for the same
//! decision to live is a second place for it to disagree.

use std::sync::Arc;

use axum::Json;
use axum::body::{Body, to_bytes};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use routectl_core::capability::Verdict;
use routectl_router::router::PurgeOutcome;

use crate::server::purge_settlement::SettlementOutcome;
use routectl_usage::{BatchCommit, CapabilityEvent};
use serde::Deserialize;
use serde_json::json;

use crate::server::AppState;

/// Wire schema version of this route's request and response envelopes.
pub const SCHEMA_VERSION: u32 = 1;

/// Ceiling on the request body. The vocabulary is two short tokens, so this is
/// generous by two orders of magnitude while still bounding what an unbounded
/// read would admit.
const MAX_BODY_BYTES: usize = 8 * 1024;

/// Ceiling on either key in the request. Matches the registry's own key
/// bounds closely enough to refuse an absurd value at the boundary rather than
/// carrying it into a log line and an audit record.
const MAX_KEY_BYTES: usize = 256;

/// Fixed code for every rejected request. ONE code on purpose: a caller
/// learning WHICH validation failed learns about the daemon's configured
/// targets, and a purge request is one an unauthenticated caller may reach on
/// a token-less loopback bind.
const INVALID_REQUEST: &str = "invalid_request";

/// Code for a request from a non-loopback peer.
const FORBIDDEN_PEER: &str = "forbidden_peer";

/// Code for a request that claimed an authority this route will not serve.
///
/// Covers every claim the request carries, not just the first `Host` header:
/// any `Host` value naming a non-loopback authority, any `Host` value that is
/// not valid UTF-8 (present but unevaluable, so it fails closed), and a request
/// URI authority -- HTTP/2's `:authority` -- that names a non-loopback endpoint.
/// One code for all of them on purpose: which claim failed, and why, is exactly
/// the detail a caller probing the guard would want.
const FORBIDDEN_HOST: &str = "forbidden_host";

/// Code for a request whose `content-type` does not name JSON.
const UNSUPPORTED_MEDIA_TYPE: &str = "unsupported_media_type";

/// Code for a purge whose durable clear did not commit.
///
/// A DISTINCT code from every refusal above and from the clean no-op, because it
/// is the one outcome where the operator must retry: the entry is unchanged and
/// still acting, and nothing was persisted. It names the class only -- an
/// unavailable writer, a full channel and a failed transaction are one answer on
/// the wire, since which one it was tells a caller about the daemon's storage
/// state rather than about its own request.
const DURABILITY_FAILED: &str = "durability_failed";

/// Code for a key another purge currently holds.
///
/// Never collapsed into the absent no-op: telling an operator the entry is gone
/// when another request is mid-commit is the one answer a refusal must not give,
/// because the operator would stop and the verdict might still be there.
const PURGE_BUSY: &str = "purge_busy";

/// Code for a purge whose entry changed under its lease.
///
/// Distinct from every other answer: the clear DID commit, so the ledger moved,
/// but nothing was removed from memory because the resident entry was no longer
/// the version the operator approved removing. Telling the operator this
/// succeeded would claim a removal that did not happen; telling them it failed to
/// persist would be false about the ledger.
const PURGE_SUPERSEDED: &str = "purge_superseded";

/// Code for a purge that arrived through a superseded Router.
///
/// Distinct from a durability failure: nothing is wrong with storage, the
/// request simply addressed a registry generation the published Router has left.
/// The daemon retries it once against the current Router before answering this.
const PURGE_STALE: &str = "purge_stale";

/// How many times the handler re-reads the live Router and retries a stale
/// reservation.
///
/// One retry, not a loop: a reload is a discrete event, so a single re-read
/// against the fresh Router resolves the ordinary race. Retrying indefinitely
/// would turn a reload storm into an unbounded hold on the control route, and it
/// is the operator -- not the daemon -- who should decide to ask again.
const STALE_RETRIES: u32 = 1;

/// The wire refusal codes, for the CLI's mirror-agreement pin.
///
/// Readers, not a re-export of the constants: the CLI deliberately mirrors the
/// wire vocabulary (it may be talking to a daemon of another build), and these
/// exist so a test can prove the mirror matches what THIS build emits.
#[cfg(test)]
pub(crate) const fn durability_failed_code() -> &'static str {
    DURABILITY_FAILED
}

#[cfg(test)]
pub(crate) const fn purge_busy_code() -> &'static str {
    PURGE_BUSY
}

#[cfg(test)]
pub(crate) const fn purge_stale_code() -> &'static str {
    PURGE_STALE
}

#[cfg(test)]
pub(crate) const fn purge_superseded_code() -> &'static str {
    PURGE_SUPERSEDED
}

/// The closed request vocabulary. `deny_unknown_fields` makes a typo'd or
/// speculative key a rejection rather than a silently-ignored field, so a
/// caller can never believe it asked for something this route did not do.
///
/// Deliberately NOT carrying a provider kind: the kind is half the registry
/// key and the daemon derives it from its own config, so accepting one would
/// let a caller address a key the learn path never minted.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PurgeRequest {
    /// Routing state key (a `[models]` nickname, a pooled seat's
    /// `nickname#label`, or a `[providers]` name) to purge on.
    state_key: String,
    /// Capability key to purge.
    capability_key: String,
}

/// Handle one purge request.
///
/// Order is load-bearing: the peer, `Host`, and content-type checks all run
/// before the body is buffered, so a rejected caller cannot make the daemon read
/// its bytes at all, and none of the three can be reached past a mutation. The
/// `Host` check precedes the content-type check so a rebound page cannot use the
/// status code as an oracle for the route's body vocabulary.
pub async fn purge_capability(
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Response {
    // The SHARED loopback predicate, not a bare `Ipv6Addr::is_loopback`: a
    // dual-stack listener presents an IPv4 client as an IPv4-MAPPED IPv6 peer
    // (`::ffff:127.0.0.1`), which the bare check refuses -- so the local CLI
    // would be turned away on exactly the listener shape that is most common.
    // The shared predicate unwraps the mapping and covers all of
    // `127.0.0.0/8`, while a mapped PUBLIC address stays refused.
    if !crate::server::is_loopback(&peer.ip().to_string()) {
        // Counted nowhere and logged without the peer address: the address is
        // caller-controlled, and the refusal itself is the whole signal.
        tracing::warn!("capability purge refused a non-loopback caller");
        return refused(StatusCode::FORBIDDEN, FORBIDDEN_PEER);
    }
    // Anti-DNS-rebinding, the same guard and the same predicate the status
    // subtree carries. A loopback PEER check is not enough on its own: an
    // attacker who controls a hostname can point it at 127.0.0.1, after which a
    // page on `http://rebind.evil` is SAME-ORIGIN with the daemon -- so it needs
    // no preflight, the JSON content-type below is allowed, and the peer really
    // is loopback. The authority the client claimed is what still carries the
    // attacker's name.
    //
    // EVERY authority the client claimed must pass, and an unreadable claim
    // fails closed. Three ways a claim arrives, all checked:
    //
    //   * every `Host` header VALUE via `get_all` -- a request may carry the
    //     header more than once, and `get` returns only the first, so a hostile
    //     second value rode along unchecked. Which duplicate a downstream reader
    //     honors is not this guard's call to assume;
    //   * a `Host` value that is not valid UTF-8 -- PRESENT but unevaluable. The
    //     client did make a claim; this build simply cannot read it, and a claim
    //     that cannot be evaluated has not been validated. Admitting it is the
    //     fail-open shape, so presence plus unreadability is a refusal;
    //   * the request URI's authority -- on HTTP/2 the `:authority`
    //     pseudo-header arrives there and no `Host` is sent at all, so a
    //     Host-only guard sees nothing on an h2c request and the absent-authority
    //     allowance below becomes a bypass.
    //
    // Checking only the first claim found would be equally wrong: an attacker
    // would satisfy the guard with whichever one it reads and carry the hostile
    // name in another. No value is ever logged -- all are caller-controlled.
    let host_values = request.headers().get_all(axum::http::header::HOST);
    let host_claims_ok = host_values.iter().all(|value| {
        value
            .to_str()
            .is_ok_and(crate::server::status_gate::is_loopback_authority)
    });
    let uri_claim_ok = request.uri().authority().is_none_or(|authority| {
        crate::server::status_gate::is_loopback_authority(authority.as_str())
    });
    if !host_claims_ok || !uri_claim_ok {
        // The SHARED `ClaimSite` token, so this refusal reads in the same
        // vocabulary the status guard uses -- an operator correlating a rejection
        // across the two surfaces should not have to learn two. It names the site
        // only; the claimed value is caller-controlled and never logged.
        let claim_site = if host_claims_ok {
            crate::server::status_gate::ClaimSite::UriAuthority
        } else {
            crate::server::status_gate::ClaimSite::HostHeader
        };
        tracing::warn!(
            claim_site = claim_site.as_str(),
            "capability purge refused a request with a disallowed authority claim",
        );
        return refused(StatusCode::FORBIDDEN, FORBIDDEN_HOST);
    }
    // A request carrying NO authority in either place is permitted: that is
    // origin-form HTTP/1 from a hand-rolled client, and it makes no authority
    // claim to validate. The rebinding vector is a browser, which always sends
    // one.
    //
    // A JSON content-type is required, which is what stops a browser's simple
    // cross-origin request from mutating (see the module docs). The predicate is
    // the ingress one, so the control route and the inference routes cannot
    // drift on what counts as JSON.
    if !crate::handlers::ingress_handle::is_json_content_type(request.headers()) {
        tracing::warn!("capability purge refused a request without a JSON content-type");
        return refused(StatusCode::UNSUPPORTED_MEDIA_TYPE, UNSUPPORTED_MEDIA_TYPE);
    }
    let Some(purge) = parse_body(request.into_body()).await else {
        return refused(StatusCode::BAD_REQUEST, INVALID_REQUEST);
    };

    purge_durably(&state, &purge).await
}

/// Reserve, durably commit, then finalize -- in that order, which is the whole
/// contract (see [`routectl_router::router::PurgeOutcome`] and the purge module's
/// docs).
///
/// Success is reported ONLY after an acknowledged commit. Every other path leaves
/// the entry resident and acting and answers non-2xx, because an operator told a
/// verdict is gone stops looking: the entry would keep steering routing until the
/// next boot, and the warm rebuild would then resurrect it.
///
/// A stale reservation is retried once against a freshly loaded Router. The
/// retry is safe precisely because a stale reservation changed nothing: it took
/// no lease, removed nothing, and committed nothing, so there is no partial state
/// for the second attempt to collide with and no way for it to clear twice.
async fn purge_durably(state: &AppState, purge: &PurgeRequest) -> Response {
    for attempt in 0..=STALE_RETRIES {
        // Re-read the live Router each attempt: on the retry the point is to
        // reserve against the CURRENT generation, and holding the first snapshot
        // would just reproduce the same staleness.
        let router = state.router.load_full();
        match router.reserve_learned_capability_purge(&purge.state_key, &purge.capability_key) {
            PurgeOutcome::Reserved(reserved) => {
                return settle(state, &router, reserved).await;
            }
            // A clean no-op, and a 2xx: the key holds nothing, which is exactly
            // what the operator wanted to be true. `generation` is ABSENT rather
            // than sampled -- a generation describes a removal, and there was
            // none.
            PurgeOutcome::Absent => {
                // Audited like any other completed operator action, with
                // `removed = false`: an operator reading the log must be able to
                // tell "I purged something" from "there was nothing to purge",
                // and the absence of a record would make the two
                // indistinguishable. The SUCCESS record is emitted at finalize
                // instead (see `settle`), so no record ever claims a removal the
                // ledger did not receive.
                router.audit_absent_purge(&purge.state_key, &purge.capability_key);
                return (
                    StatusCode::OK,
                    Json(json!({
                        "schema_version": SCHEMA_VERSION,
                        "purged": false,
                        "state_key": purge.state_key,
                        "capability_key": purge.capability_key,
                    })),
                )
                    .into_response();
            }
            // Busy is RETRYABLE on the same bound as stale, and for the same
            // reason: the common cause is a reload boundary that is admitted but
            // not yet settled, which resolves in the moment it takes to commit.
            // A second purge of the same key resolves the same way (the lease
            // lasts one transaction). Nothing was reserved either way, so the
            // retry carries no obligation forward.
            PurgeOutcome::Busy if attempt < STALE_RETRIES => {
                // Yield, rather than spin: the condition clears on another task's
                // progress, so re-reading immediately would just burn the retry.
                tokio::task::yield_now().await;
            }
            PurgeOutcome::Busy => {
                tracing::warn!("capability purge refused: the key or the registry is busy");
                return refused(StatusCode::CONFLICT, PURGE_BUSY);
            }
            // Retry the ordinary reload race once, then report it. Nothing was
            // reserved, so the loop carries no obligation forward.
            //
            // Yielded for the same reason as Busy: the condition clears on the
            // reload coordinator's progress (publishing the fresh Router), and
            // re-reading immediately, without giving that task a turn, would
            // just burn the one retry this bound allows.
            PurgeOutcome::Stale if attempt < STALE_RETRIES => {
                tokio::task::yield_now().await;
            }
            PurgeOutcome::Stale => {
                tracing::warn!("capability purge refused: the request addressed a stale registry");
                return refused(StatusCode::CONFLICT, PURGE_STALE);
            }
        }
    }
    // Unreachable: the loop returns on every outcome except a retryable stale,
    // and the last iteration's stale arm returns. Kept as a refusal rather than
    // an unreachable!() so a future change to STALE_RETRIES cannot turn a
    // control-flow slip into a panic on an operator surface.
    refused(StatusCode::CONFLICT, PURGE_STALE)
}

/// Admit the reservation's durable clear, hand the settlement to the DAEMON, and
/// report what it did.
///
/// The transfer is the point, and it happens before the first await: ownership of
/// the reservation, the receipt, and a router handle moves to a task the server
/// owns. An HTTP future is cancelled when its client goes away, and a cancel
/// inside the commit would drop the receipt mid-transaction -- the clear might
/// still land, with nothing alive to remove the entry or release the lease, and
/// the daemon would hold a ledger and a registry that disagree. Here the handler
/// keeps only a result receiver; dropping it cancels nothing.
///
/// Nothing is awaited while a registry lock is held: the reservation returned
/// holding none.
async fn settle(
    state: &AppState,
    router: &Arc<routectl_router::Router>,
    reserved: Box<routectl_router::router::ReservedPurge>,
) -> Response {
    let event = cleared_event(&reserved.settlement(), router);
    // Stamped with the generation the REMOVAL will run under, carried out of the
    // reservation rather than sampled here: a value read at this point could sit
    // on the far side of a reload boundary from the mutation it describes, and
    // the writer would then replay this settlement after a tombstone that
    // evicted the very entry it clears.
    let generation = reserved.generation();
    // Submitted with the CAPTURED incarnation: on commit the writer records it as
    // this key's purge floor, which is what makes a pre-purge event delayed past
    // the clear drop while a genuine post-purge relearn still lands.
    let incarnation = reserved.generation_incarnation();
    // Keys read BEFORE the transfer: the settlement consumes the reservation, and
    // echoing what the registry keyed on (the normalized capability key, not the
    // caller's raw value) is what lets an operator confirm the purge addressed
    // what they meant.
    let state_key = reserved.state_key.clone();
    let capability_key = reserved.capability_key.clone();
    // CLAIM FIRST, before the durable batch is admitted. The ordering is the
    // contract: if the claim succeeds, shutdown is already waiting for this
    // settlement, so it cannot drain the writer out from under the commit. If it
    // fails, no batch was ever admitted and there is no lease obligation -- the
    // reservation is released here and the entry is untouched.
    //
    // Admitting first and claiming second was the hole: a batch could be queued
    // and then find the tracker closed, leaving a commit in flight with nothing
    // owning its settlement.
    let Some(claim) = state.purge_settlements.claim() else {
        router.abandon_learned_capability_purge(reserved);
        tracing::warn!("capability purge refused: the daemon is shutting down");
        return refused(StatusCode::SERVICE_UNAVAILABLE, DURABILITY_FAILED);
    };
    let receipt = match state
        .usage
        .admit_capability_batch_at(vec![event], generation, incarnation)
    {
        Ok(receipt) => receipt,
        // Never queued, so nothing is pending: the claim releases on drop and the
        // reservation is released here, leaving the entry untouched.
        Err(failure) => {
            drop(claim);
            router.abandon_learned_capability_purge(reserved);
            return durability_refusal(failure);
        }
    };
    // TRANSFER, before any await: ownership of the reservation, the receipt and a
    // router handle moves to the daemon's task, so a client that vanishes cannot
    // drop a receipt mid-commit and strand a lease.
    let waiter = state
        .purge_settlements
        .settle(claim, Arc::clone(router), reserved, receipt);
    match waiter.await {
        Ok(SettlementOutcome::Purged) => (
            StatusCode::OK,
            Json(json!({
                "schema_version": SCHEMA_VERSION,
                "purged": true,
                "state_key": state_key,
                "capability_key": capability_key,
                "generation": generation,
            })),
        )
            .into_response(),
        // Committed, but the resident entry no longer matched what the operator
        // approved removing, so nothing was removed. The ledger HAS the clear, so
        // this is not a durability failure -- and it is not a success either.
        Ok(SettlementOutcome::Superseded) => {
            tracing::warn!("capability purge superseded: the entry changed under its lease");
            refused(StatusCode::CONFLICT, PURGE_SUPERSEDED)
        }
        Ok(SettlementOutcome::Failed(failure)) => durability_refusal(failure),
        // The settlement task went away without answering. It reports its own
        // ambiguity (and triggers shutdown); this answer only has to avoid
        // claiming a purge that may not have happened.
        Err(_) => refused(StatusCode::SERVICE_UNAVAILABLE, DURABILITY_FAILED),
    }
}

/// The refusal for a clear that did not commit: one wire code, the cause in the
/// operator's own log.
fn durability_refusal(failure: BatchCommit) -> Response {
    tracing::warn!(
        outcome = commit_token(failure),
        "capability purge could not persist its clear; the entry is unchanged",
    );
    refused(StatusCode::SERVICE_UNAVAILABLE, DURABILITY_FAILED)
}

/// Stable token for a non-committed batch outcome, for the operator log only.
/// The wire answer stays one code (see [`DURABILITY_FAILED`]); an operator
/// reading their own daemon's log is entitled to the distinction.
const fn commit_token(outcome: BatchCommit) -> &'static str {
    match outcome {
        BatchCommit::Committed { .. } => "committed",
        BatchCommit::Unavailable => "writer_unavailable",
        BatchCommit::ChannelFull => "writer_channel_full",
        BatchCommit::WriteFailed => "write_failed",
    }
}

/// Buffer and validate a request body, or `None` for anything outside the
/// vocabulary. Every rejection collapses to the same `None`: the caller emits
/// one fixed code, so no parse detail or config knowledge reaches the wire.
///
/// A blank key is refused rather than answered. An empty `state_key` cannot
/// name a target, so answering it would report `purged: false` for a request
/// that never addressed anything -- a false clean-no-op, which is exactly the
/// answer an operator would misread as "already gone".
async fn parse_body(body: Body) -> Option<PurgeRequest> {
    let bytes = to_bytes(body, MAX_BODY_BYTES).await.ok()?;
    let purge: PurgeRequest = serde_json::from_slice(&bytes).ok()?;
    let bounded = |key: &str| {
        !key.trim().is_empty() && key.len() <= MAX_KEY_BYTES && !key.contains(char::is_control)
    };
    (bounded(&purge.state_key) && bounded(&purge.capability_key)).then_some(purge)
}

/// The `cleared` capability event for a settlement, stamped with the live
/// router's boundary revision exactly as the live drain stamps its own rows.
///
/// `source` is `live`: the settlement records that this daemon's registry no
/// longer holds the entry, which is the same class of fact a probe-settled
/// clear records. `phase` / `tier` / `evidence_class` are empty because a
/// clear is not an observation of upstream behavior -- inventing tokens for
/// them would fabricate provenance the purge does not have.
pub(crate) fn cleared_event(
    settlement: &routectl_router::router::CapabilityClearedEvent,
    router: &routectl_router::Router,
) -> CapabilityEvent {
    CapabilityEvent {
        ts: super::usage_capture::epoch_ms_now(),
        lane_key: settlement.state_key.clone(),
        capability: settlement.capability_key.clone(),
        verdict: Verdict::Cleared.as_str().to_string(),
        phase: String::new(),
        source: routectl_core::capability::EvidenceSource::Live
            .as_str()
            .to_string(),
        tier: String::new(),
        evidence_class: None,
        upstream_token: None,
        catalog_version: i64::from(router.catalog_version()),
        overlay_revision: i64::try_from(router.overlay_revision()).unwrap_or(i64::MAX),
    }
}

/// The fixed refusal envelope. Same shape for both codes so a caller parses
/// one dialect, and carrying no detail beyond the code so a refusal cannot
/// become a probe of the daemon's configuration.
fn refused(status: StatusCode, code: &'static str) -> Response {
    (
        status,
        Json(json!({
            "schema_version": SCHEMA_VERSION,
            "error": {
                "code": code,
                "message": "capability purge request refused",
            },
        })),
    )
        .into_response()
}

#[cfg(test)]
#[path = "control_tests.rs"]
mod control_tests;
