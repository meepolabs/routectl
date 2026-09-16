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
        .admit_provisional(&display, REMOTE_BASE, t0)
        .expect("an unknown pair admits one repair")
        .commit(400, vec![], t0);

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
        .admit_provisional(&here, REMOTE_BASE, t0)
        .expect("unknown pair admits")
        .commit(400, vec![], t0);

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
        .admit_provisional(&anthropic, REMOTE_BASE, t0)
        .expect("unknown pair admits")
        .commit(400, vec![], t0);

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
        .admit_provisional(&k, REMOTE_BASE, t0)
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
        .admit_provisional(&k, REMOTE_BASE, t0)
        .expect("unknown pair admits");

    // Act -- the repaired retry came back 2xx, so the rejection is confirmed.
    let event = guard.commit(400, vec!["thinking".to_string()], t0);

    // Assert
    assert!(reg.is_negative_acting(&k, t0));
    assert_eq!(event.observations, 1);
    assert_eq!(event.capability_key, k.capability_key());
}

#[test]
fn a_failed_repair_leaves_resident_state_unchanged() {
    // Arrange -- an entry learned earlier, now lapsed and re-verifying.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    let _ = reg
        .admit_provisional(&k, REMOTE_BASE, t0)
        .expect("unknown pair admits")
        .commit(400, vec![], t0);
    let lapsed = t0 + DECAY + Duration::from_secs(1);
    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, lapsed)
        .expect("a lapsed entry admits one re-verification");

    // Act -- the repair itself failed, so nothing was proven either way.
    guard.release();

    // Assert -- neither refreshed nor cleared: the entry survives on its
    // ORIGINAL window and the next request re-verifies.
    assert!(reg.is_negative_acting(&k, t0 + DECAY / 2));
    assert!(reg.admit_provisional(&k, REMOTE_BASE, lapsed).is_some());
}

#[test]
fn an_unrelated_error_learns_nothing_on_an_unknown_pair() {
    // Arrange
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, t0)
        .expect("unknown pair admits");

    // Act -- a timeout, a 5xx, a disconnect: not evidence about the field.
    guard.release();

    // Assert
    assert!(!reg.is_negative_acting(&k, t0));
    assert_eq!(reg.snapshot_len(), 0);
    assert!(reg.admit_provisional(&k, REMOTE_BASE, t0).is_some());
}

#[test]
fn dropping_an_unsettled_guard_releases_its_slot_and_learns_nothing() {
    // Arrange -- a dispatch path that returns early never settles.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");

    // Act
    drop(
        reg.admit_provisional(&k, REMOTE_BASE, t0)
            .expect("unknown pair admits"),
    );

    // Assert -- no learning by omission, and the slot is free again.
    assert!(!reg.is_negative_acting(&k, t0));
    assert_eq!(reg.snapshot_len(), 0);
    assert!(reg.admit_provisional(&k, REMOTE_BASE, t0).is_some());
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
        .admit_provisional(&k, REMOTE_BASE, lapsed)
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
        .admit_provisional(&k, REMOTE_BASE, t0)
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
                let guard = reg.admit_provisional(&k, REMOTE_BASE, t0);
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
        .admit_provisional(&k, REMOTE_BASE, t0)
        .expect("unknown pair admits");

    // Act / Assert -- no negative is resident yet, and the second caller is
    // still refused rather than mounting its own repair.
    assert!(!reg.is_negative_acting(&k, t0));
    assert!(reg.admit_provisional(&k, REMOTE_BASE, t0).is_none());

    // Once the repair settles the slot reopens.
    guard.release();
    assert!(reg.admit_provisional(&k, REMOTE_BASE, t0).is_some());
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
    assert!(reg.admit_provisional(&k, REMOTE_BASE, t0).is_none());
}

// --- loopback suppression ---

#[test]
fn a_loopback_anthropic_target_cannot_mint_a_field_verdict() {
    // Arrange -- the local hop: configured anthropic-api, loopback base.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("local-hop");

    // Act
    let admitted = reg.admit_provisional(&k, LOOPBACK_BASE, t0);

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
        .admit_provisional(&k, REMOTE_BASE, t0)
        .expect("the public Anthropic endpoint may mint")
        .commit(400, vec![], t0);

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
    let admitted = reg.admit_provisional(&k, "https://anthropic.upstream.example/v1", t0);

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
            reg.admit_provisional(&k, LOOPBACK_BASE, t0).is_none(),
            "a loopback target must not mint on kind {kind}"
        );
        assert!(
            reg.admit_provisional(&k, REMOTE_BASE, t0).is_some(),
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
            reg.admit_provisional(&k, base, t0).is_none(),
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
            reg.admit_provisional(&k, base, t0).is_some(),
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
    assert!(reg.admit_provisional(&single, REMOTE_BASE, t0).is_some());
}

// --- emission ---

#[test]
fn the_committed_row_reuses_the_existing_event_shape() {
    // Arrange
    let reg = registry();
    let t0 = Instant::now();
    let k = key("prod-target");
    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, t0)
        .expect("unknown pair admits");

    // Act
    let event = guard.commit(400, vec!["thinking".to_string()], t0);

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

#[test]
fn a_repeated_confirmed_rejection_refreshes_the_same_row() {
    // Arrange -- learned once, lapsed, and rejected again.
    let reg = registry();
    let t0 = Instant::now();
    let k = key("t");
    let _ = reg
        .admit_provisional(&k, REMOTE_BASE, t0)
        .expect("unknown pair admits")
        .commit(400, vec![], t0);
    let lapsed = t0 + DECAY + Duration::from_secs(1);
    let guard = reg
        .admit_provisional(&k, REMOTE_BASE, lapsed)
        .expect("a lapsed entry admits one re-verification");

    // Act
    let event = guard.commit(400, vec![], lapsed);

    // Assert -- one row with its history intact, acting on a fresh window.
    assert_eq!(event.observations, 2);
    assert_eq!(reg.snapshot_len(), 1);
    assert!(reg.is_negative_acting(&k, lapsed));
}
