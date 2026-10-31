//! The tuning stamped per runtime-state key at install: which tier each
//! slot's values come from, and which install wins a shared key.

use super::*;
use crate::config::{Resolved, TuningSource};
use crate::resolved::ResolvedModel;
use crate::seat_pool::SeatTarget;
use std::collections::BTreeMap;

fn provider() -> Arc<dyn Provider> {
    super::scripted_provider_test_support::ScriptedProvider::new(None, |_| Ok(()))
}

fn parse(toml_text: &str) -> Config {
    toml::from_str(toml_text).expect("valid test toml")
}

fn first_byte(router: &Router, key: &str) -> Resolved<Option<u64>> {
    router
        .tuning
        .get(key)
        .unwrap_or_else(|| panic!("{key} is stamped"))
        .stream_first_byte_timeout_ms
}

fn at(ms: u64, source: TuningSource) -> Resolved<Option<u64>> {
    Resolved {
        value: Some(ms),
        source,
    }
}

const POOL_TOML: &str = r#"
[providers.member-a]
kind = "anthropic-api"
api_key_ref = "literal:k"
stream_first_byte_timeout_ms = 50

[providers.member-b]
kind = "anthropic-api"
api_key_ref = "literal:k"

[pools.pool]
members = ["member-a", "member-b"]
stream_first_byte_timeout_ms = 5000
"#;

fn pooled_router(model_first_byte_ms: Option<u64>) -> Router {
    let seats: Vec<SeatTarget> = ["member-a", "member-b"]
        .iter()
        .map(|member| SeatTarget::new((*member).to_string(), provider(), None, false))
        .collect();
    let mut model = ResolvedModel::new("opus", "pool", provider(), "u");
    if let Some(ms) = model_first_byte_ms {
        model = model.with_stream_first_byte_timeout_ms(ms);
    }
    let mut router = Router::new(Arc::new(parse(POOL_TOML)));
    router.install_resolved_models(BTreeMap::from([(
        "opus".to_string(),
        Arc::new(model.with_seats(Arc::from(seats))),
    )]));
    router
}

#[test]
fn each_seat_slot_is_stamped_from_its_own_member_then_the_pool() {
    let router = pooled_router(None);

    assert_eq!(
        first_byte(&router, "opus#member-a"),
        at(50, TuningSource::Provider)
    );
    assert_eq!(
        first_byte(&router, "opus#member-b"),
        at(5_000, TuningSource::Pool)
    );
    assert_eq!(
        first_byte(&router, "opus"),
        at(5_000, TuningSource::Pool),
        "the model's own slot resolves through its pool"
    );
}

#[test]
fn a_model_first_content_override_stamps_every_slot_it_occupies() {
    let router = pooled_router(Some(9_000));

    for key in ["opus", "opus#member-a", "opus#member-b"] {
        assert_eq!(
            first_byte(&router, key),
            at(9_000, TuningSource::Model),
            "{key}"
        );
    }
}

#[test]
fn a_direct_model_reusing_its_providers_slot_carries_its_own_override() {
    // Arrange: `p1` already holds a slot (and stamp) from `Router::new`; a
    // direct model named after its provider reuses that state slot.
    let mut router = Router::new(Arc::new(parse(
        r#"
[providers.p1]
kind = "anthropic-api"
api_key_ref = "literal:k"
stream_first_byte_timeout_ms = 60000
"#,
    )));
    assert_eq!(
        first_byte(&router, "p1"),
        at(60_000, TuningSource::Provider)
    );
    let model =
        ResolvedModel::new("p1", "p1", provider(), "u").with_stream_first_byte_timeout_ms(5_000);

    // Act
    router.install_resolved_models(BTreeMap::from([("p1".to_string(), Arc::new(model))]));

    // Assert
    assert!(router.resolved_models.contains_key("p1"));
    assert_eq!(first_byte(&router, "p1"), at(5_000, TuningSource::Model));
}

#[test]
fn register_leaves_an_installed_models_stamp_in_place() {
    let mut router = pooled_router(Some(9_000));

    router.register("opus", provider());

    assert_eq!(first_byte(&router, "opus"), at(9_000, TuningSource::Model));
}

#[test]
fn register_stamps_an_unconfigured_name_with_the_defaults() {
    let mut router = Router::new(Arc::new(Config::default()));

    router.register("p-unconfigured", provider());

    assert_eq!(
        *router
            .tuning
            .get("p-unconfigured")
            .expect("stamped")
            .as_ref(),
        crate::config::resolve_tuning(&router.config, None, None)
    );
}

#[test]
fn an_untuned_slot_is_stamped_with_the_shipped_timeouts() {
    let mut router = Router::new(Arc::new(Config::default()));

    router.register("p-untuned", provider());

    let stamp = router.tuning.get("p-untuned").expect("stamped");
    assert_eq!(
        stamp.stream_first_byte_timeout_ms,
        at(300_000, TuningSource::ShippedDefault)
    );
    assert_eq!(
        stamp.request_timeout_ms,
        at(1_800_000, TuningSource::ShippedDefault)
    );
}
