// Rewrite-eligibility and canary scenarios, driven over real HTTP against the
// assembled daemon. `include!`d into preflight_daemon_tests.rs; the fixtures
// and imports live there, so do not add `use` lines here.

// ---------------------------------------------------------------------------
// Feature present / absent, through real HTTP
// ---------------------------------------------------------------------------

/// An acknowledged eligible verdict rewrites a request that arrived over HTTP.
///
/// THE assembled-daemon proof, asserted on the BYTES the provider received. A
/// pipeline wired into a handler the request never reaches, or behind an ingress that
/// drops the field before planning, passes every router-level test and fails this.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_request_is_rewritten_when_a_verdict_is_eligible() {
    let (config, dir) = daemon_config(None);
    let recorder = Arc::new(Recorder::default());
    let router = router_with(&config, Arc::clone(&recorder));
    plant_acting_field_verdict_for_tests(&router, STATE_KEY, FIELD_PATH, 1);
    let daemon = spawn_daemon(config, dir, router).await;

    let status = post_inference(&daemon, &body_with_field()).await;

    assert_eq!(status, reqwest::StatusCode::OK, "the request serves");
    assert_eq!(recorder.calls(), 1, "one dispatch, so no repair retry ran");
    assert_eq!(
        recorder.carried_per_attempt(),
        vec![false],
        "the body the provider RECEIVED carries neither carrier of the field -- the \
         rewrite happened on a request that arrived over HTTP, through the real \
         handler and the real planner",
    );
    daemon.shutdown().await;
}

/// The FEATURE-ABSENT control: with no verdict, the same HTTP request forwards
/// unchanged.
///
/// Without it the test above would pass against a daemon that stripped the field
/// unconditionally, which is a fidelity defect rather than the feature.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_request_forwards_unchanged_when_no_verdict_is_resident() {
    let (config, dir) = daemon_config(None);
    let recorder = Arc::new(Recorder::default());
    let router = router_with(&config, Arc::clone(&recorder));
    let daemon = spawn_daemon(config, dir, router).await;

    let status = post_inference(&daemon, &body_with_field()).await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(
        recorder.carried_per_attempt(),
        vec![true],
        "no resident verdict authorizes no rewrite, so the client's field is \
         forwarded verbatim",
    );
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// The canary, through the daemon
// ---------------------------------------------------------------------------

/// A daemon request that is the canary sends the field UNREPAIRED, its success
/// clears the verdict, and the NEXT HTTP request forwards unchanged.
///
/// What this adds, and what it deliberately leaves to its sibling, measured by
/// mutation rather than asserted:
///
/// - HERE: the settlement reaching the daemon's LIVE registry over a real request
///   boundary. Mutating the settlement to a bare `drop` reds this test by name.
/// - NOT here: that the clear is DURABLE. Dropping only the ledger event leaves this
///   test green, because an unrepaired canary forwards the field either way and the
///   in-memory clear still happens. The restart proof in `canary_span_tests` is what
///   catches that, and it does -- skipping the drain reds it with the verdict
///   resurrected.
/// - NOT here: the cadence NUMBER. `canary_span_tests` walks all hundred against a
///   real Router; re-dispatching a hundred HTTP requests would test the count a second
///   time and this boundary no better.
///
/// The canary is made due through the gated seed, which is the production writer for
/// exactly that state (a cold boot seeds an already-acting verdict due on its next
/// eligible request). Dispatching a hundred HTTP requests to reach the same state
/// would test the cadence a second time and nothing else.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_canary_request_clears_the_verdict_and_the_next_request_forwards_unchanged() {
    let (config, dir) = daemon_config(None);
    let recorder = Arc::new(Recorder::default());
    let router = router_with(&config, Arc::clone(&recorder));
    plant_acting_field_verdict_for_tests(&router, STATE_KEY, FIELD_PATH, 1);
    routectl_router::make_field_canary_due_for_tests(&router, STATE_KEY, FIELD_PATH);
    let daemon = spawn_daemon(config, dir, router).await;

    // The canary request: the daemon restores the field, the upstream accepts it, and
    // accepting the field the verdict calls broken is the disproof.
    let first = post_inference(&daemon, &body_with_field()).await;
    assert_eq!(first, reqwest::StatusCode::OK);
    assert_eq!(
        recorder.carried_per_attempt(),
        vec![true],
        "the canary request carried the field UNREPAIRED over the wire",
    );

    // The clear itself, read from the daemon's live router BEFORE the second request.
    //
    // Asserted here rather than only through the next request's bytes, and the
    // distinction is one mutation testing established: an unrepaired canary forwards
    // the field either way, so "request two carried the field" is ALSO true of a
    // build whose settlement produced no clear at all. Skipping the durable clear left
    // the byte assertion green. What only the clear can produce is an EMPTY acting
    // set on the live registry.
    let after_canary = daemon.live_router().fidelity_snapshot();
    assert!(
        after_canary.acting.is_empty(),
        "the disproof CLEARED the verdict on the daemon's live router: without the \
         clear the verdict is still acting, and the next request's bytes alone cannot \
         tell the two apart -- an unrepaired canary forwards the field either way. \
         Acting: {:?}",
        after_canary.acting,
    );

    // The observable consequence, over a second real request.
    let second = post_inference(&daemon, &body_with_field()).await;
    assert_eq!(second, reqwest::StatusCode::OK);
    assert_eq!(
        recorder.carried_per_attempt(),
        vec![true, true],
        "and with the verdict cleared the NEXT HTTP request forwards unchanged rather \
         than being rewritten",
    );
    // Read from the daemon's own live router: the clear is gone from the registry the
    // handlers serve from, not merely from a copy.
    assert!(
        daemon.live_router().fidelity_snapshot().acting.is_empty(),
        "the daemon's live router holds no acting verdict: the disproof cleared it",
    );
    daemon.shutdown().await;
}
