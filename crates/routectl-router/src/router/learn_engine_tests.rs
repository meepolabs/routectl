//! The engine's one arbitration order: for a rejection two learners could
//! both claim, the earlier slot of `ARBITRATION_ORDER` decides it, and a
//! repair whose effect does not take hands the rejection to the next slot.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use routectl_core::capability::{REASONING_REPLAY, SignalTier};
use routectl_core::failure_class::{ClassifiedFailure, FailureClass, MatchedBy};
use routectl_core::{Error, Provider};
use serde_json::json;

use super::super::field_repair::FieldRepairTable;
use super::super::probe_test_support::{GROUNDED_PATH, OkProvider};
use super::super::rejection_parser::NamedRejection;
use super::super::{DispatchTarget, Router};
use super::{Arbitration, RejectionInput, RepairSink, arbitrate};
use crate::config::Config;
use crate::resolved::ResolvedModel;

/// A capability the generic resolver attributes on an anthropic-api lane
/// when the effective class names it.
const GENERIC_CAPABILITY: &str = "web_search";

/// Which repair effects take for one row. A `false` stands for every reason
/// an effect refuses: already spent on this target, no plan, or no allowance
/// left in the request's repair budget.
#[derive(Clone, Copy)]
struct Takes {
    strip: bool,
    field: bool,
    canary: bool,
    beta: bool,
}

const ALL_TAKE: Takes = Takes {
    strip: true,
    field: true,
    canary: true,
    beta: true,
};

/// A sink that records nothing but whether each effect takes.
struct ScriptedSink(Takes);

impl RepairSink for ScriptedSink {
    fn strip_replay(&mut self) -> bool {
        self.0.strip
    }
    fn repair_field(&mut self, _: NamedRejection) -> bool {
        self.0.field
    }
    fn canary_retry(&mut self, _: NamedRejection) -> bool {
        self.0.canary
    }
    fn withhold_betas(&mut self, _: Vec<String>) -> bool {
        self.0.beta
    }
}

fn anthropic_target() -> DispatchTarget {
    let config: Config =
        toml::from_str("[providers.p1]\nkind = \"anthropic-api\"\n").expect("valid test toml");
    let router = Router::new(Arc::new(config));
    let provider: Arc<dyn Provider> = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let model = ResolvedModel::new("m1", "p1", provider, "wire-model");
    router
        .expand_chain_to_targets(vec![Arc::new(model)], None)
        .pop()
        .expect("one target for a non-seat model")
}

/// An anthropic-api refusal of the grounded closed-table field, which the
/// field-rejection parser names on the native `BadRequest` class.
fn field_rejection() -> Error {
    Error::upstream(
        "p1",
        400,
        json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": format!("{GROUNDED_PATH}: Extra inputs are not permitted"),
            },
        })
        .to_string(),
    )
}

fn effective(class: FailureClass) -> ClassifiedFailure {
    ClassifiedFailure {
        class,
        matched_by: MatchedBy::UpstreamType,
    }
}

fn feature_unsupported(capability: &str) -> FailureClass {
    FailureClass::FeatureUnsupported {
        capability: capability.to_string(),
    }
}

/// One named arbitration case: the effective class, whether replay artifacts
/// were carried, which effects take, and the decision expected.
struct Row {
    name: &'static str,
    effective: FailureClass,
    replay_carried: bool,
    takes: Takes,
    expected: Arbitration,
}

fn generic_proposal() -> Arbitration {
    Arbitration::Proceed(Some((
        GENERIC_CAPABILITY.to_string(),
        SignalTier::SelfIdentifying,
        routectl_core::capability::FailurePhase::F1,
    )))
}

#[test]
fn the_arbitration_order_decides_every_contested_rejection() {
    // Arrange: one rejection the field parser names on its NATIVE class. The
    // effective class each row sets is what makes a second proposer claim it
    // too -- the replay rejection or a generic capability -- so every row is
    // a real contest between two proposals.
    let target = anthropic_target();
    let err = field_rejection();
    let native = FailureClass::BadRequest;
    let rows = [
        Row {
            name: "replay strip beats the field row",
            effective: feature_unsupported(REASONING_REPLAY),
            replay_carried: true,
            takes: ALL_TAKE,
            expected: Arbitration::ReplayStrip,
        },
        Row {
            name: "field row beats the generic capability",
            effective: feature_unsupported(GENERIC_CAPABILITY),
            replay_carried: false,
            takes: ALL_TAKE,
            expected: Arbitration::FieldRepair,
        },
        Row {
            name: "canary retry beats the generic capability",
            effective: feature_unsupported(GENERIC_CAPABILITY),
            replay_carried: false,
            takes: Takes {
                strip: true,
                field: false,
                canary: true,
                beta: true,
            },
            expected: Arbitration::CanaryRetry,
        },
        Row {
            name: "field row with the budget exhausted falls through to the generic capability",
            effective: feature_unsupported(GENERIC_CAPABILITY),
            replay_carried: false,
            takes: Takes {
                strip: false,
                field: false,
                canary: false,
                beta: false,
            },
            expected: generic_proposal(),
        },
    ];

    for row in rows {
        let cf = effective(row.effective);
        let input = RejectionInput {
            err: &err,
            class: &cf,
            native_class: &native,
            target: &target,
            replay_carried: row.replay_carried,
            table: FieldRepairTable::default(),
            site: None,
            client_betas: &[],
            withheld_betas: &[],
            beta_assembly: None,
        };

        // Act
        let decided = arbitrate(&input, &mut ScriptedSink(row.takes));

        // Assert
        assert_eq!(decided, row.expected, "{}", row.name);
    }
}

/// The field-row proposer resolves an effort refusal against the level the
/// site read off the refused attempt's egress body, and against nothing when
/// there is no site.
#[test]
fn the_field_row_proposer_reads_the_sent_effort_off_the_site() {
    use super::super::learn_proposers::FieldRowProposer;
    use super::{EffortRank, Proposal, Proposer, RejectionSite, SentEffort, ThinkingShape};

    // Arrange
    let target = anthropic_target();
    let err = Error::upstream(
        "p1",
        400,
        json!({"type": "error", "error": {
            "type": "invalid_request_error",
            "message": "This model does not support effort level 'xhigh'. Supported levels: high, low, medium.",
        }})
        .to_string(),
    );
    let class = effective(FailureClass::BadRequest);
    let site = |sent_effort| RejectionSite {
        provenance: routectl_core::wire_check::RejectionProvenance::ClientUnchanged,
        in_egress: Some(true),
        matched_edit: super::MatchedEdit::None,
        locator: None,
        sent: super::SentSlot::default(),
        pending: 0,
        sent_effort,
        thinking: ThinkingShape::Adaptive,
    };
    let xhigh = SentEffort::Rank(EffortRank::of_token("xhigh").expect("a rank member"));
    for (name, site, expected) in [
        (
            "sent xhigh",
            Some(site(xhigh)),
            Some("output_config.effort.xhigh"),
        ),
        ("sent effort unknown", Some(site(SentEffort::Unknown)), None),
        ("no site", None, None),
    ] {
        let input = RejectionInput {
            err: &err,
            class: &class,
            native_class: &FailureClass::BadRequest,
            target: &target,
            replay_carried: false,
            table: FieldRepairTable::default(),
            site,
            client_betas: &[],
            withheld_betas: &[],
            beta_assembly: None,
        };

        // Act
        let proposed = FieldRowProposer.propose(&input);

        // Assert
        let row = proposed.map(|proposal| match proposal {
            Proposal::FieldRow { row, .. } => row.path,
            other => panic!("{name}: not a field row: {other:?}"),
        });
        assert_eq!(row, expected, "{name}");
    }
}

#[path = "learn_engine_surface_tests.rs"]
mod learn_engine_surface_tests;
