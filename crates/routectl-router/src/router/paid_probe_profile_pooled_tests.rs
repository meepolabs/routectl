// Pooled-identity seat resolution for the paid-probe profile: a member's own
// lane resolves, and a lane no live member or upstream matches refuses.
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

/// The lane member `member` of the pooled `m1` learns on.
fn member_lane(member: &str) -> crate::state_key::StateKey {
    crate::state_key::StateKey::new(member, "claude-sonnet-4-5").expect("lane")
}

#[test]
fn a_pooled_identity_profiles_through_its_own_member_lane() {
    let router = pooled_router(fully_priced(), false);
    let pooled = FieldVerdictKey::new(&member_lane("seat-b"), GROUNDED_PATH, "anthropic-api")
        .expect("identity");

    let profile = router
        .paid_probe_profile(&pooled)
        .expect("a pooled member's identity must resolve its model's cell");

    assert_eq!(profile.max_tokens(), LEGACY_MIN_VIABLE_MAX_TOKENS);
}

#[test]
fn a_pooled_identity_naming_no_live_member_yields_no_profile() {
    // Load-bearing negative; its positive control is the test above, same
    // router, a member that exists.
    let router = pooled_router(fully_priced(), false);
    let pooled = FieldVerdictKey::new(&member_lane("seat-gone"), GROUNDED_PATH, "anthropic-api")
        .expect("identity");

    assert_eq!(router.paid_probe_profile(&pooled), None);
}

#[test]
fn a_live_member_on_an_upstream_no_model_sends_yields_no_profile() {
    // The upstream half of the lane binds as tightly as the entry half: the
    // member is live, but nothing on it sends this upstream.
    let router = pooled_router(fully_priced(), false);
    let lane = crate::state_key::StateKey::new("seat-b", "another-upstream").expect("lane");
    let pooled = FieldVerdictKey::new(&lane, GROUNDED_PATH, "anthropic-api").expect("identity");

    assert_eq!(router.paid_probe_profile(&pooled), None);
}
