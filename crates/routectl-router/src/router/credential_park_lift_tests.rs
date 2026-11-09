//! A reload lifts only the credential parks whose credential it may have
//! changed: the named OAuth seats a credentials reload changed (an
//! entry-shaped name among them lifts nothing), or the account of each
//! `[providers]` entry a config reload changed and no OAuth seat, never one
//! the new Router built as a placeholder. A lift starts a new generation on
//! each account it names, so an attempt admitted before it cannot re-park
//! that account, while an account it leaves alone keeps its generation and
//! its park.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use routectl_auth::SecretRef;
use routectl_core::{Provider, UpstreamReason};

use super::super::credential_park_dispatch_tests::{
    INVALID, ScriptedProvider, Step, park_reason, rejection,
};
use super::super::{Config, Router};
use crate::config::{ProviderEntry, ProviderRuntimePolicy};
use crate::pool_build::{PoolPlaceholder, PoolReport};
use crate::resolved::ResolvedModel;
use crate::runtime_state::{CREDENTIAL_PARK_BACKSTOP, UnusableCause};

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
    OauthSeats(&'static [&'static str]),
    EntryChange(&'static str),
}

/// The accounts `lift` reset, and how many of them had a park lifted.
fn apply(router: &Router, lift: Lift) -> (Vec<String>, usize) {
    let resets = match lift {
        Lift::Accounts(accounts) => router.lift_credential_parks(accounts),
        Lift::OauthSeats(seats) => router.lift_oauth_credential_parks(seats),
        Lift::EntryChange(entry) => router.lift_parks_for_entry_change_for_tests(entry),
    };
    let parks_lifted = resets.iter().filter(|r| r.outcome.park_lifted).count();
    (
        resets.into_iter().map(|r| r.account).collect(),
        parks_lifted,
    )
}

#[tokio::test]
async fn a_lift_clears_exactly_the_accounts_it_targets() {
    let rows: [(&str, Lift, [bool; 3]); 11] = [
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
        (
            "a changed entry's account",
            Lift::EntryChange("p-c"),
            [false, false, true],
        ),
        (
            "a changed entry whose model is on an oauth seat",
            Lift::EntryChange("p-a"),
            [false, false, false],
        ),
        (
            "oauth: one seat",
            Lift::OauthSeats(&["anthropic#b"]),
            [false, true, false],
        ),
        (
            "oauth: an entry-shaped name",
            Lift::OauthSeats(&["#p-c"]),
            [false, false, false],
        ),
        (
            "oauth: a seat beside an entry-shaped name",
            Lift::OauthSeats(&["anthropic#a", "#p-c"]),
            [true, false, false],
        ),
        (
            "oauth: a name of both shapes",
            Lift::OauthSeats(&["#anthropic#b"]),
            [false, false, false],
        ),
        (
            "oauth: no seat",
            Lift::OauthSeats(&[]),
            [false, false, false],
        ),
    ];
    for (name, lift, lifted) in rows {
        // Arrange
        let router = three_accounts();
        park_every_account(&router);

        // Act
        let (reset, count) = apply(&router, lift);

        // Assert
        for (model, lifted) in MODELS.into_iter().zip(lifted) {
            let expected = (!lifted).then_some(UpstreamReason::CredentialInvalid);
            assert_eq!(park_reason(&router, model), expected, "{name}: {model}");
        }
        assert_eq!(count, lifted.iter().filter(|l| **l).count(), "{name}");
        assert_eq!(reset.len(), count, "{name}: every reset account was parked");
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
        (
            "a changed entry's account",
            Lift::EntryChange("p-c"),
            [true, true, false],
        ),
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
    for lift in [
        Lift::Accounts(&["anthropic#a", "#p-c"]),
        Lift::EntryChange("p-c"),
    ] {
        // Arrange
        let router = three_accounts();

        // Act
        let (reset, count) = apply(&router, lift);

        // Assert
        assert_eq!((reset.len(), count), (0, 0), "{lift:?}");
        for model in MODELS {
            assert_eq!(
                router.credential_generation_for_tests(model),
                0,
                "{lift:?}: {model}"
            );
        }
    }
}

/// How a row's next config differs from the router's own.
#[derive(Clone, Copy, Debug)]
enum Edit {
    None,
    Runtime(&'static str),
    Add(&'static str),
    Remove(&'static str),
}

fn next_config(router: &Router, edit: Edit) -> Config {
    let mut next = Config::clone(&router.config);
    match edit {
        Edit::None => {}
        Edit::Runtime(entry) => {
            let edited = next
                .providers
                .remove(entry)
                .expect("the edited entry exists")
                .with_runtime(ProviderRuntimePolicy {
                    rpm_limit: Some(7),
                    ..ProviderRuntimePolicy::default()
                });
            next.providers.insert(entry.into(), edited);
        }
        Edit::Add(entry) => {
            next.providers
                .insert(entry.into(), ProviderEntry::anthropic_api("literal:k"));
        }
        Edit::Remove(entry) => {
            next.providers
                .remove(entry)
                .expect("the removed entry exists");
        }
    }
    next
}

#[tokio::test]
async fn a_config_lift_resets_only_the_accounts_of_changed_entries() {
    // (case, edit, park expected to lift on m-a, m-b, m-c)
    let rows: [(&str, Edit, [bool; 3]); 5] = [
        ("unchanged config", Edit::None, [false, false, false]),
        (
            "the entry edited",
            Edit::Runtime("p-c"),
            [false, false, true],
        ),
        (
            "a different entry edited",
            Edit::Runtime("p-a"),
            [false, false, false],
        ),
        ("an entry added", Edit::Add("p-new"), [false, false, false]),
        (
            "the entry removed",
            Edit::Remove("p-c"),
            [false, false, true],
        ),
    ];
    for (name, edit, lifted) in rows {
        // Arrange
        let router = three_accounts();
        park_every_account(&router);
        let next = next_config(&router, edit);

        // Act
        let resets = router.lift_entry_credential_parks(&router.config, &next);

        // Assert
        for (model, lifted) in MODELS.into_iter().zip(lifted) {
            let expected = (!lifted).then_some(UpstreamReason::CredentialInvalid);
            assert_eq!(park_reason(&router, model), expected, "{name}: {model}");
        }
        let accounts: Vec<&str> = resets.iter().map(|r| r.account.as_str()).collect();
        let expected: &[&str] = if lifted[2] { &["#p-c"] } else { &[] };
        assert_eq!(accounts, expected, "{name}");
    }
}

/// [`three_accounts`] with entry `p-c`'s account held as a placeholder seat
/// the build could not read, so it is marked unusable at install.
fn with_placeholder_on_p_c() -> Router {
    let mut router = three_accounts();
    router.install_pool_reports(vec![PoolReport {
        pool: "the-pool".into(),
        models: vec!["m-c".into()],
        configured_members: 1,
        usable_members: 0,
        placeholders: vec![PoolPlaceholder {
            member: "p-c".into(),
            provider_kind: "anthropic-api",
            cause: UnusableCause::Unreadable,
            account: Some("#p-c".into()),
        }],
    }]);
    router
}

#[tokio::test]
async fn a_config_lift_never_resets_an_account_built_as_a_placeholder() {
    // Arrange
    let router = with_placeholder_on_p_c();
    let marked = |router: &Router| {
        router
            .gate_status_for_tests("m-c")
            .credential_unusable
            .map(|mark| mark.cause)
    };
    assert_eq!(marked(&router), Some(UnusableCause::Unreadable), "premise");
    let next = next_config(&router, Edit::Runtime("p-c"));

    // Act
    let resets = router.lift_entry_credential_parks(&router.config, &next);

    // Assert
    assert!(resets.is_empty(), "{resets:?}");
    assert_eq!(marked(&router), Some(UnusableCause::Unreadable));
}
