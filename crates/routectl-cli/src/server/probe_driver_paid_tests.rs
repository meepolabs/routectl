// Driver-tick paid-probe coverage: the same production tick that exhausts a
// lane's free plan must dial the candidate it surfaced, and shutdown must
// cancel an in-flight PAID dial on the same terms as a free validator.
//
// An `include!` fragment of `reload_tests.rs` rather than its own `#[path]
// mod`: the module path segment comes from the `mod` NAME, so a second module
// would rename every test here. Imports live in the host file.

/// A provider whose `count_tokens` spends its one free step without
/// answering it (a well-formed zero), which is the shape that exhausts a
/// one-step free plan toward a paid candidate rather than settling or
/// retrying it, and whose `complete` counts how many times the paid half
/// actually dials it.
struct ZeroCountThenCountingComplete {
    complete_calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl routectl_core::Provider for ZeroCountThenCountingComplete {
    fn id(&self) -> &'static str {
        "p1"
    }
    fn normalize_request(
        &self,
        _: &routectl_core::ChatRequest,
    ) -> routectl_core::Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(
        &self,
        _: serde_json::Value,
    ) -> routectl_core::Result<routectl_core::ChatResponse> {
        Err(routectl_core::Error::normalize_response("p1", "unused"))
    }
    async fn complete(
        &self,
        _: routectl_core::ChatRequest,
    ) -> routectl_core::Result<routectl_core::ChatResponse> {
        self.complete_calls.fetch_add(1, Ordering::SeqCst);
        Ok(routectl_core::ChatResponse {
            model: "wire-model".to_string(),
            usage: Some(routectl_core::Usage::default()),
            ..Default::default()
        })
    }
    async fn stream(
        &self,
        _: routectl_core::ChatRequest,
    ) -> routectl_core::Result<
        futures::stream::BoxStream<'static, routectl_core::Result<routectl_core::ChatChunk>>,
    > {
        Err(routectl_core::Error::upstream("p1", 500, "body"))
    }
    async fn count_tokens(
        &self,
        _: routectl_core::ChatRequest,
    ) -> routectl_core::Result<routectl_core::TokenCount> {
        Ok(routectl_core::TokenCount {
            input_tokens: 0,
            extras: serde_json::Map::new(),
        })
    }
}

/// A probe router with a non-zero paid-probe daily cap and an installed
/// ledger, so an exhausted free plan's candidate can reach a real paid
/// dial -- the only way to observe, from outside `routectl-router`, that a
/// driver tick did more than the free half.
fn probe_router_paid_capable() -> (Router, Arc<AtomicUsize>) {
    let complete_calls = Arc::new(AtomicUsize::new(0));
    let mut config = routectl_router::config::Config::default();
    config.providers.insert(
        "p1".to_string(),
        routectl_router::config::ProviderEntry::anthropic_api("literal:k"),
    );
    config.models.insert(
        "m1".to_string(),
        routectl_router::config::ModelEntry::new("p1", "claude-sonnet-4-5"),
    );
    config.aliases.insert(
        "default".to_string(),
        routectl_router::config::AliasValue::Single("m1".to_string()),
    );
    config
        .fidelity
        .paid_probe_daily_caps
        .insert("p1".to_string(), 5);
    let router = Router::new(Arc::new(config))
        .with_paid_probe_ledger(std::sync::Arc::new(AlwaysCommitsLedger));
    let mut models = std::collections::BTreeMap::new();
    models.insert(
        "m1".to_string(),
        Arc::new(
            routectl_router::ResolvedModel::new(
                "m1",
                "p1",
                Arc::new(ZeroCountThenCountingComplete {
                    complete_calls: Arc::clone(&complete_calls),
                }) as Arc<dyn routectl_core::Provider>,
                "claude-sonnet-4-5",
            )
            .with_effective_row(paid_capable_effective_row()),
        ),
    );
    let mut router = router;
    router.install_resolved_models(models);
    (router, complete_calls)
}

/// A ledger double that always commits, so a claimed candidate can reach the
/// upstream dial rather than being refused for want of a ledger.
struct AlwaysCommitsLedger;

#[async_trait::async_trait]
impl routectl_router::PaidProbeLedger for AlwaysCommitsLedger {
    async fn reserve_paid_probe_unit(
        &self,
        _provider: &str,
        daily_cap: u32,
    ) -> routectl_router::PaidProbeReservation {
        routectl_router::PaidProbeReservation::Committed {
            used: 1,
            cap: daily_cap,
        }
    }
}

/// A fully-priced, viable effective row -- the only shape the paid-probe
/// profile lookup accepts as a candidate for a real dial.
///
/// The per-token rate fields are crate-visible on `CatalogRow`, so this
/// crate reaches them through the same public override merge an operator's
/// `[cache_pricing]` entry uses, rather than a struct literal.
fn paid_capable_effective_row() -> routectl_router::EffectiveRow {
    let row = routectl_router::CatalogRow::sentinel()
        .with_overrides(&routectl_router::CachePricingOverride {
            input_cost_per_token: Some(3.0e-6),
            output_cost_per_token: Some(1.5e-5),
            max_output_tokens: Some(64_000),
            ..Default::default()
        })
        .expect("a well-formed priced override merges cleanly");
    routectl_router::EffectiveRow::Present {
        row,
        source: routectl_router::Source::Baked,
        verified_at: "2026-01-01".to_string(),
    }
}

/// A provider that exhausts its one free step the same way
/// [`ZeroCountThenCountingComplete`] does, but whose `complete` never
/// answers once the free client call is behind it -- so the PAID dial, not
/// the free validator, is what is in flight when shutdown arrives.
struct ZeroCountThenStallingComplete {
    live_paid_dials: Arc<AtomicUsize>,
    client_call_done: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl routectl_core::Provider for ZeroCountThenStallingComplete {
    fn id(&self) -> &'static str {
        "p1"
    }
    fn normalize_request(
        &self,
        _: &routectl_core::ChatRequest,
    ) -> routectl_core::Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(
        &self,
        _: serde_json::Value,
    ) -> routectl_core::Result<routectl_core::ChatResponse> {
        Err(routectl_core::Error::normalize_response("p1", "unused"))
    }
    async fn complete(
        &self,
        _: routectl_core::ChatRequest,
    ) -> routectl_core::Result<routectl_core::ChatResponse> {
        if !self.client_call_done.swap(true, Ordering::SeqCst) {
            return Ok(routectl_core::ChatResponse {
                model: "wire-model".to_string(),
                usage: Some(routectl_core::Usage::default()),
                ..Default::default()
            });
        }
        struct Live(Arc<AtomicUsize>);
        impl Drop for Live {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        self.live_paid_dials.fetch_add(1, Ordering::SeqCst);
        let _live = Live(Arc::clone(&self.live_paid_dials));
        // Far past every bound: only cancellation ends this.
        tokio::time::sleep(std::time::Duration::from_hours(1)).await;
        Ok(routectl_core::ChatResponse {
            model: "wire-model".to_string(),
            ..Default::default()
        })
    }
    async fn stream(
        &self,
        _: routectl_core::ChatRequest,
    ) -> routectl_core::Result<
        futures::stream::BoxStream<'static, routectl_core::Result<routectl_core::ChatChunk>>,
    > {
        Err(routectl_core::Error::upstream("p1", 500, "body"))
    }
    async fn count_tokens(
        &self,
        _: routectl_core::ChatRequest,
    ) -> routectl_core::Result<routectl_core::TokenCount> {
        Ok(routectl_core::TokenCount {
            input_tokens: 0,
            extras: serde_json::Map::new(),
        })
    }
}

/// The same paid-capable router, with the stalling dial installed.
fn stalling_paid_dial_router() -> (Router, Arc<AtomicUsize>) {
    let live_paid_dials = Arc::new(AtomicUsize::new(0));
    let mut config = routectl_router::config::Config::default();
    config.providers.insert(
        "p1".to_string(),
        routectl_router::config::ProviderEntry::anthropic_api("literal:k"),
    );
    config.models.insert(
        "m1".to_string(),
        routectl_router::config::ModelEntry::new("p1", "claude-sonnet-4-5"),
    );
    config.aliases.insert(
        "default".to_string(),
        routectl_router::config::AliasValue::Single("m1".to_string()),
    );
    config
        .fidelity
        .paid_probe_daily_caps
        .insert("p1".to_string(), 5);
    let mut router = Router::new(Arc::new(config))
        .with_paid_probe_ledger(std::sync::Arc::new(AlwaysCommitsLedger));
    let mut models = std::collections::BTreeMap::new();
    models.insert(
        "m1".to_string(),
        Arc::new(
            routectl_router::ResolvedModel::new(
                "m1",
                "p1",
                Arc::new(ZeroCountThenStallingComplete {
                    live_paid_dials: Arc::clone(&live_paid_dials),
                    client_call_done: Arc::new(AtomicBool::new(false)),
                }) as Arc<dyn routectl_core::Provider>,
                "claude-sonnet-4-5",
            )
            .with_effective_row(paid_capable_effective_row()),
        ),
    );
    router.install_resolved_models(models);
    (router, live_paid_dials)
}

/// Shutdown must cancel an in-flight PAID dial on the same terms as a free
/// validator: the dial future is dropped rather than awaited, its slot is
/// released, and nothing is queued for a retry.
///
/// The free half cannot stand in for this: it settles instantly here, so the
/// only thing holding the pass when shutdown fires is the paid dial. A
/// driver that awaited the pass before reading shutdown would hold the drain
/// for the whole stall and never return inside this bound.
#[tokio::test(start_paused = true)]
async fn probe_driver_cancels_an_in_flight_paid_dial_at_shutdown_without_retrying_it() {
    let (router, live_paid_dials) = stalling_paid_dial_router();
    let router_swap = Arc::new(ArcSwap::from_pointee(router));
    let _ = router_swap.load().complete(probe_grounding_request()).await;

    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let driver = tokio::spawn(run_probe_driver(router_swap.clone(), shutdown_rx));

    // Let the first tick run the free step to exhaustion and get the paid
    // dial it surfaced under way.
    tokio::time::sleep(PROBE_DRIVER_INTERVAL + std::time::Duration::from_secs(1)).await;
    assert_eq!(
        router_swap
            .load()
            .probe_scheduler_snapshot()
            .free_exhausted_total,
        1,
        "premise: the free plan must exhaust on the tick that runs it"
    );
    assert_eq!(
        live_paid_dials.load(Ordering::SeqCst),
        1,
        "premise: a paid dial must be in flight when shutdown fires"
    );

    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(crate::server::serve::DRAIN_DEADLINE, driver)
        .await
        .expect("the driver must return within the server's wait bound")
        .expect("the driver task must not panic");

    assert_eq!(
        live_paid_dials.load(Ordering::SeqCst),
        0,
        "the in-flight paid dial must be dropped, releasing its slot"
    );
    assert_eq!(
        router_swap.load().probe_scheduler_snapshot().queued,
        0,
        "a cancelled paid dial must leave nothing queued to retry"
    );
    // THE IRREVERSIBLE MILESTONES SURVIVE THE CANCELLATION. Each was recorded
    // where it happened rather than at a settlement the dropped pass never
    // reached, so a shutdown mid-call still reports the candidate off the list,
    // the unit spent, and the request possibly delivered.
    let snapshot = router_swap.load().probe_scheduler_snapshot();
    assert_eq!(
        snapshot.paid_candidate_attempts_total, 1,
        "the claim is irreversible and must stay counted"
    );
    assert_eq!(
        snapshot.paid_reservations_committed_total, 1,
        "the committed unit is spent, with no refund, and must stay counted"
    );
    assert_eq!(
        snapshot.paid_provider_calls_started_total, 1,
        "the upstream may have received the request, so the call has started"
    );
    assert_eq!(
        snapshot.paid_completed_total
            + snapshot.paid_provider_failed_total
            + snapshot.paid_timeouts_total
            + snapshot.paid_gate_deferrals_total
            + snapshot.paid_authorization_refusals_total,
        0,
        "no terminal outcome may be counted: the pass never settled"
    );
}

/// THE driver-tick contract: a candidate an admitted request's free step
/// surfaces THIS tick must be claimable and dialed by the SAME production
/// tick, not merely leave the free validator running.
///
/// A driver that ran only the free half would exhaust the plan, queue the
/// candidate, and never ask the paid half, so `complete` would never be
/// reached after the client call -- which is what the assertion below reads.
#[tokio::test(start_paused = true)]
async fn probe_driver_dials_a_paid_candidate_the_same_tick_its_free_step_surfaced_it() {
    let (router, complete_calls) = probe_router_paid_capable();
    let router_swap = Arc::new(ArcSwap::from_pointee(router));

    let _ = router_swap.load().complete(probe_grounding_request()).await;
    assert_eq!(
        router_swap.load().probe_scheduler_snapshot().queued,
        1,
        "premise: the admitted request must have queued exactly one free job"
    );
    // The admitted dispatch above already reached `complete` once as the
    // client-facing call the probe lane rode in on; only a call AFTER this
    // point can be the driver's own paid dial.
    let calls_before_driver = complete_calls.load(Ordering::SeqCst);

    let (_shutdown_tx, shutdown_rx) = watch::channel(());
    let _ = tokio::time::timeout(
        PROBE_DRIVER_INTERVAL * 2,
        run_probe_driver(router_swap.clone(), shutdown_rx),
    )
    .await;

    assert_eq!(
        router_swap
            .load()
            .probe_scheduler_snapshot()
            .free_exhausted_total,
        1,
        "premise: the one-step free plan must exhaust on the tick that runs it"
    );
    assert!(
        complete_calls.load(Ordering::SeqCst) > calls_before_driver,
        "the same tick that exhausted the free plan must dial the paid \
         candidate it surfaced, not merely leave it queued"
    );
}
