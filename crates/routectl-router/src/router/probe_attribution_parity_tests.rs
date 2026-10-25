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
//! read gets WRONG, since its configured base IS that default while it egresses
//! to the Mantle endpoint its region derives. It is attributable, and its lane
//! view carries no row, so the two acting stages decline it AFTER attribution:
//! the reactive arm on its empty view and the pre-flight with
//! `no_grounded_field`. Those declines are asserted on their own, with their
//! reasons, rather than read as attribution verdicts.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use routectl_core::{ChatRequest, Provider};

use super::class_observe::DispatchSurface;
use super::field_preflight::{
    FIELD_PREFLIGHT_ACTION_CEILING, FIELD_PREFLIGHT_ACTION_DROP, FIELD_PREFLIGHT_ACTION_REWRITE,
    FIELD_PREFLIGHT_AMBIGUOUS_MUTATION, FIELD_PREFLIGHT_BELOW_QUORUM,
    FIELD_PREFLIGHT_CANARY_RESTORED, FIELD_PREFLIGHT_MASKED_BY_OVERRIDE,
    FIELD_PREFLIGHT_NO_GROUNDED_FIELD, FIELD_PREFLIGHT_NO_IDENTITY,
    FIELD_PREFLIGHT_NO_STORED_VALUE, FIELD_PREFLIGHT_NO_TARGET_OPT_IN,
    FIELD_PREFLIGHT_NOT_ELIGIBLE, FIELD_PREFLIGHT_UNATTRIBUTABLE_TARGET,
    FIELD_PREFLIGHT_UNSUPPORTED_LANE, FIELD_PREFLIGHT_VALUE_ALREADY_SENT,
    FIELD_PREFLIGHT_WITHIN_CEILING, FIELD_PREFLIGHT_WRITER_UNHEALTHY,
};
use super::field_repair::FieldSettlementMode;
use super::probe_test_support::{
    OkProvider, RECHECK_PATH, grounding_request, plant_acting_negative,
};
use super::{DispatchTarget, Router};
use crate::config::{AliasValue, Config, CredentialSource, ModelEntry, ProviderEntry};
use crate::field_verdict::FieldVerdictKey;
use crate::probe_scheduler::{FreeValidatorOutcome, ProbePayload, ProbeValidator};
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
    /// Whether the admitted lane's view carries no row, so the acting stages
    /// decline after attribution rather than at it.
    empty_view: bool,
    /// Whether the `[providers]` entry is DROPPED after the dispatch target has
    /// been expanded from it -- a reload removing the entry under a target that
    /// already carries its provider kind and forwarded flag.
    entry_removed_after_expansion: bool,
    /// Whether every stage must find this shape attributable.
    attributable: bool,
}

/// The parity table. Admitting shapes (two direct bases and, where the build
/// can construct one, a Bedrock Mantle sub-lane), and refusing ones that each
/// refuse for a DIFFERENT reason (loopback, forwarded credential, vanished
/// entry).
const SHAPES: &[Shape] = &[
    Shape {
        name: "the Anthropic default base, routectl's own credential",
        base_url: "https://api.anthropic.com",
        credential_source: CredentialSource::Own,
        #[cfg(feature = "bedrock")]
        bedrock_mantle: false,
        empty_view: false,
        entry_removed_after_expansion: false,
        attributable: true,
    },
    Shape {
        name: "an operator's remote mirror",
        base_url: "https://api.example.com/anthropic",
        credential_source: CredentialSource::Own,
        #[cfg(feature = "bedrock")]
        bedrock_mantle: false,
        empty_view: false,
        entry_removed_after_expansion: false,
        attributable: true,
    },
    Shape {
        name: "a loopback base URL",
        base_url: "http://127.0.0.1:8899",
        credential_source: CredentialSource::Own,
        #[cfg(feature = "bedrock")]
        bedrock_mantle: false,
        empty_view: false,
        entry_removed_after_expansion: false,
        attributable: false,
    },
    Shape {
        name: "a forwarded client credential",
        base_url: "https://api.anthropic.com",
        credential_source: CredentialSource::Forwarded,
        #[cfg(feature = "bedrock")]
        bedrock_mantle: false,
        empty_view: false,
        entry_removed_after_expansion: false,
        attributable: false,
    },
    Shape {
        name: "an entry a reload removed",
        base_url: "https://api.anthropic.com",
        credential_source: CredentialSource::Own,
        #[cfg(feature = "bedrock")]
        bedrock_mantle: false,
        empty_view: false,
        entry_removed_after_expansion: true,
        attributable: false,
    },
    // The Mantle row is the one a raw base-url read gets WRONG: its configured
    // base is the Anthropic default, so a stage reading the base URL directly
    // reads a remote Anthropic host, while every stage must read the Mantle
    // endpoint its region derives.
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
        empty_view: true,
        entry_removed_after_expansion: false,
        attributable: true,
    },
];

/// The ONLY base URL a Mantle entry may carry: `region` is the single source of
/// truth for its endpoint, so the production validator requires this exact
/// value. Spelled here, not shared with the plain-default row, because the whole
/// point of the Mantle row is that this base reads as a legitimate remote
/// Anthropic host while the upstream is Bedrock Mantle.
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
/// - the base is the production Anthropic default AND not loopback, so a stage
///   reading the configured base instead of the derived one would still admit
///   it -- which is what keeps it distinct from the loopback row;
/// - `anthropic_api_base_url()` answers `None`, so the row's admission comes
///   from the region-derived egress every stage reads, not from that base.
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
        "premise: a mantle entry carries no direct anthropic-api base URL, so its \
         admission rests on the region-derived egress"
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
    fixture_with(
        shape,
        Arc::new(OkProvider {
            count_calls: AtomicUsize::new(0),
        }),
    )
}

/// [`fixture`], dispatching to `provider`.
fn fixture_with(shape: &Shape, provider: Arc<dyn Provider>) -> (Router, DispatchTarget) {
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
        | FIELD_PREFLIGHT_NO_STORED_VALUE
        | FIELD_PREFLIGHT_VALUE_ALREADY_SENT
        | FIELD_PREFLIGHT_WITHIN_CEILING
        | FIELD_PREFLIGHT_CANARY_RESTORED
        // AFTER the attribution gate: the persistence refusal is reached only once
        // a lane has been admitted and an identity minted, so seeing it means
        // attribution said yes. It is also why this fixture needs no durability
        // opt-in -- reaching this token is itself the admission this table reads.
        | FIELD_PREFLIGHT_WRITER_UNHEALTHY
        | FIELD_PREFLIGHT_ACTION_DROP
        | FIELD_PREFLIGHT_ACTION_REWRITE
        | FIELD_PREFLIGHT_ACTION_CEILING => true,
        other => panic!(
            "unclassified pre-flight reason {other:?}: a new refusal must be placed \
             before or after the attribution gate here, or this table cannot tell \
             which gate answered"
        ),
    })
}

/// Whether probe ACTIVATION admitted the lane, driven through the same
/// per-target facts the dispatch walk hands it.
///
/// The lane holds an acting negative on the re-check row first: activation only
/// ever re-checks one, so without it every shape would read as refused and the
/// row would measure the missing negative rather than attributability.
fn activation_admits(shape: &Shape, req: &ChatRequest) -> bool {
    let (router, target) = fixture(shape);
    let lane = target
        .learned_lane
        .as_ref()
        .expect("a fixture target mints a lane");
    plant_acting_negative(&router, lane, RECHECK_PATH, "anthropic-api");
    router.activate_probe_lanes_for_admitted_request(
        lane,
        &target.provider_name,
        target.provider_kind,
        target.use_forwarded_credential,
        &super::probe_payload_capture::ProbeFieldCapture::of(
            crate::router::FieldRepairTable::default(),
            req,
        ),
        req,
    );
    router.probe_scheduler_snapshot().activations_total > 0
}

/// Whether the PRE-DIAL recheck would let a queued job dial. It has no target
/// by then and reads the forwarded fact off the entry instead, which is exactly
/// why its answer has to match the other three.
fn pre_dial_recheck_admits(shape: &Shape, _req: &ChatRequest) -> bool {
    let (router, target) = fixture(shape);
    router
        .probe_entry_lane(&target.provider_name, &target.upstream)
        .is_some()
}

/// One stage's verdict for one shape: does this stage consider the target
/// attributable?
type StageVerdict = fn(&Shape, &ChatRequest) -> bool;

/// One field stage under the parity table.
struct Stage {
    name: &'static str,
    admits: StageVerdict,
    /// Whether the stage's verdict reads attribution alone, whatever the
    /// lane's view. An acting stage also declines a lane whose view carries
    /// no row, which its verdict cannot tell from a refusal at attribution.
    reads_attribution_only: bool,
}

const STAGES: [Stage; 4] = [
    Stage {
        name: "reactive",
        admits: reactive_admits,
        reads_attribution_only: false,
    },
    Stage {
        name: "pre-flight",
        admits: preflight_admits,
        reads_attribution_only: false,
    },
    Stage {
        name: "activation",
        admits: activation_admits,
        reads_attribution_only: true,
    },
    Stage {
        name: "pre-dial recheck",
        admits: pre_dial_recheck_admits,
        reads_attribution_only: true,
    },
];

#[test]
fn every_field_stage_reaches_the_same_attributability_verdict() {
    let req = grounding_request();
    // Premise: the table discriminates. A table that was all-admitting or
    // all-refusing would be satisfied by a stage answering one constant.
    assert!(
        SHAPES.iter().any(|s| s.attributable) && SHAPES.iter().any(|s| !s.attributable),
        "premise: the table must carry both verdicts"
    );

    for shape in SHAPES {
        // The acting stages decline an empty view after attribution, which no
        // verdict of theirs can tell from a refusal at it; that shape's
        // attribution and declines are asserted below.
        let stages = STAGES
            .iter()
            .filter(|stage| !shape.empty_view || stage.reads_attribution_only);
        for stage in stages {
            let Stage {
                name: stage,
                admits,
                ..
            } = stage;
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

/// The Mantle sub-lane is attributable to the shared decision and to both
/// probe stages, and its empty lane view is why the two acting stages then
/// decline: the reactive arm plans nothing and the pre-flight reports
/// `no_grounded_field` for a request whose rows the direct sibling's view
/// presents.
///
/// With the sub-lane's allowlist empty the acting stages cannot show
/// attribution themselves, so for them this asserts the shared decision they
/// read and their declines, each with its reason. The acting-stage assertion
/// becomes discriminating once a row is admitted on that Mantle sub-lane,
/// when the shape can join the parity loop above.
///
/// Mutation check: map the Mantle egresses to their kind's scope in
/// `RowScope::of_lane` -> red (the reactive arm plans, the pre-flight passes
/// `no_grounded_field`); drop the anthropic Mantle arm of
/// `classifiable_egress_base_url` -> red on attribution.
#[cfg(feature = "bedrock")]
#[test]
fn the_mantle_sub_lane_is_attributable_and_declined_on_its_empty_view() {
    let shape = SHAPES
        .iter()
        .find(|shape| shape.bedrock_mantle)
        .expect("the table carries the Mantle shape");
    let req = grounding_request();
    assert!(shape.empty_view && shape.attributable, "premise");

    // Attribution, at every stage.
    let (router, target) = fixture(shape);
    let lane = super::field_repair::attributable_field_lane(
        &router.config,
        &target.provider_name,
        target.use_forwarded_credential,
    )
    .expect("the shared decision the acting stages read admits the Mantle lane");
    assert_eq!(
        lane.base_url,
        "https://bedrock-mantle.us-west-2.api.aws/anthropic"
    );
    assert!(activation_admits(shape, &req), "activation admits");
    assert!(
        pre_dial_recheck_admits(shape, &req),
        "the pre-dial recheck admits"
    );

    // The view, empty on the lane and not on its direct sibling.
    let table = router.field_repair_table();
    let rows = |egress| {
        super::field_repair::present_rows(table.for_lane(lane.provider_kind, Some(egress)), &req)
            .count()
    };
    assert_eq!(rows(lane.egress), 0, "the Mantle view presents no row");
    assert!(
        rows(routectl_core::wire_check::EgressKind::AnthropicApi) > 0,
        "control: the direct sibling's view presents the request's rows",
    );

    // The declines, each with its reason.
    assert!(
        !reactive_admits(shape, &req),
        "the reactive arm plans nothing"
    );
    let (router, target) = fixture(shape);
    let (_planned, records, _plan) =
        router.plan_field_preflight(&req, &target, DispatchSurface::Complete);
    let reasons: Vec<&str> = records.iter().map(|record| record.reason).collect();
    assert_eq!(reasons, vec![FIELD_PREFLIGHT_NO_GROUNDED_FIELD]);
}

/// The free re-check of `fixture(shape)`'s lane, keyed under `provider_kind`
/// with a resident negative on the re-check row: its outcome and how many
/// counts it dialed.
async fn recheck_on(shape: &Shape, provider_kind: &str) -> (FreeValidatorOutcome, usize) {
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let (router, target) = fixture_with(shape, Arc::clone(&provider) as Arc<dyn Provider>);
    let lane = target
        .learned_lane
        .as_ref()
        .expect("a fixture target mints a lane");
    plant_acting_negative(&router, lane, RECHECK_PATH, provider_kind);
    let key = FieldVerdictKey::new(lane, RECHECK_PATH, provider_kind).expect("identity");
    let payload = ProbePayload::new(RECHECK_PATH, "summarized".to_string(), &[], &[], false)
        .expect("a modeled value within every bound");
    let outcome = router
        .run_free_validator_for_tests(&key, &payload, ProbeValidator::CountTokens)
        .await;
    (outcome, provider.count_calls.load(Ordering::SeqCst))
}

/// The pre-dial recheck dials only when the entry the seat names is still
/// the identity's kind AND that lane's view carries the probed row. Capture
/// is not lane-scoped, so a negative keyed on a row the Mantle view omits
/// must not be cleared by a count on that lane, and an identity of a kind the
/// entry no longer has must not be cleared by a count of another kind. The
/// direct lane, whose view carries the row, is the positive control.
///
/// Mutation check: drop the `lane_rows().any(..)` half of
/// `probe_lane_carries` -> red on the Mantle row (it dials and settles); drop
/// the `provider_kind` half -> red on the re-kinded row.
#[tokio::test]
async fn the_pre_dial_recheck_dials_only_where_the_lane_carries_the_row() {
    let direct = &SHAPES[0];
    assert!(direct.attributable && !direct.empty_view, "premise");
    #[cfg_attr(not(feature = "bedrock"), allow(unused_mut))]
    let mut cases = vec![
        (
            "direct lane, its own kind",
            direct,
            "anthropic-api",
            (FreeValidatorOutcome::Settled, 1),
        ),
        (
            "an identity of a kind the entry no longer has",
            direct,
            "bedrock",
            (FreeValidatorOutcome::Unavailable, 0),
        ),
    ];
    #[cfg(feature = "bedrock")]
    cases.push((
        "Mantle lane, whose view omits the row",
        SHAPES
            .iter()
            .find(|shape| shape.bedrock_mantle)
            .expect("the table carries the Mantle shape"),
        "anthropic-api",
        (FreeValidatorOutcome::Unavailable, 0),
    ));

    for (name, shape, provider_kind, expected) in cases {
        assert!(
            pre_dial_recheck_admits(shape, &grounding_request()),
            "{name}: premise: the entry is attributable",
        );
        assert_eq!(recheck_on(shape, provider_kind).await, expected, "{name}");
    }
}
