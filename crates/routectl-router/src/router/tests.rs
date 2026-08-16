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
    let entries = after.sticky_pins.export_entries();
    assert!(
        entries.contains(&(
            "sess-1".to_string(),
            crate::seat_pool::SeatPin {
                state_key: "opus#seat-b".to_string(),
                repinned: true,
            }
        )),
        "carry_over_sticky_from must preserve session->seat pins (with the \
             repinned flag) across a rebuild",
    );
}

#[test]
fn carry_over_k_store_from_preserves_windows_and_lru_order() {
    // Regression guard for the silent-collapse trap, K-store edition: a
    // hot-reload must NOT drop per-session K windows, and it must keep
    // their LRU ordering so the destination's eviction frontier matches
    // what the source would have evicted next.
    use crate::k_estimator::{KSessionKey, KSessionWindow, Sample};
    use std::time::{Duration, UNIX_EPOCH};

    fn key(session: &str) -> KSessionKey {
        KSessionKey {
            session_key: session.into(),
            provider_kind: "anthropic-api".into(),
            model: "opus".into(),
        }
    }

    fn sample(secs: u64, reused: bool) -> Sample {
        Sample {
            ts: UNIX_EPOCH + Duration::from_secs(secs),
            observed_reuse: reused,
        }
    }

    // Arrange: insert A, B, C in that order, then touch A so the source's
    // LRU order is [B (LRU), C, A (MRU)].
    let config = Arc::new(Config::default());
    let before = Router::new(config.clone());
    let mut win_a = KSessionWindow::new();
    win_a.push(sample(1, true));
    let mut win_b = KSessionWindow::new();
    win_b.push(sample(2, false));
    let mut win_c = KSessionWindow::new();
    win_c.push(sample(3, true));
    before.k_session_store.put(key("A"), win_a.clone());
    before.k_session_store.put(key("B"), win_b.clone());
    before.k_session_store.put(key("C"), win_c.clone());
    let _ = before.k_session_store.get(&key("A"));

    let mut after = Router::new(config);

    // Act
    after.carry_over_k_store_from(&before);

    // Assert: every entry survived AND the LRU order matches the source.
    // A scattered carry-over (e.g. HashMap iteration order) would pass
    // the per-key survival check but fail this ordering one.
    let entries = after.k_session_store.export_entries();
    let observed_keys: Vec<&KSessionKey> = entries.iter().map(|(k, _)| k).collect();
    assert_eq!(
        observed_keys,
        vec![&key("B"), &key("C"), &key("A")],
        "carry_over_k_store_from must preserve LRU recency order",
    );
    let observed_windows: Vec<&KSessionWindow> = entries.iter().map(|(_, w)| w).collect();
    assert_eq!(observed_windows, vec![&win_b, &win_c, &win_a]);
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
    // The store has no LRU because the lane keyspace is bounded by the loaded
    // config. An unfiltered carry-over breaks that bound: a run of reloads
    // that rename models would carry every retired name forward forever,
    // growing the map with lanes no request can ever reach. Only the lane
    // whose nickname the new resolved table still holds survives.
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

    // Act: the replacement router serves only `kept`.
    let mut after = router_serving_nicknames(&config, &["kept"]);
    after.carry_over_calibration_from(&before);

    // Assert
    let surviving: Vec<String> = after
        .calibration_store
        .export_entries()
        .into_iter()
        .map(|(key, _)| key.nickname)
        .collect();
    assert_eq!(surviving, vec!["kept".to_string()]);
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

/// A fresh router's `k_estimator` reads the store entries imported by
/// `carry_over_k_store_from`: the estimator field needs no carry-over of
/// its own because the constructor points it at the same store the
/// carry-over populates. Builds a `Calibrated`-sized window in the source,
/// carries it over, and proves the new router's estimator returns a
/// non-cold estimate for the carried triple.
#[test]
fn carried_store_is_read_by_new_routers_estimator() {
    use crate::k_estimator::{Confidence, KQuery, KSessionKey, KSessionWindow, Sample};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    // Arrange: enough TTL-separated runs in the source store that the
    // estimator classifies the triple as `Calibrated` (>= 8 runs). Each
    // run is one reuse hit separated from the next by more than the TTL.
    let ttl = Duration::from_mins(5);
    let mut window = KSessionWindow::new();
    for i in 0..12u64 {
        window.push(Sample {
            ts: UNIX_EPOCH + Duration::from_secs(i * 10_000),
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

    // Act: a freshly-built router imports the source's entries, then its
    // OWN estimator (pointed at its own store at construction) is queried.
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
