// Pooled-identity seat resolution for the paid-probe profile: a member's own
// composed key resolves, and every AMBIGUOUS key refuses.
//
// An `include!`d FRAGMENT of `paid_probe_profile_tests.rs`, not a module of its
// own: the host's imports and fixture helpers stay in scope and every test here
// keeps its original fully qualified name. Carries no top-level `use` for that
// reason -- imports live in the host.

// ---------------------------------------------------------------------------
// Pooled identities
// ---------------------------------------------------------------------------

/// A pooled `m1` with two members, carrying `effective_row` and `adaptive` on
/// the model -- which is where both facts live, since a pool's members share
/// one wire model id and therefore one catalog cell.
fn pooled_router(effective_row: EffectiveRow, adaptive: bool) -> Router {
    let mut config = Config::default();
    for member in ["seat-a", "seat-b"] {
        config.providers.insert(
            member.to_string(),
            ProviderEntry::anthropic_api("literal:k"),
        );
    }
    config.models.insert(
        "m1".to_string(),
        ModelEntry::new("pool", "claude-sonnet-4-5"),
    );
    config
        .aliases
        .insert("default".to_string(), AliasValue::Single("m1".to_string()));
    let mut router = Router::new(Arc::new(config));
    let seats: Vec<crate::seat_pool::SeatTarget> = ["seat-a", "seat-b"]
        .into_iter()
        .map(|member| crate::seat_pool::SeatTarget {
            provider_name: member.to_string(),
            provider: Arc::new(OkProvider {
                count_calls: std::sync::atomic::AtomicUsize::new(0),
            }) as Arc<dyn Provider>,
            auth_secret_ref: None,
        })
        .collect();
    let provider: Arc<dyn Provider> = Arc::new(OkProvider {
        count_calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let model = ResolvedModel::new("m1", "pool", provider, "claude-sonnet-4-5")
        .with_supports_adaptive_thinking(adaptive)
        .with_effective_row(effective_row)
        .with_seats(Arc::from(seats));
    let mut models = std::collections::BTreeMap::new();
    models.insert("m1".to_string(), Arc::new(model));
    router.install_resolved_models(models);
    router
}

#[test]
fn a_pooled_identity_profiles_through_its_own_composed_key() {
    let router = pooled_router(fully_priced(), false);
    let composed = crate::seat_pool::seat_state_key("m1", Some("seat-b"));
    let pooled = FieldVerdictKey::new(&composed, GROUNDED_PATH, "anthropic-api").expect("identity");

    let profile = router
        .paid_probe_profile(&pooled)
        .expect("a pooled member's identity must resolve its model's cell");

    assert_eq!(profile.max_tokens(), LEGACY_MIN_VIABLE_MAX_TOKENS);
}

#[test]
fn a_pooled_identity_naming_no_live_member_yields_no_profile() {
    // Load-bearing negative; its positive control is the test above, same
    // router, same composer, a member that exists.
    let router = pooled_router(fully_priced(), false);
    let composed = crate::seat_pool::seat_state_key("m1", Some("seat-gone"));
    let pooled = FieldVerdictKey::new(&composed, GROUNDED_PATH, "anthropic-api").expect("identity");

    assert_eq!(router.paid_probe_profile(&pooled), None);
}

/// A router with TWO pooled models whose (model, member) pairs both compose the
/// SAME state key, plus one model whose composed key is unique.
///
/// The collision is real, not contrived to be impossible: `seat_state_key` joins
/// the two halves with a separator that is legal in BOTH, so model `a` with
/// member `b#c` and model `a#b` with member `c` compose `a#b#c` alike -- and
/// each names a DIFFERENT credential. The unique model is installed in the same
/// router so the positive control shares every other fixture fact with the
/// collision case.
fn colliding_pooled_router() -> Router {
    let members = ["b#c", "c", "unique-seat"];
    let mut config = Config::default();
    for member in members {
        config.providers.insert(
            member.to_string(),
            ProviderEntry::anthropic_api("literal:k"),
        );
    }
    for nickname in ["a", "a#b", "solo"] {
        config.models.insert(
            nickname.to_string(),
            ModelEntry::new("pool", "claude-sonnet-4-5"),
        );
    }
    config
        .aliases
        .insert("default".to_string(), AliasValue::Single("a".to_string()));
    let mut router = Router::new(Arc::new(config));

    let seat = |member: &str| crate::seat_pool::SeatTarget {
        provider_name: member.to_string(),
        provider: Arc::new(OkProvider {
            count_calls: std::sync::atomic::AtomicUsize::new(0),
        }) as Arc<dyn Provider>,
        auth_secret_ref: None,
    };
    let pooled_model = |nickname: &str, member: &str| {
        Arc::new(
            ResolvedModel::new(
                nickname,
                "pool",
                Arc::new(OkProvider {
                    count_calls: std::sync::atomic::AtomicUsize::new(0),
                }) as Arc<dyn Provider>,
                "claude-sonnet-4-5",
            )
            .with_effective_row(fully_priced())
            .with_seats(Arc::from(vec![seat(member)])),
        )
    };

    let mut models = std::collections::BTreeMap::new();
    models.insert("a".to_string(), pooled_model("a", "b#c"));
    models.insert("a#b".to_string(), pooled_model("a#b", "c"));
    models.insert("solo".to_string(), pooled_model("solo", "unique-seat"));
    router.install_resolved_models(models);
    router
}

#[test]
fn a_state_key_two_model_member_pairs_compose_alike_yields_no_profile() {
    let router = colliding_pooled_router();
    // Premise, asserted rather than assumed: the composer really does produce
    // ONE key from two different (model, member) pairs. Without this the test
    // could pass because the key matched neither pair.
    let left = crate::seat_pool::seat_state_key("a", Some("b#c"));
    let right = crate::seat_pool::seat_state_key("a#b", Some("c"));
    assert_eq!(left, right, "premise: both pairs must compose one key");
    assert_eq!(left, "a#b#c");
    let ambiguous = FieldVerdictKey::new(&left, GROUNDED_PATH, "anthropic-api").expect("identity");

    assert_eq!(
        router.paid_probe_profile(&ambiguous),
        None,
        "an ambiguous key names no single account, so no body may be sized",
    );
}

#[test]
fn an_unambiguous_pooled_key_in_the_same_router_still_profiles() {
    // The control that keeps the refusal above from being "this router profiles
    // nothing": same router, same composer, a key exactly one pair produces.
    let router = colliding_pooled_router();
    let composed = crate::seat_pool::seat_state_key("solo", Some("unique-seat"));
    let unique = FieldVerdictKey::new(&composed, GROUNDED_PATH, "anthropic-api").expect("identity");

    let profile = router
        .paid_probe_profile(&unique)
        .expect("a uniquely-composed pooled key must resolve");

    assert_eq!(profile.max_tokens(), LEGACY_MIN_VIABLE_MAX_TOKENS);
}

/// A router where a DIRECT `[models]` nickname equals a POOLED (model, member)
/// pair's composed key, alongside a direct-only and a pooled-only model.
///
/// The second collision kind, and the one a direct early return hides: model
/// `p` carries member `s`, composing `p#s`, while a separate `[models]` entry is
/// literally named `p#s` -- the adversarial shape `seat_pool::seat_state_key`'s
/// own collision note describes. Both name a real, DIFFERENT account, so neither
/// is the defensible answer. The two single-kind models live in the same router
/// so each control shares every other fixture fact with the collision case.
fn direct_versus_pooled_collision_router() -> Router {
    let mut config = Config::default();
    for provider in ["s", "direct-p", "pooled-seat"] {
        config.providers.insert(
            provider.to_string(),
            ProviderEntry::anthropic_api("literal:k"),
        );
    }
    // `p#s` is a MODEL nickname here, not a composed key -- that is the point.
    for nickname in ["p", "p#s", "direct-only", "pooled-only"] {
        config.models.insert(
            nickname.to_string(),
            ModelEntry::new("pool", "claude-sonnet-4-5"),
        );
    }
    config
        .aliases
        .insert("default".to_string(), AliasValue::Single("p".to_string()));
    let mut router = Router::new(Arc::new(config));

    let provider = || {
        Arc::new(OkProvider {
            count_calls: std::sync::atomic::AtomicUsize::new(0),
        }) as Arc<dyn Provider>
    };
    let base = |nickname: &str, provider_name: &str| {
        ResolvedModel::new(nickname, provider_name, provider(), "claude-sonnet-4-5")
            .with_effective_row(fully_priced())
    };
    let pooled = |nickname: &str, member: &str| {
        Arc::new(
            base(nickname, "pool").with_seats(Arc::from(vec![crate::seat_pool::SeatTarget {
                provider_name: member.to_string(),
                provider: provider(),
                auth_secret_ref: None,
            }])),
        )
    };

    let mut models = std::collections::BTreeMap::new();
    // The colliding pair: a pooled model composing `p#s`, and a direct model
    // whose nickname IS `p#s`.
    models.insert("p".to_string(), pooled("p", "s"));
    models.insert("p#s".to_string(), Arc::new(base("p#s", "direct-p")));
    // The two controls: one direct-only nickname no pool composes, one pooled
    // pair no nickname shadows.
    models.insert(
        "direct-only".to_string(),
        Arc::new(base("direct-only", "direct-p")),
    );
    models.insert(
        "pooled-only".to_string(),
        pooled("pooled-only", "pooled-seat"),
    );
    router.install_resolved_models(models);
    router
}

#[test]
fn a_direct_nickname_equal_to_a_pooled_composed_key_yields_no_profile() {
    let router = direct_versus_pooled_collision_router();
    // Premise, asserted rather than assumed: the pooled pair really does compose
    // the direct model's own nickname. Without this the test could pass because
    // the key matched neither candidate.
    let composed = crate::seat_pool::seat_state_key("p", Some("s"));
    assert_eq!(composed, "p#s", "premise: the pair must compose this key");
    assert!(
        router.resolved_models.contains_key(&composed),
        "premise: a DIRECT model of that exact nickname must also exist",
    );
    let ambiguous =
        FieldVerdictKey::new(&composed, GROUNDED_PATH, "anthropic-api").expect("identity");

    assert_eq!(
        router.paid_probe_profile(&ambiguous),
        None,
        "a direct nickname and a pooled pair name two accounts, so neither wins",
    );
}

#[test]
fn a_direct_only_nickname_in_the_same_router_still_profiles() {
    // Control one: the DIRECT candidate kind alone still resolves, so the
    // refusal above is about ambiguity rather than about direct hits.
    let router = direct_versus_pooled_collision_router();
    let direct =
        FieldVerdictKey::new("direct-only", GROUNDED_PATH, "anthropic-api").expect("identity");

    let profile = router
        .paid_probe_profile(&direct)
        .expect("an unshadowed direct nickname must resolve");

    assert_eq!(profile.max_tokens(), LEGACY_MIN_VIABLE_MAX_TOKENS);
}

#[test]
fn a_pooled_only_key_in_the_same_router_still_profiles() {
    // Control two: the POOLED candidate kind alone still resolves, in the very
    // router that refuses the collision.
    let router = direct_versus_pooled_collision_router();
    let composed = crate::seat_pool::seat_state_key("pooled-only", Some("pooled-seat"));
    assert!(
        !router.resolved_models.contains_key(&composed),
        "premise: no direct nickname may shadow this composed key",
    );
    let pooled_key =
        FieldVerdictKey::new(&composed, GROUNDED_PATH, "anthropic-api").expect("identity");

    let profile = router
        .paid_probe_profile(&pooled_key)
        .expect("an unshadowed pooled pair must resolve");

    assert_eq!(profile.max_tokens(), LEGACY_MIN_VIABLE_MAX_TOKENS);
}
