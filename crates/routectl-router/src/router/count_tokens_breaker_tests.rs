// Breaker-accounting group: what the walk records on the shared per-seat
// circuit breaker for each count_tokens outcome. The breaker also gates
// completions and streams, so a count-only signal recorded as health
// would starve real traffic. Imports and shared helpers live in the host
// `count_tokens_tests.rs`; do not add `use` lines here.

#[tokio::test]
async fn wire_501_on_half_open_probe_releases_slot_without_debiting_breaker() {
    // The incident pin. An admitted seat whose upstream cannot
    // count returns a WIRE 501. On a half-open count_tokens probe this
    // must be treated as a capability signal: release the probe slot
    // and leave the shared breaker un-debited. Recording it as a
    // health failure would re-trip the breaker (baseline cooldown) and
    // starve completions that gate on the same per-seat breaker.
    let (router, counters) = build_router(vec![Leg {
        nickname: "anthropic-only",
        provider_name: "anthropic-prov",
        entry: anthropic_api_entry_with_breaker(1, 60_000),
        behavior: CountBehavior::UpstreamError(501),
        upstream: None,
    }]);
    assert!(
        router.force_open_breaker("anthropic-only", Duration::ZERO),
        "seat breaker slot must exist to arm half-open",
    );

    let _ = router.count_tokens(count_req()).await;

    assert_eq!(
        counters[0].load(Ordering::SeqCst),
        1,
        "the half-open probe must reach the upstream exactly once",
    );
    assert!(
        !half_open_in_flight(&router, "anthropic-only"),
        "a capability wire-501 must release the half-open probe slot",
    );
    assert_eq!(
        circuit_phase(&router, "anthropic-only"),
        crate::runtime_state::CircuitPhase::HalfOpenReady,
        "a capability wire-501 must NOT debit the breaker: no record_failure, \
             so the breaker keeps its armed zero-cooldown state (HalfOpenReady) \
             rather than re-tripping Open with the 60s baseline",
    );
}

#[tokio::test]
async fn local_not_implemented_on_half_open_probe_releases_slot_without_debiting() {
    // Guards the already-exempt case: a local Error::NotImplemented
    // from the selected capable seat is a capability signal and must
    // behave exactly like the wire-501 -- release the half-open slot,
    // no breaker debit.
    let (router, counters) = build_router(vec![Leg {
        nickname: "anthropic-only",
        provider_name: "anthropic-prov",
        entry: anthropic_api_entry_with_breaker(1, 60_000),
        behavior: CountBehavior::NotImplemented,
        upstream: None,
    }]);
    assert!(
        router.force_open_breaker("anthropic-only", Duration::ZERO),
        "seat breaker slot must exist to arm half-open",
    );

    let _ = router.count_tokens(count_req()).await;

    assert_eq!(counters[0].load(Ordering::SeqCst), 1);
    assert!(
        !half_open_in_flight(&router, "anthropic-only"),
        "a capability NotImplemented must release the half-open probe slot",
    );
    assert_eq!(
        circuit_phase(&router, "anthropic-only"),
        crate::runtime_state::CircuitPhase::HalfOpenReady,
        "a capability NotImplemented must NOT debit the breaker",
    );
}

#[tokio::test]
async fn walks_to_next_capable_seat_on_wire_501_and_returns_its_count() {
    // Chain [anthropic-api(501), anthropic-api(ok)]. The selected
    // capable seat returns a capability wire-501; count_tokens must
    // advance to the NEXT capable seat and return its count -- not
    // surface the 501 to the client. The first seat's breaker must
    // NOT be debited.
    let (router, counters) = build_router(vec![
        Leg {
            nickname: "anthropic-first",
            provider_name: "anthropic-prov-a",
            entry: anthropic_api_entry_with_breaker(1, 60_000),
            behavior: CountBehavior::UpstreamError(501),
            upstream: None,
        },
        Leg {
            nickname: "anthropic-second",
            provider_name: "anthropic-prov-b",
            entry: anthropic_api_entry(),
            behavior: CountBehavior::Ok(42),
            upstream: None,
        },
    ]);

    let tc = router
        .count_tokens(count_req())
        .await
        .expect("walk must reach the second capable seat and return its count");

    assert_eq!(
        tc.input_tokens, 42,
        "the second capable seat serves the count",
    );
    assert_eq!(
        counters[0].load(Ordering::SeqCst),
        1,
        "first seat attempted once",
    );
    assert_eq!(
        counters[1].load(Ordering::SeqCst),
        1,
        "walk advanced to the second seat",
    );
    assert_eq!(
        circuit_phase(&router, "anthropic-first"),
        crate::runtime_state::CircuitPhase::Closed,
        "a capability 501 must not debit the first seat's breaker (stays Closed)",
    );
}

#[tokio::test]
async fn walk_terminates_with_not_implemented_when_all_capable_seats_501() {
    // Every capable seat returns a capability error. The walk must
    // visit each seat at most once (bounded upstream calls) and
    // terminate with the stable Error::NotImplemented rather than
    // looping or leaking the last upstream's raw 501 to the client.
    let (router, counters) = build_router(vec![
        Leg {
            nickname: "anthropic-first",
            provider_name: "anthropic-prov-a",
            entry: anthropic_api_entry(),
            behavior: CountBehavior::UpstreamError(501),
            upstream: None,
        },
        Leg {
            nickname: "anthropic-second",
            provider_name: "anthropic-prov-b",
            entry: anthropic_api_entry(),
            behavior: CountBehavior::UpstreamError(501),
            upstream: None,
        },
    ]);

    let err = router.count_tokens(count_req()).await.unwrap_err();

    match err {
        Error::NotImplemented(model, msg) => {
            assert_eq!(model, "alias");
            assert!(
                msg.contains("count_tokens"),
                "message must name the operation; got: {msg}",
            );
        }
        other => panic!("expected a terminal Error::NotImplemented; got {other:?}"),
    }
    assert_eq!(
        counters[0].load(Ordering::SeqCst),
        1,
        "first seat visited exactly once",
    );
    assert_eq!(
        counters[1].load(Ordering::SeqCst),
        1,
        "second seat visited exactly once (no re-visit, no loop)",
    );
}

#[tokio::test]
async fn non_capability_429_debits_and_returns_without_walking() {
    // Scope guard: a 429 is a HEALTH error, not a capability error. It
    // must keep today's behavior -- debit the breaker and propagate --
    // and must NOT walk to a later capable seat.
    let (router, counters) = build_router(vec![
        Leg {
            nickname: "anthropic-first",
            provider_name: "anthropic-prov-a",
            entry: anthropic_api_entry_with_breaker(1, 60_000),
            behavior: CountBehavior::UpstreamError(429),
            upstream: None,
        },
        Leg {
            nickname: "anthropic-second",
            provider_name: "anthropic-prov-b",
            entry: anthropic_api_entry(),
            behavior: CountBehavior::Ok(42),
            upstream: None,
        },
    ]);

    let err = router.count_tokens(count_req()).await.unwrap_err();

    assert!(
        matches!(err, Error::Upstream { status: 429, .. }),
        "a 429 must propagate verbatim; got {err:?}",
    );
    assert_eq!(
        counters[0].load(Ordering::SeqCst),
        1,
        "first seat attempted once",
    );
    assert_eq!(
        counters[1].load(Ordering::SeqCst),
        0,
        "a health error must NOT walk to a later capable seat",
    );
    assert_eq!(
        circuit_phase(&router, "anthropic-first"),
        crate::runtime_state::CircuitPhase::Open,
        "a 429 must debit the breaker (threshold 1 -> Open)",
    );
}

#[tokio::test]
async fn non_retryable_4xx_leaves_breaker_closed() {
    // A caller-shaped 4xx (BadRequest class) from a capable count_tokens
    // seat must NOT debit the per-seat breaker that also gates
    // completions and streams. The debit keys off the failure CLASS, so
    // a repeated 4xx storm here leaves the shared breaker CLOSED and
    // every dispatch keeps reaching the seat.
    let (router, counters) = build_router(vec![Leg {
        nickname: "anthropic-only",
        provider_name: "anthropic-prov",
        entry: anthropic_api_entry_with_breaker(2, 60_000),
        behavior: CountBehavior::UpstreamError(400),
        upstream: None,
    }]);

    for _ in 0..4 {
        let err = router.count_tokens(count_req()).await.unwrap_err();
        assert!(
            matches!(err, Error::Upstream { status: 400, .. }),
            "a count_tokens 4xx must surface verbatim; got {err:?}",
        );
    }

    assert_eq!(
        counters[0].load(Ordering::SeqCst),
        4,
        "a non-debiting 4xx must never trip the breaker, so every \
             dispatch reaches the capable seat",
    );
    assert_eq!(
        circuit_phase(&router, "anthropic-only"),
        crate::runtime_state::CircuitPhase::Closed,
        "a non-retryable 4xx storm must leave the count_tokens seat \
             breaker CLOSED (BadRequest class does not debit)",
    );
}

#[tokio::test]
async fn health_5xx_still_debits_breaker() {
    // Complement to the 4xx case: a 5xx (ServerError class) from a
    // capable count_tokens seat is a health failure and must still debit
    // and trip the shared per-seat breaker.
    let (router, counters) = build_router(vec![Leg {
        nickname: "anthropic-only",
        provider_name: "anthropic-prov",
        entry: anthropic_api_entry_with_breaker(1, 60_000),
        behavior: CountBehavior::UpstreamError(503),
        upstream: None,
    }]);

    let err = router.count_tokens(count_req()).await.unwrap_err();

    assert!(
        matches!(err, Error::Upstream { status: 503, .. }),
        "a count_tokens 5xx must surface verbatim; got {err:?}",
    );
    assert_eq!(counters[0].load(Ordering::SeqCst), 1);
    assert_eq!(
        circuit_phase(&router, "anthropic-only"),
        crate::runtime_state::CircuitPhase::Open,
        "a count_tokens 5xx (ServerError class) must debit and trip the \
             breaker (threshold 1 -> Open)",
    );
}

#[tokio::test]
async fn walk_reruns_gate_on_next_seat_and_respects_open_breaker() {
    // Guardrail: the capability walk must re-run the gate on each new
    // seat. If the next capable seat's breaker is open, the walk must
    // NOT bypass it -- the gate blocks the dispatch and the
    // circuit-open error surfaces (the seat is never called).
    let (router, counters) = build_router(vec![
        Leg {
            nickname: "anthropic-first",
            provider_name: "anthropic-prov-a",
            entry: anthropic_api_entry(),
            behavior: CountBehavior::UpstreamError(501),
            upstream: None,
        },
        Leg {
            nickname: "anthropic-second",
            provider_name: "anthropic-prov-b",
            entry: anthropic_api_entry(),
            behavior: CountBehavior::Ok(42),
            upstream: None,
        },
    ]);
    // Park the second seat's breaker open for a long, un-elapsed
    // cooldown so its gate returns CircuitOpen (not a half-open probe
    // admission).
    assert!(
        router.force_open_breaker("anthropic-second", Duration::from_hours(1)),
        "second seat breaker slot must exist",
    );

    let err = router.count_tokens(count_req()).await.unwrap_err();

    assert!(
        matches!(&err, Error::Upstream { status: 0, body, .. } if body.contains("circuit breaker")),
        "the walk must re-gate the second seat and surface its open-breaker block; got {err:?}",
    );
    assert_eq!(
        counters[0].load(Ordering::SeqCst),
        1,
        "first seat attempted once (capability 501)",
    );
    assert_eq!(
        counters[1].load(Ordering::SeqCst),
        0,
        "an open breaker on the walked-to seat must block the dispatch, not be bypassed",
    );
}
