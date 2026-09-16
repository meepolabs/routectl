//! Envelope-field verdict lifecycle: keying, two-phase learn, single-flight
//! admission, and the loopback-target mint suppression.
//!
//! The concrete sibling of the reasoning-replay lifecycle, deliberately NOT a
//! shared generic over both. The two identities have no field in common -- a
//! replay truth is keyed on a lane discriminant plus an artifact scheme, a
//! field truth on a target plus a qualified envelope path -- and one of them
//! carries an admission predicate the other has no notion of. A parameterized
//! guard over two callers that share only their shape would put the shape
//! under one roof and leave every actual rule in the caller.
//!
//! The learned-capability registry ([`LearnedCapabilityRegistry`]) owns storage
//! and decay, so warm rebuild, doctor surfacing and the events ledger all
//! apply to a field verdict unchanged: it rides the existing row shape as a
//! new string VALUE in an already open-set column, with no schema change and
//! no second store.
//!
//! - **Keying.** `(state_key, field capability key, provider_kind)`. The
//!   capability half is minted by the namespace owner
//!   ([`field_capability_key`]) from the qualified dotted path the upstream
//!   named, so this module cannot spell a key the grammar would refuse, and a
//!   path the grammar refuses mints no identity at all.
//! - **Two-phase learn.** A rejection alone persists NOTHING. It opens
//!   request-local provisional state; the verdict is persisted only once the
//!   repaired retry actually succeeds ([`FieldRepairGuard::commit`]). A repair
//!   that failed, or an error unrelated to the field, settles without learning
//!   ([`FieldRepairGuard::release`], and the same path on an unsettled
//!   `Drop`). This is the first verdict class that MOVES TRAFFIC rather than
//!   only logging a signal, so a single misread or transient upstream fault
//!   must not be able to mint a permanent negative.
//! - **Single-flight.** Only ONE in-flight request repairs an unknown or
//!   lapsed identity; concurrent callers are refused. Otherwise N parallel
//!   requests each get rejected and each repair, N times the cost for one
//!   fact.
//! - **Loopback suppression.** A target whose `base_url` is loopback can never
//!   mint. The rejection a local hop returns is not attributable to the wire
//!   format that hop was configured with: the configured kind answers "what
//!   dialect did the operator write", not "what actually rejected this".
//!   Suppression therefore keys on the base URL, never on the kind and never
//!   on "the base differs from the kind default" -- a remote mirror on a
//!   custom base does reject with its own envelope and is not suppressed.
//!   Classification is SYNTACTIC and bounded: address literals in every
//!   spelling, the reserved local name and its subtree, and a closed set of
//!   stock hosts-file aliases. It resolves nothing, so an arbitrary DNS name
//!   that a resolver points at a local address is deliberately outside the
//!   contract -- no resolver or network dependency enters the dispatch path.
//!   Inferring an address from a name's SHAPE was tried and removed: it is
//!   wrong in both directions at once, suppressing legitimate remote domains
//!   with numeric labels while still missing the other spellings the same
//!   wildcard-DNS services accept.
//!
//! # Emission
//!
//! A committed verdict returns a [`CapabilityLearnEvent`] and a cleared one a
//! [`CapabilityClearedEvent`], the same rows the replay lifecycle emits, for
//! the dispatch layer to push onto the usage-capture drain. Every string on
//! them is a normalized key or a closed-set token; nothing in this module's
//! API can accept a request body.

// The lifecycle is staged ahead of its dispatch caller: the repair action that
// admits through it lands in a later change, so until then only the tests
// call in.
#![allow(dead_code)]

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use routectl_core::capability::{
    EvidenceSource, FailurePhase, SignalTier, normalize_capability_key,
};

use crate::field_capability::field_capability_key;
use crate::learned_capability::{LearnedCapabilityRegistry, NegativeState};
use crate::router::{CapabilityClearedEvent, CapabilityLearnEvent};

/// The identity of one learned envelope-field truth: a qualified wire path
/// rejected by one configured target.
///
/// Identity is `(state_key, field capability key, provider_kind)`. The
/// capability half is minted by the namespace owner from the path, so a
/// malformed path yields no key rather than a permanent token nobody can
/// attribute; `provider_kind` rides along because every registry call
/// normalizes the capability key with it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FieldVerdictKey {
    state_key: String,
    capability_key: String,
    provider_kind: String,
}

impl FieldVerdictKey {
    /// Build the identity for the qualified dotted `field_path` an upstream
    /// rejection named on the target `state_key`, or `None` when no identity
    /// can be built for it.
    ///
    /// Two independent refusals, both upstream of every mint path:
    ///
    /// - the field namespace does not accept `field_path` as a qualified path;
    /// - this provider kind's capability-key normalization would REWRITE the
    ///   minted key. A lane whose normalizer reduces a dotted key to a shorter
    ///   form would collapse structurally distinct fields onto one token, and
    ///   because the token is permanent that collision could never be
    ///   un-minted. Refusing the identity excludes such a lane without
    ///   changing the shared normalizer, whose behavior other capability
    ///   classes depend on. A lane whose normalization is a pass-through for
    ///   this key is unaffected.
    #[must_use]
    pub fn new(state_key: &str, field_path: &str, provider_kind: &str) -> Option<Self> {
        let minted = field_capability_key(field_path)?;
        // Normalized once at construction so every registry call and the
        // emitted row meet on one canonical string -- and compared against the
        // minted bytes, so a lane that would not carry them acquires no
        // identity at all.
        if normalize_capability_key(&minted, provider_kind) != minted {
            return None;
        }
        Some(Self {
            state_key: state_key.to_string(),
            capability_key: minted,
            provider_kind: provider_kind.to_string(),
        })
    }

    /// The routing state key this identity is keyed on.
    #[must_use]
    pub fn state_key(&self) -> &str {
        &self.state_key
    }

    /// The normalized field capability key this identity is keyed on.
    #[must_use]
    pub fn capability_key(&self) -> &str {
        &self.capability_key
    }

    /// The provider-kind token this identity is keyed on.
    #[must_use]
    pub fn provider_kind(&self) -> &str {
        &self.provider_kind
    }

    /// Whether a registry snapshot row names this identity. The registry's own
    /// row key is `(state_key, normalized feature key)` -- the provider kind is
    /// the input that normalization consumed, not a third component -- so the
    /// comparison is over exactly those two halves.
    fn matches_registry_row(&self, state_key: &str, feature_key: &str) -> bool {
        self.state_key == state_key && self.capability_key == feature_key
    }
}

/// Two-phase, single-flight lifecycle over the learned-capability registry for
/// envelope-field verdicts.
#[derive(Debug)]
pub struct FieldVerdictRegistry {
    learned: Arc<LearnedCapabilityRegistry>,
    /// Identities whose repair is unresolved. Purely request-local
    /// coordination: nothing here is persisted, and every settlement path --
    /// including a dropped guard -- clears its entry.
    in_flight: Mutex<HashSet<FieldVerdictKey>>,
}

impl FieldVerdictRegistry {
    /// Wrap the shared learned-capability registry.
    #[must_use]
    pub fn new(learned: Arc<LearnedCapabilityRegistry>) -> Self {
        Self {
            learned,
            in_flight: Mutex::new(HashSet::new()),
        }
    }

    /// Claim the single-flight repair slot for `key` on a target reached at
    /// `target_base_url`.
    ///
    /// `Some(guard)` means THIS request may repair and settle the identity.
    /// `None` means it may not: the target is loopback and can never mint, an
    /// acting verdict is already resident, or another request holds the slot
    /// with its repair unresolved.
    ///
    /// A lapsed verdict is admissible: exactly one caller gets the guard and
    /// re-verifies the field against live upstream behavior.
    pub fn admit_provisional(
        &self,
        key: &FieldVerdictKey,
        target_base_url: &str,
        now: Instant,
    ) -> Option<FieldRepairGuard<'_>> {
        // Checked before the slot is claimed: a suppressed target must not
        // even hold a slot, or it would refuse a sibling request that could
        // legitimately mint.
        if loopback_target_suppresses_minting(target_base_url) {
            return None;
        }
        // The claim is taken under the in-flight lock together with the decay
        // read, so two callers racing an unknown identity cannot both observe
        // "absent" and both repair.
        let mut in_flight = self.in_flight.lock();
        if in_flight.contains(key) {
            return None;
        }
        match self.negative_state(key, now) {
            NegativeState::Acting => None,
            NegativeState::Absent | NegativeState::Lapsed => {
                in_flight.insert(key.clone());
                Some(FieldRepairGuard {
                    registry: self,
                    key: key.clone(),
                    settled: false,
                })
            }
        }
    }

    /// Whether an acting verdict currently applies to this identity,
    /// independent of any in-flight repair. Read-only: it never claims the
    /// slot. Test-only, because the dispatch path settles through
    /// `admit_provisional`, which reads the same state while claiming.
    #[cfg(test)]
    pub fn is_negative_acting(&self, key: &FieldVerdictKey, now: Instant) -> bool {
        matches!(self.negative_state(key, now), NegativeState::Acting)
    }

    /// The wrapped registry, so a test can plant a resident verdict through
    /// the registry's own carry-over seam rather than through this lifecycle.
    #[cfg(test)]
    pub fn learned(&self) -> &LearnedCapabilityRegistry {
        &self.learned
    }

    /// How many entries are resident in the wrapped registry. Test-only: it is
    /// what makes "persists nothing" an assertion about the store rather than
    /// only about one key's acting state.
    #[cfg(test)]
    pub fn snapshot_len(&self) -> usize {
        self.learned.snapshot().len()
    }

    fn negative_state(&self, key: &FieldVerdictKey, now: Instant) -> NegativeState {
        self.learned
            .negative_state(&key.state_key, &key.capability_key, &key.provider_kind, now)
    }

    fn release_slot(&self, key: &FieldVerdictKey) {
        self.in_flight.lock().remove(key);
    }
}

/// The single-flight repair claim for one envelope-field identity.
///
/// Holding this guard is the PROVISIONAL phase: no verdict is persisted while
/// it lives. Exactly one settlement applies:
///
/// - [`commit`](FieldRepairGuard::commit) -- the request was rejected AND the
///   repaired retry succeeded: persist (or refresh) the verdict.
/// - [`clear`](FieldRepairGuard::clear) -- the field was ACCEPTED: drop any
///   resident verdict.
/// - [`release`](FieldRepairGuard::release) -- the repair failed, or the
///   request hit an unrelated error: learn nothing, leave any resident entry
///   exactly as it was.
///
/// Dropping the guard without settling releases the slot as `release` would,
/// so an early return, a `?` propagation, or a client disconnect can never
/// strand an identity behind a permanently claimed slot -- and can never learn
/// by omission either.
#[derive(Debug)]
pub struct FieldRepairGuard<'a> {
    registry: &'a FieldVerdictRegistry,
    key: FieldVerdictKey,
    /// Set by whichever settlement runs, so the subsequent `Drop` cannot free
    /// a slot a different request has since claimed.
    settled: bool,
}

impl FieldRepairGuard<'_> {
    /// Phase two: the repaired retry succeeded, so the rejection is confirmed
    /// as a real envelope-field incompatibility. Persists the verdict
    /// (refreshing a resident or lapsed one) and returns the emission row for
    /// the capability-event sink.
    ///
    /// A refresh re-stamps the base decay window rather than applying the
    /// registry's geometric re-probe backoff: this is a corroborated
    /// self-identifying observation, not a failed probe, so a chronically
    /// rejecting field re-verifies once per base decay by design. The backoff
    /// ladder stays reserved for the registry's own re-probe path.
    ///
    /// `request_features` is the request's derived in-flight feature set; no
    /// request body can enter the row.
    #[must_use]
    pub fn commit(
        mut self,
        upstream_status: u16,
        request_features: Vec<String>,
        now: Instant,
    ) -> CapabilityLearnEvent {
        self.settled = true;
        let key = self.key.clone();
        // A rejection corroborated by a successful repaired retry is direct
        // proof, not an inference: it acts on this one observation.
        self.registry.learned.observe(
            &key.state_key,
            &key.capability_key,
            &key.provider_kind,
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            now,
        );
        let observations = self
            .registry
            .learned
            .snapshot()
            .into_iter()
            .find(|entry| key.matches_registry_row(&entry.state_key, &entry.feature_key))
            .map_or(0, |entry| entry.observations);
        self.registry.release_slot(&key);
        tracing::info!(
            event = "field_verdict_commit",
            state_key = %key.state_key,
            capability_key = %key.capability_key,
            upstream_status,
            observations,
            "envelope-field verdict persisted after a successful repaired retry",
        );
        CapabilityLearnEvent {
            state_key: key.state_key,
            capability_key: key.capability_key,
            provider_kind: key.provider_kind,
            signal_tier: SignalTier::SelfIdentifying,
            observations,
            upstream_status,
            remapped: false,
            request_features,
            phase: FailurePhase::F1,
            source: EvidenceSource::Live,
        }
    }

    /// The field was ACCEPTED: drop any resident verdict so the request shape
    /// is re-enabled at once rather than after the remaining decay.
    ///
    /// Returns a [`CapabilityClearedEvent`] when a resident entry was actually
    /// removed, so the caller rides the clear out on the dispatch meta and a
    /// warm rebuild does not resurrect the verdict from the ledger. An
    /// identity that had no resident entry clears nothing and returns `None`.
    pub fn clear(mut self) -> Option<CapabilityClearedEvent> {
        self.settled = true;
        let cleared = self.registry.learned.remove_keyed(
            &self.key.state_key,
            &self.key.capability_key,
            &self.key.provider_kind,
        );
        self.registry.release_slot(&self.key);
        if !cleared {
            return None;
        }
        tracing::info!(
            event = "field_verdict_clear",
            state_key = %self.key.state_key,
            capability_key = %self.key.capability_key,
            "envelope-field verdict cleared by an accepted request",
        );
        Some(CapabilityClearedEvent {
            state_key: self.key.state_key.clone(),
            capability_key: self.key.capability_key.clone(),
            provider_kind: self.key.provider_kind.clone(),
        })
    }

    /// Settle WITHOUT learning: the repair failed, or the request hit an error
    /// unrelated to the field. Any resident entry is left exactly as it was,
    /// so the next request re-verifies rather than inheriting a conclusion
    /// nothing proved. The dispatch path reaches this same no-learn settlement
    /// by dropping an unsettled guard (see [`Drop`]).
    pub fn release(mut self) {
        self.settled = true;
        self.registry.release_slot(&self.key);
    }
}

impl Drop for FieldRepairGuard<'_> {
    fn drop(&mut self) {
        if !self.settled {
            self.registry.release_slot(&self.key);
        }
    }
}

/// The reserved name whose whole subtree names a local destination
/// (RFC 6761): `localhost` itself, and anything under it.
const LOCALHOST_NAME: &str = "localhost";

/// Names a stock hosts file maps to a local address. A CLOSED set, matched
/// EXACTLY: the Debian-family entries plus the RHEL/Fedora ones. Exact match is
/// the whole discipline here -- a suffix or prefix rule over these would
/// suppress remote names that merely resemble one, which is the failure mode
/// that got a name-shape heuristic removed from this module.
const LOCAL_HOST_ALIASES: &[&str] = &[
    "localhost.localdomain",
    "ip6-localhost",
    "ip6-loopback",
    "localhost4",
    "localhost6",
    "localhost4.localdomain4",
    "localhost6.localdomain6",
];

/// True when a target reached at `base_url` must never mint an envelope-field
/// verdict, because that base URL names a LOCAL destination rather than a
/// remote upstream.
///
/// The predicate keys on the BASE URL, and on nothing else. A local hop is
/// configured with whatever wire format routectl speaks to it, while the real
/// upstream behind it may be a different dialect entirely, so the configured
/// kind cannot identify one -- and "the base differs from this kind's default"
/// cannot either, because a remote mirror also runs on a custom base and its
/// rejections ARE attributable.
///
/// Local means any of:
///
/// - a loopback address, in every spelling an operator can write: the whole
///   `127.0.0.0/8` range, `::1`, and the IPv4-mapped / IPv4-compatible forms;
/// - an UNSPECIFIED wildcard address (`0.0.0.0`, `::`), which is not a remote
///   host at all and reaches a local listener in practice;
/// - the RFC 6761 reserved name `localhost` or any subdomain of it;
/// - an exact match against the closed set of stock hosts-file aliases.
///
/// Terminal DNS root dots are trimmed before any name comparison, so a fully
/// qualified or malformed-but-local spelling classifies with its bare form.
///
/// Fails toward suppression: a scheme that is not http(s), and a base URL that
/// names no host (including a malformed address literal the parser refuses), are
/// all treated as local. Minting is the irreversible direction -- the token is
/// permanent and steers routing -- so a target this predicate cannot positively
/// identify as a remote upstream must not mint.
///
/// # What is deliberately OUT of scope
///
/// An arbitrary DNS name that RESOLVES to a loopback address is not caught, and
/// that is a design boundary rather than a gap to close incrementally. This
/// predicate is synchronous, on the dispatch path, and reads only the configured
/// string: no resolver, no network dependency, no clock. A name-shape heuristic
/// was tried here and removed, because approximating resolution from spelling is
/// wrong in BOTH directions at once -- it suppressed legitimate remote domains
/// carrying numeric labels, while still missing the prefixed, dashed and hex
/// spellings the same wildcard-DNS services accept. A partial classifier that
/// silently misroutes both ways is worse than a stated boundary, and the
/// residual exposure is bounded: one spurious verdict on a target the operator
/// deliberately aliased to a local listener.
pub fn loopback_target_suppresses_minting(base_url: &str) -> bool {
    let Ok(url) = url::Url::parse(base_url.trim()) else {
        return true;
    };
    // Classified before the host: an egress is http(s), so any other scheme
    // names something this predicate cannot attribute a rejection to -- and it
    // may still carry an ordinary-looking remote hostname.
    if url.scheme() != "http" && url.scheme() != "https" {
        return true;
    }
    match url.host() {
        Some(url::Host::Domain(domain)) => is_local_domain(domain),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback() || ip.is_unspecified(),
        // The native loopback check is REQUIRED for `::1`, not merely ordered
        // before the reduction: that address matches the IPv4-compatible prefix
        // (`::/96`) with an embedded quad of `0.0.0.1`, which is neither
        // loopback nor unspecified, so the reduction below cannot classify it at
        // all. Removing this check makes `::1` read as remote. The native
        // UNSPECIFIED check is redundant against the reduction (`::` reduces to
        // `0.0.0.0`, which the fallback accepts) and is kept to state the intent
        // at the point of decision rather than leave it resting on an
        // arithmetic coincidence.
        Some(url::Host::Ipv6(ip)) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip
                    .to_ipv4_mapped()
                    .or_else(|| crate::factory::ipv4_compatible_embedded(&ip))
                    .is_some_and(|v4| v4.is_loopback() || v4.is_unspecified())
        }
        None => true,
    }
}

/// True when `domain` names a local destination by NAME rather than by address
/// literal.
///
/// Every terminal DNS root dot is trimmed first. A single-dot strip would leave
/// `localhost..` with an empty last label, which is not the reserved name and
/// would mint -- so a malformed local spelling has to fail closed, and trimming
/// the whole run is what does it.
///
/// Two rules, both anchored, and deliberately no third:
///
/// - the RFC 6761 reserved name, or any subdomain of it. Compared on whole
///   LABELS, never a byte suffix: `notlocalhost` and `mylocalhost.example` are
///   ordinary remote hosts that merely contain those bytes, and
///   `localhost.upstream.example` is a remote name whose FIRST label happens to
///   be the reserved one.
/// - an exact match against the closed alias set. Exact, so it cannot creep into
///   a suffix heuristic that swallows `localhost4.upstream.example`.
///
/// Nothing here infers an address from the SHAPE of a name. See
/// [`loopback_target_suppresses_minting`] for why that inference was removed
/// rather than refined.
fn is_local_domain(domain: &str) -> bool {
    // The `url` crate already lowercases a parsed domain; the explicit
    // case-insensitive comparisons keep this correct for a direct caller too.
    let name = domain.trim_end_matches('.');
    name.eq_ignore_ascii_case(LOCALHOST_NAME)
        || name
            .rsplit_once('.')
            .is_some_and(|(_, last_label)| last_label.eq_ignore_ascii_case(LOCALHOST_NAME))
        || LOCAL_HOST_ALIASES
            .iter()
            .any(|alias| name.eq_ignore_ascii_case(alias))
}

#[cfg(test)]
#[path = "field_verdict_tests.rs"]
mod tests;
