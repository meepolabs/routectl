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
async fn probe_rate_limit_on_half_open_count_releases_slot_without_debiting() {
    // A probe (max_tokens <= probe_max_tokens) that draws a 429 or 529 is
    // a transient upstream condition, not a provider fault, on every
    // surface: the half-open slot is freed and the breaker keeps its
    // armed zero-cooldown state rather than re-tripping Open. The count
    // still does not walk to a later capable seat.
    for status in [429, 529] {
        let (router, counters) = build_router(vec![
            Leg {
                nickname: "anthropic-only",
                provider_name: "anthropic-prov",
                entry: anthropic_api_entry_with_breaker(1, 60_000),
                behavior: CountBehavior::UpstreamError(status),
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
        assert!(
            router.force_open_breaker("anthropic-only", Duration::ZERO),
            "{status}: seat breaker slot must exist to arm half-open",
        );
        let req = ChatRequest {
            max_tokens: Some(1),
            ..count_req()
        };

        let err = router.count_tokens(req).await.unwrap_err();

        assert!(
            matches!(err, Error::Upstream { status: s, .. } if s == status),
            "{status}: the probe's status propagates verbatim; got {err:?}",
        );
        assert_eq!(counters[0].load(Ordering::SeqCst), 1, "{status}: one call");
        assert_eq!(
            counters[1].load(Ordering::SeqCst),
            0,
            "{status}: no walk to a later capable seat",
        );
        assert!(
            !half_open_in_flight(&router, "anthropic-only"),
            "{status}: the probe must free the half-open slot",
        );
        assert_eq!(
            circuit_phase(&router, "anthropic-only"),
            crate::runtime_state::CircuitPhase::HalfOpenReady,
            "{status}: a probe's rate limit must NOT debit the breaker",
        );
    }
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

/// One exit a count_tokens dispatch can take from a half-open slot it
/// claimed, and the leg that drives it there.
struct HalfOpenExitRow {
    name: &'static str,
    entry: ProviderEntry,
    behavior: CountBehavior,
    req: ChatRequest,
}

fn half_open_exit_rows() -> Vec<HalfOpenExitRow> {
    vec![
        HalfOpenExitRow {
            name: "debit (5xx, no reset hint)",
            entry: anthropic_api_entry_with_breaker(5, 60_000),
            behavior: CountBehavior::UpstreamError(503),
            req: count_req(),
        },
        HalfOpenExitRow {
            name: "park (5xx, large reset hint)",
            entry: anthropic_api_entry_with_breaker(5, 60_000),
            behavior: CountBehavior::UpstreamErrorWithReset(503, Duration::from_mins(1)),
            req: count_req(),
        },
        HalfOpenExitRow {
            name: "non-debiting 400",
            entry: anthropic_api_entry_with_breaker(5, 60_000),
            behavior: CountBehavior::UpstreamError(400),
            req: count_req(),
        },
        HalfOpenExitRow {
            name: "forwarded-credential terminal 401",
            entry: forwarded_anthropic_entry_with_breaker(5, 60_000),
            behavior: CountBehavior::UpstreamError(401),
            req: forwarded_count_req(),
        },
        HalfOpenExitRow {
            name: "401 whose auth refresh fails",
            entry: anthropic_api_entry_with_breaker(5, 60_000),
            behavior: CountBehavior::UnauthorizedRefreshFails,
            req: count_req(),
        },
    ]
}

#[tokio::test]
async fn every_terminal_exit_from_a_half_open_count_frees_the_probe_slot() {
    // A count_tokens dispatch admitted as the half-open probe must leave
    // the slot free on every terminal exit, or every later dispatch to
    // the seat (completions and streams included) sees CircuitOpen until
    // restart. The capability 501 exit is pinned by
    // `wire_501_on_half_open_probe_releases_slot_without_debiting_breaker`.
    for row in half_open_exit_rows() {
        let (router, counters) = build_router(vec![Leg {
            nickname: "anthropic-only",
            provider_name: "anthropic-prov",
            entry: row.entry,
            behavior: row.behavior,
            upstream: None,
        }]);
        assert!(
            router.force_open_breaker("anthropic-only", Duration::ZERO),
            "{}: seat breaker slot must exist to arm half-open",
            row.name,
        );

        let result = router.count_tokens(row.req).await;

        assert!(result.is_err(), "{}: the exit is terminal", row.name);
        assert_eq!(
            counters[0].load(Ordering::SeqCst),
            1,
            "{}: the half-open probe must reach the upstream exactly once",
            row.name,
        );
        assert!(
            !half_open_in_flight(&router, "anthropic-only"),
            "{}: the exit must free the half-open probe slot",
            row.name,
        );
    }
}

/// A count-capable provider whose call signals `entered`, then never
/// completes: only cancellation ends it.
struct PendingCountProvider {
    entered: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl Provider for PendingCountProvider {
    fn id(&self) -> &'static str {
        "anthropic-prov"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        unreachable!()
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        unreachable!()
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        unreachable!()
    }
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        self.entered.notify_one();
        std::future::pending::<()>().await;
        unreachable!("only cancellation ends this call")
    }
}

#[tokio::test]
async fn a_cancelled_half_open_count_frees_the_probe_slot() {
    // A count admitted as the half-open probe and then dropped mid-call
    // (client disconnect) must not strand the slot, or every later
    // dispatch to the seat sees CircuitOpen until restart.
    let entered = Arc::new(tokio::sync::Notify::new());
    let router = Arc::new(build_router_with(
        vec![Leg {
            nickname: "anthropic-only",
            provider_name: "anthropic-prov",
            entry: anthropic_api_entry_with_breaker(1, 60_000),
            behavior: CountBehavior::Ok(0),
            upstream: None,
        }],
        |_| {
            Arc::new(PendingCountProvider {
                entered: Arc::clone(&entered),
            })
        },
    ));
    assert!(
        router.force_open_breaker("anthropic-only", Duration::ZERO),
        "seat breaker slot must exist to arm half-open",
    );
    let worker = tokio::spawn({
        let router = Arc::clone(&router);
        async move { router.count_tokens(count_req()).await }
    });
    entered.notified().await;
    assert!(
        half_open_in_flight(&router, "anthropic-only"),
        "premise: the count holds the half-open slot while in flight",
    );

    worker.abort();
    let _ = worker.await;

    assert!(
        !half_open_in_flight(&router, "anthropic-only"),
        "the cancelled count must free the half-open slot",
    );
    let (refusal, guard) = router.admit_dispatch("anthropic-only", "anthropic-prov");
    assert!(
        refusal.is_none(),
        "a later request is admitted; got {refusal:?}"
    );
    assert!(guard.owns_slot(), "and it claims the freed probe slot");
}
