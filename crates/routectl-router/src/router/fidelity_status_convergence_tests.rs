// PLANNER-vs-STATUS convergence, table-driven over one shared fixture matrix.
//
// # What this proves that neither side's own tests can
//
// The status surface's `blocked_reason` exists to explain why pre-flight is not
// acting. That is a claim ABOUT THE PLANNER, and every test elsewhere checks only one
// side of it: the planner's own suite asserts what it does to a request, and the
// status suite asserts what a row reports. Both stay green if the two disagree --
// and a surface that reports `none` for a verdict the planner refuses, or names a
// gate that is not the one holding, is worse than no surface: it sends an operator to
// fix something that was never the problem.
//
// `fidelity_status.rs` states the convergence as a design intent ("each variant is
// produced by consulting the planner's own predicate, not by paraphrasing its
// conditions"). Consulting the same predicate is not the same as agreeing, because
// the two sides also order their gates and derive their inputs independently. This
// file executes the claim over one fixture per gate, so a disagreement is a failure
// rather than a comment somebody has to re-verify by reading.
//
// # Why ONE matrix rather than a case per gate
//
// Because the property is uniform -- for every state, the planner's admit/deny and
// the status row's blocked/unblocked must agree, and when both deny they must name
// the same gate. Written as separate tests, each would assert its own row and none
// would assert the property; a new gate would then be added with no obligation to
// converge. The matrix makes the obligation structural: a new
// `PreflightBlockedReason` needs a row here or `every_blocked_reason_is_exercised`
// fails.
//
// An `include!`d FRAGMENT of `fidelity_status_tests.rs` -- no top-level `use`, no
// `mod`; see that host's note.

/// What the planner did with a request, reduced to the two facts the status surface
/// makes a claim about: whether it ACTED, and the reason token it recorded.
///
/// Read off the planner's own `FieldPreflight` record rather than inferred from the
/// planned bytes, because a reason is exactly what the status row claims to mirror --
/// and the record is where the planner states it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlannerVerdict {
    acted: bool,
    reason: &'static str,
}

/// Drive the REAL planner over `req` for `router`'s single target and return the
/// decision for the row whose class `class_path` names.
///
/// The target comes from the router's own chain expansion, so `provider_kind`, the
/// forwarded-credential flag, and the state key are exactly what a dispatch walk
/// would hand the planner -- a hand-built target could carry a combination
/// expansion never produces, and then neither side's answer would be about a
/// reachable state.
fn planner_verdict_for(
    router: &Router,
    req: &routectl_core::ChatRequest,
    class_path: &str,
) -> PlannerVerdict {
    let model = router
        .resolved_models
        .values()
        .next()
        .map(std::sync::Arc::clone)
        .expect("the convergence fixtures install exactly one resolved model");
    let target = router
        .expand_chain_to_targets(vec![model], None)
        .into_iter()
        .next()
        .expect("one target for the fixture model");
    let (_planned, records, _plan) = router.plan_field_preflight(
        req,
        &target,
        crate::router::class_observe::DispatchSurface::Complete,
    );
    // The record for THIS row. A request can carry two rows (the envelope carriers
    // and the test-only prefix-impacting system prompt), each gated on its own
    // terms, so picking by index would silently compare one side's envelope decision
    // against the other side's prefix decision.
    let record = records
        .iter()
        .find(|r| r.field_path == Some(class_path))
        .unwrap_or_else(|| {
            panic!(
                "the planner must have considered {class_path}; it recorded {:?}",
                records
                    .iter()
                    .map(|r| (r.field_path, r.reason))
                    .collect::<Vec<_>>(),
            )
        });
    PlannerVerdict {
        acted: record.acted,
        reason: record.reason,
    }
}

/// The status row for the identity `class_path` names, as `(blocked, token)`.
fn status_verdict_for(router: &Router, class_path: &str) -> (bool, &'static str) {
    let key = crate::field_capability::field_capability_key(class_path)
        .expect("the fixture's path is well-formed");
    let rows = router.field_verdict_status();
    let row = rows
        .iter()
        .find(|r| r.capability_key == key)
        .unwrap_or_else(|| {
            panic!(
                "the status surface must report a row for {class_path}; it reported {:?}",
                rows.iter().map(|r| &r.capability_key).collect::<Vec<_>>(),
            )
        });
    (row.blocked_reason.is_some(), row.blocked_reason_token())
}

/// One row of the convergence matrix: a named state, the router and request that
/// realize it, which closed-table row it is about, and the blocked reason the status
/// surface must report.
struct ConvergenceCase {
    /// What state this row establishes, named for the failure message.
    name: &'static str,
    router: Router,
    req: routectl_core::ChatRequest,
    /// The closed-table path whose row both sides are asked about.
    path: &'static str,
    /// The status token expected. `None` means the row must read UNBLOCKED, which
    /// also obliges the planner to have ACTED.
    expect: Option<PreflightBlockedReason>,
    /// The planner reason token expected, when it differs from the status token.
    ///
    /// The two vocabularies coincide for most gates because the status surface
    /// deliberately spells the same words. Where they legitimately differ it is
    /// DOCUMENTED and stated here per row rather than tolerated globally -- a blanket
    /// "any token is fine" would make the whole comparison vacuous.
    planner_reason_override: Option<&'static str>,
}

/// A request carrying the ENVELOPE row's grounded carrier.
fn convergence_envelope_req() -> routectl_core::ChatRequest {
    let mut req = routectl_core::ChatRequest {
        model: CONVERGENCE_ALIAS.to_string(),
        max_tokens: Some(64),
        ..routectl_core::ChatRequest::default()
    };
    req.routectl_internal.anthropic_thinking_display = Some("summarized".to_string());
    let reasoning = req.reasoning.get_or_insert_with(Default::default);
    reasoning.enabled = Some(true);
    req
}

/// A request carrying the PREFIX-IMPACTING row's surface: the exact system-prompt
/// sentinel its presence predicate recognizes, read from the surface's own constant.
fn convergence_prefix_req() -> routectl_core::ChatRequest {
    routectl_core::ChatRequest {
        model: CONVERGENCE_ALIAS.to_string(),
        max_tokens: Some(64),
        system: Some(routectl_core::SystemContent::Text(
            crate::router::field_repair::TEST_PREFIX_SENTINEL.into(),
        )),
        ..routectl_core::ChatRequest::default()
    }
}

/// The alias every convergence fixture routes, so the planner resolves the one
/// installed model.
const CONVERGENCE_ALIAS: &str = "convergence-alias";

/// The GROUNDED envelope path, from the table's own row.
const CONVERGENCE_ENVELOPE_PATH: &str = GROUNDED_PATH;

/// The PREFIX-IMPACTING path, from the table's own test-only row.
fn convergence_prefix_path() -> &'static str {
    crate::router::field_repair::TEST_PREFIX_IMPACTING_PATH
}

/// A convergence fixture router: one `anthropic-api` provider on `base_url`, one
/// model, one alias, and the model INSTALLED as a resolved single-seat target so the
/// planner's chain expansion finds it.
fn convergence_router(base_url: &str, kind: &str) -> Router {
    use crate::config::{AliasValue, ModelEntry};
    use crate::resolved::ResolvedModel;

    let toml_text = format!(
        "\n[providers.p0]\nkind = \"{kind}\"\napi_key_ref = \"env://K\"\n\
         base_url = \"{base_url}\"\n"
    );
    let mut config: Config = toml::from_str(&toml_text).expect("valid convergence toml");
    config
        .models
        .insert(STATE_KEY.to_string(), ModelEntry::new("p0", "wire-model"));
    config.aliases.insert(
        CONVERGENCE_ALIAS.to_string(),
        AliasValue::Single(STATE_KEY.to_string()),
    );
    let mut router = Router::new(Arc::new(config));
    let provider: std::sync::Arc<dyn routectl_core::Provider> = std::sync::Arc::new(InertProvider);
    let mut models: std::collections::BTreeMap<String, std::sync::Arc<ResolvedModel>> =
        std::collections::BTreeMap::new();
    models.insert(
        STATE_KEY.to_string(),
        std::sync::Arc::new(ResolvedModel::new(
            STATE_KEY,
            "p0",
            provider,
            "wire-model".to_string(),
        )),
    );
    router.install_resolved_models(models);
    router
}

/// The ordinary fixture: an attributable remote `anthropic-api` lane.
fn convergence_ok_router() -> Router {
    convergence_router("https://api.anthropic.com", "anthropic-api")
}

/// Plant a resident ACTING verdict for `path` on the convergence fixture and
/// acknowledge `confirmations` at the kind the router resolves.
fn plant_convergence_verdict(router: &Router, path: &str, confirmations: u32) {
    let capability_key = crate::field_capability::field_capability_key(path)
        .expect("the fixture's path is well-formed");
    plant_verdict_on(router, STATE_KEY, capability_key.clone());
    let kind = router.provider_kind_for_state_key(STATE_KEY).to_string();
    let key = crate::field_verdict::FieldVerdictKey::from_capability_key(
        STATE_KEY.to_string(),
        capability_key.clone(),
        kind.clone(),
    );
    let incarnation = router.learned_capabilities.resident_incarnation_for_tests(
        STATE_KEY,
        &capability_key,
        &kind,
    );
    router
        .field_verdicts()
        .canaries()
        .seed_from_rebuild(&key, incarnation, confirmations, false);
}

/// Every convergence case, one per gate the two surfaces can disagree about.
fn convergence_matrix() -> Vec<ConvergenceCase> {
    let envelope = CONVERGENCE_ENVELOPE_PATH;
    let prefix = convergence_prefix_path();
    let mut cases = Vec::new();

    // UNBLOCKED: the state in which pre-flight rewrites traffic. First, because it
    // is the one every other row must be distinguishable FROM -- a matrix of only
    // refusals passes against a surface that reports a refusal unconditionally.
    let router = convergence_ok_router();
    plant_convergence_verdict(&router, envelope, ENVELOPE_QUORUM);
    cases.push(ConvergenceCase {
        name: "eligible envelope verdict, nothing blocking",
        router,
        req: convergence_envelope_req(),
        path: envelope,
        expect: None,
        planner_reason_override: Some(crate::router::field_preflight::FIELD_PREFLIGHT_ACTION_DROP),
    });

    // The GLOBAL kill switch. Reported for every verdict at once, and its remedy is
    // flipping one setting rather than anything per-target.
    let mut config_off = convergence_ok_router();
    {
        let mut config = (*config_off.config).clone();
        config.capability.enabled = false;
        config_off = rebuild_convergence_router_with(config_off, config);
    }
    plant_convergence_verdict(&config_off, envelope, ENVELOPE_QUORUM);
    cases.push(ConvergenceCase {
        name: "capability kill switch off",
        router: config_off,
        req: convergence_envelope_req(),
        path: envelope,
        expect: Some(PreflightBlockedReason::CapabilityDisabled),
        // The planner folds the kill switch into its LANE refusal: its
        // `preflight_lane_admits` checks `capability.enabled` first and reports
        // `unsupported_lane`. The status surface separates them because the operator
        // action differs -- flip a setting versus repoint a target -- which is a
        // documented divergence, not a drift.
        planner_reason_override: Some(
            crate::router::field_preflight::FIELD_PREFLIGHT_UNSUPPORTED_LANE,
        ),
    });

    // A NON-ANTHROPIC lane: not the one kind this stage acts on.
    let other_kind = convergence_router("https://example.test/v1", "openai-compat");
    plant_convergence_verdict(&other_kind, envelope, ENVELOPE_QUORUM);
    cases.push(ConvergenceCase {
        name: "non-anthropic provider kind",
        router: other_kind,
        req: convergence_envelope_req(),
        path: envelope,
        expect: Some(PreflightBlockedReason::UnsupportedLane),
        planner_reason_override: None,
    });

    // An UNATTRIBUTABLE base URL: a local hop. A rejection from one cannot be
    // attributed to an upstream, so neither side may act on a verdict for it.
    let loopback = convergence_router("http://127.0.0.1:8899", "anthropic-api");
    plant_convergence_verdict(&loopback, envelope, ENVELOPE_QUORUM);
    cases.push(ConvergenceCase {
        name: "unattributable base URL (local hop)",
        router: loopback,
        req: convergence_envelope_req(),
        path: envelope,
        expect: Some(PreflightBlockedReason::UnsupportedLane),
        // The planner distinguishes the ATTRIBUTION refusal from the lane-kind one
        // (`unattributable_target`); the status surface folds both into
        // `unsupported_lane`, because from an operator's seat the lane is one this
        // stage will never act on either way. Documented divergence.
        planner_reason_override: Some(
            crate::router::field_preflight::FIELD_PREFLIGHT_UNATTRIBUTABLE_TARGET,
        ),
    });

    // An operator MASK forcing the capability supported.
    let masked = convergence_ok_router();
    {
        let mut config = (*masked.config).clone();
        config.capability.overrides.insert(
            "p0".to_string(),
            crate::config::OverrideEntry {
                unsupported: Vec::new(),
                force_supported: vec![
                    crate::field_capability::field_capability_key(envelope).expect("well-formed"),
                ],
            },
        );
        let masked = rebuild_convergence_router_with(masked, config);
        plant_convergence_verdict(&masked, envelope, ENVELOPE_QUORUM);
        cases.push(ConvergenceCase {
            name: "operator force_supported mask",
            router: masked,
            req: convergence_envelope_req(),
            path: envelope,
            expect: Some(PreflightBlockedReason::MaskedByOverride),
            planner_reason_override: None,
        });
    }

    // THE GATE-ORDERING case: a mask AND a refused lane hold at once.
    //
    // Every other row here has exactly one gate holding, so it is satisfied by any
    // ordering and none of them can see the precedence at all. This one is the
    // discriminating fixture: an operator mask on a NON-ANTHROPIC lane. The planner
    // refuses the lane before it ever reads the override registry, so the status
    // surface must too -- reporting the mask here would tell an operator to edit a
    // `[capability.overrides]` cell to un-block a verdict that would stay inert on
    // that lane whatever they did to the cell.
    let masked_on_bad_lane = convergence_router("https://example.test/v1", "openai-compat");
    {
        let mut config = (*masked_on_bad_lane.config).clone();
        config.capability.overrides.insert(
            "p0".to_string(),
            crate::config::OverrideEntry {
                unsupported: Vec::new(),
                force_supported: vec![
                    crate::field_capability::field_capability_key(envelope).expect("well-formed"),
                ],
            },
        );
        let masked_on_bad_lane = rebuild_convergence_router_with_kind(
            masked_on_bad_lane,
            config,
            "https://example.test/v1",
            "openai-compat",
        );
        plant_convergence_verdict(&masked_on_bad_lane, envelope, ENVELOPE_QUORUM);
        cases.push(ConvergenceCase {
            name: "operator mask on a lane the stage refuses (gate ordering)",
            router: masked_on_bad_lane,
            req: convergence_envelope_req(),
            path: envelope,
            expect: Some(PreflightBlockedReason::UnsupportedLane),
            planner_reason_override: None,
        });
    }

    // NOT ELIGIBLE: resident, never acknowledged.
    let unacked = convergence_ok_router();
    plant_verdict_on(
        &unacked,
        STATE_KEY,
        crate::field_capability::field_capability_key(envelope).expect("well-formed"),
    );
    cases.push(ConvergenceCase {
        name: "resident but unacknowledged verdict",
        router: unacked,
        req: convergence_envelope_req(),
        path: envelope,
        expect: Some(PreflightBlockedReason::NotEligible),
        planner_reason_override: None,
    });

    // An UNKNOWN CLASS is unreachable through a request: the planner only considers
    // rows in this build's table, so a foreign-key verdict is covered by its own
    // status case. Its convergence obligation is vacuous by construction -- there is
    // no planner decision to compare -- and stating that here is why no row exists.

    // BELOW QUORUM: the prefix-impacting class at one confirmation, where it needs
    // two. The class whose quorum is higher than the envelope's is the only one on
    // which this gate is reachable at all.
    let short = convergence_ok_router();
    plant_convergence_verdict(&short, prefix, ENVELOPE_QUORUM);
    cases.push(ConvergenceCase {
        name: "prefix-impacting verdict below its class quorum",
        router: short,
        req: convergence_prefix_req(),
        path: prefix,
        expect: Some(PreflightBlockedReason::BelowQuorum),
        planner_reason_override: None,
    });

    // MISSING PREFIX OPT-IN: quorum satisfied, target not named in `[fidelity]`.
    let no_opt_in = convergence_ok_router();
    plant_convergence_verdict(&no_opt_in, prefix, PREFIX_QUORUM);
    cases.push(ConvergenceCase {
        name: "prefix-impacting at quorum with no target opt-in",
        router: no_opt_in,
        req: convergence_prefix_req(),
        path: prefix,
        expect: Some(PreflightBlockedReason::NoTargetOptIn),
        planner_reason_override: None,
    });

    // The OPT-IN present: the same fixture unblocks, which is what makes the row
    // above the opt-in's refusal rather than a permanently-dormant class.
    let opted_in = convergence_ok_router();
    {
        let mut config = (*opted_in.config).clone();
        config.fidelity.prefix_impact_opt_in = vec!["p0".to_string()];
        let opted_in = rebuild_convergence_router_with(opted_in, config);
        plant_convergence_verdict(&opted_in, prefix, PREFIX_QUORUM);
        cases.push(ConvergenceCase {
            name: "prefix-impacting at quorum WITH the target opt-in",
            router: opted_in,
            req: convergence_prefix_req(),
            path: prefix,
            expect: None,
            planner_reason_override: Some(
                crate::router::field_preflight::FIELD_PREFLIGHT_ACTION_DROP,
            ),
        });
    }

    cases
}

/// Rebuild a convergence router over `config`, carrying the resolved table across.
///
/// The config is swapped rather than edited in place because `Router` holds it behind
/// an `Arc`, and the resolved table is reinstalled so the planner's chain expansion
/// still finds the target -- a fixture that lost it would produce no planner record
/// at all and the comparison would panic rather than fail informatively.
fn rebuild_convergence_router_with(previous: Router, config: Config) -> Router {
    rebuild_convergence_router_inner(&previous, config)
}

/// [`rebuild_convergence_router_with`] for a fixture whose provider entry must keep a
/// non-default kind or base URL.
///
/// The config is rebuilt from toml rather than carried, because the caller's `config`
/// was cloned from a router whose `[providers]` table this helper must preserve
/// exactly -- a rebuild that reset the kind would silently turn the refused-lane
/// fixture into an ordinary one, and the gate-ordering case would stop
/// discriminating.
fn rebuild_convergence_router_with_kind(
    previous: Router,
    mut config: Config,
    base_url: &str,
    kind: &str,
) -> Router {
    let toml_text = format!(
        "\n[providers.p0]\nkind = \"{kind}\"\napi_key_ref = \"env://K\"\n\
         base_url = \"{base_url}\"\n"
    );
    let parsed: Config = toml::from_str(&toml_text).expect("valid convergence toml");
    config.providers = parsed.providers;
    rebuild_convergence_router_inner(&previous, config)
}

fn rebuild_convergence_router_inner(previous: &Router, config: Config) -> Router {
    use crate::resolved::ResolvedModel;

    let mut router = Router::new(Arc::new(config));
    let provider: std::sync::Arc<dyn routectl_core::Provider> = std::sync::Arc::new(InertProvider);
    let upstream = previous
        .resolved_models
        .values()
        .next()
        .map_or_else(|| "wire-model".to_string(), |m| m.upstream.clone());
    let mut models: std::collections::BTreeMap<String, std::sync::Arc<ResolvedModel>> =
        std::collections::BTreeMap::new();
    models.insert(
        STATE_KEY.to_string(),
        std::sync::Arc::new(ResolvedModel::new(STATE_KEY, "p0", provider, upstream)),
    );
    router.install_resolved_models(models);
    router
}

/// The PLANNER and the STATUS surface agree, for every state in the matrix: they
/// admit or deny together, and when they deny they name the same gate.
///
/// The disagreement this forbids is the one an operator cannot detect: a row reading
/// `none` while the planner refuses says pre-flight is rewriting traffic that is in
/// fact untouched, and a row naming the wrong gate sends them to fix something that
/// was never holding. Both are well-formed readings of a broken surface.
///
/// Mutation check: swap the order of the lane gate and the override mask in
/// `blocked_reason_for` -> the masked case reds, because a masked verdict on a
/// refused lane then reports the mask while the planner reports the lane.
#[test]
fn the_planner_and_the_status_surface_agree_on_every_gate() {
    for case in convergence_matrix() {
        let planner = planner_verdict_for(&case.router, &case.req, case.path);
        let (status_blocked, status_token) = status_verdict_for(&case.router, case.path);

        // AGREEMENT ON THE VERDICT: acted <=> unblocked. This is the half that
        // matters most, because the two tokens could match while one side acted.
        assert_eq!(
            planner.acted, !status_blocked,
            "[{}] the planner and the status surface must agree on WHETHER pre-flight \
             acts: planner acted={} reason={}, status blocked={} token={}. A row \
             reading unblocked while the planner refuses tells an operator their \
             traffic is being rewritten when it is not",
            case.name, planner.acted, planner.reason, status_blocked, status_token,
        );

        // AGREEMENT ON THE GATE, against the expected token so the row also pins
        // WHICH gate holds rather than only that some gate does.
        let expected_token = case
            .expect
            .map_or(BLOCKED_REASON_NONE, PreflightBlockedReason::as_str);
        assert_eq!(
            status_token, expected_token,
            "[{}] the status surface must name this gate",
            case.name,
        );
        let expected_planner = case.planner_reason_override.unwrap_or(expected_token);
        assert_eq!(
            planner.reason, expected_planner,
            "[{}] and the planner must report its own documented token for the same \
             state -- an unexpected token here means the two surfaces diverged \
             somewhere this row does not describe",
            case.name,
        );
    }
}

/// Every `PreflightBlockedReason` the status surface can emit is either exercised by
/// the convergence matrix or explicitly excused with a reason.
///
/// Without this the matrix is a snapshot rather than an obligation: a new gate could
/// be added to both surfaces, diverge, and never be compared -- which is precisely
/// how the two came to need this file. A new variant now fails here until somebody
/// either converges it or states why it cannot be.
#[test]
fn every_blocked_reason_is_exercised_or_excused_by_the_convergence_matrix() {
    let exercised: Vec<&'static str> = convergence_matrix()
        .iter()
        .filter_map(|case| case.expect.map(PreflightBlockedReason::as_str))
        .collect();

    // EXCUSED, each with the reason it cannot appear in a planner comparison:
    //
    // - `canary_suspended`: the planner has no such token at all. It folds a
    //   suspension into `not_eligible` because its decision is the same either way,
    //   and the status surface separates them for the operator. So there is no
    //   planner reason to converge ON -- only the acted/blocked half applies, and
    //   `a_canary_suspended_verdict_reports_the_suspension_rather_than_ineligibility`
    //   in this same module pins the status side. Comparing tokens here would assert
    //   a divergence the design requires.
    const EXCUSED: &[&str] = &["canary_suspended"];

    for reason in [
        PreflightBlockedReason::CapabilityDisabled,
        PreflightBlockedReason::UnsupportedLane,
        PreflightBlockedReason::MaskedByOverride,
        PreflightBlockedReason::CanarySuspended,
        PreflightBlockedReason::NotEligible,
        PreflightBlockedReason::BelowQuorum,
        PreflightBlockedReason::NoTargetOptIn,
    ] {
        let token = reason.as_str();
        assert!(
            exercised.contains(&token) || EXCUSED.contains(&token),
            "the convergence matrix must exercise {token} or excuse it with a stated \
             reason: an unconverged gate is one the two surfaces can disagree about \
             with nothing failing",
        );
    }
    // The UNBLOCKED case is not a `PreflightBlockedReason`, so assert it separately:
    // a matrix of refusals only would pass against a surface that refuses always.
    assert!(
        convergence_matrix()
            .iter()
            .any(|case| case.expect.is_none()),
        "and it must carry at least one UNBLOCKED case, or every refusal above is \
         satisfied by a surface that never admits anything",
    );
}
