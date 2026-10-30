//! The account gate refuses a live park even for a state key with no model
//! slot: the park belongs to the account, so a target admitted outside the
//! slot table still cannot dial a parked credential. Only an unparked or
//! absent account keeps the slotless allow.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use routectl_core::{Error, Provider, UpstreamReason};

use super::super::credential_park_dispatch_tests::{ScriptedProvider, Step};
use super::super::{Config, Router};
use super::CREDENTIAL_PARKED_GATE_KIND;
use crate::config::ProviderEntry;
use crate::resolved::ResolvedModel;

const SLOTLESS: &str = "no-such-slot";
const ACCOUNT: &str = "#p-a";

/// `m-a` on entry `p-a`, so its account `#p-a` can be parked through a real
/// target.
fn router() -> Router {
    let mut config = Config::default();
    config
        .providers
        .insert("p-a".into(), ProviderEntry::anthropic_api("literal:k"));
    let provider = ScriptedProvider::new("p-a", &[Step::Ok]) as Arc<dyn Provider>;
    let models = BTreeMap::from([(
        "m-a".to_string(),
        Arc::new(ResolvedModel::new("m-a", "p-a", provider, "claude-m-a")),
    )]);
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    router
}

/// The gate kind and credential reason of a refusal, or `None` for an allow.
fn refusal(
    router: &Router,
    state_key: &str,
    account: Option<&str>,
) -> Option<(&'static str, UpstreamReason)> {
    let (refusal, mut guard) = router.admit_client(state_key, account, "p-a");
    guard.release();
    refusal.map(|(kind, err)| match err {
        Error::Credential { reason, .. } => (kind, reason),
        other => panic!("a park refusal is a credential error, got {other:?}"),
    })
}

#[test]
fn a_slotless_admission_refuses_only_a_live_account_park() {
    // (case, park lifetime on `#p-a`, account admitted, expected refusal)
    let rows: [(&str, Option<Duration>, Option<&str>, bool); 4] = [
        (
            "live park",
            Some(Duration::from_hours(1)),
            Some(ACCOUNT),
            true,
        ),
        ("lapsed park", Some(Duration::ZERO), Some(ACCOUNT), false),
        ("account with no record", None, Some("#p-other"), false),
        ("no account", Some(Duration::from_hours(1)), None, false),
    ];
    for (case, park_for, account, refused) in rows {
        // Arrange
        let router = router();
        if let Some(park_for) = park_for {
            assert!(router.park_credential_for_tests(
                "m-a",
                UpstreamReason::CredentialInvalid,
                Instant::now() + park_for,
            ));
        }

        // Act
        let got = refusal(&router, SLOTLESS, account);

        // Assert
        let expected = refused.then_some((
            CREDENTIAL_PARKED_GATE_KIND,
            UpstreamReason::CredentialInvalid,
        ));
        assert_eq!(got, expected, "{case}");
    }
}

#[test]
fn a_slotless_park_refusal_matches_the_slotted_one() {
    // Arrange
    let router = router();
    assert!(router.park_credential_for_tests(
        "m-a",
        UpstreamReason::CredentialExpired,
        Instant::now() + Duration::from_hours(1),
    ));

    // Act
    let slotted = refusal(&router, "m-a", Some(ACCOUNT));
    let slotless = refusal(&router, SLOTLESS, Some(ACCOUNT));

    // Assert
    assert_eq!(
        slotted,
        Some((
            CREDENTIAL_PARKED_GATE_KIND,
            UpstreamReason::CredentialExpired
        ))
    );
    assert_eq!(slotless, slotted);
}
