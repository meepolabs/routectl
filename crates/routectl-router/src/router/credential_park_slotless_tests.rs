//! The account gate refuses a live park even for a state key with no model
//! slot: the park belongs to the account, so a target admitted outside the
//! slot table still cannot dial a parked credential. An unusable mark
//! refuses the same way. Only a usable, unparked or absent account keeps the
//! slotless allow. A slotless admission registers the account and carries its
//! credential generation, so a reset fences its settlement as on a slot.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use routectl_core::{EligibilityReason, Error, Provider, UpstreamReason};

use super::super::credential_park_dispatch_tests::{
    INVALID, ScriptedProvider, Step, park_reason, rejection,
};
use super::super::{Config, Router};
use super::{CREDENTIAL_PARKED_GATE_KIND, CREDENTIAL_UNUSABLE_GATE_KIND};
use crate::config::ProviderEntry;
use crate::resolved::ResolvedModel;
use crate::runtime_state::UnusableCause;

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

/// The gate kind and eligibility reason of a refusal, or `None` for an allow.
fn refusal(
    router: &Router,
    state_key: &str,
    account: Option<&str>,
) -> Option<(&'static str, EligibilityReason)> {
    let (refusal, mut guard) = router.admit_client(state_key, account);
    guard.release();
    refusal.map(|(kind, err)| match err {
        Error::LocallyUnavailable { reason, .. } => (kind, reason),
        other => panic!("a park refusal is a local refusal, got {other:?}"),
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
            EligibilityReason::AccountParked,
        ));
        assert_eq!(got, expected, "{case}");
    }
}

/// The credential generation a slotless admission on `account` captured.
fn slotless_generation(router: &Router, account: &str) -> u64 {
    let (refusal, mut guard) = router.admit_client(SLOTLESS, Some(account));
    guard.release();
    assert!(refusal.is_none(), "a healthy account admits");
    guard.credential_generation()
}

/// Settle a credential rejection of a slotless attempt admitted under
/// `generation`, returning whether it parked the account.
fn settle_rejection(router: &Router, generation: u64) -> bool {
    router.park_credential_on_rejection(
        Some(ACCOUNT),
        &rejection("p-a", 403, INVALID),
        false,
        generation,
    )
}

#[test]
fn a_slotless_admission_captures_the_generation_a_reset_started() {
    // Arrange
    let router = router();
    let first = slotless_generation(&router, ACCOUNT);
    let resets = router.lift_credential_parks([ACCOUNT]);

    // Act
    let after_reset = slotless_generation(&router, ACCOUNT);

    // Assert
    assert_eq!(
        resets.len(),
        1,
        "the slotless admission registered the account"
    );
    assert_eq!(after_reset, first.wrapping_add(1));
    assert!(after_reset > 0);
}

#[test]
fn a_reset_between_a_slotless_admission_and_its_settlement_fences_it() {
    // Arrange: an attempt admitted slotless on an account nothing else has
    // seen, then a reload resets the account.
    let router = router();
    let before = slotless_generation(&router, ACCOUNT);
    router.lift_credential_parks([ACCOUNT]);

    // Act
    let stale_parked = settle_rejection(&router, before);
    let stale_park = park_reason(&router, "m-a");
    let current = slotless_generation(&router, ACCOUNT);
    let current_parked = settle_rejection(&router, current);

    // Assert: the stale settlement parks nothing; an attempt admitted after
    // the reset parks under the generation its guard carries.
    assert!(!stale_parked);
    assert_eq!(stale_park, None);
    assert!(current_parked);
    assert_eq!(park_reason(&router, "m-a"), INVALID);
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
            EligibilityReason::AccountParked
        ))
    );
    assert_eq!(slotless, slotted);
}

#[test]
fn a_slotless_admission_refuses_an_unusable_account() {
    // (case, mark `#p-a` unusable, also park it, account admitted, expected)
    type Row = (
        &'static str,
        bool,
        bool,
        Option<&'static str>,
        Option<(&'static str, EligibilityReason)>,
    );
    let unusable = Some((
        CREDENTIAL_UNUSABLE_GATE_KIND,
        EligibilityReason::CredentialUnusable,
    ));
    let rows: [Row; 4] = [
        ("unusable", true, false, Some(ACCOUNT), unusable),
        ("unusable and parked", true, true, Some(ACCOUNT), unusable),
        ("another account", true, false, Some("#p-other"), None),
        ("no account", true, false, None, None),
    ];
    for (case, mark, parked, account, expected) in rows {
        // Arrange
        let router = router();
        if mark {
            assert!(router.mark_credential_unusable_for_tests("m-a", UnusableCause::Unreadable));
        }
        if parked {
            assert!(router.park_credential_for_tests(
                "m-a",
                UpstreamReason::CredentialInvalid,
                Instant::now() + Duration::from_hours(1),
            ));
        }

        // Act
        let got = refusal(&router, SLOTLESS, account);

        // Assert
        assert_eq!(got, expected, "{case}");
    }
}

#[test]
fn a_slotless_success_lifts_the_account_park_in_its_generation() {
    // (case, settle in the park's generation, expected park afterwards)
    let rows: [(&str, bool, Option<UpstreamReason>); 2] = [
        ("current generation", true, None),
        (
            "stale generation",
            false,
            Some(UpstreamReason::CredentialInvalid),
        ),
    ];
    for (case, current, expected) in rows {
        // Arrange: the account is parked by a slotless rejection.
        let router = router();
        let stale = slotless_generation(&router, ACCOUNT);
        router.lift_credential_parks([ACCOUNT]);
        let generation = slotless_generation(&router, ACCOUNT);
        assert!(settle_rejection(&router, generation), "{case}: premise");
        let settled_in = if current { generation } else { stale };

        // Act
        router.settle_seat_success_in_generation(SLOTLESS, Some(ACCOUNT), true, 0, settled_in);

        // Assert
        assert_eq!(park_reason(&router, "m-a"), expected, "{case}");
    }
}
