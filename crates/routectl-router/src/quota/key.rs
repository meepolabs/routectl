//! The per-seat quota store key, and the ONE derivation both sides share.
//!
//! # Why the key is the ACCOUNT and never the model
//!
//! Two seat-shaped keys exist in this crate and they are not
//! interchangeable. `seat_pool::seat_state_key` is `"{nickname}#{label}"` --
//! MODEL-scoped, one entry per model per seat, which is exactly right for the
//! per-seat circuit breaker and RPM bucket it was built for.
//! `seat_pool::seat_identity` is the OAuth `provider#label` account key (bare
//! `provider` for a default seat) -- ACCOUNT-scoped, so several models sharing
//! one upstream account collapse onto one entry.
//!
//! Subscription quota is an ACCOUNT-level fact: the upstream reports one
//! budget per credential, no matter how many model nicknames route through
//! it. Keying this store on the model-scoped key would shard one account's
//! single reading across one entry per nickname, each only as fresh as the
//! last request for that nickname, all describing the same budget and all
//! racing to overwrite each other. A deployment running several nicknames
//! over two accounts would hold a double-digit number of entries for two real
//! readings.
//!
//! # Why the derivation is a type and not a convention
//!
//! The store is written post-response, where the account identity has already
//! been derived (`DispatchTarget::seat`, set from `seat_identity` at target
//! construction), and read at placement, where only a `SecretRef` is in hand.
//! Two sites deriving "the seat key" independently is how they end up
//! deriving different ones -- and that failure is SILENTLY GREEN: every write
//! lands, every read misses, every lane reads as no-evidence, and no test
//! built on hand-made keys can see it.
//!
//! So [`SeatKey`] has a private field and no public constructor, and the two
//! functions below are the only ways to obtain one. That much IS enforced: a
//! model-scoped `state_key` cannot be passed where a seat key is expected.
//!
//! The read side re-derives through `seat_pool::seat_identity` itself. The write
//! side wraps the identity a dispatch already derived through that same helper,
//! because a `DispatchTarget` carries the derived identity and not the
//! `SecretRef` it came from -- so its correctness rests on its CALLERS passing
//! `DispatchMeta::served_seat`, which both production sites do. That residual is
//! documented on the function rather than claimed away, and the live smoke is
//! what closes it: only a real multi-model, multi-account pool proves the two
//! sides agree.

use routectl_auth::SecretRef;

/// Store key for one credential seat: the OAuth `provider#label` ACCOUNT
/// identity, bare `provider` for a default seat.
///
/// The field is private and the two minting functions in this module are the
/// only constructors, so a model-scoped `state_key` cannot be passed where a
/// seat key is expected -- the compiler refuses it rather than a reviewer
/// having to notice.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SeatKey(String);

impl SeatKey {
    /// The account identity, for a diagnostic that needs to name the seat.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The store key for a credential seat, derived from that seat's own
/// `SecretRef` -- the READ side.
///
/// `None` for every non-OAuth scheme and for an absent ref, because
/// `seat_identity` yields an identity only for the OAuth arm: a `file://`
/// path or an `env://` variable name is not an account, and there is no
/// upstream quota budget to key by.
pub fn seat_key_for_secret_ref(secret_ref: Option<&SecretRef>) -> Option<SeatKey> {
    crate::seat_pool::seat_identity(secret_ref).map(SeatKey)
}

/// The store key for the seat that actually SERVED, from the identity a
/// dispatch already derived -- the WRITE side.
///
/// # What this does and does not guarantee
///
/// It wraps bytes; it does not re-derive them. The two production call sites
/// pass `DispatchMeta::served_seat`, which `mark_target` copies from
/// `DispatchTarget::seat`, which both seat-target builders set through
/// `seat_pool::seat_identity` -- so the write side reaches the same bytes as
/// [`seat_key_for_secret_ref`] through the same helper, one step earlier.
///
/// But that chain is a property of the CALLERS, not of this signature: a
/// `DispatchTarget` carries the derived identity and not the `SecretRef` it came
/// from, so this side cannot re-derive from source without threading the ref
/// through dispatch. A future caller passing a model-scoped `state_key` here
/// would mint a write key no read ever matches, and the failure is silently
/// green. Hence the name says `served_identity`: pass what a dispatch derived,
/// never a key you assembled. [`SeatKey`]'s private field stops the reverse
/// mistake -- a `state_key` cannot be used where a seat key is expected.
///
/// The live smoke is what closes this: it is the only check that the write and
/// read sides agree on a real multi-model, multi-account pool.
pub fn seat_key_for_served_identity(served_identity: &str) -> SeatKey {
    SeatKey(served_identity.to_string())
}

#[cfg(test)]
#[path = "key_tests.rs"]
mod key_tests;
