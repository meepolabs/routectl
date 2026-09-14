//! Structured-log contract for the two Claude-Code-version warnings, and
//! the proof that they stay distinguishable.
//!
//! routectl carries TWO version checks answering two questions. The MITM
//! guard (`proxy::cc_version`) asks "is this client the version the operator
//! recorded as tested in `[mitm]`", is opt-in, and only sees the front-proxy
//! leg. The ingress guard (`server::cc_pin_drift`) asks "is this client the
//! version THIS BUILD mints", and fires in every mode. Front-proxy traffic
//! can produce both at once.
//!
//! That is only useful if an operator can tell them apart in a log, so this
//! pins each event's target and its EXACT field set -- including that they
//! share no version field NAME, so a grep for one cannot silently match the
//! other. It also pins what must never appear: the full User-Agent, any
//! header value, and any billing attribution text.
//!
//! Its own integration binary for the same reason as the sibling admission
//! log test: a thread-local capture subscriber over a shared `warn!`
//! callsite is unreliable inside the big lib test binary, where other tests
//! hit the callsite under `NoSubscriber` first and poison tracing's
//! per-callsite `Interest` cache. `with_capture` installs a thread-local
//! subscriber, so every test here is `#[tokio::test]` (current-thread by
//! default) rather than plain `#[test]`.

use axum::http::{HeaderMap, HeaderValue, header::USER_AGENT};
use routectl_cli::proxy::cc_version::CcVersionWarnGuard;
use routectl_cli::server::cc_pin_drift::CcPinDriftGuard;
use routectl_testkit::{CapturedEvent, with_capture};

/// Log target the ingress compiled-pin guard emits under.
const INGRESS_TARGET: &str = "routectl_cli::server::cc_pin_drift";

/// Log target the opt-in MITM tested-version guard emits under.
const MITM_TARGET: &str = "routectl_cli::proxy::cc_version";

/// A drifted version well clear of anything routectl could mint.
const DRIFTED: &str = "99.9.9";

fn ua_for(version: &str) -> String {
    format!("claude-cli/{version} (external, sdk-cli)")
}

fn headers_with_ua(ua: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_str(ua).unwrap());
    headers
}

fn warns(events: &[CapturedEvent], target: &str) -> Vec<CapturedEvent> {
    events
        .iter()
        .filter(|e| e.target == target && e.level == tracing::Level::WARN)
        .cloned()
        .collect()
}

fn field_names(event: &CapturedEvent) -> Vec<&str> {
    event.fields.iter().map(|(k, _)| k.as_str()).collect()
}

#[tokio::test]
async fn the_ingress_drift_warning_carries_its_pinned_target_and_exact_field_set() {
    let (emitted, events) = with_capture(async {
        let guard = CcPinDriftGuard::new();
        guard.observe_headers(&headers_with_ua(&ua_for(DRIFTED)))
    })
    .await;
    assert!(
        emitted,
        "control: this observation must be the emitting one"
    );

    let warns = warns(&events, INGRESS_TARGET);
    assert_eq!(
        warns.len(),
        1,
        "exactly one warning per new drifted version"
    );
    assert_eq!(
        field_names(&warns[0]),
        vec!["pinned_cc_version", "ingress_cc_version", "source"],
        "the ingress guard's field set is pinned exactly, so a rename or an \
         added request-derived field is a review moment"
    );
    assert_eq!(warns[0].field("ingress_cc_version"), Some(DRIFTED));
    assert_eq!(warns[0].field("source"), Some("ingress_user_agent"));
    assert!(!warns[0].message.is_empty());
}

#[tokio::test]
async fn the_two_guards_share_no_version_field_name() {
    // The whole point of separate vocabularies: an operator grepping for one
    // question's field must not match the other's answer.
    let (_, ingress_events) = with_capture(async {
        CcPinDriftGuard::new().observe_headers(&headers_with_ua(&ua_for(DRIFTED)))
    })
    .await;
    let (_, mitm_events) =
        with_capture(async { CcVersionWarnGuard::new().check(Some("2.1.0"), Some(DRIFTED)) }).await;

    let ingress = warns(&ingress_events, INGRESS_TARGET);
    let mitm = warns(&mitm_events, MITM_TARGET);
    assert_eq!(ingress.len(), 1, "control: the ingress guard emitted");
    assert_eq!(mitm.len(), 1, "control: the MITM guard emitted");

    let ingress_names = field_names(&ingress[0]);
    let mitm_names = field_names(&mitm[0]);
    assert!(
        ingress_names.contains(&"ingress_cc_version"),
        "got {ingress_names:?}"
    );
    assert!(
        mitm_names.contains(&"observed_cc_version"),
        "got {mitm_names:?}"
    );
    assert!(
        !ingress_names.contains(&"observed_cc_version"),
        "the ingress guard must not reuse the MITM guard's observed-version field name"
    );
    assert!(
        !mitm_names.contains(&"ingress_cc_version"),
        "the MITM guard must not adopt the ingress guard's field name"
    );
    assert_ne!(
        ingress[0].target, mitm[0].target,
        "the two questions keep distinct targets"
    );
    assert!(
        mitm_names.contains(&"tested_cc_version"),
        "the MITM guard still names the operator-recorded side; got {mitm_names:?}"
    );
    assert!(
        ingress_names.contains(&"pinned_cc_version"),
        "the ingress guard names the compiled side; got {ingress_names:?}"
    );
}

#[tokio::test]
async fn the_drift_warning_never_carries_the_full_user_agent_or_a_header_value() {
    let (_, events) = with_capture(async {
        CcPinDriftGuard::new().observe_headers(&headers_with_ua(
            "claude-cli/99.9.9 (external, sdk-cli) session=SECRET-SESSION-ID",
        ))
    })
    .await;

    let warns = warns(&events, INGRESS_TARGET);
    assert_eq!(warns.len(), 1);
    let rendered = format!("{:?}{}", warns[0].fields, warns[0].message);
    assert!(
        !rendered.contains("SECRET-SESSION-ID"),
        "no part of the header value beyond the parsed version may be logged: {rendered}"
    );
    assert!(
        !rendered.contains("external"),
        "the User-Agent's platform detail must not be logged: {rendered}"
    );
    assert!(
        !rendered.contains("sdk-cli"),
        "the surface token is not part of the drift warning: {rendered}"
    );
}

#[tokio::test]
async fn a_steady_drifted_client_warns_once_across_many_requests() {
    let ((), events) = with_capture(async {
        let guard = CcPinDriftGuard::new();
        for _ in 0..25 {
            guard.observe_headers(&headers_with_ua(&ua_for(DRIFTED)));
        }
    })
    .await;

    assert_eq!(
        warns(&events, INGRESS_TARGET).len(),
        1,
        "a client that keeps sending one drifted version is one warning, not 25"
    );
}

#[tokio::test]
async fn a_client_on_the_compiled_pin_emits_nothing_at_all() {
    let pinned_ua = format!(
        "claude-cli/{} (external, cli)",
        routectl_core::identity::anthropic::compiled_claude_cli_version()
    );

    let (emitted, events) = with_capture(async {
        CcPinDriftGuard::new().observe_headers(&headers_with_ua(&pinned_ua))
    })
    .await;

    assert!(!emitted);
    assert!(
        warns(&events, INGRESS_TARGET).is_empty(),
        "no drift, no line"
    );
}

#[tokio::test]
async fn a_request_without_a_parseable_version_emits_nothing() {
    // The stated silence ceiling: a caller with no readable Claude Code
    // User-Agent contributes no observation, so absence of a warning never
    // means absence of drift.
    for ua in [
        "Mozilla/5.0",
        "claude-cli/",
        "claude-cli/99.9.9.1e8 (external, cli)",
    ] {
        let (emitted, events) =
            with_capture(async { CcPinDriftGuard::new().observe_headers(&headers_with_ua(ua)) })
                .await;

        assert!(!emitted, "ua={ua}");
        assert!(
            warns(&events, INGRESS_TARGET).is_empty(),
            "an unobservable client is silent, not warned about; ua={ua}"
        );
    }

    let (emitted, events) =
        with_capture(async { CcPinDriftGuard::new().observe_headers(&HeaderMap::new()) }).await;
    assert!(!emitted);
    assert!(warns(&events, INGRESS_TARGET).is_empty());
}

#[tokio::test]
async fn the_cap_notice_is_one_line_and_names_the_bound_it_hit() {
    let ((), events) = with_capture(async {
        let guard = CcPinDriftGuard::with_cap(2);
        // Fill the set, then push three keys past it.
        for version in ["9.9.1", "9.9.2", "9.9.3", "9.9.4", "9.9.5"] {
            guard.observe_version(Some(version));
        }
    })
    .await;

    let warns = warns(&events, INGRESS_TARGET);
    let cap_lines: Vec<&CapturedEvent> =
        warns.iter().filter(|e| e.field("cap").is_some()).collect();
    assert_eq!(
        cap_lines.len(),
        1,
        "at most one cap-reached line however many keys arrive past the cap"
    );
    assert_eq!(cap_lines[0].field("cap"), Some("2"));
    assert_eq!(cap_lines[0].field("source"), Some("ingress_user_agent"));
    assert_eq!(
        warns.len(),
        3,
        "two per-version lines below the cap plus exactly one cap line"
    );
}

/// End-to-end at the guard's own seam (no log scraping): the return values
/// across the cap boundary. Complements the log pins above -- one proves the
/// decision, the other proves what an operator sees.
#[tokio::test]
async fn the_return_values_across_the_cap_boundary_are_emit_then_silence() {
    let guard = CcPinDriftGuard::with_cap(2);

    assert!(guard.observe_version(Some("9.9.1")));
    assert!(guard.observe_version(Some("9.9.2")));

    assert!(
        !guard.observe_version(Some("9.9.3")),
        "the first key past the cap does not warn about itself"
    );
    assert!(
        !guard.observe_version(Some("9.9.4")),
        "nor does the next one"
    );
    assert!(
        !guard.observe_version(Some("9.9.1")),
        "a key recorded before the cap still dedups after it"
    );
}
