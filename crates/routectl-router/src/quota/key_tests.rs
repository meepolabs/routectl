//! Tests for the store key derivation.
//!
//! The load-bearing assertions here are the two that pin the key to the ACCOUNT
//! scope: one seat's identity is the same key for every model that routes
//! through it, and the model-scoped `state_key` is not reachable as a key at
//! all. Neither can be observed from a test that builds keys by hand, which is
//! why they are pinned structurally instead.

use super::*;

use routectl_auth::SecretRef;

fn oauth(provider: &str, label: Option<&str>) -> SecretRef {
    SecretRef::OAuth {
        provider: provider.to_string(),
        label: label.map(str::to_string),
    }
}

#[test]
fn a_default_seats_key_is_the_bare_provider() {
    let key = seat_key_for_secret_ref(Some(&oauth("anthropic", None)));

    assert_eq!(
        key.expect("an oauth ref yields a key").as_str(),
        "anthropic"
    );
}

#[test]
fn a_labeled_seats_key_is_the_provider_and_label() {
    let key = seat_key_for_secret_ref(Some(&oauth("anthropic", Some("seat-b"))));

    assert_eq!(
        key.expect("an oauth ref yields a key").as_str(),
        "anthropic#seat-b"
    );
}

#[test]
fn the_read_side_and_the_write_side_derive_the_same_key_for_one_seat() {
    let secret_ref = oauth("anthropic", Some("seat-b"));

    // The read side derives from the seat's own ref; the write side takes the
    // identity a dispatch target already carries, which the target builders set
    // from the same helper.
    let read = seat_key_for_secret_ref(Some(&secret_ref));
    let write = crate::seat_pool::seat_identity(Some(&secret_ref))
        .as_deref()
        .map(seat_key_for_served_identity);

    assert_eq!(
        read, write,
        "the two sides must agree byte-for-byte or every read misses every \
         write while both look healthy"
    );
}

/// The whole point of the account scope: several MODELS on one credential are
/// one budget, so they must be one key. A model-scoped key would give this pair
/// two entries describing the same upstream reading.
#[test]
fn two_models_sharing_one_credential_collapse_onto_one_key() {
    let shared = oauth("anthropic", Some("seat-b"));

    let from_one_model = seat_key_for_secret_ref(Some(&shared));
    let from_another_model = seat_key_for_secret_ref(Some(&shared));

    assert_eq!(from_one_model, from_another_model);
    assert_eq!(
        from_one_model.expect("an oauth ref yields a key").as_str(),
        "anthropic#seat-b",
        "the key must carry no model dimension at all"
    );
}

/// The negative control for the account scope. `seat_state_key` is the
/// model-scoped key the breaker and RPM bucket use, and it must never equal a
/// store key -- a store keyed on it would shard one account's reading per model.
#[test]
fn the_model_scoped_state_key_is_not_what_a_store_key_derives_to() {
    let state_key = crate::seat_pool::seat_state_key("opus", Some("seat-b"));
    let store_key = seat_key_for_secret_ref(Some(&oauth("anthropic", Some("seat-b"))))
        .expect("an oauth ref yields a key");

    assert_eq!(state_key, "opus#seat-b");
    assert_ne!(
        store_key.as_str(),
        state_key,
        "the store key is the OAuth account, never the nickname-scoped \
         runtime-state key"
    );
}

#[test]
fn a_non_oauth_ref_yields_no_key() {
    let env_ref = SecretRef::Env("ANTHROPIC_API_KEY".to_string());

    assert_eq!(seat_key_for_secret_ref(Some(&env_ref)), None);
    assert_eq!(seat_key_for_secret_ref(None), None);
}

/// The write side takes an identity that is already known to exist, so the
/// absent case is the CALLER's skip -- `served_seat` is `None` for a
/// pre-dispatch failure, a non-OAuth credential, and a forwarded credential, and
/// none of those is an account to key by.
#[test]
fn the_write_side_key_is_the_identity_verbatim() {
    assert_eq!(
        seat_key_for_served_identity("anthropic#seat-b").as_str(),
        "anthropic#seat-b"
    );
}
