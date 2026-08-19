use super::*;
use crate::config::{ProviderEntry, RetryPolicy};
use routectl_core::capability::EvidenceSource;
use std::collections::BTreeMap;

/// An otherwise-empty overlay stamped at `revision`, for the carry-over
/// tests that turn on a revision CHANGE and not on any cell content.
fn overlay_at_revision(revision: u64) -> Arc<crate::catalog_overlay::CatalogOverlay> {
    Arc::new(crate::catalog_overlay::CatalogOverlay {
        revision,
        ..Default::default()
    })
}

/// Build a Router with one openai-compat provider that has the given
/// runtime-policy timeouts, and an alias chain of length 1 pointing
/// at it. The base RetryPolicy passed to compose_attempt_policy
/// represents what `policy_for(alias)` resolved to.
fn build_router_with_provider_timeouts(
    provider_request_timeout: Option<u64>,
    provider_first_byte_timeout: Option<u64>,
) -> Router {
    let mut providers = BTreeMap::new();
    let mut entry = ProviderEntry::openai_compat("https://example.test/v1", "literal:k");
    if let ProviderEntry::OpenaiCompat { runtime, .. } = &mut entry {
        runtime.request_timeout_ms = provider_request_timeout;
        runtime.stream_first_byte_timeout_ms = provider_first_byte_timeout;
    }
    providers.insert("p1".to_string(), entry);

    let cfg = Config {
        providers,
        ..Default::default()
    };
    Router::new(Arc::new(cfg))
}

#[test]
fn compose_inherits_timeout_from_provider_when_alias_left_none() {
    // Alias-resolved policy has no timeout overrides.
    // Provider config sets both timeouts.
    // Expected: provider's values land in the per-attempt policy.
    let router = build_router_with_provider_timeouts(Some(180_000), Some(60_000));
    let base = RetryPolicy {
        stream_first_byte_timeout_ms: None, // alias left this unset
        ..RetryPolicy::default()
    };
    let composed = router.compose_attempt_policy(&base, "p1", None);
    assert_eq!(composed.request_timeout_ms, Some(180_000));
    assert_eq!(composed.stream_first_byte_timeout_ms, Some(60_000));
}

#[test]
fn compose_alias_override_wins_over_provider() {
    // Alias-resolved policy has BOTH timeouts set explicitly.
    // Provider config also sets values. Alias wins.
    let router = build_router_with_provider_timeouts(Some(180_000), Some(60_000));
    let base = RetryPolicy {
        request_timeout_ms: Some(30_000),
        stream_first_byte_timeout_ms: Some(5_000),
        ..RetryPolicy::default()
    };
    let composed = router.compose_attempt_policy(&base, "p1", None);
    assert_eq!(composed.request_timeout_ms, Some(30_000));
    assert_eq!(composed.stream_first_byte_timeout_ms, Some(5_000));
}

#[test]
fn compose_independent_per_field_resolution() {
    // Alias sets ONLY request_timeout_ms; provider sets ONLY
    // stream_first_byte_timeout_ms. Expected: each field falls
    // through independently.
    let router = build_router_with_provider_timeouts(None, Some(120_000));
    let base = RetryPolicy {
        request_timeout_ms: Some(45_000),
        stream_first_byte_timeout_ms: None, // alias left this unset
        ..RetryPolicy::default()
    };
    let composed = router.compose_attempt_policy(&base, "p1", None);
    assert_eq!(composed.request_timeout_ms, Some(45_000));
    assert_eq!(composed.stream_first_byte_timeout_ms, Some(120_000));
}

#[test]
fn compose_no_provider_entry_passes_base_through_unchanged() {
    // If the chain entry's provider isn't in config (e.g. test
    // harness that registered a Provider without adding a config
    // ProviderEntry), provider-level lookup returns None and the
    // base policy survives unchanged.
    let router = build_router_with_provider_timeouts(Some(99_999), Some(99_999));
    let base = RetryPolicy {
        request_timeout_ms: Some(7_000),
        stream_first_byte_timeout_ms: None, // alias left this unset
        ..RetryPolicy::default()
    };
    let composed = router.compose_attempt_policy(&base, "missing-provider", None);
    assert_eq!(composed.request_timeout_ms, Some(7_000));
    assert!(composed.stream_first_byte_timeout_ms.is_none());
}

#[test]
fn compose_no_overrides_anywhere_yields_none() {
    // Belt-and-braces: alias = None, provider = None, default
    // policy = None. composed.request_timeout_ms stays None
    // (router falls through to reqwest's default).
    let router = build_router_with_provider_timeouts(None, None);
    let base = RetryPolicy {
        stream_first_byte_timeout_ms: None, // alias left this unset
        ..RetryPolicy::default()
    };
    let composed = router.compose_attempt_policy(&base, "p1", None);
    assert!(composed.request_timeout_ms.is_none());
    assert!(composed.stream_first_byte_timeout_ms.is_none());
}

#[test]
fn compose_model_first_byte_timeout_wins_over_provider_and_global() {
    // Per-model > per-provider > global. The per-model override
    // pins 5000 even though global is 90000 and provider is 60000.
    let router = build_router_with_provider_timeouts(None, Some(60_000));
    let base = RetryPolicy {
        stream_first_byte_timeout_ms: Some(90_000),
        ..RetryPolicy::default()
    };
    let composed = router.compose_attempt_policy(&base, "p1", Some(5_000));
    assert_eq!(composed.stream_first_byte_timeout_ms, Some(5_000));
}

#[test]
fn compose_model_first_byte_timeout_none_falls_back_to_provider_resolution() {
    // No per-model override -> provider + global path resolves
    // exactly as before. With base unset, the provider's value wins.
    let router = build_router_with_provider_timeouts(None, Some(60_000));
    let base = RetryPolicy {
        stream_first_byte_timeout_ms: None, // alias left this unset
        ..RetryPolicy::default()
    };
    let composed = router.compose_attempt_policy(&base, "p1", None);
    assert_eq!(composed.stream_first_byte_timeout_ms, Some(60_000));
}

#[test]
fn compose_model_first_byte_timeout_wins_over_base_too() {
    // Per-model override beats base (global) even when base is set.
    // Pins the per-model > global precedence regardless of provider state.
    let router = build_router_with_provider_timeouts(None, None);
    let base = RetryPolicy {
        stream_first_byte_timeout_ms: Some(45_000),
        ..RetryPolicy::default()
    };
    let composed = router.compose_attempt_policy(&base, "p1", Some(10_000));
    assert_eq!(composed.stream_first_byte_timeout_ms, Some(10_000));
}

#[test]
fn should_fallback_status_zero_is_always_true() {
    // status 0 == network error (DNS, TCP, TLS, request body,
    // request timeout). `should_fallback` returns true for the
    // network-error class default; the predicate governs HTTP-status
    // outcomes (>= 400) via per-class policy, and a status-0 network
    // error resolves through the NetworkError class, which falls back
    // by default.
    let err = Error::upstream("p", 0, "tcp connect refused");
    let class = classify(&err, None).class;
    let policy = RetryPolicy::default();
    assert!(should_fallback(&err, &class, &policy, false));
}

// --- Per-class operator overrides route through `resolved_class` ---

#[test]
fn retry_override_on_one_class_leaves_the_sibling_5xx_class_untouched() {
    // Arrange: [retry.classes.overloaded] retry = 0, with a distinct
    // baked retry_on_5xx cap so a leak into ServerError is visible.
    use crate::class_policy::{ClassPolicy, ConfigFailureClass};
    let mut classes = std::collections::BTreeMap::new();
    classes.insert(
        ConfigFailureClass::Overloaded,
        ClassPolicy {
            retry: Some(0),
            fallback: None,
        },
    );
    let policy = RetryPolicy {
        retry_on_5xx: Some(3),
        classes,
        ..RetryPolicy::default()
    };
    let overloaded_err = Error::upstream_full(
        "p",
        503,
        "body",
        None,
        Some("overloaded_error".into()),
        None,
    );
    let server_err = Error::upstream("p", 500, "body");
    let overloaded_class = classify(&overloaded_err, None).class;
    let server_class = classify(&server_err, None).class;

    // Act + Assert: the overridden class caps to 0 and cannot retry.
    assert_eq!(retry_cap_for(&overloaded_class, &policy), 0);
    assert!(!should_retry_same_provider(
        &overloaded_err,
        &overloaded_class,
        &policy,
        0,
        false,
    ));
    // The un-overridden sibling class in the same baked 5xx family
    // keeps its own retry_on_5xx cap.
    assert_eq!(retry_cap_for(&server_class, &policy), 3);
    assert!(should_retry_same_provider(
        &server_err,
        &server_class,
        &policy,
        0,
        false,
    ));
    // Fallback is untouched for both -- only the retry leaf was
    // overridden.
    assert!(should_fallback(
        &overloaded_err,
        &overloaded_class,
        &policy,
        false
    ));
    assert!(should_fallback(&server_err, &server_class, &policy, false));
}

#[test]
fn hard_retry_cap_folds_per_class_overlay_above_max_attempts() {
    // A per-class retry override above max_attempts must lift the hard
    // ceiling too, or the retry loop's hard-cap guard silently clips
    // the class cap the resolver honors.
    use crate::class_policy::{ClassPolicy, ConfigFailureClass};
    let mut classes = BTreeMap::new();
    classes.insert(
        ConfigFailureClass::ServerError,
        ClassPolicy {
            retry: Some(5),
            fallback: None,
        },
    );
    let policy = RetryPolicy {
        max_attempts: 2,
        classes,
        ..RetryPolicy::default()
    };

    assert_eq!(policy.hard_retry_cap(), 5);

    let server_err = Error::upstream("p", 500, "body");
    let server_class = classify(&server_err, None).class;
    assert_eq!(retry_cap_for(&server_class, &policy), 5);
    assert!(
        policy.hard_retry_cap() >= retry_cap_for(&server_class, &policy),
        "hard cap must never sit below an enforced class cap"
    );
}

#[test]
fn emit_class_observability_logs_enforced_and_hard_retry_cap() {
    // The emitted class-decision event must carry the SAME retry_cap
    // the shared resolver enforces, plus hard_retry_cap, so logging can
    // never drift from enforcement.
    use crate::class_policy::{ClassPolicy, ConfigFailureClass};

    struct StubProvider;
    #[async_trait::async_trait]
    impl Provider for StubProvider {
        fn id(&self) -> &'static str {
            "stub"
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response("stub", "unused"))
        }
        async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
            unreachable!()
        }
        async fn stream(
            &self,
            _: ChatRequest,
        ) -> Result<futures::stream::BoxStream<'static, Result<ChatChunk>>> {
            unreachable!()
        }
    }

    let router = build_router_with_provider_timeouts(None, None);
    let provider: Arc<dyn Provider> = Arc::new(StubProvider);
    let model = Arc::new(ResolvedModel::new("nick", "p1", provider, "upstream"));
    let target = into_one_dispatch_target(model);

    let mut classes = BTreeMap::new();
    classes.insert(
        ConfigFailureClass::ServerError,
        ClassPolicy {
            retry: Some(5),
            fallback: None,
        },
    );
    let policy = RetryPolicy {
        max_attempts: 2,
        classes,
        ..RetryPolicy::default()
    };

    let err = Error::upstream("p1", 500, "body");
    let cf = classify(&err, None);
    let expected_retry = retry_cap_for(&cf.class, &policy);
    let expected_hard = policy.hard_retry_cap();

    let events = routectl_testkit::capture_events(|| {
        router.emit_class_observability(
            &err,
            &cf,
            &cf.class,
            false,
            None,
            DispatchSurface::Complete,
            "p1",
            &target,
            false,
            &policy,
            false,
            false,
            false,
        );
    });

    let decision = events
        .iter()
        .find(|e| e.message == "router failure class decision")
        .expect("one class-decision event emitted");
    let retry_str = expected_retry.to_string();
    let hard_str = expected_hard.to_string();
    assert_eq!(decision.field("retry_cap"), Some(retry_str.as_str()));
    assert_eq!(decision.field("hard_retry_cap"), Some(hard_str.as_str()));
    assert_eq!(
        expected_retry, 5,
        "class overlay must lift the enforced cap"
    );
    assert!(
        expected_hard >= expected_retry,
        "emitted hard cap must never sit below the enforced retry cap"
    );
}

#[test]
fn fallback_override_on_bad_request_leaves_auth_untouched() {
    // Arrange: [retry.classes.bad-request] fallback = false.
    use crate::class_policy::{ClassPolicy, ConfigFailureClass};
    let mut classes = std::collections::BTreeMap::new();
    classes.insert(
        ConfigFailureClass::BadRequest,
        ClassPolicy {
            retry: None,
            fallback: Some(false),
        },
    );
    let policy = RetryPolicy {
        classes,
        ..RetryPolicy::default()
    };
    let bad_request_err = Error::upstream("p", 400, "body");
    let auth_err = Error::upstream("p", 401, "body");
    let bad_request_class = classify(&bad_request_err, None).class;
    let auth_class = classify(&auth_err, None).class;

    // Act + Assert: a plain 400 stops falling back...
    assert!(!should_fallback(
        &bad_request_err,
        &bad_request_class,
        &policy,
        false,
    ));
    // ...but a 401 (different class, no overlay entry) still does.
    assert!(should_fallback(&auth_err, &auth_class, &policy, false));
}

#[test]
fn unknown_provider_falls_back_regardless_of_class_or_override() {
    // Regression: `Error::UnknownProvider` short-circuits to true
    // BEFORE the class match, independent of both the class passed in
    // (classify() never sees an UnknownProvider, so this pins the
    // caller can pass any class here) and any per-class override that
    // would otherwise deny fallback for that class.
    use crate::class_policy::{ClassPolicy, ConfigFailureClass};
    let err = Error::UnknownProvider("missing-provider".to_string());
    let mut classes = std::collections::BTreeMap::new();
    classes.insert(
        ConfigFailureClass::BadRequest,
        ClassPolicy {
            retry: None,
            fallback: Some(false),
        },
    );
    let policy = RetryPolicy {
        classes,
        ..RetryPolicy::default()
    };
    assert!(should_fallback(
        &err,
        &FailureClass::BadRequest,
        &policy,
        false,
    ));
}

#[test]
fn carry_over_runtime_state_from_preserves_existing_state_arcs() {
    // Arrange: build two fresh Routers; insert named state entries
    // directly to simulate pre-loaded model nicknames without requiring
    // real Provider impls.
    use crate::config::ProviderRuntimePolicy;
    use crate::runtime_state::ProviderState;

    let config = Arc::new(Config::default());
    let policy = ProviderRuntimePolicy::default();

    let mut old = Router::new(config.clone());
    // "model-a" exists in both routers -- state must be carried over.
    let old_arc = Arc::new(Mutex::new(ProviderState::new(&policy)));
    old.state.insert("model-a".to_string(), old_arc.clone());
    // "model-x" exists only in the old router -- must NOT be injected.
    let old_only_arc = Arc::new(Mutex::new(ProviderState::new(&policy)));
    old.state.insert("model-x".to_string(), old_only_arc);

    let mut new = Router::new(config);
    let fresh_arc = Arc::new(Mutex::new(ProviderState::new(&policy)));
    new.state.insert("model-a".to_string(), fresh_arc);
    // "model-new" exists only in the new router -- must remain unchanged.
    let new_only_arc = Arc::new(Mutex::new(ProviderState::new(&policy)));
    new.state
        .insert("model-new".to_string(), new_only_arc.clone());

    // Act
    new.carry_over_runtime_state_from(&old);

    // Assert: "model-a" holds the old Arc, not the fresh one.
    let after_a = new.state.get("model-a").cloned().unwrap();
    assert!(
        Arc::ptr_eq(&after_a, &old_arc),
        "carry_over must reuse the old Arc for nicknames present in both routers",
    );

    // Assert: "model-new" (new-only) is unchanged.
    let after_new = new.state.get("model-new").cloned().unwrap();
    assert!(
        Arc::ptr_eq(&after_new, &new_only_arc),
        "carry_over must not replace entries absent from the old router",
    );

    // Assert: "model-x" (old-only) is NOT injected into the new router.
    assert!(
        !new.state.contains_key("model-x"),
        "carry_over must not inject old-only nicknames into the new router",
    );
}

#[test]
fn carry_over_sticky_from_preserves_pins() {
    // Regression guard for the silent-collapse trap: a hot-reload must
    // NOT drop StickyLeastLoaded pins, or every live conversation cold-
    // misses its warm-cache seat.

    // Arrange: pin a session in the outgoing Router, with the one-time
    // overflow marker set so we can prove it survives the rebuild.
    let config = Arc::new(Config::default());
    let before = Router::new(config.clone());
    before.sticky_pins.put(
        "sess-1",
        crate::seat_pool::SeatPin {
            state_key: "opus#seat-b".into(),
            repinned: true,
        },
    );

    let mut after = Router::new(config);

    // Act
    after.carry_over_sticky_from(&before);

    // Assert: the pin survived the rebuild, INCLUDING the repinned flag --
    // otherwise a reload would reset the one-time cap and re-open the flap
    // window.
    assert_eq!(
        after.sticky_pins.get("sess-1"),
        Some(crate::seat_pool::SeatPin {
            state_key: "opus#seat-b".to_string(),
            repinned: true,
        }),
        "carry_over_sticky_from must preserve session->seat pins (with the \
             repinned flag) across a rebuild",
    );
}

#[test]
fn carry_over_sticky_from_shares_the_pins_arc() {
    // Regression guard for the silent-collapse trap, sticky-pin edition: the
    // carry-over shares the outgoing Router's `StickyPins` Arc rather than
    // snapshotting it, mirroring `carry_over_k_store_from`.
    let config = Arc::new(Config::default());
    let before = Router::new(config.clone());
    let mut after = Router::new(config);

    // Act
    after.carry_over_sticky_from(&before);

    // Assert: the map handle itself is shared, not copied.
    assert!(
        Arc::ptr_eq(&after.sticky_pins, &before.sticky_pins),
        "carry_over_sticky_from must share the StickyPins Arc, not snapshot it",
    );
}

#[test]
fn carry_over_sticky_from_makes_a_swap_window_pin_visible() {
    // Regression guard for the snapshot-copy race: a request that mints or
    // migrates a pin AFTER `carry_over_sticky_from` runs but BEFORE the new
    // Router is published still holds a reference to the OUTGOING Router.
    // Under a copy-based carry-over that late pin lands only in the map the
    // swap discards; under a shared map it lands in the same map the new
    // Router reads.
    let config = Arc::new(Config::default());
    let before = Router::new(config.clone());
    let mut after = Router::new(config);

    // Act: carry over first, exactly as the hot-reload coordinator does
    // before publishing the new Router...
    after.carry_over_sticky_from(&before);

    // ...then a request still in flight against the OUTGOING router mints a
    // pin after the carry-over ran.
    before.sticky_pins.put(
        "late-sess",
        crate::seat_pool::SeatPin {
            state_key: "opus#seat-b".into(),
            repinned: false,
        },
    );

    // Assert: the shared map already reflects the late pin.
    assert_eq!(
        after.sticky_pins.get("late-sess"),
        Some(crate::seat_pool::SeatPin {
            state_key: "opus#seat-b".to_string(),
            repinned: false,
        }),
        "a pin written on the outgoing router after carry-over must land in \
             the map the new router reads",
    );
}

#[test]
fn carry_over_k_store_from_shares_the_store_arc() {
    // Regression guard for the silent-collapse trap, K-store edition: a
    // hot-reload must NOT drop per-session K windows. The carry-over shares
    // the outgoing Router's store Arc rather than snapshotting it (see
    // `carry_over_k_store_from`'s doc comment for why); LRU-order
    // preservation under Arc-sharing is covered at the store level
    // (`k_session_store_export_returns_lru_order` in `k_estimator::store`).
    let config = Arc::new(Config::default());
    let before = Router::new(config.clone());
    let mut after = Router::new(config);

    // Act
    after.carry_over_k_store_from(&before);

    // Assert: the store handle itself is shared, not copied.
    assert!(
        Arc::ptr_eq(&after.k_session_store, &before.k_session_store),
        "carry_over_k_store_from must share the store Arc, not snapshot it",
    );
}

#[test]
fn carry_over_calibration_from_preserves_a_learned_factor() {
    // Regression guard for the silent-collapse trap, calibration edition: a
    // hot-reload must NOT drop a lane's learned correction. It is the worst
    // instance of the trap, because a wiped lane falls back to the
    // uncorrected estimate -- which is the pre-correction behavior, so the
    // loss reads as health rather than as breakage.
    use std::time::SystemTime;

    let kind = "openai-compat";
    let nickname = "opus";
    let now = SystemTime::now();

    // Arrange: enough balanced evidence for one lane to produce a factor.
    let config = Arc::new(Config::default());
    let before = Router::new(config.clone());
    for i in 0..9 {
        before.record_calibration_sample(
            Some(kind),
            Some(nickname),
            Some(&format!("caller-{}", i % 3)),
            10_000,
            13_000,
            now,
        );
    }
    let key = crate::calibration::LaneKey {
        provider_kind: kind.to_string(),
        nickname: nickname.to_string(),
    };
    let learned = before
        .calibration_store
        .factor_for(&key, now)
        .expect("the fed evidence clears the reduction's floors");

    let mut after = router_serving_nicknames(&config, &[nickname]);
    assert_eq!(
        after.calibration_store.factor_for(&key, now),
        None,
        "a freshly built router starts with no learned lanes",
    );

    // Act
    after.carry_over_calibration_from(&before);

    // Assert: the SAME factor survives, not merely some factor.
    assert_eq!(after.calibration_store.factor_for(&key, now), Some(learned));
    assert_eq!(after.calibration_store.export_entries().len(), 1);
}

#[test]
fn carry_over_calibration_from_drops_lanes_the_new_config_no_longer_serves() {
    // The carry-over contract flipped from import-time filtering to
    // Arc-sharing plus an active prune: a retired lane's SAMPLES may still
    // sit in the shared map for one bounded window (see the store's own
    // doc), but it must never again be OBSERVABLE through the current
    // Router -- `factor_for` must read it as unseen, exactly as it would a
    // lane that never existed. Only the lane whose nickname the new
    // resolved table still holds may produce a factor.
    use std::time::SystemTime;

    let kind = "openai-compat";
    let now = SystemTime::now();

    let config = Arc::new(Config::default());
    let before = Router::new(config.clone());
    for nickname in ["kept", "retired"] {
        for i in 0..9 {
            before.record_calibration_sample(
                Some(kind),
                Some(nickname),
                Some(&format!("caller-{}", i % 3)),
                10_000,
                13_000,
                now,
            );
        }
    }
    assert_eq!(before.calibration_store.len(), 2);

    let kept_key = crate::calibration::LaneKey {
        provider_kind: kind.to_string(),
        nickname: "kept".to_string(),
    };
    let retired_key = crate::calibration::LaneKey {
        provider_kind: kind.to_string(),
        nickname: "retired".to_string(),
    };

    // Act: the replacement router serves only `kept`.
    let mut after = router_serving_nicknames(&config, &["kept"]);
    after.carry_over_calibration_from(&before);

    // Assert: the kept lane's learned factor survives...
    assert!(
        after.calibration_store.factor_for(&kept_key, now).is_some(),
        "a lane the new config still serves must remain observable",
    );
    // ...and the retired lane is unobservable through the current Router,
    // exactly as an unseen lane would read.
    assert_eq!(
        after.calibration_store.factor_for(&retired_key, now),
        None,
        "a lane the new config no longer serves must not be observable",
    );
}

#[test]
fn carry_over_calibration_from_shares_the_store_arc() {
    // Regression guard for the silent-collapse trap, calibration edition: the
    // carry-over shares the outgoing Router's store Arc rather than
    // snapshotting it, mirroring `carry_over_k_store_from`.
    let config = Arc::new(Config::default());
    let before = Router::new(config.clone());
    let mut after = Router::new(config);

    // Act
    after.carry_over_calibration_from(&before);

    // Assert: the store handle itself is shared, not copied.
    assert!(
        Arc::ptr_eq(&after.calibration_store, &before.calibration_store),
        "carry_over_calibration_from must share the store Arc, not snapshot it",
    );
}

#[test]
fn carry_over_calibration_from_makes_a_swap_window_sample_visible() {
    // Regression guard for the snapshot-copy race: a response that completes
    // AFTER `carry_over_calibration_from` runs but BEFORE the new Router is
    // published still holds a reference to the OUTGOING Router. Under a
    // copy-based carry-over that late sample lands only in the store the
    // swap discards; under a shared store it lands in the same map the new
    // Router reads.
    use std::time::SystemTime;

    let kind = "openai-compat";
    let nickname = "opus";
    let now = SystemTime::now();

    let config = Arc::new(Config::default());
    let before = Router::new(config.clone());
    let mut after = router_serving_nicknames(&config, &[nickname]);

    // Act: carry over first, exactly as the hot-reload coordinator does
    // before publishing the new Router...
    after.carry_over_calibration_from(&before);

    // ...then a request still in flight against the OUTGOING router records
    // its sample after the carry-over ran.
    for i in 0..9 {
        before.record_calibration_sample(
            Some(kind),
            Some(nickname),
            Some(&format!("caller-{}", i % 3)),
            10_000,
            13_000,
            now,
        );
    }

    // Assert: the shared store already reflects the late sample.
    let key = crate::calibration::LaneKey {
        provider_kind: kind.to_string(),
        nickname: nickname.to_string(),
    };
    assert!(
        after.calibration_store.factor_for(&key, now).is_some(),
        "a sample recorded on the outgoing router after carry-over must \
             land in the store the new router reads",
    );
}

/// Building the quota store's key from the model's own credential ref is what
/// makes the READ side and the WRITE side share one derivation; a hand-built
/// key here would pass whichever key the store used, so the test goes through
/// the exposed helper exactly as production does.
fn quota_seat_key(provider: &str, label: Option<&str>) -> crate::quota::key::SeatKey {
    let secret_ref = routectl_auth::SecretRef::OAuth {
        provider: provider.to_string(),
        label: label.map(str::to_string),
    };
    crate::quota::key::seat_key_for_secret_ref(Some(&secret_ref)).expect("an oauth ref has a key")
}

/// A Router serving one model on an OAuth credential, so the quota store has a
/// declared seat to admit readings for.
fn router_serving_one_oauth_seat(config: &Arc<Config>, label: Option<&str>) -> Router {
    let mut router = router_serving_nicknames(config, &["opus"]);
    let provider = router
        .resolved_models
        .get("opus")
        .expect("the installed model")
        .provider
        .clone();
    let model = ResolvedModel::new("opus", "p", provider, "upstream").with_auth_secret_ref(
        routectl_auth::SecretRef::OAuth {
            provider: "anthropic".to_string(),
            label: label.map(str::to_string),
        },
    );
    router
        .install_resolved_models(std::iter::once(("opus".to_string(), Arc::new(model))).collect());
    router
}

/// A Router serving TWO models on distinct OAuth credentials, so the quota
/// store has two declared seats -- used by the re-admit test to prove the
/// OUTGOING router genuinely admitted a seat the incoming router later
/// drops (as opposed to a seat it never declared in the first place).
fn router_serving_two_oauth_seats(config: &Arc<Config>, label_a: &str, label_b: &str) -> Router {
    let mut router = router_serving_nicknames(config, &["opus-a", "opus-b"]);
    let provider = router
        .resolved_models
        .get("opus-a")
        .expect("the installed model")
        .provider
        .clone();
    let model_a = ResolvedModel::new("opus-a", "p", provider.clone(), "upstream")
        .with_auth_secret_ref(routectl_auth::SecretRef::OAuth {
            provider: "anthropic".to_string(),
            label: Some(label_a.to_string()),
        });
    let model_b = ResolvedModel::new("opus-b", "p", provider, "upstream").with_auth_secret_ref(
        routectl_auth::SecretRef::OAuth {
            provider: "anthropic".to_string(),
            label: Some(label_b.to_string()),
        },
    );
    router.install_resolved_models(
        [
            ("opus-a".to_string(), Arc::new(model_a)),
            ("opus-b".to_string(), Arc::new(model_b)),
        ]
        .into_iter()
        .collect(),
    );
    router
}

/// One observed reading, for the carry-over tests. Built through the reducer so
/// the snapshot is shaped exactly as a real observation is.
fn observed_quota_reading(utilization: &str) -> crate::quota::reduce::QuotaSnapshot {
    use routectl_core::upstream_meta::AnthropicUnifiedQuota;

    let reset_secs = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .expect("a post-epoch clock")
        .as_secs()
        + 3_600;
    let mut quota = AnthropicUnifiedQuota::default();
    quota.utilization = Some(utilization.to_string());
    quota.extras = vec![("5h-reset".into(), reset_secs.to_string())];
    crate::quota::reduce::reduce_anthropic(
        &quota,
        &crate::quota::freshness::ObservationStamp::now(),
    )
    .snapshot
}

/// The fraction of a seat's FAST window, or `None` when it reads as no
/// evidence.
fn quota_fast_fraction(router: &Router, key: &crate::quota::key::SeatKey) -> Option<f64> {
    let reading = router
        .quota_store
        .reading_for(key, &crate::quota::freshness::ObservationStamp::now())?;
    match reading.fast {
        crate::quota::window::QuotaWindow::Known { utilization, .. } => {
            Some(utilization.fraction())
        }
        crate::quota::window::QuotaWindow::Unknown => None,
    }
}

#[test]
fn installing_resolved_models_declares_their_oauth_seats_to_the_quota_store() {
    // The keyspace bound: the store admits the config's declared seats and
    // nothing else, so a stray identity cannot mint an entry.
    let config = Arc::new(Config::default());

    let router = router_serving_one_oauth_seat(&config, Some("seat-b"));

    assert!(
        router
            .quota_store
            .admits(&quota_seat_key("anthropic", Some("seat-b")))
    );
    assert!(
        !router
            .quota_store
            .admits(&quota_seat_key("anthropic", Some("seat-never-configured"))),
        "an undeclared seat must not be admitted"
    );
}

#[test]
fn carry_over_quota_from_preserves_a_seats_latest_reading() {
    // Regression guard for the silent-collapse trap, quota edition -- and the
    // worst instance of it: an emptied quota store is indistinguishable from a
    // fleet of seats that have not reported yet, which IS the cap-dormant
    // fallback. So the loss reads as health.
    let config = Arc::new(Config::default());
    let key = quota_seat_key("anthropic", Some("seat-b"));
    let before = router_serving_one_oauth_seat(&config, Some("seat-b"));
    assert!(
        before
            .quota_store
            .observe(&key, observed_quota_reading("0.42")),
        "the declared seat accepts a reading"
    );

    let mut after = router_serving_one_oauth_seat(&config, Some("seat-b"));
    assert!(
        after.quota_store.is_empty(),
        "a freshly built router starts with no readings"
    );

    after.carry_over_quota_from(&before);

    assert_eq!(quota_fast_fraction(&after, &key), Some(0.42));
    assert_eq!(after.quota_store.len(), 1);
}

#[test]
fn carry_over_quota_from_drops_a_seat_the_new_config_no_longer_declares() {
    // Same bound the calibration carry-over defends one dimension over: an
    // unfiltered carry-over would keep every seat any past config declared,
    // growing the map with accounts no request can reach.
    let config = Arc::new(Config::default());
    let retired = quota_seat_key("anthropic", Some("retired"));
    let before = router_serving_one_oauth_seat(&config, Some("retired"));
    before
        .quota_store
        .observe(&retired, observed_quota_reading("0.42"));
    assert_eq!(before.quota_store.len(), 1);

    let mut after = router_serving_one_oauth_seat(&config, Some("kept"));
    after.carry_over_quota_from(&before);

    assert!(
        after.quota_store.is_empty(),
        "a retired seat's reading must not survive the rebuild"
    );
}

#[test]
fn carry_over_quota_from_shares_the_store_arc() {
    // Regression guard for the silent-collapse trap, quota edition: the
    // carry-over shares the outgoing Router's store Arc rather than
    // snapshotting it, mirroring `carry_over_k_store_from`.
    let config = Arc::new(Config::default());
    let before = router_serving_one_oauth_seat(&config, Some("seat-b"));
    let mut after = router_serving_one_oauth_seat(&config, Some("seat-b"));

    // Act
    after.carry_over_quota_from(&before);

    // Assert: the store handle itself is shared, not copied.
    assert!(
        Arc::ptr_eq(&after.quota_store, &before.quota_store),
        "carry_over_quota_from must share the store Arc, not snapshot it",
    );
}

#[test]
fn carry_over_quota_from_makes_a_swap_window_reading_visible() {
    // Regression guard for the snapshot-copy race: a response that completes
    // AFTER `carry_over_quota_from` runs but BEFORE the new Router is
    // published still holds a reference to the OUTGOING Router. Under a
    // copy-based carry-over that late reading lands only in the store the
    // swap discards; under a shared store it lands in the same map the new
    // Router reads.
    let config = Arc::new(Config::default());
    let key = quota_seat_key("anthropic", Some("seat-b"));
    let before = router_serving_one_oauth_seat(&config, Some("seat-b"));
    let mut after = router_serving_one_oauth_seat(&config, Some("seat-b"));

    // Act: carry over first, exactly as the hot-reload coordinator does
    // before publishing the new Router...
    after.carry_over_quota_from(&before);

    // ...then a request still in flight against the OUTGOING router feeds
    // its reading after the carry-over ran.
    assert!(
        before
            .quota_store
            .observe(&key, observed_quota_reading("0.42")),
        "the still-admitted seat accepts the late reading"
    );

    // Assert: the shared store already reflects the late reading.
    assert_eq!(
        quota_fast_fraction(&after, &key),
        Some(0.42),
        "a reading observed on the outgoing router after carry-over must \
             land in the store the new router reads",
    );
}

#[test]
fn carry_over_quota_from_re_admits_exactly_the_new_seat_set() {
    // The re-admit half of D6c: a reload that changes the seat set leaves
    // the SHARED store admitting exactly the new seats -- a dropped seat's
    // write is refused and counted, a kept seat's late write still lands.
    let config = Arc::new(Config::default());
    let kept = quota_seat_key("anthropic", Some("kept"));
    let dropped = quota_seat_key("anthropic", Some("dropped"));

    // The OUTGOING router genuinely admits BOTH seats -- proven by writing
    // through it and asserting the write landed, not assumed from the seat
    // set it was constructed with.
    let before = router_serving_two_oauth_seats(&config, "kept", "dropped");
    assert!(
        before
            .quota_store
            .observe(&kept, observed_quota_reading("0.10"))
    );
    assert!(
        before
            .quota_store
            .observe(&dropped, observed_quota_reading("0.20")),
        "the outgoing router must genuinely admit the seat it is about to drop"
    );

    // The INCOMING router declares only `kept`.
    let mut after = router_serving_one_oauth_seat(&config, Some("kept"));
    after.carry_over_quota_from(&before);

    assert_eq!(
        quota_fast_fraction(&after, &dropped),
        None,
        "the dropped seat's pre-existing reading must be pruned by the re-admit"
    );

    let refused_before = after.quota_store.refused_by_admission_total();

    // A late write for a seat the new config never declared is refused and
    // counted.
    assert!(
        !after
            .quota_store
            .observe(&dropped, observed_quota_reading("0.99"))
    );
    assert_eq!(
        after.quota_store.refused_by_admission_total(),
        refused_before + 1,
        "a write for a seat outside the re-admitted set must be counted",
    );

    // A late write for the still-admitted seat lands.
    assert!(
        after
            .quota_store
            .observe(&kept, observed_quota_reading("0.55"))
    );
    assert_eq!(quota_fast_fraction(&after, &kept), Some(0.55));
}

/// A Router serving exactly `nicknames` out of its resolved table, so the
/// calibration carry-over's nickname filter has something to admit against.
/// The models resolve onto a stub provider; these tests never dispatch.
fn router_serving_nicknames(config: &Arc<Config>, nicknames: &[&str]) -> Router {
    use routectl_core::{ChatChunk, ChatResponse, Provider};

    struct StubProvider;

    #[async_trait::async_trait]
    impl Provider for StubProvider {
        fn id(&self) -> &'static str {
            "stub"
        }
        fn normalize_request(&self, _: &ChatRequest) -> routectl_core::Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> routectl_core::Result<ChatResponse> {
            Err(Error::normalize_response("stub", "unused"))
        }
        async fn complete(&self, _: ChatRequest) -> routectl_core::Result<ChatResponse> {
            unreachable!("calibration carry-over tests never dispatch")
        }
        async fn stream(
            &self,
            _: ChatRequest,
        ) -> routectl_core::Result<
            futures::stream::BoxStream<'static, routectl_core::Result<ChatChunk>>,
        > {
            unreachable!("calibration carry-over tests never dispatch")
        }
    }

    let provider: Arc<dyn Provider> = Arc::new(StubProvider);
    let mut router = Router::new(config.clone());
    let models = nicknames
        .iter()
        .map(|nickname| {
            (
                (*nickname).to_string(),
                Arc::new(ResolvedModel::new(
                    *nickname,
                    "p",
                    provider.clone(),
                    "upstream",
                )),
            )
        })
        .collect();
    router.install_resolved_models(models);
    router
}

#[test]
fn router_new_builds_learned_registry_reflecting_config_knobs() {
    use routectl_core::capability::{FailurePhase, SignalTier};
    use std::time::{Duration, Instant};

    // Arrange: a `[capability]` block with a non-default 1h decay so the
    // smoke test can prove the registry was built from the config knobs
    // (not the registry's own hard-coded default).
    let mut config = Config::default();
    config.capability.decay_hours = 1;
    config.capability.inferred_window_hours = 1;
    let router = Router::new(Arc::new(config));

    // Assert: a fresh registry is present and empty.
    assert!(router.learned_capabilities.is_empty());

    // A self-identifying negative acts, then lapses into a re-probe
    // exactly at the configured 1h decay -- not the registry default.
    let t0 = Instant::now();
    router.learned_capabilities.observe(
        "nick",
        "web_search",
        "openai-compat",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        t0,
    );
    assert_eq!(
        router.learned_capabilities.acting_negative_for(
            "nick",
            "web_search",
            "openai-compat",
            t0 + Duration::from_mins(30),
        ),
        crate::learned_capability::RoutingDecision::RouteAway {
            signal: SignalTier::SelfIdentifying,
            phase: FailurePhase::F1,
        },
        "must still act well inside the configured decay window",
    );
    assert_eq!(
        router.learned_capabilities.acting_negative_for(
            "nick",
            "web_search",
            "openai-compat",
            t0 + Duration::from_hours(1) + Duration::from_secs(1),
        ),
        crate::learned_capability::RoutingDecision::ProbeAdmitted,
        "must lapse into a re-probe just past the configured 1h decay",
    );
}

#[test]
fn router_new_builds_override_registry_with_static_provenance_from_legacy_config() {
    // Arrange: a legacy-only config (provider + model
    // `unsupported_features`, no `[capability.overrides]`). The
    // override read-model must be built from it at construction and
    // carry the legacy static provenance so labels stay unchanged.
    let toml_text = "\
            [providers.p]\n\
            kind = \"openai-compat\"\n\
            base_url = \"https://x\"\n\
            api_key_ref = \"literal:k\"\n\
            unsupported_features = [\"web_search\"]\n\
            [models.nick]\n\
            provider = \"p\"\n\
            upstream = \"gpt-x\"\n\
            unsupported_features = [\"computer_use\"]\n";
    let config: Config = toml::from_str(toml_text).expect("config parses");

    // Act
    let router = Router::new(Arc::new(config));

    // Assert: the accessor exposes a registry whose legacy entries
    // carry ProviderStatic / ModelStatic provenance.
    let registry = router.override_registry();
    assert_eq!(
        registry.resolve("p", "nick", "web_search", "openai-compat"),
        Some((
            crate::override_registry::OverrideVerdict::RouteAway,
            crate::override_registry::OverrideProvenance::ProviderStatic
        )),
    );
    assert_eq!(
        registry.resolve("p", "nick", "computer_use", "openai-compat"),
        Some((
            crate::override_registry::OverrideVerdict::RouteAway,
            crate::override_registry::OverrideProvenance::ModelStatic
        )),
    );
}

#[test]
fn carry_over_learned_from_carries_when_catalog_and_overlay_unchanged() {
    use routectl_core::capability::{FailurePhase, SignalTier};
    use std::time::Instant;

    // Arrange: learn a negative in the outgoing Router; both Routers
    // share the same catalog version and overlay revision (the
    // config-only reload case).
    let config = Arc::new(Config::default());
    let before = Router::new(config.clone());
    before.learned_capabilities.observe(
        "nick",
        "web_search",
        "openai-compat",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        Instant::now(),
    );
    let mut after = Router::new(config);
    assert_eq!(after.catalog_version, before.catalog_version);
    assert_eq!(after.overlay_revision, before.overlay_revision);

    // Act
    after.carry_over_learned_from(&before);

    // Assert: the negative rode across untouched; no invalidation.
    assert_eq!(after.learned_capabilities.snapshot().len(), 1);
    assert_eq!(after.metrics.invalidations_total(), 0);
}

#[test]
fn carry_over_learned_from_clears_in_flight_slot() {
    use crate::learned_capability::RoutingDecision;
    use routectl_core::capability::{FailurePhase, SignalTier};
    use std::time::{Duration, Instant};

    // Arrange: a 1h decay so the probe slot can be claimed on a lapsed
    // entry. Learn a self-identifying negative, then admit a re-probe on
    // the outgoing Router so its entry carries `in_flight = true`.
    let mut config = Config::default();
    config.capability.decay_hours = 1;
    config.capability.inferred_window_hours = 1;
    let config = Arc::new(config);
    let before = Router::new(config.clone());
    let t0 = Instant::now();
    before.learned_capabilities.observe(
        "nick",
        "web_search",
        "openai-compat",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        t0,
    );
    let t_probe = t0 + Duration::from_hours(1) + Duration::from_secs(1);
    assert_eq!(
        before.learned_capabilities.acting_negative_for(
            "nick",
            "web_search",
            "openai-compat",
            t_probe,
        ),
        RoutingDecision::ProbeAdmitted,
        "outgoing entry must hold an in-flight probe slot to carry across",
    );

    // Act: config-only reload -- same catalog version and overlay
    // revision, so the entry rides across.
    let mut after = Router::new(config);
    assert_eq!(after.catalog_version, before.catalog_version);
    assert_eq!(after.overlay_revision, before.overlay_revision);
    after.carry_over_learned_from(&before);

    // Assert: the entry rode across with its non-in-flight fields intact.
    let carried = after.learned_capabilities.snapshot();
    assert_eq!(carried.len(), 1);
    assert_eq!(carried[0].signal_tier, SignalTier::SelfIdentifying);

    // The carried entry is still acting AND its in-flight slot was
    // cleared, so the next matching request re-admits a probe rather than
    // latching on a slot no outcome on the new Router can ever release.
    let t_query = t_probe + Duration::from_secs(1);
    assert_eq!(
        after.learned_capabilities.acting_negative_for(
            "nick",
            "web_search",
            "openai-compat",
            t_query,
        ),
        RoutingDecision::ProbeAdmitted,
        "carried-over slot must not stay latched after the reload",
    );
}

#[test]
fn carry_over_expires_learned_entries_whose_override_cell_changed() {
    use crate::learned_capability::RoutingDecision;
    use routectl_core::capability::{FailurePhase, SignalTier};
    use std::time::Instant;

    // Arrange: the outgoing Router masked `web_search` on provider `p`
    // with a force_supported override; the incoming Router drops that
    // override. Both share catalog version + overlay revision (the
    // config-only reload case), so entries ride across.
    let before_cfg: Config = toml::from_str(
        "version = 3\n\
             [capability.overrides.p]\n\
             force_supported = [\"web_search\"]\n",
    )
    .expect("config parses");
    let before = Router::new(Arc::new(before_cfg));
    let t0 = Instant::now();
    // A masked entry (the cell that changes) plus an unrelated healthy
    // entry (no override in either config).
    before.learned_capabilities.observe(
        "p",
        "web_search",
        "",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        t0,
    );
    before.learned_capabilities.observe(
        "p",
        "computer_use",
        "",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        t0,
    );

    let mut after = Router::new(Arc::new(Config::default()));
    assert_eq!(after.catalog_version, before.catalog_version);
    assert_eq!(after.overlay_revision, before.overlay_revision);

    // Act
    after.carry_over_learned_from(&before);

    // Assert: both entries carried across, no full invalidation.
    assert_eq!(after.learned_capabilities.snapshot().len(), 2);
    assert_eq!(after.metrics.invalidations_total(), 0);

    let now = Instant::now();
    // The changed cell's entry lapsed into a single re-probe...
    assert_eq!(
        after
            .learned_capabilities
            .acting_negative_for("p", "web_search", "", now),
        RoutingDecision::ProbeAdmitted,
        "an override cell that changed across reload must lapse its entry",
    );
    // ...while the unrelated healthy entry survived the reload intact.
    assert_eq!(
        after
            .learned_capabilities
            .acting_negative_for("p", "computer_use", "", now),
        RoutingDecision::RouteAway {
            signal: SignalTier::SelfIdentifying,
            phase: FailurePhase::F1,
        },
        "an entry with no override change must ride across untouched",
    );
}

#[test]
fn carry_over_learned_from_clears_and_warns_on_catalog_bump() {
    use routectl_core::capability::{FailurePhase, SignalTier};
    use std::time::Instant;

    // Arrange
    let config = Arc::new(Config::default());
    let before = Router::new(config.clone());
    before.learned_capabilities.observe(
        "nick",
        "web_search",
        "openai-compat",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        Instant::now(),
    );
    let mut after = Router::new(config);
    // Simulate a baked-catalog version bump across the rebuild.
    after.catalog_version = before.catalog_version + 1;

    // Act
    let events = routectl_testkit::capture_events(|| {
        after.carry_over_learned_from(&before);
    });

    // Assert: fresher catalog truth wins -- registry starts empty, one
    // WARN names the catalog trigger, invalidation counter bumped.
    assert!(after.learned_capabilities.is_empty());
    assert_eq!(after.metrics.invalidations_total(), 1);
    let warn = events
        .iter()
        .find(|e| e.level == tracing::Level::WARN)
        .expect("catalog bump must emit a WARN");
    assert_eq!(warn.field("event"), Some("invalidation"));
    assert_eq!(warn.field("catalog_changed"), Some("true"));
    assert_eq!(warn.field("overlay_changed"), Some("false"));
    let prev_cat = before.catalog_version.to_string();
    let cur_cat = after.catalog_version.to_string();
    assert_eq!(
        warn.field("previous_catalog_version"),
        Some(prev_cat.as_str())
    );
    assert_eq!(warn.field("catalog_version"), Some(cur_cat.as_str()));
    assert_eq!(warn.field("previous_overlay_revision"), Some("0"));
    assert_eq!(warn.field("overlay_revision"), Some("0"));
}

#[test]
fn carry_over_learned_from_clears_and_warns_on_overlay_revision_change() {
    use routectl_core::capability::{FailurePhase, SignalTier};
    use std::time::Instant;

    // Arrange: the outgoing Router was built against overlay revision 3.
    let config = Arc::new(Config::default());
    let mut before = Router::new(config.clone());
    before.install_catalog_overlay(overlay_at_revision(3));
    before.learned_capabilities.observe(
        "nick",
        "web_search",
        "openai-compat",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        Instant::now(),
    );
    // The rebuild picked up a newer overlay revision.
    let mut after = Router::new(config);
    after.install_catalog_overlay(overlay_at_revision(4));

    // Act
    let events = routectl_testkit::capture_events(|| {
        after.carry_over_learned_from(&before);
    });

    // Assert: overlay change invalidates -- empty registry, one WARN
    // naming the overlay trigger, invalidation counter bumped.
    assert!(after.learned_capabilities.is_empty());
    assert_eq!(after.metrics.invalidations_total(), 1);
    let warn = events
        .iter()
        .find(|e| e.level == tracing::Level::WARN)
        .expect("overlay revision change must emit a WARN");
    assert_eq!(warn.field("event"), Some("invalidation"));
    assert_eq!(warn.field("overlay_changed"), Some("true"));
    assert_eq!(warn.field("catalog_changed"), Some("false"));
    let prev_cat = before.catalog_version.to_string();
    let cur_cat = after.catalog_version.to_string();
    assert_eq!(
        warn.field("previous_catalog_version"),
        Some(prev_cat.as_str())
    );
    assert_eq!(warn.field("catalog_version"), Some(cur_cat.as_str()));
    assert_eq!(warn.field("previous_overlay_revision"), Some("3"));
    assert_eq!(warn.field("overlay_revision"), Some("4"));
}

#[test]
fn record_k_sample_skips_keyless_and_records_keyed() {
    use crate::k_estimator::KSessionKey;
    use std::time::UNIX_EPOCH;

    // Arrange
    let config = Arc::new(Config::default());
    let router = Router::new(config);

    // Act: a keyless request must NOT create any window.
    router.record_k_sample(None, "anthropic-api", "opus", 5, UNIX_EPOCH);

    // Assert: the store stays empty -- keyless requests are untracked.
    assert!(
        router.k_session_store.is_empty(),
        "a keyless request must not be recorded",
    );

    // Act: a keyed request with a cache hit records one reuse sample.
    router.record_k_sample(Some("sess-1"), "anthropic-api", "opus", 7, UNIX_EPOCH);
    // A keyed request with no cache hit records a no-reuse sample.
    router.record_k_sample(Some("sess-1"), "anthropic-api", "opus", 0, UNIX_EPOCH);

    // Assert: both samples landed under the one triple, with
    // observed_reuse tracking cache_read > 0.
    let window = router
        .k_session_store
        .get(&KSessionKey {
            session_key: "sess-1".into(),
            provider_kind: "anthropic-api".into(),
            model: "opus".into(),
        })
        .expect("triple recorded");
    let reuse: Vec<bool> = window.iter().map(|s| s.observed_reuse).collect();
    assert_eq!(reuse, vec![true, false]);
}

/// `carry_over_k_store_from` rebinds `k_estimator` over the shared store:
/// the freshly-built router's estimator was constructed against its OWN
/// (about-to-be-discarded) store, so the carry-over must repoint it at the
/// shared one or the estimator silently keeps reading an empty map even
/// though `k_session_store` itself is now shared. Builds a
/// `Calibrated`-sized window in the source, carries it over, and proves the
/// new router's estimator returns a non-cold estimate for the carried
/// triple.
#[test]
fn carried_store_is_read_by_new_routers_estimator() {
    use crate::k_estimator::{Confidence, KQuery, KSessionKey, KSessionWindow, Sample};
    use std::time::{Duration, SystemTime};

    // Arrange: enough samples in the source store that the estimator
    // classifies the triple as `Calibrated` (>= CALIBRATED_MIN_TRIALS
    // trials). They are timestamped inside the TTL window of the query
    // below, which runs on the real dispatch clock: the estimator counts
    // only samples younger than the queried TTL.
    let ttl = Duration::from_mins(5);
    let base = SystemTime::now() - Duration::from_secs(30);
    let mut window = KSessionWindow::new();
    for i in 0..12u64 {
        window.push(Sample {
            ts: base + Duration::from_secs(i),
            observed_reuse: true,
        });
    }
    let key = KSessionKey {
        session_key: "carried-sess".into(),
        provider_kind: "anthropic-api".into(),
        model: "opus".into(),
    };

    let config = Arc::new(Config::default());
    let before = Router::new(config.clone());
    before.k_session_store.put(key, window);

    // Act: a freshly-built router shares the source's store, then its
    // rebound estimator is queried.
    let mut after = Router::new(config);
    after.carry_over_k_store_from(&before);
    let estimate = after.k_estimator.estimate(&KQuery {
        session_key: Some("carried-sess"),
        provider_kind: "anthropic-api",
        model: "opus",
        ttl,
        now: SystemTime::now(),
    });

    // Assert: the estimator saw the carried samples (not a cold default).
    assert_eq!(
        estimate.confidence,
        Confidence::Calibrated,
        "new router's estimator must read the carried-over store",
    );
    assert!(estimate.samples >= 12);
}

#[test]
fn carry_over_k_store_from_makes_a_swap_window_sample_visible() {
    // Regression guard for the snapshot-copy race: a response that completes
    // AFTER `carry_over_k_store_from` runs but BEFORE the new Router is
    // published still holds a reference to the OUTGOING Router. Under a
    // copy-based carry-over that late sample lands only in the store the
    // swap discards; under a shared store it lands in the same map the new
    // Router reads. Exercises both the shared store directly and the
    // rebound estimator, so a regression in either the field-share or the
    // estimator rebind fails this test.
    use crate::k_estimator::{Confidence, KQuery};
    use std::time::SystemTime;

    let config = Arc::new(Config::default());
    let before = Router::new(config.clone());
    let mut after = Router::new(config);

    // Act: carry over first, exactly as the hot-reload coordinator does
    // before publishing the new Router...
    after.carry_over_k_store_from(&before);

    // ...then a request still in flight against the OUTGOING router records
    // its sample after the carry-over ran.
    let now = SystemTime::now();
    for _ in 0..12 {
        before.record_k_sample(Some("late-sess"), "anthropic-api", "opus", 7, now);
    }

    // Assert: the shared store already reflects the late sample.
    assert!(
        !after.k_session_store.is_empty(),
        "a sample recorded on the outgoing router after carry-over must \
             land in the store the new router reads",
    );

    // Assert: the new router's OWN estimator (rebound by the carry-over)
    // reads it too, not just the raw store.
    let estimate = after.k_estimator.estimate(&KQuery {
        session_key: Some("late-sess"),
        provider_kind: "anthropic-api",
        model: "opus",
        ttl: std::time::Duration::from_mins(5),
        now,
    });
    assert_eq!(
        estimate.confidence,
        Confidence::Calibrated,
        "the new router's estimator must observe a swap-window sample \
             recorded through the outgoing router after carry-over",
    );
}

/// The `would_trim_k_floor_for_meta` truth table, one assertion per row.
/// The verdict (met / unmet / cold / unpriced) is derived downstream from
/// the numeric advisory columns; here we only pin the recorded Option.
#[test]
fn would_trim_k_floor_for_meta_truth_table() {
    use crate::k_estimator::{Confidence, EstimateSource, KEstimate};

    fn estimate(k_floor: f64, confidence: Confidence) -> KEstimate {
        KEstimate {
            k_floor,
            k_point: k_floor,
            k_ceiling: k_floor,
            samples: 16,
            confidence,
            source: EstimateSource::LiveLedger,
        }
    }

    // Row 1: Some(K*), Calibrated, k_floor >= K* -> Some(k_floor).
    assert_eq!(
        would_trim_k_floor_for_meta(Some(50.0), &estimate(60.0, Confidence::Calibrated)),
        Some(60.0),
    );

    // Row 2: Some(K*), Calibrated, k_floor < K* -> Some(k_floor)
    // (both met and unmet record the floor; the comparison is derived).
    assert_eq!(
        would_trim_k_floor_for_meta(Some(50.0), &estimate(40.0, Confidence::Calibrated)),
        Some(40.0),
    );

    // Row 3a: Some(K*), Low -> None.
    assert_eq!(
        would_trim_k_floor_for_meta(Some(50.0), &estimate(99.0, Confidence::Low)),
        None,
    );

    // Row 3b: Some(K*), Cold -> None.
    assert_eq!(
        would_trim_k_floor_for_meta(Some(50.0), &estimate(0.0, Confidence::Cold)),
        None,
    );

    // Row 4: None (unverified pricing), any confidence -> None.
    for conf in [Confidence::Calibrated, Confidence::Low, Confidence::Cold] {
        assert_eq!(
            would_trim_k_floor_for_meta(None, &estimate(99.0, conf)),
            None,
            "conf={conf:?}",
        );
    }
}

#[test]
fn carry_over_metrics_from_shares_storage_across_a_rebuild() {
    // Arrange: bump a counter on the outgoing Router directly through its
    // shared metrics handle, mimicking a late-completing request that
    // still holds the old Router after a swap.
    let config = Arc::new(Config::default());
    let before = Router::new(config.clone());
    before.metrics.incr_window_gate_skip();
    let mut after = Router::new(config);
    assert_eq!(
        after.metrics.window_gate_skips_total(),
        0,
        "a freshly-built Router starts at zero before any carry-over",
    );

    // Act
    after.carry_over_metrics_from(&before);

    // Assert: the new Router observes the pre-carry-over increment,
    // proving the storage is SHARED rather than snapshotted.
    assert_eq!(after.metrics.window_gate_skips_total(), 1);

    // A further increment through EITHER handle must be visible through
    // both, since carry-over shares the underlying Arc rather than
    // copying a value at one instant.
    after.metrics.incr_window_gate_skip();
    assert_eq!(before.metrics.window_gate_skips_total(), 2);
}

#[test]
fn log_snapshot_emits_one_complete_event_with_current_counter_values() {
    // Arrange: bump a couple of counters so the snapshot's assertions
    // pin real numbers, not just field presence.
    let config = Arc::new(Config::default());
    let router = Router::new(config);
    router.metrics.incr_window_gate_skip();
    router.metrics.incr_window_gate_skip();
    router.metrics.incr_context_window_overflow();
    // Refused-by-admission lives on QuotaStore, not RouterMetrics, but must
    // ride the same snapshot event -- an undeclared seat's write refuses.
    router.quota_store.observe(
        &crate::quota::key::seat_key_for_secret_ref(Some(&routectl_auth::SecretRef::OAuth {
            provider: "anthropic".to_string(),
            label: None,
        }))
        .expect("an oauth ref has a key"),
        observed_quota_reading("0.10"),
    );

    // Act
    let events = routectl_testkit::capture_events(|| {
        router.log_metrics_snapshot();
    });

    // Assert: exactly one event on the stable target/message a
    // structured-log consumer matches on -- a regression that re-splits
    // the snapshot into two partial events, or drops the spawn entirely,
    // fails here.
    let snapshots: Vec<_> = events
        .iter()
        .filter(|e| {
            e.target == "routectl_router::router::metrics" && e.message == "router metrics snapshot"
        })
        .collect();
    assert_eq!(
        snapshots.len(),
        1,
        "exactly one router metrics snapshot event must be emitted, got {}",
        snapshots.len()
    );
    let snapshot = snapshots[0];

    // The two counters bumped above carry their real accumulated values.
    assert_eq!(snapshot.field("rc_window_gate_skips_total"), Some("2"));
    assert_eq!(
        snapshot.field("rc_quota_refused_by_admission_total"),
        Some("1"),
        "the store-owned counter must ride the same snapshot event",
    );
    assert_eq!(
        snapshot.field("rc_context_window_overflow_total"),
        Some("1")
    );

    // Every other counter is present (proving the field is not dropped)
    // at its untouched zero value.
    for field in [
        "rc_unknown_failure_classifications_total",
        "rc_feature_unsupported_total",
        "rc_learned_negatives_total",
        "rc_learned_negatives_f1_total",
        "rc_learned_negatives_f2_total",
        "rc_probe_attempts_total",
        "rc_probe_failures_total",
        "rc_invalidations_total",
        "rc_d17_tail_total",
        "rc_strip_total",
        "rc_strip_rollback_total",
        "rc_strip_strict_rejected_total",
        "rc_mask_suppressed_total",
        "rc_f2_same_chain_suppressed_total",
        "rc_feature_naming_unmatched_total",
        "rc_verified_working_total",
        "rc_f3_suspect_total",
        "rc_quota_placement_below_cap_total",
        "rc_quota_placement_all_capped_total",
        "rc_quota_placement_mixed_unknown_total",
        "rc_quota_placement_all_unknown_total",
        // The pool counters, including the one whose only driver today is the
        // seam `note_pool_removed_pin_repick`: a counter absent from the
        // snapshot is unobservable, and a zero-valued one is the honest report
        // of a fleet with no pools.
        "rc_pool_dispatch_total",
        "rc_pool_degraded_dispatch_total",
        "rc_pool_unavailable_total",
        "rc_pool_member_omitted_credential_missing_total",
        "rc_pool_member_omitted_credential_unreadable_total",
        "rc_pool_member_omitted_credential_invalid_total",
        "rc_pool_member_omitted_provider_init_failed_total",
        "rc_pool_removed_pin_repick_total",
    ] {
        assert_eq!(
            snapshot.field(field),
            Some("0"),
            "field {field} must be present on the single snapshot event"
        );
    }

    // The bedrock-only field's presence must track the build's feature
    // set exactly -- present under the default (bedrock-on) build,
    // absent when the feature is compiled out. A field split back onto
    // a second partial event, rather than folded into this one, fails
    // this assertion because the field would be missing here.
    #[cfg(feature = "bedrock")]
    assert_eq!(
        snapshot.field("rc_bedrock_validation_unmatched_total"),
        Some("0"),
        "bedrock build must carry the bedrock counter on the single snapshot event"
    );
    #[cfg(not(feature = "bedrock"))]
    assert_eq!(
        snapshot.field("rc_bedrock_validation_unmatched_total"),
        None,
        "non-bedrock build must not emit the bedrock-only counter"
    );
}

#[test]
fn emit_class_observability_bumps_context_window_overflow_on_context_window_class() {
    use routectl_core::failure_class::{ClassifiedFailure, MatchedBy};

    struct StubProvider;
    #[async_trait::async_trait]
    impl Provider for StubProvider {
        fn id(&self) -> &'static str {
            "stub"
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response("stub", "unused"))
        }
        async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
            unreachable!()
        }
        async fn stream(
            &self,
            _: ChatRequest,
        ) -> Result<futures::stream::BoxStream<'static, Result<ChatChunk>>> {
            unreachable!()
        }
    }

    let router = build_router_with_provider_timeouts(None, None);
    let provider: Arc<dyn Provider> = Arc::new(StubProvider);
    let model = Arc::new(ResolvedModel::new("nick", "p1", provider, "upstream"));
    let target = into_one_dispatch_target(model);
    let policy = RetryPolicy::default();

    let err = Error::upstream("p1", 400, "prompt is too long");
    let cf = ClassifiedFailure {
        class: FailureClass::ContextWindow,
        matched_by: MatchedBy::Status,
    };

    assert_eq!(router.metrics.context_window_overflow_total(), 0);

    router.emit_class_observability(
        &err,
        &cf,
        &cf.class,
        false,
        None,
        DispatchSurface::Complete,
        "p1",
        &target,
        false,
        &policy,
        false,
        false,
        false,
    );

    assert_eq!(
        router.metrics.context_window_overflow_total(),
        1,
        "a dispatch error arm reaching FailureClass::ContextWindow means the \
         target cleared the proactive window gate and still overflowed",
    );
}
