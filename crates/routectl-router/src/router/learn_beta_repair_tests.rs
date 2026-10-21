//! The beta repair's eligibility, driven through the engine directly: which
//! named flags it withholds and re-dispatches without, which it refuses with
//! no budget drawn, and that the count surface repairs without minting.
//!
//! Every row is the same anthropic-api target and the same captured
//! anthropic-api beta-rejection envelope shape; only the one input a row
//! names differs, so each refusal is attributable to that input alone.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use routectl_core::{BetaAssembly, ChatRequest, Error, Provider};
use serde_json::json;

use super::super::super::capability_observe::UntranslatedCapabilities;
use super::super::super::class_observe::DispatchSurface;
use super::super::super::overlays::apply_layered_overlays;
use super::super::super::probe_test_support::OkProvider;
use super::super::super::repair_budget::{REPAIRS_PER_REQUEST, RepairBudget};
use super::super::super::runtime_gate::LearnedProbeGuard;
use super::super::{AttemptLearning, FailureDecision, Rejection};
use crate::config::Config;
use crate::resolved::ResolvedModel;
use crate::router::{DispatchMeta, DispatchTarget, Router};

const NICKNAME: &str = "m1";
const ANTHROPIC: &str = "[providers.p1]\nkind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\n";

/// The flag every row's rejection names.
const NAMED: &str = "zz-named-2099-01-01";
/// A client flag the upstream accepts.
const KEPT: &str = "kept-2099-01-01";

/// The committed Mantle capture naming two flags in one envelope.
const TWO_FLAGS_FIXTURE: &str =
    include_str!("../../tests/fixtures/rejections/anthropic-api/beta-two-flags-mantle.json");
/// The two flags [`TWO_FLAGS_FIXTURE`] names, in envelope order.
const FIRST: &str = "advanced-tool-use-2025-11-20";
const SECOND: &str = "prompt-caching-scope-2026-01-05";

/// The anthropic-api 400 naming `flag` in the exact beta-rejection envelope.
fn named_rejection(flag: &str) -> Error {
    beta_envelope_rejection(&format!(
        "Unexpected value(s) `{flag}` for the `anthropic-beta` header. Please consult our \
         documentation at platform.claude.com/docs or try again without the header."
    ))
}

/// The two-flag capture as the anthropic-api egress surfaces it.
fn two_flag_rejection() -> Error {
    let fixture: serde_json::Value =
        serde_json::from_str(TWO_FLAGS_FIXTURE).expect("fixture is JSON");
    let body = fixture["body"].as_str().expect("body");
    assert!(
        body.contains(&format!("`{FIRST}`, `{SECOND}`")),
        "premise: the capture names both flags"
    );
    Error::upstream_full(
        "p1",
        u16::try_from(fixture["status"].as_u64().expect("status")).expect("u16 status"),
        body.to_string(),
        None,
        Some("invalid_request_error".into()),
        None,
    )
}

fn beta_envelope_rejection(message: &str) -> Error {
    Error::upstream_full(
        "p1",
        400,
        json!({"type": "error", "error": {"type": "invalid_request_error", "message": message}})
            .to_string(),
        None,
        Some("invalid_request_error".into()),
        None,
    )
}

fn router(model_header_extras: &[(&str, &str)]) -> Router {
    let config: Config = toml::from_str(ANTHROPIC).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let provider: Arc<dyn Provider> = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let headers: BTreeMap<String, String> = model_header_extras
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    let model =
        ResolvedModel::new(NICKNAME, "p1", provider, "wire-model").with_header_extras(headers);
    router.install_resolved_models(BTreeMap::from([(NICKNAME.to_string(), Arc::new(model))]));
    router
}

fn target(router: &Router, req: &ChatRequest) -> DispatchTarget {
    router
        .dispatch_chain_for_request(req, DispatchSurface::Complete)
        .expect("the fixture chain resolves")
        .0
        .pop()
        .expect("one target")
}

fn client_request(betas: &[&str]) -> ChatRequest {
    ChatRequest {
        model: NICKNAME.into(),
        messages: vec![].into(),
        anthropic_beta: betas.iter().map(|b| (*b).to_string()).collect(),
        ..Default::default()
    }
}

fn owned(flags: &[&str]) -> Vec<String> {
    flags.iter().map(|f| (*f).to_string()).collect()
}

/// One eligibility case: everything a row varies.
struct Row {
    name: &'static str,
    /// The operator's model `header_extras["anthropic-beta"]`, which the
    /// overlay merges into the attempt's beta set.
    operator_pin: Option<&'static str>,
    client: &'static [&'static str],
    /// The target ships the client's own bearer and beta set.
    forwarded: bool,
    already_withheld: &'static [&'static str],
    /// `None` when the egress recorded no assembly.
    assembly: Option<BetaAssembly>,
    err: Error,
    /// The flags a repair withholds; empty when the row is refused.
    repaired: &'static [&'static str],
}

fn empty_assembly() -> Option<BetaAssembly> {
    Some(BetaAssembly::new(Vec::new(), Vec::new()))
}

/// What one row's rejection did to the attempt.
struct Outcome {
    retried: bool,
    withheld: Vec<String>,
    drawn: u8,
}

/// One row's router, client request and target.
struct Fixture {
    router: Router,
    req: ChatRequest,
    target: DispatchTarget,
}

impl Fixture {
    fn new(row: &Row) -> Self {
        let pin: Vec<(&str, &str)> = row
            .operator_pin
            .map(|flag| ("anthropic-beta", flag))
            .into_iter()
            .collect();
        let router = router(&pin);
        let req = client_request(row.client);
        let mut target = target(&router, &req);
        target.use_forwarded_credential = row.forwarded;
        Self {
            router,
            req,
            target,
        }
    }

    /// The attempt as the walk builds it: the client request with the
    /// operator overlay applied and the row's withheld set.
    fn attempt(&self, row: &Row) -> ChatRequest {
        let mut attempt_req = self.req.clone();
        apply_layered_overlays(&self.router.config, &self.target, &mut attempt_req);
        attempt_req.routectl_internal.withheld_betas = owned(row.already_withheld).into();
        attempt_req
    }

    /// Hand `row`'s rejection to `learning` and report what it did.
    fn reject(
        &self,
        row: &Row,
        learning: &mut AttemptLearning<'_>,
        meta: &mut DispatchMeta,
    ) -> Outcome {
        let mut attempt_req = self.attempt(row);
        let mut budget = RepairBudget::per_request();
        let cf = learning.classify(&row.err, self.target.provider_kind);
        let native = cf.class.clone();
        let rejection = Rejection {
            err: &row.err,
            class: &cf,
            native_class: &native,
            target: &self.target,
            client_req: &self.req,
            site: None,
            transcript_rejection: false,
            beta_assembly: row.assembly.as_ref(),
        };
        let decision = learning.on_rejection(rejection, &mut attempt_req, meta, &mut budget);
        Outcome {
            retried: matches!(decision, FailureDecision::Retry { .. }),
            withheld: attempt_req.routectl_internal.withheld_betas.to_vec(),
            drawn: REPAIRS_PER_REQUEST - budget.remaining(),
        }
    }
}

#[test]
fn only_a_transmitted_removable_client_flag_is_repaired() {
    let rows = [
        Row {
            name: "transmitted client flag",
            operator_pin: None,
            client: &[KEPT, NAMED],
            forwarded: false,
            already_withheld: &[],
            assembly: empty_assembly(),
            err: named_rejection(NAMED),
            repaired: &[NAMED],
        },
        Row {
            name: "two transmitted client flags in one envelope",
            operator_pin: None,
            client: &[KEPT, FIRST, SECOND],
            forwarded: false,
            already_withheld: &[],
            assembly: empty_assembly(),
            err: two_flag_rejection(),
            repaired: &[FIRST, SECOND],
        },
        // All or nothing: the removable flag is not withheld alone.
        Row {
            name: "two named flags, one in the floor",
            operator_pin: None,
            client: &[KEPT, FIRST, SECOND],
            forwarded: false,
            already_withheld: &[],
            assembly: Some(BetaAssembly::new(owned(&[SECOND]), Vec::new())),
            err: two_flag_rejection(),
            repaired: &[],
        },
        Row {
            name: "forwarded-credential target",
            operator_pin: None,
            client: &[KEPT, NAMED],
            forwarded: true,
            already_withheld: &[],
            assembly: empty_assembly(),
            err: named_rejection(NAMED),
            repaired: &[],
        },
        Row {
            name: "floor flag",
            operator_pin: None,
            client: &[KEPT, NAMED],
            forwarded: false,
            already_withheld: &[],
            assembly: Some(BetaAssembly::new(owned(&[NAMED]), Vec::new())),
            err: named_rejection(NAMED),
            repaired: &[],
        },
        Row {
            name: "body-implied flag",
            operator_pin: None,
            client: &[KEPT, NAMED],
            forwarded: false,
            already_withheld: &[],
            assembly: Some(BetaAssembly::new(Vec::new(), owned(&[NAMED]))),
            err: named_rejection(NAMED),
            repaired: &[],
        },
        Row {
            name: "flag the client did not send",
            operator_pin: None,
            client: &[KEPT],
            forwarded: false,
            already_withheld: &[],
            assembly: empty_assembly(),
            err: named_rejection(NAMED),
            repaired: &[],
        },
        Row {
            name: "flag already withheld, so not transmitted",
            operator_pin: None,
            client: &[KEPT, NAMED],
            forwarded: false,
            already_withheld: &[NAMED],
            assembly: empty_assembly(),
            err: named_rejection(NAMED),
            repaired: &[],
        },
        Row {
            name: "malformed envelope",
            operator_pin: None,
            client: &[KEPT, NAMED],
            forwarded: false,
            already_withheld: &[],
            assembly: empty_assembly(),
            err: beta_envelope_rejection(&format!(
                "Unexpected value(s): `{NAMED}` for the `anthropic-beta` header."
            )),
            repaired: &[],
        },
        Row {
            name: "no beta assembly recorded",
            operator_pin: None,
            client: &[KEPT, NAMED],
            forwarded: false,
            already_withheld: &[],
            assembly: None,
            err: named_rejection(NAMED),
            repaired: &[],
        },
        // The operator pin reaches the attempt's beta set through the
        // overlay, never the client's. The egress lists it in its floor, so
        // this row refuses it on two grounds.
        Row {
            name: "operator-pinned flag, listed in the floor",
            operator_pin: Some(NAMED),
            client: &[KEPT],
            forwarded: false,
            already_withheld: &[],
            assembly: Some(BetaAssembly::new(owned(&[NAMED]), Vec::new())),
            err: named_rejection(NAMED),
            repaired: &[],
        },
        // The same pin with a floor that omits it: only the client-set read
        // refuses it, so this row is the single source for that read.
        Row {
            name: "operator-pinned flag, absent from the floor",
            operator_pin: Some(NAMED),
            client: &[KEPT],
            forwarded: false,
            already_withheld: &[],
            assembly: empty_assembly(),
            err: named_rejection(NAMED),
            repaired: &[],
        },
    ];

    for row in &rows {
        let fixture = Fixture::new(row);
        let mut learning = AttemptLearning::new(
            &fixture.router,
            DispatchSurface::Complete,
            LearnedProbeGuard::inert(),
        );
        let outcome = fixture.reject(row, &mut learning, &mut DispatchMeta::for_alias(NICKNAME));

        if row.repaired.is_empty() {
            assert!(!outcome.retried, "{}: proceeds unrepaired", row.name);
            assert_eq!(
                outcome.withheld,
                owned(row.already_withheld),
                "{}: withheld set untouched",
                row.name
            );
            assert_eq!(outcome.drawn, 0, "{}: nothing drawn", row.name);
        } else {
            assert!(outcome.retried, "{}: re-dispatched", row.name);
            assert_eq!(outcome.withheld, owned(row.repaired), "{}", row.name);
            assert_eq!(outcome.drawn, 1, "{}: one draw", row.name);
        }
    }
}

#[test]
fn the_premise_of_the_pinned_rows_holds_the_flag_on_the_attempt_only() {
    let row = Row {
        name: "operator pin",
        operator_pin: Some(NAMED),
        client: &[KEPT],
        forwarded: false,
        already_withheld: &[],
        assembly: empty_assembly(),
        err: named_rejection(NAMED),
        repaired: &[],
    };
    let fixture = Fixture::new(&row);

    let attempt_req = fixture.attempt(&row);

    assert_eq!(
        attempt_req.anthropic_beta,
        owned(&[KEPT, NAMED]),
        "{}",
        row.name
    );
    assert_eq!(fixture.req.anthropic_beta, owned(&[KEPT]), "{}", row.name);
}

#[test]
fn count_tokens_repairs_a_named_beta_and_never_mints() {
    for surface in [DispatchSurface::Complete, DispatchSurface::CountTokens] {
        // Arrange
        let row = Row {
            name: "transmitted client flag",
            operator_pin: None,
            client: &[KEPT, NAMED],
            forwarded: false,
            already_withheld: &[],
            assembly: empty_assembly(),
            err: named_rejection(NAMED),
            repaired: &[NAMED],
        };
        let fixture = Fixture::new(&row);
        let mut learning =
            AttemptLearning::new(&fixture.router, surface, LearnedProbeGuard::inert());
        let mut meta = DispatchMeta::for_alias(NICKNAME);
        let outcome = fixture.reject(&row, &mut learning, &mut meta);
        assert!(outcome.retried, "{surface:?}: the repair takes");
        assert_eq!(outcome.withheld, owned(&[NAMED]), "{surface:?}");

        // Act: the repaired attempt succeeds.
        learning.on_success(
            &UntranslatedCapabilities::default(),
            &fixture.target,
            &fixture.req,
            &mut HashSet::new(),
            &mut meta,
        );

        // Assert
        let learned: Vec<&str> = meta
            .learned_capabilities
            .iter()
            .map(|e| e.capability_key.as_str())
            .collect();
        if surface == DispatchSurface::Complete {
            let key = crate::beta_capability::beta_capability_key(NAMED).expect("well-formed flag");
            assert_eq!(learned, [key], "{surface:?}: the control mints");
        } else {
            assert!(learned.is_empty(), "{surface:?}: no verdict: {learned:?}");
        }
    }
}
