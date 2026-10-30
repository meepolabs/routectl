//! A reload lifts only the credential parks whose credential it may have
//! changed: a named set of accounts (the OAuth seats a credentials reload
//! changed), or every `[providers]` entry account and no OAuth seat (a config
//! reload). A lift starts a new generation on each account it names, so an
//! attempt admitted before it cannot re-park that account, while an account
//! it leaves alone keeps its generation and its park.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use routectl_auth::SecretRef;
use routectl_core::{Provider, UpstreamReason};

use super::super::credential_park_dispatch_tests::{
    INVALID, ScriptedProvider, Step, park_reason, rejection,
};
use super::super::{Config, Router};
use crate::config::ProviderEntry;
use crate::resolved::ResolvedModel;
use crate::runtime_state::CREDENTIAL_PARK_BACKSTOP;

/// `m-a` on OAuth seat `anthropic#a`, `m-b` on OAuth seat `anthropic#b`, and
/// `m-c` on entry `p-c` with a non-OAuth credential: three accounts.
fn three_accounts() -> Router {
    let mut config = Config::default();
    let mut models = BTreeMap::new();
    for (nickname, entry, seat) in [
        ("m-a", "p-a", Some("a")),
        ("m-b", "p-b", Some("b")),
        ("m-c", "p-c", None),
    ] {
        config
            .providers
            .insert(entry.into(), ProviderEntry::anthropic_api("literal:k"));
        let provider = ScriptedProvider::new(entry, &[Step::Ok]) as Arc<dyn Provider>;
        let mut model = ResolvedModel::new(nickname, entry, provider, format!("claude-{nickname}"));
        if let Some(label) = seat {
            model = model.with_auth_secret_ref(SecretRef::OAuth {
                provider: "anthropic".into(),
                label: Some(label.into()),
            });
        }
        models.insert(nickname.to_string(), Arc::new(model));
    }
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    router
}

const MODELS: [&str; 3] = ["m-a", "m-b", "m-c"];

fn park_every_account(router: &Router) {
    let until = Instant::now() + CREDENTIAL_PARK_BACKSTOP;
    for model in MODELS {
        assert!(
            router.park_credential_for_tests(model, UpstreamReason::CredentialInvalid, until),
            "{model} must own a state slot",
        );
    }
}

/// Which lift a row applies.
#[derive(Clone, Copy, Debug)]
enum Lift {
    Accounts(&'static [&'static str]),
    Entries,
}

fn apply(router: &Router, lift: Lift) -> usize {
    match lift {
        Lift::Accounts(accounts) => router.lift_credential_parks(accounts),
        Lift::Entries => router.lift_entry_credential_parks(),
    }
}

#[tokio::test]
async fn a_lift_clears_exactly_the_accounts_it_targets() {
    let rows: [(&str, Lift, [bool; 3]); 5] = [
        (
            "one oauth seat",
            Lift::Accounts(&["anthropic#b"]),
            [false, true, false],
        ),
        (
            "an entry account by name",
            Lift::Accounts(&["#p-c"]),
            [false, false, true],
        ),
        ("no account", Lift::Accounts(&[]), [false, false, false]),
        (
            "an account no target uses",
            Lift::Accounts(&["anthropic#gone"]),
            [false, false, false],
        ),
        ("every entry account", Lift::Entries, [false, false, true]),
    ];
    for (name, lift, lifted) in rows {
        // Arrange
        let router = three_accounts();
        park_every_account(&router);

        // Act
        let count = apply(&router, lift);

        // Assert
        for (model, lifted) in MODELS.into_iter().zip(lifted) {
            let expected = (!lifted).then_some(UpstreamReason::CredentialInvalid);
            assert_eq!(park_reason(&router, model), expected, "{name}: {model}");
        }
        assert_eq!(count, lifted.iter().filter(|l| **l).count(), "{name}");
    }
}

#[tokio::test]
async fn a_rejection_admitted_before_a_lift_parks_only_an_account_the_lift_left_alone() {
    let rows: [(&str, Lift, [bool; 3]); 2] = [
        (
            "one oauth seat",
            Lift::Accounts(&["anthropic#b"]),
            [true, false, true],
        ),
        ("every entry account", Lift::Entries, [true, true, false]),
    ];
    for (name, lift, parks) in rows {
        // Arrange: one attempt in flight on every account.
        let router = three_accounts();
        let admitted: Vec<u64> = MODELS
            .iter()
            .map(|model| {
                router
                    .admit_for_credential_tests(model)
                    .expect("a healthy account admits")
            })
            .collect();
        apply(&router, lift);

        // Act
        let parked: Vec<bool> = MODELS
            .iter()
            .zip(admitted)
            .map(|(model, generation)| {
                router.settle_credential_rejection_for_tests(
                    model,
                    &rejection("p", 403, INVALID),
                    generation,
                )
            })
            .collect();

        // Assert
        assert_eq!(parked, parks, "{name}");
        for (model, parks) in MODELS.into_iter().zip(parks) {
            let expected = parks.then_some(UpstreamReason::CredentialInvalid);
            assert_eq!(park_reason(&router, model), expected, "{name}: {model}");
        }
    }
}

/// An account no attempt has admitted on has nothing in flight a lift must
/// fence, so a lift leaves it without a record: its first admission starts
/// it at the same generation it would have without the lift.
#[tokio::test]
async fn a_lift_of_an_account_with_no_record_leaves_it_without_one() {
    for lift in [Lift::Accounts(&["anthropic#a", "#p-c"]), Lift::Entries] {
        // Arrange
        let router = three_accounts();

        // Act
        let count = apply(&router, lift);

        // Assert
        assert_eq!(count, 0, "{lift:?}");
        for model in MODELS {
            assert_eq!(
                router.credential_generation_for_tests(model),
                0,
                "{lift:?}: {model}"
            );
        }
    }
}
