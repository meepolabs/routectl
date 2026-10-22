//! The dispatch walk acts on the precedence seam's resolution: one row per
//! tier drives `filter_chain_by_features` and compares the outcome with the
//! `Resolution` the seam gives for inputs gathered independently here.

use super::capability_precedence_matrix_tests::{
    KIND, base_config, lane_of, seed_learned, seed_verified, target_with_priors,
};
use super::feature_filter_read_tests::seed_lapsed;
use super::*;

use std::sync::Arc;
use std::time::Instant;

use routectl_core::capability::FailurePhase;

use crate::capability_precedence::{
    CellContext, Layer, LearnedInput, Resolution, Routing, Stage, Wire, operator_pins_capability,
    resolve_open, resolve_operator,
};

/// How the fixture cell is seeded before the walk.
#[derive(Clone, Copy)]
enum Seed {
    Nothing,
    Acting(FailurePhase),
    Lapsed,
    Verified,
}

/// The seam's resolution of `(target, key)`, from inputs gathered here on
/// the test side (the walk under test gathers its own), with the learned
/// cell read off a snapshot taken before the walk.
fn expected_resolution(
    router: &Router,
    target: &DispatchTarget,
    key: &str,
    prior: Option<bool>,
) -> Resolution {
    let config = &router.config;
    let pinned = operator_pins_capability(config, "p", &target.model.header_extras, key);
    let verdict =
        router
            .override_registry
            .resolve("p", target.nickname.as_deref().unwrap_or(""), key, KIND);
    let ctx = CellContext::new(key, &config.capability, None, &target.upstream);
    let now = Instant::now();
    let learned = router
        .learned_capability_snapshot()
        .iter()
        .find(|entry| entry.state_key == lane_of("nick") && entry.feature_key == key)
        .and_then(|entry| LearnedInput::from_entry(entry, now));
    match resolve_operator(pinned, verdict, ctx) {
        Stage::Decided(decided) => decided,
        Stage::Open(cell) => resolve_open(learned, None, prior, cell),
    }
}

#[test]
fn the_dispatch_walk_acts_on_the_seam_resolution_for_every_tier() {
    const PIN: &str = "[providers.p.header_extras]\n\
                       \"anthropic-beta\" = \"context-management-2025-06-27\"\n";
    // Each row: (name, extra config, kill switch on, key, prior, seed, the
    // tier expected to decide). The walk runs `[nick, clean]`, so the
    // fixture's place in the result reads the routing axis; an override is
    // scoped to `nick` alone so the sibling stays clean.
    let rows = [
        (
            "override route-away drops",
            "[models.nick]\nprovider = \"p\"\nupstream = \"nick\"\n\
             [capability.overrides.\"p:nick\"]\nunsupported = [\"web_search\"]\n",
            true,
            "web_search",
            None,
            Seed::Verified,
            Some(Layer::Override),
        ),
        (
            "force_supported masks a learned negative and a prior",
            "[models.nick]\nprovider = \"p\"\nupstream = \"nick\"\n\
             [capability.overrides.\"p:nick\"]\nforce_supported = [\"web_search\"]\n",
            true,
            "web_search",
            Some(false),
            Seed::Acting(FailurePhase::F2),
            Some(Layer::Override),
        ),
        (
            "learned F1 on a droppable strips",
            "",
            true,
            "advisor",
            None,
            Seed::Acting(FailurePhase::F1),
            Some(Layer::Learned),
        ),
        (
            "learned F2 tails",
            "",
            true,
            "web_search",
            Some(false),
            Seed::Acting(FailurePhase::F2),
            Some(Layer::Learned),
        ),
        (
            "pinned feature with an F1 negative tails instead of stripping",
            PIN,
            true,
            "context_management",
            None,
            Seed::Acting(FailurePhase::F1),
            Some(Layer::Learned),
        ),
        (
            "lapsed negative keeps and re-probes",
            "",
            true,
            "context_management",
            None,
            Seed::Lapsed,
            Some(Layer::Learned),
        ),
        (
            "verified positive masks a prior",
            "",
            true,
            "web_search",
            Some(false),
            Seed::Verified,
            Some(Layer::Learned),
        ),
        (
            "prior false tails",
            "",
            true,
            "web_search",
            Some(false),
            Seed::Nothing,
            Some(Layer::Prior),
        ),
        (
            "live F3 negative is advisory; the prior decides",
            "",
            true,
            "web_search",
            Some(false),
            Seed::Acting(FailurePhase::F3),
            Some(Layer::Prior),
        ),
        (
            "kill switch off ignores learned and prior",
            "",
            false,
            "web_search",
            Some(false),
            Seed::Acting(FailurePhase::F2),
            None,
        ),
    ];
    for (name, extra, enabled, key, prior, seed, expected_layer) in rows {
        // Arrange
        let mut config = base_config(extra);
        config.capability.enabled = enabled;
        let router = Router::new(Arc::new(config));
        let priors: Vec<(&str, bool)> = prior.map(|p| (key, p)).into_iter().collect();
        let target = target_with_priors(&router, "nick", &priors);
        let clean = target_with_priors(&router, "clean", &[]);
        match seed {
            Seed::Nothing => {}
            Seed::Acting(phase) => seed_learned(&router, "nick", key, phase),
            Seed::Lapsed => seed_lapsed(&router, "nick", key),
            Seed::Verified => seed_verified(&router, "nick", key),
        }
        let expected = expected_resolution(&router, &target, key, prior);
        assert_eq!(expected.layer(), expected_layer, "{name}: the row's tier");

        // Act
        let mut admissions = Vec::new();
        let out = router
            .filter_chain_by_features(
                vec![target, clean],
                &[key.to_string()],
                "alias",
                DispatchSurface::Complete,
                &mut admissions,
            )
            .expect("the clean sibling keeps the chain non-empty");

        // Assert
        let order: Vec<&str> = out.iter().map(|t| t.state_key.as_str()).collect();
        let expected_order = match expected.routing() {
            Routing::Keep => vec!["nick", "clean"],
            Routing::Tail => vec!["clean", "nick"],
            Routing::Drop => vec!["clean"],
        };
        assert_eq!(order, expected_order, "{name}: routing");
        let strip: Vec<&str> = out
            .iter()
            .flat_map(|t| t.strip_capabilities.iter().map(String::as_str))
            .collect();
        let expected_strip = if expected.wire() == Wire::Strip {
            vec![key]
        } else {
            Vec::new()
        };
        assert_eq!(strip, expected_strip, "{name}: wire");
        assert_eq!(
            admissions.len(),
            usize::from(expected.reprobe()),
            "{name}: re-probe"
        );
    }
}
