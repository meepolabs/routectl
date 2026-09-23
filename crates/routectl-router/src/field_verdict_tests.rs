use std::sync::Arc;
use std::sync::Barrier;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use routectl_core::capability::{
    EvidenceSource, FailurePhase, SignalTier, normalize_capability_key,
};

use super::*;
use crate::field_capability::field_capability_key;
use crate::learned_capability::{DEFAULT_MAX_ENTRIES, EntryVerdict, ExportedEntry};

const DECAY: Duration = Duration::from_hours(48);
const WINDOW: Duration = Duration::from_hours(1);

/// Stage 1 mints on this lane only.
const ANTHROPIC: &str = "anthropic-api";

/// The qualified dotted path a real anthropic-api rejection names. Kept as
/// one const so every assertion below is about the same wire bytes.
const GROUNDED_PATH: &str = "thinking.enabled.display";

/// The public Anthropic endpoint: a target that MAY mint.
const REMOTE_BASE: &str = "https://api.anthropic.com";

/// The local hop: configured `anthropic-api`, loopback base, fronting some
/// other dialect entirely. A target that may NEVER mint.
const LOOPBACK_BASE: &str = "http://127.0.0.1:8787";

/// Every provider kind the config accepts, so a suppression claim can be
/// shown to follow the base URL rather than the configured kind.
const EVERY_KIND: [&str; 5] = [
    "openai-compat",
    "anthropic-api",
    "bedrock",
    "openai-responses",
    "gemini",
];

fn registry() -> Arc<FieldVerdictRegistry> {
    Arc::new(FieldVerdictRegistry::new(Arc::new(
        LearnedCapabilityRegistry::new(DECAY, WINDOW, DEFAULT_MAX_ENTRIES),
    )))
}

fn key(state_key: &str) -> FieldVerdictKey {
    FieldVerdictKey::new(state_key, GROUNDED_PATH, ANTHROPIC).expect("a qualified path mints a key")
}

/// Plant a resident, acting field negative the way existing capability tests
/// plant negatives: through the registry's own carry-over import seam, never
/// by parsing a rejection envelope.
fn plant_acting_negative(learned: &LearnedCapabilityRegistry, k: &FieldVerdictKey, now: Instant) {
    learned.import_entries(vec![ExportedEntry {
        state_key: k.state_key().to_string(),
        feature_key: k.capability_key().to_string(),
        verdict: EntryVerdict::Negative,
        signal: SignalTier::SelfIdentifying,
        observations: 1,
        first_seen: now,
        last_seen: now,
        expires_at: now + DECAY,
        evidence_class: None,
        phase: FailurePhase::F1,
        source: EvidenceSource::Live,
        in_flight: false,
        consecutive_failed_probes: 0,
    }]);
}

// --- keying ---

#[test]
fn two_distinct_field_paths_are_distinct_entries() {
    // Arrange -- one target, two envelope fields it rejected.
    let reg = registry();
    let t0 = Instant::now();
    let display = FieldVerdictKey::new("t", GROUNDED_PATH, ANTHROPIC).expect("accepted");
    let budget = FieldVerdictKey::new("t", "thinking.budget_tokens", ANTHROPIC).expect("accepted");

    // Act -- learn the display field only.
    let _ = reg
        .admit_provisional(&display, REMOTE_BASE, 1, t0)
        .expect("an unknown pair admits one repair")
        .commit(400, vec![], t0)
        .expect("a live commit emits its row");

    // Assert -- the sibling field inherits nothing.
    assert!(reg.is_negative_acting(&display, t0));
    assert!(!reg.is_negative_acting(&budget, t0));
}

#[test]
fn the_same_field_on_two_targets_is_two_distinct_entries() {
    // Arrange -- one field path, rejected by two configured targets.
    let reg = registry();
    let t0 = Instant::now();
    let here = FieldVerdictKey::new("target-a", "thinking", ANTHROPIC).expect("accepted");
    let other_target = FieldVerdictKey::new("target-b", "thinking", ANTHROPIC).expect("accepted");

    // Act
    let _ = reg
        .admit_provisional(&here, REMOTE_BASE, 1, t0)
        .expect("unknown pair admits")
        .commit(400, vec![], t0)
        .expect("a live commit emits its row");

    // Assert -- a fact proven about one target says nothing about another.
    assert!(reg.is_negative_acting(&here, t0));
    assert!(!reg.is_negative_acting(&other_target, t0));
}

#[test]
fn the_identity_carries_the_provider_kind_the_key_was_normalized_under() {
    // Arrange -- the same target and field reached under two kinds. The
    // provider kind is part of the guard's single-flight identity, and it is
    // the input the shared registry normalizes the capability key with; it is
    // NOT a third component of the registry's own row key. So two kinds whose
    // normalization agrees deliberately share ONE persisted row -- this
    // lifecycle rides the existing storage contract rather than redefining it.
    let reg = registry();
    let t0 = Instant::now();
    let anthropic = FieldVerdictKey::new("t", "thinking", ANTHROPIC).expect("accepted");
    let compat = FieldVerdictKey::new("t", "thinking", "openai-compat").expect("accepted");
    assert_ne!(anthropic, compat, "the guard identity keeps the kind apart");
    assert_eq!(
        anthropic.capability_key(),
        compat.capability_key(),
        "neither kind's normalizer rewrites a field key"
    );

    // Act
    let _ = reg
        .admit_provisional(&anthropic, REMOTE_BASE, 1, t0)
        .expect("unknown pair admits")
        .commit(400, vec![], t0)
        .expect("a live commit emits its row");

    // Assert -- one row, and the single-flight slot is still per-identity: the
    // second kind is refused because the verdict now ACTS, not because it
    // shares a slot.
    assert_eq!(reg.snapshot_len(), 1);
    assert!(reg.is_negative_acting(&compat, t0));
}

#[test]
fn a_path_the_namespace_grammar_refuses_mints_no_key() {
    // Arrange / Act / Assert -- the namespace owner decides what a path is;
    // this lifecycle cannot route around it.
    for path in ["", ".thinking", "thinking..display", "thinking display"] {
        assert!(
            FieldVerdictKey::new("t", path, ANTHROPIC).is_none(),
            "a malformed path must mint no key: {path:?}"
        );
    }
    assert!(FieldVerdictKey::new("t", GROUNDED_PATH, ANTHROPIC).is_some());
}

#[test]
fn the_key_carries_the_normalized_field_capability_key() {
    // Arrange
    let minted = field_capability_key(GROUNDED_PATH).expect("accepted");

    // Act
    let k = key("t");

    // Assert -- normalized once at construction, so the registry row and the
    // emitted event meet on one canonical string.
    assert_eq!(k.state_key(), "t");
    assert_eq!(
        k.capability_key(),
        normalize_capability_key(&minted, ANTHROPIC)
    );
    assert_eq!(k.provider_kind(), ANTHROPIC);
}

// --- two-phase learn ---

#[test]
fn the_rejection_alone_persists_nothing_before_the_repair_succeeds() {
    // Arrange -- the upstream rejected the field; the guard is the whole of
    // the provisional phase.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");

    // Act
    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, 1, t0)
        .expect("unknown pair admits");

    // Assert -- nothing resident, nothing acting, no row emitted yet.
    assert!(!reg.is_negative_acting(&k, t0));
    assert!(reg.snapshot_len() == 0, "a rejection must persist no entry");
    drop(guard);
}

#[test]
fn a_successful_repaired_retry_commits_the_field_negative() {
    // Arrange
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, 1, t0)
        .expect("unknown pair admits");

    // Act -- the repaired retry came back 2xx, so the rejection is confirmed.
    let event = guard
        .commit(400, vec!["thinking".to_string()], t0)
        .expect("a live commit emits its row");

    // Assert
    assert!(reg.is_negative_acting(&k, t0));
    assert_eq!(event.observations, 1);
    assert_eq!(event.capability_key, k.capability_key());
}

#[test]
fn commit_emits_its_own_captured_count_despite_a_sibling_observation_during_the_pause() {
    // Arrange -- admit a guard, then install a hook that runs a sibling
    // observation on the SAME key after the guard's own observe has
    // returned but before its event is built.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, 1, t0)
        .expect("unknown pair admits");
    let sibling_reg = Arc::clone(&reg);
    let state_key = k.state_key().to_string();
    let capability_key = k.capability_key().to_string();
    let provider_kind = k.provider_kind().to_string();
    reg.learned().set_post_observe_test_hook(Box::new(move || {
        let _ = sibling_reg
            .learned()
            .observe_in_generation_with_observations(
                1,
                &state_key,
                &capability_key,
                &provider_kind,
                SignalTier::SelfIdentifying,
                FailurePhase::F1,
                EvidenceSource::Live,
                None,
                t0,
            );
    }));

    // Act
    let event = guard
        .commit(400, vec![], t0)
        .expect("a live commit emits its row");

    // Assert -- the emitted event carries the count this commit's OWN
    // guarded read captured, not the resident count as it stands after the
    // sibling's later observation.
    assert_eq!(
        event.observations, 1,
        "the emitted event must retain its own captured count"
    );
    let resident = reg
        .learned()
        .snapshot()
        .into_iter()
        .find(|entry| entry.state_key == k.state_key() && entry.feature_key == k.capability_key())
        .expect("a resident entry after both observations");
    assert_eq!(
        resident.observations, 2,
        "the sibling observation must have bumped the resident count"
    );
}

#[test]
fn a_failed_repair_leaves_resident_state_unchanged() {
    // Arrange -- an entry learned earlier, now lapsed and re-verifying.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    let _ = reg
        .admit_provisional(&k, REMOTE_BASE, 1, t0)
        .expect("unknown pair admits")
        .commit(400, vec![], t0)
        .expect("a live commit emits its row");
    let lapsed = t0 + DECAY + Duration::from_secs(1);
    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, 1, lapsed)
        .expect("a lapsed entry admits one re-verification");

    // Act -- the repair itself failed, so nothing was proven either way.
    guard.release();

    // Assert -- neither refreshed nor cleared: the entry survives on its
    // ORIGINAL window and the next request re-verifies.
    assert!(reg.is_negative_acting(&k, t0 + DECAY / 2));
    assert!(reg.admit_provisional(&k, REMOTE_BASE, 1, lapsed).is_some());
}

#[test]
fn an_unrelated_error_learns_nothing_on_an_unknown_pair() {
    // Arrange
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, 1, t0)
        .expect("unknown pair admits");

    // Act -- a timeout, a 5xx, a disconnect: not evidence about the field.
    guard.release();

    // Assert
    assert!(!reg.is_negative_acting(&k, t0));
    assert_eq!(reg.snapshot_len(), 0);
    assert!(reg.admit_provisional(&k, REMOTE_BASE, 1, t0).is_some());
}

#[test]
fn dropping_an_unsettled_guard_releases_its_slot_and_learns_nothing() {
    // Arrange -- a dispatch path that returns early never settles.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");

    // Act
    drop(
        reg.admit_provisional(&k, REMOTE_BASE, 1, t0)
            .expect("unknown pair admits"),
    );

    // Assert -- no learning by omission, and the slot is free again.
    assert!(!reg.is_negative_acting(&k, t0));
    assert_eq!(reg.snapshot_len(), 0);
    assert!(reg.admit_provisional(&k, REMOTE_BASE, 1, t0).is_some());
}

#[test]
fn a_successful_retry_clears_a_resident_field_negative() {
    // Arrange -- a planted, resident negative that has since lapsed.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    plant_acting_negative(reg.learned(), &k, t0);
    let lapsed = t0 + DECAY + Duration::from_secs(1);
    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, 1, lapsed)
        .expect("a lapsed entry admits one re-verification");

    // Act -- upstream now accepts the field.
    let cleared = guard.clear();

    // Assert -- dropped at once, and the removal rides out on an event so a
    // warm rebuild cannot resurrect it.
    let cleared = cleared.expect("a resident entry was removed");
    assert_eq!(cleared.capability_key, k.capability_key());
    assert_eq!(cleared.state_key, k.state_key());
    assert_eq!(cleared.provider_kind, k.provider_kind());
    assert!(!reg.is_negative_acting(&k, lapsed));
    assert_eq!(reg.snapshot_len(), 0);
}

#[test]
fn clearing_a_pair_that_had_no_resident_entry_emits_nothing() {
    // Arrange
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, 1, t0)
        .expect("unknown pair admits");

    // Act
    let cleared = guard.clear();

    // Assert
    assert!(cleared.is_none());
    assert_eq!(reg.snapshot_len(), 0);
}

// --- single-flight ---

#[test]
fn two_racing_callers_yield_exactly_one_holder() {
    // Arrange
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    let holders = Arc::new(AtomicUsize::new(0));
    const CALLERS: usize = 2;

    // Act -- both callers race the same unknown pair. The barrier holds each
    // thread past its admission BEFORE any holder settles, so no admission
    // can observe a freed slot: the count is decided by the claim, never by
    // scheduler timing.
    let barrier = Arc::new(Barrier::new(CALLERS));
    let handles: Vec<_> = (0..CALLERS)
        .map(|_| {
            let reg = Arc::clone(&reg);
            let holders = Arc::clone(&holders);
            let barrier = Arc::clone(&barrier);
            let k = k.clone();
            thread::spawn(move || {
                let guard = reg.admit_provisional(&k, REMOTE_BASE, 1, t0);
                if guard.is_some() {
                    holders.fetch_add(1, Ordering::SeqCst);
                }
                barrier.wait();
                if let Some(guard) = guard {
                    guard.release();
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("no thread panics");
    }

    // Assert -- one repair in flight for one fact.
    assert_eq!(holders.load(Ordering::SeqCst), 1);
}

#[test]
fn a_concurrent_caller_is_refused_while_the_repair_is_unresolved() {
    // Arrange
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, 1, t0)
        .expect("unknown pair admits");

    // Act / Assert -- no negative is resident yet, and the second caller is
    // still refused rather than mounting its own repair.
    assert!(!reg.is_negative_acting(&k, t0));
    assert!(reg.admit_provisional(&k, REMOTE_BASE, 1, t0).is_none());

    // Once the repair settles the slot reopens.
    guard.release();
    assert!(reg.admit_provisional(&k, REMOTE_BASE, 1, t0).is_some());
}

#[test]
fn a_resident_acting_negative_refuses_admission() {
    // Arrange -- a planted verdict inside its decay window.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    plant_acting_negative(reg.learned(), &k, t0);

    // Act / Assert
    assert!(reg.is_negative_acting(&k, t0));
    assert!(reg.admit_provisional(&k, REMOTE_BASE, 1, t0).is_none());
}

// --- loopback suppression ---

#[test]
fn a_loopback_anthropic_target_cannot_mint_a_field_verdict() {
    // Arrange -- the local hop: configured anthropic-api, loopback base.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("local-hop");

    // Act
    let admitted = reg.admit_provisional(&k, LOOPBACK_BASE, 1, t0);

    // Assert -- refused at admission, which is the only mint path, so no
    // rejection reaching this target can ever persist a verdict.
    assert!(admitted.is_none());
    assert!(!reg.is_negative_acting(&k, t0));
    assert_eq!(reg.snapshot_len(), 0);
}

#[test]
fn the_default_anthropic_target_can_mint_a_field_verdict() {
    // Arrange -- the accept control for the suppression above: without it
    // the refusal could be a lifecycle that admits nothing at all.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("cloud");

    // Act
    let event = reg
        .admit_provisional(&k, REMOTE_BASE, 1, t0)
        .expect("the public Anthropic endpoint may mint")
        .commit(400, vec![], t0)
        .expect("a live commit emits its row");

    // Assert
    assert!(reg.is_negative_acting(&k, t0));
    assert_eq!(event.state_key, "cloud");
}

#[test]
fn a_non_loopback_custom_base_is_not_suppressed() {
    // Arrange -- an anthropic-api entry on a NON-default, non-loopback base.
    // The cache-capability precedent fails closed on exactly this shape; the
    // suppression predicate deliberately does not, because a remote mirror
    // does reject with its own envelope.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("mirror");

    // Act
    let admitted = reg.admit_provisional(&k, "https://anthropic.upstream.example/v1", 1, t0);

    // Assert
    assert!(admitted.is_some());
    assert!(!loopback_target_suppresses_minting(
        "https://anthropic.upstream.example/v1"
    ));
}

#[test]
fn suppression_follows_the_base_url_and_not_the_configured_kind() {
    // Arrange / Act / Assert -- for every configured kind, the loopback base
    // suppresses and the remote base does not. A predicate keyed on the kind
    // could not produce this pattern.
    for kind in EVERY_KIND {
        let reg = registry();
        let t0 = Instant::now();
        let k = FieldVerdictKey::new("t", "thinking", kind).expect("accepted");

        assert!(
            reg.admit_provisional(&k, LOOPBACK_BASE, 1, t0).is_none(),
            "a loopback target must not mint on kind {kind}"
        );
        assert!(
            reg.admit_provisional(&k, REMOTE_BASE, 1, t0).is_some(),
            "a remote target must mint on kind {kind}"
        );
    }
}

#[test]
fn every_loopback_spelling_suppresses_and_no_remote_host_does() {
    // Arrange / Act / Assert -- the whole loopback range and its IPv6 forms,
    // including the mapped and compatible spellings an operator can write.
    for base in [
        "http://127.0.0.1:8787",
        "http://127.0.0.1",
        "https://127.0.0.1:8443/v1",
        "http://127.1.2.3:9000",
        "http://localhost:8787",
        "http://LOCALHOST:8787",
        "http://[::1]:8787",
        "http://[::ffff:127.0.0.1]:8787",
        "http://[::127.0.0.1]:8787",
        "http://user:secret@127.0.0.1:8787/v1",
    ] {
        assert!(
            loopback_target_suppresses_minting(base),
            "a loopback base must suppress minting: {base}"
        );
    }

    // The reject side: real remote hosts, plus near-miss addresses that are
    // NOT loopback. Without these the predicate could be one that suppresses
    // everything.
    for base in [
        REMOTE_BASE,
        "https://anthropic.upstream.example/v1",
        "http://128.0.0.1:8787",
        "http://10.0.0.1:8787",
        "http://126.255.255.255",
        "http://[::2]:8787",
        "https://localhost.upstream.example",
        "https://notlocalhost",
    ] {
        assert!(
            !loopback_target_suppresses_minting(base),
            "a remote base must not suppress minting: {base}"
        );
    }
}

#[test]
fn a_base_url_naming_no_reachable_host_suppresses_minting() {
    // Arrange / Act / Assert -- a target this predicate cannot resolve to a
    // remote host is treated as local: minting a permanent, routing-affecting
    // verdict is the irreversible direction, so an unreadable target fails
    // toward silence. Bedrock's empty base_url lands here, which matches the
    // Stage 1 exclusion of that lane.
    //
    // `127.0.0.1.0` belongs here rather than with any name rule: the parser
    // reads a five-label all-numeric host as a malformed IPv4 literal and
    // refuses it outright, so what suppresses it is the parse failure -- the
    // only branch that can be credited with it.
    for base in ["", "   ", "not a url", "https://", "http://127.0.0.1.0"] {
        assert!(
            loopback_target_suppresses_minting(base),
            "an unresolvable base must suppress minting: {base:?}"
        );
        assert!(
            url::Url::parse(base.trim()).is_err()
                || url::Url::parse(base.trim()).unwrap().host().is_none(),
            "the fixture must be unparseable or hostless, or it is not this branch's evidence: {base:?}"
        );
    }
}

#[test]
fn a_non_http_scheme_suppresses_minting_whatever_host_it_names() {
    // Arrange / Act / Assert -- an egress is http(s); anything else is not a
    // target whose rejection this predicate can attribute. Classified BEFORE
    // the host, so a non-http scheme carrying a perfectly ordinary remote
    // hostname is still refused rather than read as a mintable target.
    for base in [
        "file:///tmp/socket",
        "unix:/var/run/upstream.sock",
        "ws://upstream.example/v1",
        "wss://upstream.example/v1",
        "ftp://upstream.example",
        "data:text/plain,upstream",
        "HTTPX://upstream.example",
    ] {
        assert!(
            loopback_target_suppresses_minting(base),
            "a non-http(s) scheme must suppress minting: {base}"
        );
    }

    // The accept control: the same remote host over each allowed scheme,
    // including mixed case, which the parser lowercases.
    for base in [
        "http://upstream.example/v1",
        "https://upstream.example/v1",
        "HTTPS://upstream.example/v1",
    ] {
        assert!(
            !loopback_target_suppresses_minting(base),
            "an http(s) remote target must not suppress minting: {base}"
        );
    }
}

#[test]
fn the_rfc_6761_localhost_spellings_and_a_trailing_root_dot_suppress_minting() {
    // Arrange / Act / Assert -- `localhost` is reserved by RFC 6761 together
    // with its whole subtree, and a fully qualified name may carry one
    // terminal DNS root dot. Both spellings name the same local destination,
    // so both must suppress.
    for base in [
        "http://localhost.",
        "http://localhost.:8787",
        "http://api.localhost:8787",
        "http://api.localhost.:8787",
        "http://deep.nested.localhost/v1",
        "http://LOCALHOST./v1",
        "http://Api.LocalHost.:8787",
        "http://127.0.0.1.:8787",
    ] {
        assert!(
            loopback_target_suppresses_minting(base),
            "an RFC 6761 local name must suppress minting: {base}"
        );
    }

    // The reject side, which is what keeps the subtree rule from becoming a
    // substring rule: `localhost` must be a whole trailing LABEL, so a name
    // that merely ends with those bytes, or carries the label in the middle,
    // is an ordinary remote host.
    for base in [
        "https://notlocalhost/v1",
        "https://notlocalhost./v1",
        "https://mylocalhost.example",
        "https://localhost.upstream.example",
        "https://localhost.upstream.example.",
        "https://localhostupstream.example",
    ] {
        assert!(
            !loopback_target_suppresses_minting(base),
            "a remote host must not suppress minting: {base}"
        );
    }
}

#[test]
fn a_wildcard_unspecified_address_suppresses_minting() {
    // Arrange / Act / Assert -- the unspecified addresses are wildcard
    // destinations, not remote hosts, and they reach a local listener in
    // practice. Classified BEFORE the IPv4-mapped/compatible canonicalization,
    // because `::` satisfies the IPv4-compatible prefix with an embedded quad
    // of `0.0.0.0`, which is not loopback and would otherwise read as remote.
    for base in [
        "http://0.0.0.0:8787",
        "http://0.0.0.0",
        "https://0.0.0.0:8443/v1",
        "http://[::]:8787",
        "http://[::0]:8787",
        "http://[::ffff:0.0.0.0]:8787",
    ] {
        assert!(
            loopback_target_suppresses_minting(base),
            "an unspecified wildcard address must suppress minting: {base}"
        );
    }

    // The reject side: addresses one bit away from unspecified are remote.
    for base in [
        "http://0.0.0.1:8787",
        "http://1.0.0.0:8787",
        "http://[::2]:8787",
    ] {
        assert!(
            !loopback_target_suppresses_minting(base),
            "a routable address must not suppress minting: {base}"
        );
    }
}

#[test]
fn a_dns_name_that_merely_encodes_an_address_is_treated_as_remote() {
    // Arrange / Act / Assert -- this predicate classifies SYNTACTICALLY and
    // resolves nothing, so a wildcard-DNS name is an ordinary remote host here
    // however it spells its target address. A heuristic over such names was
    // tried and removed: reading leading numeric labels as an address is
    // simultaneously over-broad (it suppresses a legitimate remote domain whose
    // labels happen to be numeric, like `127.0.0.1.example.com`) and
    // under-broad (the same services accept prefixed, dashed and hex forms it
    // cannot see). A partial classifier that silently misroutes both directions
    // is worse than a stated boundary.
    //
    // These cases are pinned as REMOTE so reintroducing name-shape DNS
    // classification has to be a deliberate design change that fails this test,
    // not an incremental tightening of a heuristic.
    for base in [
        // The over-broad direction: a legitimate remote domain under a numeric
        // subdomain.
        "https://127.0.0.1.example.com",
        "https://127.0.0.1.example.com/v1",
        // The dotted wildcard form the removed heuristic did catch.
        "http://127.0.0.1.nip.io",
        "https://127.0.0.1.sslip.io/v1",
        "http://0.0.0.0.nip.io",
        // The under-broad direction: forms the same services resolve to a local
        // address but no leading-label parse can see.
        "http://app-127-0-0-1.nip.io",
        "http://127-0-0-1.nip.io",
        "http://7f000001.nip.io",
        "http://app.127.0.0.1.nip.io",
        // Near-misses that were the removed rule's own reject side; they stay
        // remote, now for the same reason as everything else here.
        "https://127.example.com",
        "https://128.0.0.1.nip.io",
        "https://upstream.127.0.0.1.example",
    ] {
        assert!(
            !loopback_target_suppresses_minting(base),
            "a DNS name is classified remote regardless of the address it encodes: {base}"
        );
    }
}

#[test]
fn the_stock_local_aliases_suppress_minting() {
    // Arrange / Act / Assert -- names a stock hosts file maps to a local
    // address: the Debian-family pair and the RHEL/Fedora set. The rule is an
    // exact match against a closed list (modulo case and terminal dots), which
    // is what keeps it from becoming the kind of shape heuristic removed above.
    for base in [
        "http://localhost.localdomain",
        "http://localhost.localdomain:8787",
        "http://ip6-localhost:8787",
        "http://ip6-loopback:8787",
        "http://IP6-Localhost.",
        "http://localhost4",
        "http://localhost4:8787",
        "http://localhost6",
        "http://localhost4.localdomain4",
        "http://localhost6.localdomain6",
        "http://LocalHost6.LocalDomain6.",
    ] {
        assert!(
            loopback_target_suppresses_minting(base),
            "a stock local alias must suppress minting: {base}"
        );
    }

    // The reject side: names that merely resemble an alias. Exact-match is what
    // separates these, so each of them would suppress under a suffix or prefix
    // rule.
    for base in [
        "https://localhost.localdomain.upstream.example",
        "https://ip6-localhost.upstream.example",
        "https://notip6-localhost",
        "https://ip6-loopback-upstream.example",
        "https://localhost4.upstream.example",
        "https://localhost6.upstream.example",
        "https://notlocalhost4",
        "https://localhost42",
        "https://localhost4.localdomain4.upstream.example",
        "https://localhost6.localdomain6x",
        "https://mylocalhost4.localdomain4",
    ] {
        assert!(
            !loopback_target_suppresses_minting(base),
            "a remote host must not suppress minting: {base}"
        );
    }
}

#[test]
fn repeated_terminal_dots_still_resolve_to_a_local_name() {
    // Arrange / Act / Assert -- a single-dot strip leaves `localhost..` looking
    // like a name whose last label is empty, which is not the reserved name and
    // would mint. Trimming ALL terminal dots makes such a malformed spelling
    // fail closed instead.
    for base in [
        "http://localhost..",
        "http://localhost...",
        "http://api.localhost..",
        "http://localhost.localdomain..",
        "http://localhost6.localdomain6..",
    ] {
        assert!(
            loopback_target_suppresses_minting(base),
            "a repeated-dot local spelling must suppress minting: {base}"
        );
    }

    // The reject side: trailing dots do not make a remote name local, so the
    // trim cannot be what decides suppression on its own.
    for base in [
        "https://upstream.example..",
        "https://notlocalhost..",
        "https://localhost.upstream.example..",
    ] {
        assert!(
            !loopback_target_suppresses_minting(base),
            "a remote host with trailing dots must not suppress minting: {base}"
        );
    }
}

#[test]
fn a_local_destination_is_refused_at_admission_not_only_by_the_predicate() {
    // Arrange / Act / Assert -- the predicate is only load-bearing because
    // admission consults it, so every newly classified local spelling is
    // proven through the guard itself. A predicate-only test would pass even
    // if admission stopped calling it.
    for base in [
        "http://api.localhost:8787",
        "http://localhost.:8787",
        "http://localhost..",
        "http://0.0.0.0:8787",
        "http://[::]:8787",
        "ws://upstream.example/v1",
        "file:///tmp/socket",
        "http://localhost.localdomain:8787",
        "http://ip6-loopback:8787",
        "http://localhost4:8787",
        "http://localhost6.localdomain6:8787",
    ] {
        let reg = registry();
        let t0 = Instant::now();
        let k = key("local-hop");

        assert!(
            reg.admit_provisional(&k, base, 1, t0).is_none(),
            "a local destination must be refused at admission: {base}"
        );
        assert_eq!(
            reg.snapshot_len(),
            0,
            "a refused admission must persist nothing: {base}"
        );
    }

    // The remote positive controls, run through the SAME admission path. The
    // wildcard-DNS forms are here deliberately: a future change that
    // reintroduces name-shape DNS classification must fail an ADMISSION test,
    // not merely a predicate unit test.
    for base in [
        "https://localhost.upstream.example/v1",
        "https://notlocalhost/v1",
        "http://0.0.0.1:8787",
        "https://127.example.com/v1",
        "https://ip6-localhost.upstream.example/v1",
        "https://localhost4.upstream.example/v1",
        // A legitimate remote domain under a numeric subdomain.
        "https://127.0.0.1.example.com/v1",
        // Wildcard-DNS spellings: dotted, prefixed, dashed, hex.
        "http://127.0.0.1.nip.io:8787",
        "http://app-127-0-0-1.nip.io:8787",
        "http://127-0-0-1.nip.io:8787",
        "http://7f000001.nip.io:8787",
    ] {
        let reg = registry();
        let t0 = Instant::now();
        let k = key("mirror");

        assert!(
            reg.admit_provisional(&k, base, 1, t0).is_some(),
            "a remote target must be admitted: {base}"
        );
    }
}

#[test]
fn the_native_ipv6_loopback_check_is_present() {
    // Arrange / Act / Assert -- `::1` must be recognized as loopback in its own
    // right. Its IPv4-compatible embedded quad is `0.0.0.1`, which is neither
    // loopback nor unspecified, so the reduction below cannot cover it: without
    // a native loopback check this address classifies as remote. Removing that
    // check is the mutation this test exists to kill.
    assert!(loopback_target_suppresses_minting("http://[::1]:8787"));
    assert!(loopback_target_suppresses_minting("http://[::1]"));

    // The reduction itself still has to work, or the assertion above could
    // hold on a predicate that ignores embedded quads entirely.
    assert!(loopback_target_suppresses_minting(
        "http://[::ffff:127.0.0.1]:8787"
    ));
    assert!(loopback_target_suppresses_minting(
        "http://[::127.0.0.1]:8787"
    ));

    // And the reject control: an IPv6 address that is neither loopback nor
    // unspecified, and whose embedded quad is routable.
    assert!(!loopback_target_suppresses_minting("http://[::2]:8787"));
    assert!(!loopback_target_suppresses_minting(
        "http://[::ffff:128.0.0.1]:8787"
    ));
}

// --- Stage 1 lane exclusion ---

#[test]
fn the_anthropic_lane_preserves_the_minted_key_exactly() {
    // Arrange -- the accept control for the refusal below.
    let minted = field_capability_key(GROUNDED_PATH).expect("accepted");

    // Act
    let k = FieldVerdictKey::new("t", GROUNDED_PATH, ANTHROPIC).expect("the Stage 1 lane admits");

    // Assert -- byte-for-byte, so the row and the ledger carry the upstream's
    // own qualified path.
    assert_eq!(k.capability_key(), minted);
}

#[test]
fn a_lane_whose_normalization_rewrites_the_key_acquires_no_field_identity() {
    // Arrange -- the Bedrock normalizer truncates a dotted capability key at
    // its first segment, so two structurally distinct fields would collapse
    // onto one permanent token. Stage 1 excludes that lane rather than
    // changing core normalization: the identity is refused at construction,
    // which is upstream of every mint path.
    let dotted = field_capability_key(GROUNDED_PATH).expect("accepted");
    assert_ne!(
        normalize_capability_key(&dotted, "bedrock"),
        dotted,
        "the exclusion is only meaningful while that lane still rewrites the key"
    );

    // Act / Assert -- no identity, so no guard and no verdict, for any dotted
    // path on that lane.
    for path in [
        GROUNDED_PATH,
        "thinking.display",
        "thinking.budget_tokens",
        "tools.0.input_schema",
    ] {
        assert!(
            FieldVerdictKey::new("t", path, "bedrock").is_none(),
            "a rewritten key must acquire no identity: {path}"
        );
    }
}

#[test]
fn a_bedrock_target_cannot_acquire_a_field_guard_for_a_dotted_path() {
    // Arrange -- the refusal proven through the lifecycle, not only through
    // the constructor: there is no path from a dotted field on that lane to a
    // held guard, even on a perfectly mintable remote base.
    let reg = registry();
    let t0 = Instant::now();

    // Act
    let identity = FieldVerdictKey::new("t", GROUNDED_PATH, "bedrock");

    // Assert
    assert!(identity.is_none());
    assert_eq!(reg.snapshot_len(), 0);

    // The accept control on the SAME lane: a single-segment path is not
    // rewritten, so it still mints. This is what keeps the refusal specific to
    // the truncation rather than a blanket ban on the lane.
    let single = FieldVerdictKey::new("t", "thinking", "bedrock")
        .expect("an unrewritten key still mints on this lane");
    assert_eq!(
        single.capability_key(),
        field_capability_key("thinking").expect("accepted")
    );
    assert!(reg.admit_provisional(&single, REMOTE_BASE, 1, t0).is_some());
}

// --- canary suspension gates eligibility ---

/// Plant the ONE state `preflight_eligible` may authorize: a resident acting
/// negative whose own incarnation carries an acknowledged confirmation.
fn plant_eligible(reg: &FieldVerdictRegistry, k: &FieldVerdictKey, now: Instant) {
    plant_acting_negative(reg.learned(), k, now);
    let incarnation = reg.learned().resident_incarnation_for_tests(
        k.state_key(),
        k.capability_key(),
        k.provider_kind(),
    );
    reg.canaries().acknowledge_confirmation(k, incarnation, 1);
    assert!(
        reg.preflight_eligible(k, reg.learned().generation(), now),
        "fixture premise: the identity must start pre-flight eligible",
    );
}

#[test]
fn a_canary_disproved_identity_is_refused_pre_flight_before_its_clear_lands() {
    // The window this closes is the one between a canary proving the verdict
    // wrong and the durable clear that removes it -- a clear that can be
    // REFUSED (a purge lease, a stale generation). While that is negotiated
    // the resident row is still acting and still confirmed, so nothing else in
    // the predicate refuses it; the suspension is the only thing that does.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    plant_eligible(&reg, &k, t0);

    // Act -- an unrepaired canary succeeded.
    reg.canaries()
        .claim_canary(
            &k,
            reg.canaries().snapshot(&k).expect("resident").incarnation,
        )
        .expect("claim admitted")
        .settle(crate::field_canary::CanaryOutcome::Regressed);

    // Assert
    assert!(
        reg.is_negative_acting(&k, t0),
        "premise: the row is STILL resident and acting -- the clear has not run",
    );
    assert!(
        !reg.preflight_eligible(&k, reg.learned().generation(), t0),
        "a verdict a canary disproved must stop moving traffic at once, without \
         waiting for the durable clear to be accepted",
    );
}

/// A disproof whose removal ran but removed NOTHING must not drop the canary
/// state. `Applied { value: false }` passes the generation barrier, so it is not
/// a refusal, but it did not clear a verdict either -- and the settlement that
/// preceded it has already suspended pre-flight and charged the alarm. Dropping
/// the state here would lift that suspension for an identity whose row may still
/// be resident under another writer, re-admitting traffic to a verdict a canary
/// proved wrong.
///
/// Mutation check: move `self.canaries.reset(key)` above the `if !cleared`
/// early return in `record_canary_disproof` and this test goes red.
#[test]
fn a_disproof_that_removed_nothing_keeps_the_identity_suspended() {
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    plant_eligible(&reg, &k, t0);
    let incarnation = reg.canaries().snapshot(&k).expect("resident").incarnation;

    // One request applied this verdict's repair, so the disproof has something
    // to charge: without it the alarm assertion below would pass on zero.
    drop(reg.canaries().begin_modified_request(&k, incarnation));

    // The claim the disproof will settle. Settling is now part of the disproof
    // itself, so it is handed over rather than consumed here.
    let claim = reg
        .canaries()
        .claim_canary(&k, incarnation)
        .expect("claim admitted");
    // Remove the row out from under the disproof, so its own guarded removal
    // finds nothing resident: Applied, but with `value: false`.
    assert!(
        reg.learned()
            .remove_keyed(&k.state_key, &k.capability_key, &k.provider_kind),
        "fixture premise: the row was resident before this removal",
    );

    let cleared = reg.record_canary_disproof(&k, claim, reg.learned().generation());

    assert!(
        cleared.is_none(),
        "no row was removed, so no clear event may ride out to the ledger",
    );
    let snap = reg
        .canaries()
        .snapshot(&k)
        .expect("the canary state must survive a removal that removed nothing");
    assert!(
        snap.preflight_suspended,
        "the suspension the settlement set must still stand",
    );
    // NOT asserted here: that the identity is refused pre-flight. The row was
    // removed to manufacture this case, so that assertion would hold with or
    // without the retained state -- it cannot fail, so it is not evidence.
    // What the retention actually protects is the accounting below.
    assert_eq!(
        snap.modified_since_confirmation, 0,
        "the settlement transferred the tally rather than copying it",
    );
    assert_eq!(
        reg.canaries().disproved_requests_total(),
        1,
        "and the request the disproved verdict rewrote stays charged to the \
         lifetime alarm, which dropping the state would have discarded",
    );
}

/// A confirmation REFUSED by the guarded mutation vouches for nothing, so the
/// claim must settle INCONCLUSIVE, not Confirmed. Confirmed zeroes
/// `modified_since_confirmation` -- the count of requests the verdict has
/// already rewritten -- and a later disproof charges that tally to the lifetime
/// alarm. Zeroing it on a confirmation that never persisted silently forgives
/// every request the verdict modified.
///
/// Mutation check: settle `Confirmed` on the refusal arm of
/// `record_canary_confirmation` and this goes red on the FIRST assertion to
/// fail, `last_outcome` (`Some(Confirmed)` vs `Some(Inconclusive)`); the tally
/// and alarm assertions below it pin the consequence that outcome carries.
#[test]
fn a_refused_confirmation_preserves_the_affected_request_tally() {
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    plant_eligible(&reg, &k, t0);
    let incarnation = reg.canaries().snapshot(&k).expect("resident").incarnation;

    // Two requests applied this verdict's repair, so the tally stands at two.
    let g1 = reg.canaries().begin_modified_request(&k, incarnation);
    let g2 = reg.canaries().begin_modified_request(&k, incarnation);
    drop(g1);
    drop(g2);
    assert_eq!(
        reg.canaries()
            .snapshot(&k)
            .expect("resident")
            .modified_since_confirmation,
        2,
        "fixture premise: two requests were modified since the last confirmation",
    );

    let claim = reg
        .canaries()
        .claim_canary(&k, incarnation)
        .expect("claim admitted");
    // A purge lease on the identity's own key refuses the confirmation's
    // guarded observation: Reserved, so nothing is recorded.
    let lease = match reg.learned().prepare_purge(
        reg.learned().generation(),
        k.state_key(),
        k.capability_key(),
        k.provider_kind(),
    ) {
        crate::learned_capability::PurgePreparation::Reserved(lease) => lease,
        other => panic!("expected the resident row to reserve a lease, got {other:?}"),
    };

    let event =
        reg.record_canary_confirmation(&k, claim, reg.learned().generation(), 400, vec![], t0);

    assert!(
        event.is_none(),
        "premise: the lease refused the confirmation, so nothing persisted",
    );
    let snap = reg.canaries().snapshot(&k).expect("resident");
    assert_eq!(
        snap.last_outcome,
        Some(crate::field_canary::CanaryOutcome::Inconclusive),
        "a confirmation that did not persist proves nothing",
    );
    assert_eq!(
        snap.modified_since_confirmation, 2,
        "and the requests the verdict already rewrote are still charged against it",
    );
    assert!(!snap.canary_claimed, "the claim is released either way");

    // The tally's purpose: a later disproof must charge exactly those requests.
    reg.learned().restore_purge(lease);
    reg.canaries()
        .claim_canary(&k, incarnation)
        .expect("reclaimable after an inconclusive settlement")
        .settle(crate::field_canary::CanaryOutcome::Regressed);
    assert_eq!(
        reg.canaries().disproved_requests_total(),
        2,
        "the preserved tally is what the alarm charges when the verdict is disproved",
    );
}

/// A settlement planned at a SUPERSEDED incarnation must not drag a newer
/// resident lifecycle backward. The carry moves the identity's incarnation, so
/// an unguarded version would rewrite a lifecycle this canary never tested --
/// and reseed its cadence and confirmation state with it.
///
/// Mutation check: drop the `entry.incarnation != planned_incarnation` guard
/// from `settle_confirmed_and_carry` and this goes red.
#[test]
fn a_stale_planned_incarnation_cannot_move_a_newer_lifecycle() {
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    plant_eligible(&reg, &k, t0);
    let stale_incarnation = reg.canaries().snapshot(&k).expect("resident").incarnation;
    let claim = reg
        .canaries()
        .claim_canary(&k, stale_incarnation)
        .expect("claim admitted");

    // The identity moves to a new lifecycle while this canary is in flight.
    let fresh_incarnation = stale_incarnation + 7;
    reg.canaries()
        .acknowledge_confirmation(&k, fresh_incarnation, 3);

    claim.settle_confirmed_and_carry(fresh_incarnation + 99);

    let snap = reg.canaries().snapshot(&k).expect("resident");
    assert_eq!(
        snap.incarnation, fresh_incarnation,
        "the stale settlement must not move the resident lifecycle",
    );
    assert_eq!(
        snap.confirmations, 3,
        "nor reseed the confirmation count the fresh lifecycle acknowledged",
    );
    assert_eq!(
        snap.last_outcome, None,
        "and it records no outcome against a lifecycle it never tested",
    );
}

/// A suspension retained because the clear removed nothing is STICKY: a relearn
/// of the same identity does NOT lift it, and this pins that real behavior rather
/// than an aspirational one.
///
/// Learned incarnations are per-entry and start at zero, so a row removed and
/// relearned comes back on the SAME incarnation the suspension was recorded
/// against. The monotonic admission reads that as `Current`, not `Newer`, so
/// nothing is reseeded. The documented recovery paths are a clear that actually
/// applies, a completed operator purge, or a restart's cold rebuild -- and the
/// last assertion here exercises the `reset` those all funnel through.
#[test]
fn a_retained_suspension_survives_a_relearn_until_the_state_is_dropped() {
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    plant_eligible(&reg, &k, t0);
    let disproved_incarnation = reg.canaries().snapshot(&k).expect("resident").incarnation;

    // Disprove the verdict, then have the clear remove nothing: the suspension
    // is retained against the incarnation it disproved.
    let claim = reg
        .canaries()
        .claim_canary(&k, disproved_incarnation)
        .expect("claim admitted");
    assert!(
        reg.learned()
            .remove_keyed(&k.state_key, &k.capability_key, &k.provider_kind),
        "fixture premise: the row was resident before this removal",
    );
    assert!(
        reg.record_canary_disproof(&k, claim, reg.learned().generation())
            .is_none(),
        "premise: the clear removed nothing, so the suspension is retained",
    );

    // A production relearn: a fresh acting negative for the same identity,
    // through the same import seam real carry-over uses.
    plant_acting_negative(reg.learned(), &k, t0);
    let relearned = reg.learned().resident_incarnation_for_tests(
        k.state_key(),
        k.capability_key(),
        k.provider_kind(),
    );
    assert_eq!(
        relearned, disproved_incarnation,
        "premise, and the whole point: a relearn reuses the incarnation, so it is \
         not a newer lifecycle and reseeds nothing",
    );
    reg.canaries().acknowledge_confirmation(&k, relearned, 1);

    assert!(
        reg.canaries()
            .snapshot(&k)
            .expect("resident")
            .preflight_suspended,
        "the suspension SURVIVES the relearn: it is lifted by dropping the state, \
         not by learning the identity again",
    );
    assert!(
        !reg.preflight_eligible(&k, reg.learned().generation(), t0),
        "so the identity stays out of pre-flight -- the safe direction, since \
         reactive forward-and-repair still serves every request",
    );

    // The documented recovery: dropping the state outright, which a clear that
    // applies, an operator purge, and a cold rebuild all funnel through.
    reg.canaries().reset(&k);
    reg.canaries().acknowledge_confirmation(&k, relearned, 1);

    assert!(
        reg.preflight_eligible(&k, reg.learned().generation(), t0),
        "and pre-flight returns once the state is dropped and the identity is \
         confirmed afresh",
    );
}

#[test]
fn a_confirmed_canary_leaves_the_identity_pre_flight_eligible() {
    // The positive control for the suspension above: the same settlement seam
    // on the CONFIRMED arm must leave the verdict acting, or the assertion
    // above would pass against a settlement that suspends unconditionally.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    plant_eligible(&reg, &k, t0);

    reg.canaries()
        .claim_canary(
            &k,
            reg.canaries().snapshot(&k).expect("resident").incarnation,
        )
        .expect("claim admitted")
        .settle(crate::field_canary::CanaryOutcome::Confirmed);

    assert!(
        reg.preflight_eligible(&k, reg.learned().generation(), t0),
        "a re-confirmed verdict keeps acting",
    );
}

#[test]
fn an_inconclusive_canary_leaves_the_identity_pre_flight_eligible() {
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    plant_eligible(&reg, &k, t0);

    reg.canaries()
        .claim_canary(
            &k,
            reg.canaries().snapshot(&k).expect("resident").incarnation,
        )
        .expect("claim admitted")
        .settle(crate::field_canary::CanaryOutcome::Inconclusive);

    assert!(
        reg.preflight_eligible(&k, reg.learned().generation(), t0),
        "an outcome that proved nothing must not suspend a verdict",
    );
}

// --- emission ---

#[test]
fn the_committed_row_reuses_the_existing_event_shape() {
    // Arrange
    let reg = registry();
    let t0 = Instant::now();
    let k = key("prod-target");
    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, 1, t0)
        .expect("unknown pair admits");

    // Act
    let event = guard
        .commit(400, vec!["thinking".to_string()], t0)
        .expect("a live commit emits its row");

    // Assert -- every field is a normalized key or a closed-set token, on the
    // same row shape the existing learn path emits: no new column, no new
    // store, nothing that could carry a request body.
    assert_eq!(event.state_key, "prod-target");
    assert_eq!(
        event.capability_key,
        field_capability_key(GROUNDED_PATH).expect("accepted"),
        "the persisted key is the namespace owner's minted bytes"
    );
    assert_eq!(event.provider_kind, ANTHROPIC);
    assert_eq!(event.signal_tier, SignalTier::SelfIdentifying);
    assert_eq!(event.phase, FailurePhase::F1);
    assert_eq!(event.source, EvidenceSource::Live);
    assert_eq!(event.upstream_status, 400);
    assert!(!event.remapped);
    assert_eq!(event.request_features, vec!["thinking".to_string()]);
    assert_eq!(event.observations, 1);
}

/// `commit` is an IN-MEMORY admission through the generation barrier, not a
/// durable writer acknowledgment: the canary registry's confirmation count
/// must stay at zero until a caller holding that separate acknowledgment
/// calls `acknowledge_confirmation` explicitly. Nothing in this build wires
/// `commit` to that call.
#[test]
fn a_bare_commit_leaves_the_canary_confirmation_count_at_zero() {
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, 1, t0)
        .expect("unknown pair admits");

    let event = guard
        .commit(400, vec![], t0)
        .expect("a live commit emits its row");

    assert_eq!(
        event.observations, 1,
        "the resident row still learns the observation"
    );
    assert!(
        reg.canaries().snapshot(&k).is_none(),
        "an in-memory admission must not itself confirm the canary count; \
         only a durable writer acknowledgment may"
    );
}

#[test]
fn a_repeated_confirmed_rejection_refreshes_the_same_row() {
    // Arrange -- learned once, lapsed, and rejected again.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    let _ = reg
        .admit_provisional(&k, REMOTE_BASE, 1, t0)
        .expect("unknown pair admits")
        .commit(400, vec![], t0)
        .expect("a live commit emits its row");
    let lapsed = t0 + DECAY + Duration::from_secs(1);
    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, 1, lapsed)
        .expect("a lapsed entry admits one re-verification");

    // Act
    let event = guard
        .commit(400, vec![], lapsed)
        .expect("a live commit emits its row");

    // Assert -- one row with its history intact, acting on a fresh window.
    assert_eq!(event.observations, 2);
    assert_eq!(reg.snapshot_len(), 1);
    assert!(reg.is_negative_acting(&k, lapsed));
}

// ---- generation barrier tests -----------------------------------------------

/// A field-verdict key is catalog-INDEPENDENT, so the generation barrier admits
/// operations from ANY generation -- including a superseded one. A commit through
/// a guard whose admission predates the live generation still persists and emits,
/// because the upstream statement about its own request envelope is not
/// invalidated by a catalog revision change.
///
/// This is a POSITIVE control against testing for a staleness that cannot happen
/// on this key class: the generation barrier rejects only catalog-scoped keys,
/// and a `field:` key is never scoped.
#[test]
fn a_field_verdict_commit_from_a_superseded_generation_still_persists() {
    let reg = registry();
    let k = key("thinking.enabled.display");
    let t0 = Instant::now();
    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, 1, t0)
        .expect("admitted");

    // The reload lands between admission and settlement.
    reg.learned().advance_generation();

    let event = guard.commit(400, vec![], t0);

    assert!(
        event.is_some(),
        "a field-verdict commit is catalog-independent and must persist from \
         any generation",
    );
    assert_eq!(reg.learned().snapshot().len(), 1);
}

/// Same for the clear path: a superseded generation clears a field verdict, and
/// the cleared event carries the persistence generation from the Applied outcome.
#[test]
fn a_field_verdict_clear_from_a_superseded_generation_still_clears() {
    let reg = registry();
    let k = key("thinking.enabled.display");
    let t0 = Instant::now();
    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, 1, t0)
        .expect("admitted");
    let _ = guard.commit(400, vec![], t0);
    let t_lapsed = t0 + DECAY + Duration::from_secs(1);
    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, 1, t_lapsed)
        .expect("a lapsed pair admits");

    reg.learned().advance_generation();
    let cleared = guard.clear();

    assert!(
        cleared.is_some(),
        "a field-verdict clear is catalog-independent and must clear from \
         any generation",
    );
    assert!(reg.learned().snapshot().is_empty());
}

/// The event stamp comes atomically from the Applied outcome, not from a
/// separate read. For a field key this is always the effective generation,
/// which is the pending generation during an admitted boundary or the active
/// one otherwise.
#[test]
fn the_event_generation_comes_from_the_applied_outcome() {
    let reg = registry();
    let k = key("thinking.enabled.display");
    let t0 = Instant::now();
    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, 1, t0)
        .expect("admitted");

    let event = guard.commit(400, vec![], t0).expect("live commit");

    assert_eq!(
        event.persistence_generation,
        reg.learned().generation(),
        "the stamp must come from the Applied outcome, atomically paired \
         with the mutation it describes",
    );
}

/// Admission with a stale generation still succeeds for a field key, because
/// the negative-state read goes through `negative_state_in_generation`, and the
/// generation barrier admits catalog-independent keys from any generation.
///
/// This is the distinction from the replay lifecycle, whose `reasoning_replay:`
/// key IS catalog-scoped and therefore stale-refused.
#[test]
fn a_stale_generation_at_admission_still_admits_a_field_key() {
    let reg = registry();
    let k = key("thinking.enabled.display");
    let stale = reg.learned().generation();
    reg.learned().advance_generation();

    let admitted = reg.admit_provisional(&k, REMOTE_BASE, stale, Instant::now());

    assert!(
        admitted.is_some(),
        "a field key is catalog-independent, so even a stale generation admits",
    );
}

/// The slot is always released, regardless of which generation the commit or
/// clear ran under. This pins that a field-verdict guard from a superseded
/// Router does not latch the slot.
#[test]
fn the_slot_is_released_on_commit_from_any_generation() {
    let reg = registry();
    let k = key("thinking.enabled.display");
    let t0 = Instant::now();
    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, 1, t0)
        .expect("admitted");
    reg.learned().advance_generation();

    let _ = guard.commit(400, vec![], t0);

    // The commit persisted an acting entry, so re-admission at the same instant
    // finds Acting and is correctly refused. Verify the slot itself is released by
    // checking that the in-flight set no longer holds it: a second commit call on
    // the same key from a LAPSED instant would be admitted if the slot were free.
    let t_lapsed = t0 + DECAY + Duration::from_secs(1);
    assert!(
        reg.admit_provisional(&k, REMOTE_BASE, reg.learned().generation(), t_lapsed)
            .is_some(),
        "the slot must be released after a commit from any generation",
    );
}

// ---- pending-boundary settlement ---------------------------------------------

/// Take a real boundary cut and leave the pending generation installed,
/// returning its receipt. The cut admits (the closure reports success) so the
/// registry holds an admitted-but-uncommitted generation, which is the state a
/// settlement must stamp against.
fn admit_pending_boundary(
    learned: &LearnedCapabilityRegistry,
) -> crate::learned_capability::BoundaryReceipt {
    let crate::learned_capability::BoundaryCut::Taken { receipt, .. } =
        learned.with_boundary_cut(|_survivors, _pending| (), |()| true)
    else {
        panic!("the boundary cut must be taken");
    };
    receipt
}

/// A commit settling DURING an admitted boundary stamps its event with the
/// PENDING generation, so the ledger row sorts after the boundary rather than
/// being dropped as older than it. The verdict survives the commit and a
/// restart.
#[test]
fn a_commit_during_a_pending_boundary_stamps_the_pending_generation() {
    let reg = registry();
    let k = key("thinking.enabled.display");
    let t0 = Instant::now();
    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, 1, t0)
        .expect("admitted before the boundary");

    // The boundary is admitted but not yet committed.
    let receipt = admit_pending_boundary(reg.learned());
    let pending = receipt.generation();
    assert!(
        pending > reg.learned().generation(),
        "the pending generation must be ahead of the active one",
    );

    let event = guard
        .commit(400, vec![], t0)
        .expect("a field commit applies");

    assert_eq!(
        event.persistence_generation, pending,
        "the event must carry the PENDING generation, or the writer drops it as \
         older than the boundary being committed",
    );

    // The boundary commits: the verdict is still resident, because a field key
    // is catalog-independent and the transition prunes only scoped entries.
    let settled = reg.learned().commit_boundary_transition(&receipt);
    assert_eq!(
        settled,
        crate::learned_capability::BoundarySettlement::Applied {
            generation: pending,
            pruned: 0,
        },
    );
    assert_eq!(
        reg.learned().snapshot().len(),
        1,
        "the field verdict must survive the boundary it was stamped for",
    );
    assert_eq!(reg.learned().generation(), pending);
}

/// The same for the CLEAR path: a clear settling during an admitted boundary
/// stamps the pending generation, and the removal survives the commit.
#[test]
fn a_clear_during_a_pending_boundary_stamps_the_pending_generation() {
    let reg = registry();
    let k = key("thinking.enabled.display");
    let t0 = Instant::now();
    // Plant the verdict, then lapse it so a repair can be admitted again.
    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, 1, t0)
        .expect("admitted");
    let _ = guard.commit(400, vec![], t0);
    let t_lapsed = t0 + DECAY + Duration::from_secs(1);
    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, 1, t_lapsed)
        .expect("a lapsed identity admits");

    let receipt = admit_pending_boundary(reg.learned());
    let pending = receipt.generation();

    let cleared = guard.clear().expect("a resident verdict clears");

    assert_eq!(
        cleared.persistence_generation, pending,
        "the cleared row must carry the PENDING generation",
    );
    let settled = reg.learned().commit_boundary_transition(&receipt);
    assert!(matches!(
        settled,
        crate::learned_capability::BoundarySettlement::Applied { .. }
    ));
    assert!(
        reg.learned().snapshot().is_empty(),
        "the clear must survive the boundary commit",
    );
}

/// After a boundary ROLLS BACK, a settlement stamps the generation in force at
/// SETTLEMENT time -- not the now-discarded token its guard is holding.
///
/// # Why the ordering in this fixture is load-bearing
///
/// The guard must be admitted WHILE the boundary is pending, so it stores the
/// pending token (2). The rollback then discards that generation, leaving 1 in
/// force. Only in that arrangement do the two candidate sources disagree:
/// stamping from `GenerationOutcome::Applied` yields 1 (correct -- the row must
/// sort under the generation that actually survived), while stamping from
/// `guard.generation` yields 2, a generation no boundary ever committed, and the
/// writer would drop the row as belonging to a boundary it never saw.
///
/// Admitting BEFORE the boundary makes the fixture vacuous: the guard token and
/// the post-rollback active generation are both 1, so either source passes. That
/// was the original shape of this test and it could not discriminate.
#[test]
fn a_settlement_after_a_rolled_back_boundary_stamps_the_generation_in_force() {
    let reg = registry();
    let k = key("thinking.enabled.display");
    let t0 = Instant::now();

    // The boundary is admitted FIRST, so the guard below stores its pending token.
    let receipt = admit_pending_boundary(reg.learned());
    let pending = receipt.generation();
    let active = reg.learned().generation();
    assert_ne!(
        pending, active,
        "the fixture needs the pending and active generations to differ",
    );

    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, 1, t0)
        .expect("admitted while the boundary is pending");

    // The boundary is discarded: `pending` now names no committed generation.
    let _ = reg.learned().rollback_pending_generation(&receipt);
    assert_eq!(reg.learned().generation(), active);

    let event = guard
        .commit(400, vec![], t0)
        .expect("a field commit applies");

    assert_eq!(
        event.persistence_generation, active,
        "the stamp must come from the settlement's Applied outcome, not from the \
         guard's discarded pending token",
    );
    assert_ne!(
        event.persistence_generation, pending,
        "a rolled-back generation must never reach a persisted row",
    );
}

/// The CLEAR twin of the rollback case: same ordering, same distinction.
///
/// Pinned independently because `clear` reaches the registry through
/// `remove_keyed_in_generation` rather than `observe_in_generation`, so a
/// regression could reintroduce guard-token stamping on one path alone.
#[test]
fn a_clear_after_a_rolled_back_boundary_stamps_the_generation_in_force() {
    let reg = registry();
    let k = key("thinking.enabled.display");
    let t0 = Instant::now();
    // Plant the verdict, then lapse it so a repair can be admitted again.
    let seed = reg
        .admit_provisional(&k, REMOTE_BASE, 1, t0)
        .expect("admitted");
    let _ = seed.commit(400, vec![], t0);
    let t_lapsed = t0 + DECAY + Duration::from_secs(1);

    // Boundary first, so the clear's guard stores the pending token.
    let receipt = admit_pending_boundary(reg.learned());
    let pending = receipt.generation();
    let active = reg.learned().generation();
    assert_ne!(pending, active);

    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, 1, t_lapsed)
        .expect("a lapsed identity admits while pending");

    let _ = reg.learned().rollback_pending_generation(&receipt);

    let cleared = guard.clear().expect("a resident verdict clears");

    assert_eq!(
        cleared.persistence_generation, active,
        "the cleared row must carry the generation in force at settlement",
    );
    assert_ne!(
        cleared.persistence_generation, pending,
        "a rolled-back generation must never reach a persisted row",
    );
}

// ---- shared single-flight identity across a rebuild -------------------------

/// A guard held by the OLD facade blocks admission through the REPLACEMENT, and
/// its release becomes visible there.
///
/// The in-flight set is shared by `Arc`, not copied: a fresh set on the
/// replacement would admit a second concurrent repair for an identity the old
/// guard still holds -- exactly the duplicate-repair cost single-flight exists to
/// prevent. Required now rather than when the production wiring lands, because
/// the wiring rebases onto this contract.
#[test]
fn an_old_guard_blocks_the_replacement_facades_admission() {
    let reg = registry();
    let k = key("thinking.enabled.display");
    let t0 = Instant::now();

    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, 1, t0)
        .expect("the first admission succeeds");

    // The reload: a replacement facade on the same shared registry.
    let replacement = reg.rebuilt_on(Arc::clone(reg.learned_arc()));
    assert!(
        replacement.shares_in_flight_with(&reg),
        "the rebuild must carry the SAME in-flight set, not a copy",
    );

    // The old guard still holds the identity, so the replacement refuses.
    assert!(
        replacement
            .admit_provisional(&k, REMOTE_BASE, 1, t0)
            .is_none(),
        "an outstanding repair must block the replacement facade's admission",
    );

    // Settling the old guard frees the identity for the replacement.
    let _ = guard.commit(400, vec![], t0);
    let t_lapsed = t0 + DECAY + Duration::from_secs(1);
    assert!(
        replacement
            .admit_provisional(&k, REMOTE_BASE, 1, t_lapsed)
            .is_some(),
        "the old guard's settlement must be visible to the replacement",
    );
}

/// The release path (drop without settling) is equally visible across the
/// rebuild, so an abandoned repair does not latch the identity forever.
#[test]
fn an_old_guards_release_is_visible_to_the_replacement_facade() {
    let reg = registry();
    let k = key("thinking.enabled.display");
    let t0 = Instant::now();

    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, 1, t0)
        .expect("admitted");
    let replacement = reg.rebuilt_on(Arc::clone(reg.learned_arc()));
    assert!(
        replacement
            .admit_provisional(&k, REMOTE_BASE, 1, t0)
            .is_none(),
        "held while the old guard lives",
    );

    guard.release();

    assert!(
        replacement
            .admit_provisional(&k, REMOTE_BASE, 1, t0)
            .is_some(),
        "the release must free the identity for the replacement facade",
    );
}

/// A confirmation acknowledged through the OLD facade's canary registry is
/// visible through the REPLACEMENT's, and both share the same underlying
/// `Arc` -- the same attach-not-copy discipline `shares_in_flight_with`
/// proves for the single-flight set, extended to the canary/quorum state.
///
/// Acknowledges directly rather than through a bare `commit`: `commit` is an
/// in-memory admission, not a durable writer acknowledgment, so it must not
/// itself move the confirmation count (see
/// `a_bare_commit_leaves_the_canary_confirmation_count_at_zero`).
#[test]
fn a_commits_confirmation_is_visible_to_the_replacement_facade() {
    let reg = registry();
    let k = key("thinking.enabled.display");

    reg.canaries().acknowledge_confirmation(&k, 1, 3);

    let replacement = reg.rebuilt_on(Arc::clone(reg.learned_arc()));
    assert!(
        replacement.shares_canaries_with(&reg),
        "the rebuild must carry the SAME canary registry, not a copy",
    );
    let snap = replacement
        .canaries()
        .snapshot(&k)
        .expect("resident after the old facade's acknowledgment");
    assert_eq!(snap.confirmations, 3);
}

/// Exactly ONE holder exists across both facades: the replacement cannot mint a
/// duplicate claim for an identity the old facade admitted.
#[test]
fn the_two_facades_never_hold_the_same_identity_twice() {
    let reg = registry();
    let k = key("thinking.enabled.display");
    let t0 = Instant::now();

    let _held = reg
        .admit_provisional(&k, REMOTE_BASE, 1, t0)
        .expect("admitted");
    let replacement = reg.rebuilt_on(Arc::clone(reg.learned_arc()));

    // Neither facade may admit again while the identity is held.
    assert!(reg.admit_provisional(&k, REMOTE_BASE, 1, t0).is_none());
    assert!(
        replacement
            .admit_provisional(&k, REMOTE_BASE, 1, t0)
            .is_none(),
        "no duplicate holder across the rebuild",
    );
}

// ---- purge lease during repair settlement ------------------------------------

/// An operator purge holding the key's lease blocks a concurrent commit: the
/// commit's `observe_in_generation` call takes the same `purge_leases` guard a
/// prepared purge holds, so it is refused rather than refreshing an entry the
/// purge has already captured.
#[test]
fn a_purge_lease_blocks_a_concurrent_commit_and_leaves_the_entry_untouched() {
    let reg = registry();
    let k = key("thinking.enabled.display");
    let t0 = Instant::now();
    // Past the acting window: the resident row lapses, so a second caller may
    // admit a fresh repair on the same identity instead of being refused by
    // the still-acting verdict from the first commit.
    let t_lapsed = t0 + DECAY + Duration::from_secs(1);

    // A prior successful repair leaves a resident row for the purge to capture.
    reg.admit_provisional(&k, REMOTE_BASE, 1, t0)
        .expect("admitted")
        .commit(400, vec![], t0)
        .expect("a live commit emits its row");
    let before = reg
        .learned()
        .snapshot()
        .into_iter()
        .find(|entry| entry.state_key == k.state_key() && entry.feature_key == k.capability_key())
        .expect("the commit left a resident row");

    // Admitted BEFORE the lease is taken: the lease also blocks the
    // negative-state read `admit_provisional` performs, so a second guard
    // must already be held when the purge reserves the key.
    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, reg.learned().generation(), t_lapsed)
        .expect("the resident verdict has lapsed, so a fresh repair may admit");

    let lease = match reg.learned().prepare_purge(
        reg.learned().generation(),
        k.state_key(),
        k.capability_key(),
        k.provider_kind(),
    ) {
        crate::learned_capability::PurgePreparation::Reserved(lease) => lease,
        other => panic!("expected the resident row to reserve a lease, got {other:?}"),
    };

    let outcome = guard.commit(400, vec![], t_lapsed);

    assert!(
        outcome.is_none(),
        "a commit racing a purge lease must emit no event",
    );
    let after = reg
        .learned()
        .snapshot()
        .into_iter()
        .find(|entry| entry.state_key == k.state_key() && entry.feature_key == k.capability_key())
        .expect("the leased row is left resident, not removed");
    assert_eq!(
        before.observations, after.observations,
        "a refused commit must not mutate the leased entry",
    );

    reg.learned().restore_purge(lease);

    // The commit's refusal arm must have released the in-flight slot: the
    // same lapsed identity admits again once the lease is gone. Without that
    // release the slot latches forever and this assertion goes RED.
    assert!(
        reg.admit_provisional(&k, REMOTE_BASE, reg.learned().generation(), t_lapsed)
            .is_some(),
        "the same lapsed identity must admit again once the purge lease is \
         released",
    );
}

/// The same lease blocks a concurrent clear: a `remove_keyed_in_generation`
/// call refused by the lease removes nothing and emits nothing.
#[test]
fn a_purge_lease_blocks_a_concurrent_clear_and_leaves_the_entry_resident() {
    let reg = registry();
    let k = key("thinking.enabled.display");
    let t0 = Instant::now();
    let t_lapsed = t0 + DECAY + Duration::from_secs(1);

    reg.admit_provisional(&k, REMOTE_BASE, 1, t0)
        .expect("admitted")
        .commit(400, vec![], t0)
        .expect("a live commit emits its row");

    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, reg.learned().generation(), t_lapsed)
        .expect("the resident verdict has lapsed, so a fresh repair may admit");

    let lease = match reg.learned().prepare_purge(
        reg.learned().generation(),
        k.state_key(),
        k.capability_key(),
        k.provider_kind(),
    ) {
        crate::learned_capability::PurgePreparation::Reserved(lease) => lease,
        other => panic!("expected the resident row to reserve a lease, got {other:?}"),
    };

    let outcome = guard.clear();

    assert!(
        outcome.is_none(),
        "a clear racing a purge lease must emit no event",
    );
    // `is_negative_acting` reads through the same lease-guarded path a real
    // caller would, so it answers "refused" while the lease is held -- not
    // the fact this assertion needs. `snapshot` bypasses the lease to read
    // the raw entry, which is the only way to prove the row was left alone.
    assert!(
        reg.learned()
            .snapshot()
            .into_iter()
            .any(|entry| entry.state_key == k.state_key()
                && entry.feature_key == k.capability_key()
                && matches!(
                    entry.verdict,
                    routectl_core::capability::Verdict::LearnedBroken(_)
                )),
        "the leased row must remain resident with its negative verdict intact",
    );

    reg.learned().restore_purge(lease);

    // The clear's refusal arm must have released the in-flight slot: the
    // same lapsed identity admits again once the lease is gone. Without that
    // release the slot latches forever and this assertion goes RED.
    assert!(
        reg.admit_provisional(&k, REMOTE_BASE, reg.learned().generation(), t_lapsed)
            .is_some(),
        "the same lapsed identity must admit again once the purge lease is \
         released",
    );
}

/// A claim whose lifecycle was carried forward while it was in flight must
/// settle with NO observation: observing refreshes the learned row's decay and
/// increments its `observations`, so a straggler reaching that call would
/// corroborate a lifecycle it never tested.
///
/// Mutation check: delete the `owns_current_incarnation` gate at the top of
/// `record_canary_confirmation` and this goes red on the observation count.
#[test]
fn a_stale_claim_confirms_nothing_and_observes_nothing() {
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    plant_eligible(&reg, &k, t0);
    let planned = reg.canaries().snapshot(&k).expect("resident").incarnation;
    let claim = reg
        .canaries()
        .claim_canary(&k, planned)
        .expect("claim admitted");

    let before = reg
        .learned()
        .snapshot()
        .into_iter()
        .find(|e| e.state_key == k.state_key() && e.feature_key == k.capability_key())
        .expect("fixture premise: the verdict row is resident");

    // The identity is carried forward while this canary is still out.
    reg.canaries().acknowledge_confirmation(&k, planned + 1, 1);

    let event =
        reg.record_canary_confirmation(&k, claim, reg.learned().generation(), 400, vec![], t0);

    assert!(
        event.is_none(),
        "a stale claim rides no learn row out to the ledger",
    );
    let after = reg
        .learned()
        .snapshot()
        .into_iter()
        .find(|e| e.state_key == k.state_key() && e.feature_key == k.capability_key())
        .expect("the row is untouched, not removed");
    assert_eq!(
        after.observations, before.observations,
        "the stale confirmation must not increment the row's observations",
    );
    assert_eq!(
        after.expires_at, before.expires_at,
        "nor refresh its decay deadline",
    );
    let snap = reg.canaries().snapshot(&k).expect("resident");
    assert_eq!(
        snap.incarnation,
        planned + 1,
        "the carried lifecycle is left exactly where it was",
    );
    assert_eq!(
        snap.last_outcome, None,
        "and no outcome is recorded against a lifecycle this canary never tested",
    );
}

/// A DISPROOF from a superseded canary must touch nothing: it must not remove a
/// row, and it must not free the identity's canary slot.
///
/// The removal is the first exposure: presenting only a generation token, it
/// names the identity without naming the lifecycle, so it removes whatever row is
/// currently resident. That row belongs to the lifecycle that REPLACED the one
/// this canary tested, and a canary that proved a superseded verdict wrong has no
/// evidence about it. The removal is therefore gated on the claim's ownership,
/// which is why the claim is handed to the registry rather than settled at the
/// call site.
///
/// The SLOT is the second exposure, and the ordering inside the settlement is
/// what closes it: the release must come BELOW the ownership check, or a stale
/// settlement frees the claim a live canary of the current lifecycle holds and
/// admits the second concurrent canary the slot exists to prevent. This test
/// holds exactly that live claim, so the slot assertions can fail -- without it
/// `canary_claimed` reads `false` whether or not the stale path touched it, and
/// the check would be vacuous.
///
/// Deterministic rather than timing-based: the carry is performed directly while
/// the claim is held, which is exactly the interleaving a real race produces.
///
/// Mutation checks, two, each red on a different assertion: (a) drop the
/// `settle_disproved_if_current` gate at the top of `record_canary_disproof` --
/// red on the clear event; (b) move `entry.canary_claimed = false` ABOVE the
/// `entry.incarnation != incarnation` check in
/// `FieldCanaryRegistry::settle_disproved_if_current` -- red on the retained
/// slot.
#[test]
fn a_stale_disproof_cannot_clear_the_current_lifecycles_verdict() {
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    plant_eligible(&reg, &k, t0);
    let stale_incarnation = reg.canaries().snapshot(&k).expect("resident").incarnation;
    let claim = reg
        .canaries()
        .claim_canary(&k, stale_incarnation)
        .expect("claim admitted");

    // A request the CURRENT lifecycle repaired, so a wrongly-charged alarm would
    // be visible rather than passing on zero.
    let carried_incarnation = stale_incarnation + 5;
    reg.canaries()
        .acknowledge_confirmation(&k, carried_incarnation, 1);
    drop(
        reg.canaries()
            .begin_modified_request(&k, carried_incarnation)
            .expect("accounts at the carried incarnation"),
    );
    // A LIVE canary of the carried lifecycle, holding the identity's single slot.
    // Without it the slot assertions below would be vacuous: the flag reads
    // `false` whether or not the stale settlement touched it.
    let live_claim = reg
        .canaries()
        .claim_canary(&k, carried_incarnation)
        .expect("the carried lifecycle claims its own slot");
    let alarm_before = reg.canaries().disproved_requests_total();

    let cleared = reg.record_canary_disproof(&k, claim, reg.learned().generation());

    assert!(
        cleared.is_none(),
        "a superseded canary removed nothing, so no clear event may ride out to \
         the ledger and no warm rebuild is told to forget a live verdict",
    );
    assert!(
        reg.is_negative_acting(&k, t0),
        "and the current lifecycle's row is still resident and acting",
    );
    let snap = reg
        .canaries()
        .snapshot(&k)
        .expect("the carried lifecycle's canary state must survive");
    assert_eq!(
        snap.incarnation, carried_incarnation,
        "on the lifecycle that replaced the tested one",
    );
    assert!(
        !snap.preflight_suspended,
        "a lifecycle nothing disproved must not be suspended",
    );
    assert_eq!(
        snap.modified_since_confirmation, 1,
        "nor its exposure transferred out from under it",
    );
    assert_eq!(
        reg.canaries().disproved_requests_total(),
        alarm_before,
        "nor charged to the lifetime alarm",
    );
    assert!(
        snap.canary_claimed,
        "and the LIVE canary of the carried lifecycle still holds the slot: a \
         stale settlement that freed it would admit a second concurrent canary \
         for one identity, which is exactly what the slot exists to prevent",
    );
    assert!(
        reg.canaries()
            .claim_canary(&k, carried_incarnation)
            .is_none(),
        "so a further request for the current lifecycle is still refused the slot",
    );
    // Held until here: the stale disproof above must not free a slot it does not
    // own.
    drop(live_claim);
}

/// The ownership gate must not turn into a blanket refusal: a disproof whose
/// claim DOES own the current lifecycle still suspends pre-flight, charges the
/// alarm, and durably clears the verdict -- in that order.
///
/// The paired positive control for the gate above. Without it, a
/// `record_canary_disproof` that simply returned `None` would satisfy the stale
/// test while disabling disproof entirely.
///
/// Mutation check: replace the ownership gate's condition with `false` (so every
/// disproof is treated as stale) and this goes red on the clear event.
#[test]
fn an_owning_disproof_still_suspends_charges_and_clears() {
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    plant_eligible(&reg, &k, t0);
    let incarnation = reg.canaries().snapshot(&k).expect("resident").incarnation;
    drop(
        reg.canaries()
            .begin_modified_request(&k, incarnation)
            .expect("accounts at the resident incarnation"),
    );
    let claim = reg
        .canaries()
        .claim_canary(&k, incarnation)
        .expect("claim admitted");
    let alarm_before = reg.canaries().disproved_requests_total();

    let cleared = reg.record_canary_disproof(&k, claim, reg.learned().generation());

    assert!(
        cleared.is_some(),
        "an owning disproof durably clears the verdict its canary proved wrong, \
         and hands the event out for the ledger",
    );
    assert!(
        !reg.is_negative_acting(&k, t0),
        "so the next request for this identity forwards unchanged",
    );
    assert_eq!(
        reg.canaries().disproved_requests_total(),
        alarm_before + 1,
        "the request the wrong verdict modified is charged to the lifetime alarm, \
         which the suspend-and-transfer step must have run BEFORE the clear \
         dropped the identity's state",
    );
    assert!(
        reg.canaries().snapshot(&k).is_none(),
        "and a clear that actually removed a row drops the canary state with it",
    );
}

// ---------------------------------------------------------------------------
// The durable-acknowledgment entry point
// ---------------------------------------------------------------------------
//
// These pin the two refusals the entry point owns -- the acting-in-generation read
// and the incarnation match -- at THIS layer, and that placement is the point. Both
// are also observable end to end (a refused acknowledgment leaves the next request
// unrewritten), but the end-to-end fixture cannot ATTRIBUTE a refusal: several
// independent conditions produce the same unrewritten request, so removing either
// check leaves that test green while this one reds. A check whose removal no test
// notices is not a check.

/// The resident entry's own incarnation for `k`, the value an acknowledgment must
/// present.
fn resident_incarnation(learned: &LearnedCapabilityRegistry, k: &FieldVerdictKey) -> u64 {
    learned.resident_incarnation_for_tests(k.state_key(), k.capability_key(), k.provider_kind())
}

#[test]
fn a_matching_acknowledgment_advances_the_count_and_makes_the_verdict_eligible() {
    // THE happy path, and it is asserted on ELIGIBILITY rather than on the count
    // alone: the count is only interesting because it gates traffic, and a test
    // reading it back would pass on a build that raised it somewhere the
    // eligibility read never looks.
    let learned = Arc::new(LearnedCapabilityRegistry::new(
        DECAY,
        WINDOW,
        DEFAULT_MAX_ENTRIES,
    ));
    let reg = FieldVerdictRegistry::new(Arc::clone(&learned));
    let now = Instant::now();
    let k = key("t");
    plant_acting_negative(&learned, &k, now);
    let generation = learned.generation();
    let incarnation = resident_incarnation(&learned, &k);
    assert!(
        !reg.preflight_eligible(&k, generation, now),
        "premise: a resident acting verdict with NO acknowledged confirmation is not \
         pre-flight eligible -- which is the whole gap this acknowledgment closes",
    );

    let acknowledged = reg.acknowledge_durable_confirmation(&k, generation, incarnation, 1, now);

    assert!(acknowledged, "a matching acknowledgment is admitted");
    assert!(
        reg.preflight_eligible(&k, generation, now),
        "and the verdict is now pre-flight eligible IN THIS PROCESS -- no restart, \
         which is exactly what the design requires",
    );
}

#[test]
fn an_acknowledgment_for_a_mismatched_incarnation_advances_nothing() {
    // THE INCARNATION REFUSAL, attributable here and nowhere else. A delayed
    // acknowledgment from a superseded lifecycle must not credit the lifecycle that
    // replaced it, and one from an unseen future incarnation must not reseed the
    // canary state -- dropping a live cadence and claim on the strength of a
    // mutation this registry never saw.
    //
    // Driven in BOTH directions, because the code is one comparison and a
    // one-sided test would pass on `<` or `>` in place of `!=`.
    //
    // Mutation check: delete the incarnation comparison from
    // `acknowledge_durable_confirmation` -> red here in both directions.
    let learned = Arc::new(LearnedCapabilityRegistry::new(
        DECAY,
        WINDOW,
        DEFAULT_MAX_ENTRIES,
    ));
    let reg = FieldVerdictRegistry::new(Arc::clone(&learned));
    let now = Instant::now();
    let k = key("t");
    plant_acting_negative(&learned, &k, now);
    let generation = learned.generation();
    let resident = resident_incarnation(&learned, &k);

    for wrong in [resident + 1, resident + 7] {
        assert!(
            !reg.acknowledge_durable_confirmation(&k, generation, wrong, 1, now),
            "an acknowledgment naming incarnation {wrong} must be refused: the \
             resident lifecycle is {resident}",
        );
        assert!(
            !reg.preflight_eligible(&k, generation, now),
            "and nothing may have advanced -- a refused acknowledgment that still \
             raised the count would make the verdict eligible on evidence for \
             another lifecycle",
        );
    }

    // THE POSITIVE CONTROL on the same fixture: the resident incarnation IS
    // admitted, so the refusals above are the comparison rather than a registry
    // that refuses everything.
    assert!(
        reg.acknowledge_durable_confirmation(&k, generation, resident, 1, now),
        "control: the resident incarnation is admitted",
    );
    assert!(reg.preflight_eligible(&k, generation, now));
}

#[test]
fn an_acknowledgment_for_a_key_with_no_acting_verdict_advances_nothing() {
    // THE ACTING-IN-GENERATION REFUSAL, attributable here. Three shapes, each a way
    // the identity can have moved on between the write and its acknowledgment:
    // absent entirely, resident but LAPSED, and under a generation this event may
    // not write against.
    //
    // Mutation check: replace the `field_acting_facts_in_generation` read with a
    // fabricated `Some(..)` -> red here on all three.
    let learned = Arc::new(LearnedCapabilityRegistry::new(
        DECAY,
        WINDOW,
        DEFAULT_MAX_ENTRIES,
    ));
    let reg = FieldVerdictRegistry::new(Arc::clone(&learned));
    let now = Instant::now();
    let generation = learned.generation();

    // ABSENT: nothing was ever planted for this identity.
    let absent = key("never-planted");
    assert!(
        !reg.acknowledge_durable_confirmation(&absent, generation, 0, 1, now),
        "an acknowledgment for an identity with no resident entry advances nothing",
    );
    assert!(!reg.preflight_eligible(&absent, generation, now));

    // LAPSED: resident, but its decay window has passed, so it is not acting.
    let lapsed = key("lapsed");
    plant_acting_negative(&learned, &lapsed, now);
    let lapsed_incarnation = resident_incarnation(&learned, &lapsed);
    let after_decay = now + DECAY + Duration::from_secs(1);
    assert!(
        !reg.acknowledge_durable_confirmation(
            &lapsed,
            generation,
            lapsed_incarnation,
            1,
            after_decay,
        ),
        "a LAPSED verdict is not acting, so an acknowledgment for it advances \
         nothing -- the verdict it describes is due for re-verification, not for \
         being handed pre-flight authority",
    );
    assert!(!reg.preflight_eligible(&lapsed, generation, after_decay));

    // The POSITIVE CONTROL for the lapsed case: the SAME identity at an instant
    // inside its window is admitted, so the refusal is the decay rather than the
    // planting.
    assert!(
        reg.acknowledge_durable_confirmation(&lapsed, generation, lapsed_incarnation, 1, now),
        "control: the same identity inside its decay window is admitted",
    );
}

#[test]
fn an_acknowledgment_the_canary_layer_refuses_reports_false_rather_than_a_success() {
    // THE TWO-LAYER GAP, and the one case neither existing test covers.
    //
    // `acknowledge_durable_confirmation` validates the event's incarnation against the
    // LEARNED row, then delegates to the canary registry, which validates it again
    // against the resident CANARY state. Those two are not the same number: the canary
    // state is reseeded by a cold-rebuild seed, by a clear, and by a confirmation
    // carry, none of which moves the learned incarnation. So an event can be CURRENT
    // by the learned row and SUPERSEDED by the canary state -- and that is the path
    // this test drives.
    //
    // Previously the function returned `true` and logged "acknowledged" on it: a FALSE
    // SUCCESS for a call that wrote nothing. The count could not have caught it either
    // -- a refusal reports the count that stands, so a refusal at three and an
    // acceptance at three are the same `u32`.
    //
    // Mutation check: make the propagation unconditional (`let _ = ack; ... true`) ->
    // red here, while every other acknowledgement test stays green, because this is
    // the only fixture where the two layers disagree.
    let learned = Arc::new(LearnedCapabilityRegistry::new(
        DECAY,
        WINDOW,
        DEFAULT_MAX_ENTRIES,
    ));
    let reg = FieldVerdictRegistry::new(Arc::clone(&learned));
    let now = Instant::now();
    let k = key("t");
    plant_acting_negative(&learned, &k, now);
    let generation = learned.generation();
    let learned_incarnation = resident_incarnation(&learned, &k);

    // The CANARY state is carried FORWARD of the learned row, through the registry's
    // own production seam (the cold-rebuild seed, which is what a boot that replayed a
    // since-confirmed verdict produces). The learned row stays where it was, so an
    // event stamped with the learned incarnation is current by that check and stale by
    // the canary one.
    reg.canaries()
        .seed_from_rebuild(&k, learned_incarnation + 1, 2, false);
    assert_eq!(
        reg.canaries().snapshot(&k).expect("seeded").incarnation,
        learned_incarnation + 1,
        "premise: the canary state names a NEWER lifecycle than the learned row",
    );

    let acknowledged =
        reg.acknowledge_durable_confirmation(&k, generation, learned_incarnation, 9, now);

    assert!(
        !acknowledged,
        "an acknowledgement the canary layer refused must report FALSE: it wrote \
         nothing, so reporting success would tell the caller a count backs \
         eligibility when it does not",
    );
    assert_eq!(
        reg.canaries().snapshot(&k).expect("resident").confirmations,
        2,
        "and the standing count is untouched -- the stale event's 9 was not written",
    );

    // THE POSITIVE CONTROL, and it has to be built rather than restated: the refusal
    // above must be attributable to the CANARY layer specifically, so the control
    // brings the two layers back into agreement and shows the same call then succeeds.
    //
    // Reseeding the canary state back to the LEARNED incarnation is the one move that
    // changes only the disagreement -- same registry, same key, same generation, same
    // event stamps.
    reg.canaries()
        .seed_from_rebuild(&k, learned_incarnation, 2, false);

    let acknowledged =
        reg.acknowledge_durable_confirmation(&k, generation, learned_incarnation, 9, now);

    assert!(
        acknowledged,
        "control: with the two layers in agreement the IDENTICAL call is accepted, so \
         the refusal above is the canary layer's disagreement rather than a registry \
         that refuses everything",
    );
    assert_eq!(
        reg.canaries().snapshot(&k).expect("resident").confirmations,
        9,
        "and an accepted acknowledgement does write its own count",
    );
}
