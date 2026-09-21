//! Every field stage agrees about whether a target's rejections are
//! attributable.
//!
//! FOUR stages ask that question: the reactive `plan_field_carry`, the
//! pre-flight planner, probe activation, and the probe's pre-dial recheck. A
//! stage narrower than another is exactly the asymmetry that mints a verdict
//! from evidence the acting arm had already rejected -- or probes a target the
//! acting arm would refuse.
//!
//! Pinned BEHAVIORALLY, as a parity table: one row per attributability-relevant
//! provider shape, each driven through all four stages, each stage required to
//! reach the same verdict.
//!
//! WHAT THIS CATCHES, exactly: a stage that narrows or widens its verdict on one
//! of the facts the table VARIES -- base URL, credential source, entry presence,
//! and (where the build can construct one) a Bedrock Mantle sub-lane. A stage
//! keying a decision on some OTHER entry fact is invisible here until a row that
//! discriminates that fact is added, and so is a local check that merely
//! restates part of the shared refusal set, since it changes no verdict on any
//! row.
//!
//! The rows include two ADMITTING shapes that differ only in base URL (the
//! production Anthropic default and an operator's remote mirror) because the
//! plausible narrowings are about which base URL counts, and a single admitting
//! row would let one of them through. The Mantle row is the shape a raw base-url
//! read gets WRONG rather than merely narrow, since its configured base IS that
//! default.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Instant;

use routectl_core::{ChatRequest, Provider};

use super::class_observe::DispatchSurface;
use super::field_preflight::{
    FIELD_PREFLIGHT_ACTION_DROP, FIELD_PREFLIGHT_AMBIGUOUS_MUTATION, FIELD_PREFLIGHT_BELOW_QUORUM,
    FIELD_PREFLIGHT_CANARY_RESTORED, FIELD_PREFLIGHT_MASKED_BY_OVERRIDE,
    FIELD_PREFLIGHT_NO_GROUNDED_FIELD, FIELD_PREFLIGHT_NO_IDENTITY,
    FIELD_PREFLIGHT_NO_TARGET_OPT_IN, FIELD_PREFLIGHT_NOT_ELIGIBLE,
    FIELD_PREFLIGHT_UNATTRIBUTABLE_TARGET, FIELD_PREFLIGHT_UNSUPPORTED_LANE,
};
use super::field_repair::FieldSettlementMode;
use super::probe_test_support::{OkProvider, grounding_request};
use super::{DispatchTarget, Router};
use crate::config::{AliasValue, Config, CredentialSource, ModelEntry, ProviderEntry};
use crate::resolved::ResolvedModel;

/// The `[providers]` key and the model nickname every fixture below uses.
const PROVIDER: &str = "p1";
const NICKNAME: &str = "m1";

/// One attributability-relevant provider shape.
struct Shape {
    /// What the row is, for a failure message.
    name: &'static str,
    base_url: &'static str,
    credential_source: CredentialSource,
    /// Whether the entry carries a Bedrock Mantle sub-lane. Such an entry reads
    /// `anthropic-api` while egressing through Mantle, so its configured
    /// `base_url` is not its effective egress -- `anthropic_api_base_url`
    /// answers `None` for it, and the fixture asserts that premise.
    #[cfg(feature = "bedrock")]
    bedrock_mantle: bool,
    /// Whether the `[providers]` entry is DROPPED after the dispatch target has
    /// been expanded from it -- a reload removing the entry under a target that
    /// already carries its provider kind and forwarded flag.
    entry_removed_after_expansion: bool,
    /// Whether every stage must find this shape attributable.
    attributable: bool,
}

/// The parity table. Two admitting shapes, and refusing ones that each refuse
/// for a DIFFERENT reason (loopback, forwarded credential, vanished entry, and
/// -- where the build can construct one -- a Bedrock Mantle sub-lane).
const SHAPES: &[Shape] = &[
    Shape {
        name: "the Anthropic default base, routectl's own credential",
        base_url: "https://api.anthropic.com",
        credential_source: CredentialSource::Own,
        #[cfg(feature = "bedrock")]
        bedrock_mantle: false,
        entry_removed_after_expansion: false,
        attributable: true,
    },
    Shape {
        name: "an operator's remote mirror",
        base_url: "https://api.example.com/anthropic",
        credential_source: CredentialSource::Own,
        #[cfg(feature = "bedrock")]
        bedrock_mantle: false,
        entry_removed_after_expansion: false,
        attributable: true,
    },
    Shape {
        name: "a loopback base URL",
        base_url: "http://127.0.0.1:8899",
        credential_source: CredentialSource::Own,
        #[cfg(feature = "bedrock")]
        bedrock_mantle: false,
        entry_removed_after_expansion: false,
        attributable: false,
    },
    Shape {
        name: "a forwarded client credential",
        base_url: "https://api.anthropic.com",
        credential_source: CredentialSource::Forwarded,
        #[cfg(feature = "bedrock")]
        bedrock_mantle: false,
        entry_removed_after_expansion: false,
        attributable: false,
    },
    Shape {
        name: "an entry a reload removed",
        base_url: "https://api.anthropic.com",
        credential_source: CredentialSource::Own,
        #[cfg(feature = "bedrock")]
        bedrock_mantle: false,
        entry_removed_after_expansion: true,
        attributable: false,
    },
    // The Mantle row is the one a raw base-url read gets WRONG rather than
    // merely narrow: its configured base is the Anthropic default, so a stage
    // reading the base URL directly reads a remote Anthropic host and admits an
    // upstream that is in fact Bedrock.
    //
    // The base is spelled EXPLICITLY as that default rather than borrowed from a
    // sibling row, because it is the discriminating fact: pointed anywhere else
    // this row either stops being a Mantle lane the validator accepts, or
    // becomes a duplicate of the loopback row. The fixture's premise assertions
    // enforce both, and it runs the production Mantle validator so the entry is
    // one an operator could actually have written.
    #[cfg(feature = "bedrock")]
    Shape {
        name: "a Bedrock Mantle sub-lane",
        base_url: MANTLE_BASE_URL,
        credential_source: CredentialSource::Own,
        bedrock_mantle: true,
        entry_removed_after_expansion: false,
        attributable: false,
    },
];

/// The ONLY base URL a Mantle entry may carry: `region` is the single source of
/// truth for its endpoint, so the production validator requires this exact
/// value. Spelled here, not shared with the plain-default row, because the whole
/// point of the Mantle row is that this base reads as a legitimate remote
/// Anthropic host while the upstream is Bedrock.
#[cfg(feature = "bedrock")]
const MANTLE_BASE_URL: &str = "https://api.anthropic.com";

/// Attach the Mantle sub-lane to `entry` and bring it to a shape PRODUCTION
/// accepts, then assert every invariant the row rests on.
///
/// The base `ProviderEntry::anthropic_api` constructor hands back a
/// direct-to-Anthropic entry carrying an `api_key_ref`, which a Mantle lane must
/// not have: the credential comes from `bedrock_mantle.creds`, and the validator
/// rejects a stray key as dead config. Clearing it is what makes this an entry an
/// operator could actually have written, rather than a struct that only the
/// accessor happens to answer `None` for.
///
/// Every assertion here is a PREMISE of the Mantle row, and each would otherwise
/// fail silently into a row that proves something else:
///
/// - the production validator accepts it, so the row is not an unreachable shape;
/// - the sub-lane is attached, else the row duplicates the plain-default row;
/// - `api_key_ref` is empty, the invariant the validator enforces;
/// - the base is the production Anthropic default AND not loopback, so the row's
///   refusal comes from the Mantle exclusion rather than from loopback
///   suppression -- which is what keeps it distinct from the loopback row;
/// - `anthropic_api_base_url()` answers `None`, the fact every stage reads.
#[cfg(feature = "bedrock")]
fn complete_mantle_entry(mut entry: ProviderEntry, base_url: &str) -> ProviderEntry {
    if let ProviderEntry::AnthropicApi {
        api_key_ref,
        bedrock_mantle,
        ..
    } = &mut entry
    {
        api_key_ref.clear();
        *bedrock_mantle = Some(crate::config::BedrockMantleConfig {
            region: "us-west-2".to_string(),
            creds: crate::config::BedrockCredsConfig::BearerKey {
                key_ref: crate::test_secret::file_ref("mantle-bearer-key"),
            },
        });
    }

    // The PRODUCTION validator, over a config carrying only this entry -- the
    // same contract `validate_provider_bedrock_mantle`'s own fixtures assert,
    // reached through the same public function rather than restated here.
    let mut probe = Config::default();
    probe.providers.insert(PROVIDER.to_string(), entry.clone());
    crate::factory::validate_provider_bedrock_mantle(&probe)
        .expect("premise: the mantle entry must be one production validation accepts");

    match &entry {
        ProviderEntry::AnthropicApi {
            api_key_ref,
            base_url: configured,
            bedrock_mantle: Some(_),
            ..
        } => {
            assert!(
                api_key_ref.is_empty(),
                "premise: a mantle lane carries no api_key_ref"
            );
            assert_eq!(
                configured,
                &crate::config::default_anthropic_base(),
                "premise: a mantle lane sits at the production default Anthropic \
                 base, which is what makes it look like a remote Anthropic host"
            );
            assert_eq!(
                configured, base_url,
                "premise: the row's base must be the one under test"
            );
            assert!(
                !crate::field_verdict::loopback_target_suppresses_minting(configured),
                "premise: the mantle base must NOT be loopback, or this row's \
                 refusal would duplicate the loopback row's"
            );
        }
        _ => panic!("premise: the mantle sub-lane must be attached to an anthropic-api entry"),
    }
    assert!(
        ProviderEntry::anthropic_api_base_url(&entry).is_none(),
        "premise: a mantle entry must carry no attributable anthropic-api base \
         URL, which is the fact every stage reads"
    );
    entry
}

/// A router carrying `shape`'s single provider entry, plus the real dispatch
/// target the chain expansion derives from it.
///
/// The target comes from `expand_chain_to_targets` rather than being
/// hand-built, so its `provider_kind` and `use_forwarded_credential` are the
/// values a dispatch walk would carry -- a hand-set flag could disagree with
/// the entry and make a stage look narrower than it is.
///
/// A FRESH router per stage, because two of the stages mutate router state (the
/// reactive admission claims an in-flight slot, activation queues a job), and a
/// shared one would let the order of the checks decide their answers.
fn fixture(shape: &Shape) -> (Router, DispatchTarget) {
    let mut config = Config::default();
    let mut entry = ProviderEntry::anthropic_api("literal:k")
        .with_base_url(shape.base_url)
        .with_credential_source(shape.credential_source);
    if let ProviderEntry::AnthropicApi { runtime, .. } = &mut entry {
        runtime.circuit_failures = Some(1);
    }
    #[cfg(feature = "bedrock")]
    if shape.bedrock_mantle {
        entry = complete_mantle_entry(entry, shape.base_url);
    }
    config.providers.insert(PROVIDER.to_string(), entry);
    config.models.insert(
        NICKNAME.to_string(),
        ModelEntry::new(PROVIDER, "claude-sonnet-4-5"),
    );
    config.aliases.insert(
        "default".to_string(),
        AliasValue::Single(NICKNAME.to_string()),
    );

    let provider: Arc<dyn Provider> = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let mut router = Router::new(Arc::new(config));
    let model = Arc::new(ResolvedModel::new(
        NICKNAME,
        PROVIDER,
        Arc::clone(&provider),
        "claude-sonnet-4-5",
    ));
    router.install_resolved_models(
        std::iter::once((NICKNAME.to_string(), Arc::clone(&model))).collect(),
    );
    let target = router
        .expand_chain_to_targets(vec![model], None)
        .pop()
        .expect("one target for a non-pooled model");

    if shape.entry_removed_after_expansion {
        let mut config = (*router.config).clone();
        config.providers.remove(PROVIDER);
        router.config = Arc::new(config);
    }
    (router, target)
}

/// Whether the REACTIVE admission would act on this target.
fn reactive_admits(shape: &Shape, req: &ChatRequest) -> bool {
    let (router, target) = fixture(shape);
    router
        .plan_field_carry(&target, req, FieldSettlementMode::Settling, Instant::now())
        .is_some()
}

/// Whether the PRE-FLIGHT planner got PAST its attribution gate.
///
/// Read off the decision's own closed-set reason token rather than off whether
/// the planner acted: these fixtures plant no verdict, so an attributable
/// target still declines -- at a LATER gate. What matters here is which gate,
/// and the token says.
///
/// An unrecognized token PANICS rather than being folded into either side: a
/// new refusal has to be classified as before-or-at attribution or after it,
/// and guessing would make this row's verdict silently wrong.
fn preflight_admits(shape: &Shape, req: &ChatRequest) -> bool {
    let (router, target) = fixture(shape);
    let (_planned, records, _plan) =
        router.plan_field_preflight(req, &target, DispatchSurface::Complete);
    assert!(
        !records.is_empty(),
        "{}: the planner must record a decision",
        shape.name
    );
    records.iter().all(|record| match record.reason {
        // Refused before or at the attribution gate.
        FIELD_PREFLIGHT_NO_GROUNDED_FIELD
        | FIELD_PREFLIGHT_UNSUPPORTED_LANE
        | FIELD_PREFLIGHT_UNATTRIBUTABLE_TARGET
        | FIELD_PREFLIGHT_NO_IDENTITY => false,
        // Reached a gate BEYOND attribution, so attribution admitted.
        FIELD_PREFLIGHT_MASKED_BY_OVERRIDE
        | FIELD_PREFLIGHT_NOT_ELIGIBLE
        | FIELD_PREFLIGHT_BELOW_QUORUM
        | FIELD_PREFLIGHT_NO_TARGET_OPT_IN
        | FIELD_PREFLIGHT_AMBIGUOUS_MUTATION
        | FIELD_PREFLIGHT_CANARY_RESTORED
        | FIELD_PREFLIGHT_ACTION_DROP => true,
        other => panic!(
            "unclassified pre-flight reason {other:?}: a new refusal must be placed \
             before or after the attribution gate here, or this table cannot tell \
             which gate answered"
        ),
    })
}

/// Whether probe ACTIVATION admitted the lane, driven through the same
/// per-target facts the dispatch walk hands it.
fn activation_admits(shape: &Shape, req: &ChatRequest) -> bool {
    let (router, target) = fixture(shape);
    router.activate_probe_lanes_for_admitted_request(
        &target.state_key,
        &target.provider_name,
        target.provider_kind,
        target.use_forwarded_credential,
        req,
    );
    router.probe_scheduler_snapshot().activations_total > 0
}

/// Whether the PRE-DIAL recheck would let a queued job dial. It has no target
/// by then and reads the forwarded fact off the entry instead, which is exactly
/// why its answer has to match the other three.
fn pre_dial_recheck_admits(shape: &Shape, _req: &ChatRequest) -> bool {
    let (router, target) = fixture(shape);
    router.probe_entry_is_attributable(&target.provider_name)
}

/// One stage's verdict for one shape: does this stage consider the target
/// attributable?
type StageVerdict = fn(&Shape, &ChatRequest) -> bool;

#[test]
fn every_field_stage_reaches_the_same_attributability_verdict() {
    let req = grounding_request();
    // Premise: the table discriminates. A table that was all-admitting or
    // all-refusing would be satisfied by a stage answering one constant.
    assert!(
        SHAPES.iter().any(|s| s.attributable) && SHAPES.iter().any(|s| !s.attributable),
        "premise: the table must carry both verdicts"
    );

    let stages: [(&str, StageVerdict); 4] = [
        ("reactive", reactive_admits),
        ("pre-flight", preflight_admits),
        ("activation", activation_admits),
        ("pre-dial recheck", pre_dial_recheck_admits),
    ];
    for shape in SHAPES {
        for (stage, admits) in stages {
            assert_eq!(
                admits(shape, &req),
                shape.attributable,
                "{stage} disagrees about {}: a stage narrower than another mints \
                 verdicts from evidence the acting arm refuses, or probes a target \
                 it refuses",
                shape.name
            );
        }
    }
}
