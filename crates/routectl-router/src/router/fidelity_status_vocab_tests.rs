// The closed-token vocabularies this surface emits, and the log-injection safety of
// its two variable-length identity fields.
//
// An `include!`d FRAGMENT of `fidelity_status_tests.rs` -- no top-level `use`, no
// `mod`; see that host's note on why the convention is one `mod tests` plus includes.

// ---------------------------------------------------------------------------
// The blocked-reason vocabulary
// ---------------------------------------------------------------------------

/// Each blocked-reason token is the exact operator-facing string, and each variant
/// resolves to its OWN.
///
/// Pinned against literals spelled here, because these tokens are a STATUS CONTRACT
/// this surface owns -- they are what an operator dashboard, alert, or log query
/// matches on, so a change is a deliberate breaking edit rather than an internal
/// rename. They were once read from the planner's internal constants, which inverted
/// that: those are a debug vocabulary the planner may retune, and a rename there would
/// silently move what every consumer matches.
///
/// Not a tautology, because what it catches is the WIRING: swapping two arms leaves
/// every literal intact and every other assertion green, and the result is a surface
/// telling an operator to add a `[fidelity]` entry for a verdict that is actually short
/// of confirmations -- or to edit config for one that simply lacks evidence.
#[test]
fn each_blocked_reason_resolves_to_its_own_operator_facing_token() {
    assert_eq!(
        PreflightBlockedReason::CapabilityDisabled.as_str(),
        "capability_disabled",
    );
    assert_eq!(
        PreflightBlockedReason::UnsupportedLane.as_str(),
        "unsupported_lane",
    );
    assert_eq!(PreflightBlockedReason::NotEligible.as_str(), "not_eligible");
    assert_eq!(PreflightBlockedReason::BelowQuorum.as_str(), "below_quorum");
    assert_eq!(
        PreflightBlockedReason::NoTargetOptIn.as_str(),
        "no_target_opt_in",
    );
    assert_eq!(
        PreflightBlockedReason::CanarySuspended.as_str(),
        "canary_suspended",
    );
    assert_eq!(
        PreflightBlockedReason::MaskedByOverride.as_str(),
        "masked_by_override",
    );
}

/// Every token this surface can emit is distinct.
///
/// A vocabulary with a collision is worse than a missing token: two different
/// operator situations rendering the same word means the surface reports a state
/// that is not the one it is in, and nothing about the reading says so.
#[test]
fn every_status_token_is_distinct() {
    let mut tokens = vec![
        PreflightBlockedReason::NotEligible.as_str(),
        PreflightBlockedReason::BelowQuorum.as_str(),
        PreflightBlockedReason::NoTargetOptIn.as_str(),
        PreflightBlockedReason::CanarySuspended.as_str(),
        PreflightBlockedReason::CapabilityDisabled.as_str(),
        PreflightBlockedReason::UnsupportedLane.as_str(),
        PreflightBlockedReason::MaskedByOverride.as_str(),
        BLOCKED_REASON_NONE,
    ];
    let planned = tokens.len();
    assert_eq!(
        planned, 8,
        "every variant including the two lane-level ones is listed"
    );
    tokens.sort_unstable();
    tokens.dedup();
    assert_eq!(
        tokens.len(),
        planned,
        "the blocked-reason vocabulary must have no collision: {tokens:?}",
    );

    let mut postures = vec![
        CanaryPosture::Counting.as_str(),
        CanaryPosture::Due.as_str(),
        CanaryPosture::InFlight.as_str(),
    ];
    postures.sort_unstable();
    postures.dedup();
    assert_eq!(postures.len(), 3, "and neither may the posture vocabulary");

    let mut outcomes = vec![
        canary_outcome_token(CanaryOutcome::Confirmed),
        canary_outcome_token(CanaryOutcome::Regressed),
        canary_outcome_token(CanaryOutcome::Inconclusive),
        CANARY_OUTCOME_NONE,
    ];
    outcomes.sort_unstable();
    outcomes.dedup();
    assert_eq!(
        outcomes.len(),
        4,
        "and the never-settled case must be distinguishable from every real \
         outcome, or a re-verification that never ran reads as one that proved nothing",
    );
}

/// The prefix-impacting class reports a HIGHER required quorum than the envelope
/// class, and reports itself prefix-impacting.
///
/// The two facts that make the row's cost legible, pinned on the class whose cost
/// is the reason the gates exist. Asserted as a RELATION rather than against
/// literals, so an order-preserving retune of either constant does not have to
/// touch this test -- and the exact values stay pinned where they are defined.
#[test]
fn the_prefix_impacting_class_reports_the_higher_quorum_and_its_prefix_cost() {
    use crate::router::field_repair::TransformClass;

    // The ORDER of the two quorums is not asserted here: it is a compile-time
    // `const _: () = assert!(...)` beside the constants themselves, which rustc
    // evaluates whenever it compiles the crate. A runtime assertion over two
    // constants is a tautology while they agree -- it can only fail in a world
    // where the build already failed.
    assert_eq!(
        TransformClass::PrefixImpacting.required_quorum(),
        PREFIX_QUORUM,
    );
    assert_eq!(TransformClass::Envelope.required_quorum(), ENVELOPE_QUORUM);
    assert!(
        TransformClass::PrefixImpacting.requires_target_opt_in(),
        "a prefix-impacting rewrite is the class that costs a cache-prefix recompute",
    );
    assert!(!TransformClass::Envelope.requires_target_opt_in());
}

// ---------------------------------------------------------------------------
// Log-injection safety of the variable-length fields
// ---------------------------------------------------------------------------

/// A hostile state key reaches the row SANITIZED and length-capped.
///
/// The state key is the one variable-length value on the row that a caller
/// outside routectl's own vocabulary can influence -- it comes from the
/// operator's `[providers]` table and from state-key construction, not from a
/// request -- so what it needs is log-injection and unbounded-length safety
/// rather than redaction. A raw newline plus an ANSI escape is how a forged
/// second log line is injected, and an uncapped value is how one field prints
/// megabytes into a journal.
///
/// The CAP is the load-bearing half: a `Debug` rendering escapes control bytes on
/// its own, so a control-byte assertion alone would pass against an unsanitized
/// value too. Both are asserted because the row is Debug-rendered TODAY and might
/// not always be, and only sanitization produces the cap.
///
/// Mutation check: replace `sanitize_for_log(&entry.state_key)` in
/// `status_row_for` with `entry.state_key.clone()` -> red on the cap assertion.
#[test]
fn a_hostile_state_key_reaches_the_row_sanitized_and_capped() {
    let hostile = format!("m0\n\u{1b}[31mforged\u{7}{}", "A".repeat(4096));
    let router = bare_router();
    let stamped = Instant::now();
    router
        .learned_capabilities
        .import_entries(vec![crate::learned_capability::ExportedEntry {
            state_key: hostile.clone(),
            feature_key: grounded_key(),
            verdict: crate::learned_capability::EntryVerdict::Negative,
            signal: SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: stamped,
            last_seen: stamped,
            expires_at: stamped + NOT_LAPSED,
            phase: FailurePhase::F1,
            source: EvidenceSource::Live,
            in_flight: false,
            consecutive_failed_probes: 0,
            evidence_class: None,
        }]);

    let rows = router.field_verdict_status();

    let row = only_row(&rows);
    assert!(
        row.state_key.len() < hostile.len(),
        "premise plus contract: the hostile key is longer than the sanitizer's cap, \
         and the row's key is shorter than it -- so the cap genuinely applied",
    );
    assert!(
        !row.state_key.contains(&"A".repeat(1024)),
        "the row's state key is length-capped, so one row cannot print an unbounded \
         string into an operator's log",
    );
    assert!(
        !row.state_key.contains('\n')
            && !row.state_key.contains('\u{1b}')
            && !row.state_key.contains('\u{7}'),
        "and no control byte survives: a newline or an escape sequence in a log field \
         is how a forged second line is injected",
    );
}

// ---------------------------------------------------------------------------
// Lane-level blocked reasons
// ---------------------------------------------------------------------------

/// With `capability.enabled = false`, every verdict reports `capability_disabled`.
#[test]
fn a_disabled_capability_kill_switch_reports_capability_disabled() {
    use crate::config::{AliasValue, ModelEntry, ProviderEntry};
    let mut config = Config::default();
    config.capability.enabled = false;
    config.providers.insert(
        "anthropic".to_string(),
        ProviderEntry::anthropic_api("env://K"),
    );
    config.models.insert(
        STATE_KEY.to_string(),
        ModelEntry::new("anthropic", "claude-sonnet-4-5"),
    );
    config.aliases.insert(
        "default".to_string(),
        AliasValue::Single(STATE_KEY.to_string()),
    );
    let router = router_assuming_durable_writes(config);
    plant_verdict(&router, grounded_key());
    seed_confirmations(&router, ENVELOPE_QUORUM);

    let rows = router.field_verdict_status();
    let row = only_row(&rows);
    assert_eq!(
        row.blocked_reason,
        Some(PreflightBlockedReason::CapabilityDisabled)
    );
}

/// A non-Anthropic provider lane reports `unsupported_lane`.
#[test]
fn a_non_anthropic_lane_reports_unsupported_lane() {
    use crate::config::{AliasValue, ModelEntry, ProviderEntry};
    let mut config = Config::default();
    config.providers.insert(
        "oc".to_string(),
        ProviderEntry::openai_compat("https://example.test/v1", "env://K"),
    );
    config
        .models
        .insert(STATE_KEY.to_string(), ModelEntry::new("oc", "gpt-4o"));
    config.aliases.insert(
        "default".to_string(),
        AliasValue::Single(STATE_KEY.to_string()),
    );
    let router = router_assuming_durable_writes(config);
    // Plant with the grounded key -- the verdict exists, the lane refuses it.
    plant_verdict(&router, grounded_key());

    let rows = router.field_verdict_status();
    let row = only_row(&rows);
    assert_eq!(
        row.blocked_reason,
        Some(PreflightBlockedReason::UnsupportedLane)
    );
}

/// A loopback-base-URL Anthropic lane reports `unsupported_lane`.
///
/// A loopback is an unattributable target: a rejection from it cannot be attributed
/// to an upstream, so a verdict must not act on it.
#[test]
fn a_loopback_anthropic_lane_reports_unsupported_lane() {
    use crate::config::{AliasValue, ModelEntry};
    let mut config = Config::default();
    // A loopback base URL with the anthropic-api kind.
    let toml_text = "\n[providers.anthropic]\nkind = \"anthropic-api\"\napi_key_ref = \"env://K\"\nbase_url = \"http://127.0.0.1:8899\"\n";
    let parsed: Config = toml::from_str(toml_text).expect("valid");
    config.providers = parsed.providers;
    config.models.insert(
        STATE_KEY.to_string(),
        ModelEntry::new("anthropic", "claude-sonnet-4-5"),
    );
    config.aliases.insert(
        "default".to_string(),
        AliasValue::Single(STATE_KEY.to_string()),
    );
    let router = router_assuming_durable_writes(config);
    plant_verdict(&router, grounded_key());

    let rows = router.field_verdict_status();
    let row = only_row(&rows);
    assert_eq!(
        row.blocked_reason,
        Some(PreflightBlockedReason::UnsupportedLane)
    );
}

/// A pooled seat (`nick#member`) resolves the MEMBER provider entry for kind, base
/// URL, and override mask, and the BASE (model nick) for nickname and opt-in.
///
/// The member-scoped override on the member's provider entry must be found; a
/// member-scoped prefix opt-in must be found. Without this, a pooled verdict would
/// resolve the pool name or the model nick as the provider entry and miss the
/// member's own override and base URL.
#[test]
fn a_pooled_seat_resolves_member_for_provider_and_base_for_model() {
    use crate::config::{AliasValue, ModelEntry, OverrideEntry, ProviderEntry};
    let mut config = Config::default();
    // The POOL provider entry: the pool itself, with NO override.
    config.providers.insert(
        "anthropic-pool".to_string(),
        ProviderEntry::anthropic_api("env://K"),
    );
    // The MEMBER provider entry: a DIFFERENT name from the pool, carrying the override.
    config.providers.insert(
        "seat-a".to_string(),
        ProviderEntry::anthropic_api("env://K"),
    );
    // The MODEL targets the pool, whose member is seat-a.
    config.models.insert(
        STATE_KEY.to_string(),
        ModelEntry::new("anthropic-pool", "claude-sonnet-4-5"),
    );
    config.aliases.insert(
        "default".to_string(),
        AliasValue::Single(STATE_KEY.to_string()),
    );
    // A member-scoped override: keyed on the MEMBER provider entry name.
    config.capability.overrides.insert(
        "seat-a".to_string(),
        OverrideEntry {
            unsupported: Vec::new(),
            force_supported: vec![grounded_key()],
        },
    );
    let router = router_assuming_durable_writes(config);
    // Plant the verdict on the POOLED state key: `nick#member`.
    let pooled_key = format!("{}#seat-a", STATE_KEY);
    let cap_key = grounded_key();
    let provider_kind = router.provider_kind_for_state_key(&pooled_key).to_string();
    let vk = crate::field_verdict::FieldVerdictKey::from_capability_key(
        pooled_key.clone(),
        cap_key.clone(),
        provider_kind,
    );
    let stamped = Instant::now();
    router
        .learned_capabilities
        .import_entries(vec![crate::learned_capability::ExportedEntry {
            state_key: pooled_key.clone(),
            feature_key: cap_key,
            verdict: crate::learned_capability::EntryVerdict::Negative,
            signal: SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: stamped,
            last_seen: stamped,
            expires_at: stamped + NOT_LAPSED,
            phase: FailurePhase::F1,
            source: EvidenceSource::Live,
            in_flight: false,
            consecutive_failed_probes: 0,
            evidence_class: None,
        }]);
    // Seed the canary at quorum so the verdict would be unblocked BUT for the mask.
    let incarnation = router.learned_capabilities.resident_incarnation_for_tests(
        &pooled_key,
        &grounded_key(),
        router.provider_kind_for_state_key(&pooled_key),
    );

    router
        .field_verdicts()
        .canaries()
        .seed_from_rebuild(&vk, incarnation, ENVELOPE_QUORUM, false);

    let rows = router.field_verdict_status();
    let row = only_row(&rows);

    assert_eq!(
        row.blocked_reason,
        Some(PreflightBlockedReason::MaskedByOverride),
        "the member-scoped override on seat-a is found for the pooled key: the \
         provider name resolved to the MEMBER (suffix), not the base (model nick)",
    );
}
