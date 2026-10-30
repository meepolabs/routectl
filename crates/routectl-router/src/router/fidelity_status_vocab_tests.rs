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
    let hostile = format!("anthropic#m0\n\u{1b}[31mforged\u{7}{}", "A".repeat(4096));
    let router = bare_router();
    let stamped = Instant::now();
    router
        .learned_capabilities
        .import_entries(vec![crate::learned_capability::ExportedEntry {
            provider_kind: router.provider_kind_for_state_key(&hostile).to_string(),
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
        NICKNAME.to_string(),
        ModelEntry::new("anthropic", "claude-sonnet-4-5"),
    );
    config.aliases.insert(
        "default".to_string(),
        AliasValue::Single(NICKNAME.to_string()),
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

/// A lane on a kind field repair does not act on reports `unsupported_lane`.
#[cfg(feature = "gemini")]
#[test]
fn a_lane_outside_the_repair_kinds_reports_unsupported_lane() {
    use crate::config::{AliasValue, ModelEntry, ProviderEntry};
    let mut config = Config::default();
    config
        .providers
        .insert("oc".to_string(), ProviderEntry::gemini("env://K"));
    config
        .models
        .insert(NICKNAME.to_string(), ModelEntry::new("oc", "gpt-4o"));
    config.aliases.insert(
        "default".to_string(),
        AliasValue::Single(NICKNAME.to_string()),
    );
    let router = router_assuming_durable_writes(config);
    // Plant with the grounded key on the model's own lane -- the verdict exists,
    // the lane refuses it.
    plant_verdict_on(&router, "oc#gpt-4o", grounded_key());

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
        NICKNAME.to_string(),
        ModelEntry::new("anthropic", "claude-sonnet-4-5"),
    );
    config.aliases.insert(
        "default".to_string(),
        AliasValue::Single(NICKNAME.to_string()),
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

/// The status token each planted row reports on a lane fixture whose
/// `[providers.p0]` body is `provider_toml`, keyed by row path.
///
/// Every row is planted unacknowledged, so a row the lane's view carries reads
/// the evidence token `not_eligible` and one it omits reads `unsupported_lane`:
/// the two differ by view membership alone.
fn lane_tokens_for(provider_toml: &str, paths: &[&str]) -> Vec<(String, &'static str)> {
    let router = convergence_router_on(provider_toml);
    for path in paths {
        let key = crate::field_capability::field_capability_key(path).expect("well-formed path");
        plant_verdict_on(&router, CONVERGENCE_LANE, key);
    }
    let rows = router.field_verdict_status();
    assert_eq!(
        rows.len(),
        paths.len(),
        "one row per planted path: {rows:?}"
    );
    paths
        .iter()
        .map(|path| {
            let key = crate::field_capability::field_capability_key(path).expect("well-formed");
            let row = rows
                .iter()
                .find(|row| row.capability_key == key)
                .unwrap_or_else(|| panic!("a row for {path}: {rows:?}"));
            ((*path).to_string(), row.blocked_reason_token())
        })
        .collect()
}

/// One lane fixture: its name, `[providers.p0]` body, and the literal token each
/// planted row must report.
type LaneViewCase = (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
);

/// Every lane kind field repair admits, each with a literal expectation.
fn lane_view_cases() -> Vec<LaneViewCase> {
    const IN: &str = "not_eligible";
    const OUT: &str = "unsupported_lane";
    #[cfg_attr(
        not(any(feature = "bedrock", feature = "openai-responses")),
        allow(unused_mut)
    )]
    let mut cases: Vec<LaneViewCase> = vec![
        (
            "anthropic-api (every row in view)",
            "kind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\n",
            &[("temperature", IN), ("output_config.effort.xhigh", IN)],
        ),
        (
            "openai-compat direct (no effort rows)",
            "kind = \"openai-compat\"\nbase_url = \"https://example.test/v1\"\n\
             api_key_ref = \"literal:k\"\n",
            &[("temperature", IN), ("output_config.effort.xhigh", OUT)],
        ),
    ];
    #[cfg(feature = "bedrock")]
    cases.extend::<[LaneViewCase; 3]>([
        (
            "bedrock (every row in view)",
            "kind = \"bedrock\"\nregion = \"us-west-2\"\ncreds = { kind = \"default-chain\" }\n",
            &[("temperature", IN), ("output_config.effort.xhigh", IN)],
        ),
        (
            "anthropic-api Mantle sub-lane",
            "kind = \"anthropic-api\"\napi_key_ref = \"\"\n\
             bedrock_mantle = { region = \"us-west-2\", creds = { kind = \"default-chain\" } }\n",
            &[("max_tokens", IN), ("temperature", OUT)],
        ),
        (
            "openai-compat Mantle sub-lane",
            "kind = \"openai-compat\"\napi_key_ref = \"\"\n\
             bedrock_mantle = { region = \"us-west-2\", creds = { kind = \"default-chain\" } }\n",
            &[("top_p", IN), ("temperature", OUT)],
        ),
    ]);
    #[cfg(feature = "openai-responses")]
    cases.push((
        "openai-responses direct (effort rows only)",
        "kind = \"openai-responses\"\napi_key_ref = \"literal:k\"\nauth_kind = \"api-key\"\n",
        &[("output_config.effort.xhigh", IN), ("temperature", OUT)],
    ));
    #[cfg(all(feature = "openai-responses", feature = "bedrock"))]
    cases.push((
        "openai-responses Mantle sub-lane",
        "kind = \"openai-responses\"\napi_key_ref = \"\"\n\
         bedrock_mantle = { region = \"us-east-1\", creds = { kind = \"default-chain\" } }\n",
        &[("output_config.effort.max", IN), ("temperature", OUT)],
    ));
    cases
}

/// Per lane kind, a row the lane's view carries reports its evidence token and a
/// row the view omits reports `unsupported_lane`, spelled as literals so a change
/// to the view derivation cannot be echoed back by the expectation.
///
/// anthropic-api and direct bedrock read every row, so both of their rows are
/// in view; the out-of-view half is reachable only on the narrower lanes.
///
/// Mutation check: drop the view clause from `lane_supports_preflight` (answer
/// `attributable_field_lane(..).is_some()`) -> every OUT row reads `not_eligible`.
#[test]
fn each_lane_kind_reports_unsupported_lane_only_for_rows_outside_its_view() {
    for (name, provider_toml, expected) in lane_view_cases() {
        let paths: Vec<&str> = expected.iter().map(|(path, _)| *path).collect();
        let tokens = lane_tokens_for(provider_toml, &paths);
        let expected: Vec<(String, &str)> = expected
            .iter()
            .map(|(path, token)| ((*path).to_string(), *token))
            .collect();
        assert_eq!(tokens, expected, "{name}");
    }
}

/// For every lane fixture and every row this build's table carries, the status
/// surface reports `unsupported_lane` exactly when the row is absent from the
/// view `FieldRepairTable::for_lane(..).lane_rows()` gives the lane's entry kind
/// and egress -- the accessor pre-flight reads -- so the two cannot drift.
#[test]
fn the_status_lane_gate_reads_the_view_pre_flight_reads() {
    for (name, provider_toml, _) in lane_view_cases() {
        let router = convergence_router_on(provider_toml);
        let entry = router.config.providers.get("p0").expect("fixture entry");
        let view: Vec<&str> = router
            .field_repair_table()
            .for_lane(entry.kind_str(), Some(entry.egress_kind()))
            .lane_rows()
            .map(|row| row.path)
            .collect();
        let all: Vec<&str> = router
            .field_repair_table()
            .rows()
            .iter()
            .map(|row| row.path)
            .collect();
        assert!(
            all.len() > view.len() || name.contains("every row"),
            "{name}: premise: a narrow lane omits some row",
        );
        for (path, token) in lane_tokens_for(provider_toml, &all) {
            let expected = if view.contains(&path.as_str()) {
                "not_eligible"
            } else {
                "unsupported_lane"
            };
            assert_eq!(token, expected, "{name}: {path}");
        }
    }
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
        NICKNAME.to_string(),
        ModelEntry::new("anthropic-pool", "claude-sonnet-4-5"),
    );
    config.aliases.insert(
        "default".to_string(),
        AliasValue::Single(NICKNAME.to_string()),
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
    // Plant the verdict on the member's lane: `member#upstream`.
    let pooled_key = "seat-a#claude-sonnet-4-5".to_string();
    let cap_key = grounded_key();
    let provider_kind = router.provider_kind_for_state_key(&pooled_key).to_string();
    let vk = crate::field_verdict::FieldVerdictKey::from_capability_key(
        crate::state_key::StateKey::parse(&pooled_key).expect("lane"),
        cap_key.clone(),
        provider_kind,
    );
    let stamped = Instant::now();
    router
        .learned_capabilities
        .import_entries(vec![crate::learned_capability::ExportedEntry {
            provider_kind: router.provider_kind_for_state_key(&pooled_key).to_string(),
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
