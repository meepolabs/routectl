//! Claiming a paid-probe candidate and authorizing one paid call: the atomic
//! claim under contention, every precondition that refuses BEFORE the ledger is
//! asked, the shared concurrency slot, and which refusals put the candidate
//! back.
//!
//! Every "no ledger call" assertion is paired with a POSITIVE control that
//! differs in exactly the fact under test and DOES reach the ledger, because a
//! zero-call count is free on a fixture that could never have reached it at
//! all. The counting ledger double is what makes both directions observable.

use super::*;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use routectl_core::Provider;

use crate::catalog::{CatalogRow, EffectiveRow, Source};
use crate::config::{AliasValue, Config, ModelEntry, ProviderEntry};
use crate::probe_scheduler::{
    FreeValidatorOutcome, PROBE_MAX_CONCURRENCY, PROBE_QUEUE_DEPTH, ProbeValidator,
};
use crate::resolved::ResolvedModel;
use crate::router::paid_probe_ledger::PaidProbeLedger;
use crate::router::paid_probe_profile::LEGACY_MIN_VIABLE_MAX_TOKENS;
use crate::router::probe_test_support::{GROUNDED_PATH, OkProvider, ZeroCountProvider};

/// A stamp date for a synthetic effective row. Never read by anything under
/// test -- staleness is not one of the facts here.
const STAMP: &str = "2026-01-01";

/// The configured provider key every fixture router installs its seat under.
const PROVIDER: &str = "p1";

/// A non-zero daily cap, so the cap-zero refusal is a FIXTURE choice rather
/// than the default.
const CAP: u32 = 5;

/// The second configured provider, for the two-lane fairness fixture.
const SECOND_PROVIDER: &str = "p2";

/// The two fairness lanes, each on its OWN provider so the accounting layer can
/// tell them apart by the `provider` argument it is handed.
const TWO_LANES: [(&str, &str); 2] = [("m1", PROVIDER), ("m2", SECOND_PROVIDER)];

/// A counting ledger double: records every reservation it is asked for and
/// answers with a fixed outcome.
///
/// The CALL COUNT is the instrument this whole sidecar rests on. Every local
/// precondition is supposed to refuse without asking the accounting layer, and
/// only a double that can report "you never asked me" can distinguish that from
/// "you asked and I said no".
struct CountingLedger {
    calls: AtomicUsize,
    seen: parking_lot::Mutex<Vec<(String, u32)>>,
    answer: PaidProbeReservation,
}

impl CountingLedger {
    fn answering(answer: PaidProbeReservation) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            seen: parking_lot::Mutex::new(Vec::new()),
            answer,
        })
    }

    fn committed() -> Arc<Self> {
        Self::answering(PaidProbeReservation::Committed { used: 2, cap: CAP })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Acquire)
    }

    fn seen(&self) -> Vec<(String, u32)> {
        self.seen.lock().clone()
    }
}

#[async_trait::async_trait]
impl PaidProbeLedger for CountingLedger {
    async fn reserve_paid_probe_unit(
        &self,
        provider: &str,
        daily_cap: u32,
    ) -> PaidProbeReservation {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.seen.lock().push((provider.to_string(), daily_cap));
        self.answer.clone()
    }
}

/// THE fully-priced control cell: both rates finite and positive, ceiling well
/// above either wire shape's floor.
fn fully_priced() -> EffectiveRow {
    let mut row = CatalogRow::sentinel();
    row.input_cost_per_token = Some(3.0e-6);
    row.output_cost_per_token = Some(1.5e-5);
    row.max_output_tokens = Some(64_000);
    EffectiveRow::Present {
        row,
        source: Source::Baked,
        verified_at: STAMP.to_string(),
    }
}

/// Which facts a fixture router deviates from the fully-admissible control on.
///
/// A struct of knobs rather than one builder per case, so every negative case
/// provably shares every OTHER fixture fact with the positive control -- which
/// is what makes a case that goes green under a removed check a case that was
/// not discriminating.
#[derive(Clone)]
struct Fixture {
    /// The `[fidelity.paid_probe_daily_caps]` value for `PROVIDER`.
    cap: u32,
    /// The catalog cell on the resolved model.
    effective_row: EffectiveRow,
    /// The provider entry's base URL. A loopback host is what makes the entry
    /// unattributable without changing anything else.
    base_url: String,
    /// Whether a resolved model is installed for the `m1` nickname at all.
    resolve_model: bool,
}

impl Default for Fixture {
    fn default() -> Self {
        Self {
            cap: CAP,
            effective_row: fully_priced(),
            base_url: "https://api.anthropic.com".to_string(),
            resolve_model: true,
        }
    }
}

impl Fixture {
    fn cap(mut self, cap: u32) -> Self {
        self.cap = cap;
        self
    }

    fn effective_row(mut self, effective_row: EffectiveRow) -> Self {
        self.effective_row = effective_row;
        self
    }

    fn loopback_entry(mut self) -> Self {
        self.base_url = "http://127.0.0.1:8080".to_string();
        self
    }

    fn without_resolved_model(mut self) -> Self {
        self.resolve_model = false;
        self
    }

    /// Build the router: one anthropic-api entry `p1`, one model `m1`, the
    /// configured cap, and a committing ledger installed.
    fn router(self) -> Router {
        self.router_with_ledger(CountingLedger::committed())
    }

    fn router_with_ledger(self, ledger: Arc<CountingLedger>) -> Router {
        self.bare_router().with_paid_probe_ledger(ledger)
    }

    /// `router_with_ledger` over ANY ledger implementation, for the
    /// generation fragment's parking double.
    fn router_with_ledger_arc(self, ledger: Arc<dyn PaidProbeLedger>) -> Router {
        self.bare_router().with_paid_probe_ledger(ledger)
    }

    /// The same router with NO ledger installed -- the fail-closed default.
    fn bare_router(self) -> Router {
        let mut config = Config::default();
        let mut entry = ProviderEntry::anthropic_api("literal:k");
        if let ProviderEntry::AnthropicApi { base_url, .. } = &mut entry {
            *base_url = self.base_url.clone();
        }
        config.providers.insert(PROVIDER.to_string(), entry);
        config.models.insert(
            "m1".to_string(),
            ModelEntry::new(PROVIDER, "claude-sonnet-4-5"),
        );
        config
            .aliases
            .insert("default".to_string(), AliasValue::Single("m1".to_string()));
        config
            .fidelity
            .paid_probe_daily_caps
            .insert(PROVIDER.to_string(), self.cap);
        let mut router = Router::new(Arc::new(config));
        if self.resolve_model {
            let provider: Arc<dyn Provider> = Arc::new(OkProvider {
                count_calls: AtomicUsize::new(0),
            });
            let model = ResolvedModel::new("m1", PROVIDER, provider, "claude-sonnet-4-5")
                .with_effective_row(self.effective_row.clone());
            let mut models = std::collections::BTreeMap::new();
            models.insert("m1".to_string(), Arc::new(model));
            router.install_resolved_models(models);
        }
        router
    }
}

/// The identity for `m1` on the acting lane.
fn key() -> FieldVerdictKey {
    FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity")
}

/// A distinct identity per `n`, for the multi-candidate and bound cases.
fn key_n(n: usize) -> FieldVerdictKey {
    FieldVerdictKey::new(&format!("m{n}"), GROUNDED_PATH, "anthropic-api").expect("identity")
}

/// The identity for an explicitly-named model nickname.
fn lane_key(nickname: &str) -> FieldVerdictKey {
    FieldVerdictKey::new(nickname, GROUNDED_PATH, "anthropic-api").expect("identity")
}

/// A router with TWO fully-admissible lanes and `ledger` installed, for the
/// fairness cases. Lane `m1` sits on provider `p1`, lane `m2` on `p2`.
///
/// TWO PROVIDERS rather than two models on one, so the ledger's `provider`
/// argument tells it WHICH lane it is being asked about. Without that the two
/// lanes are indistinguishable to the accounting layer, and a fairness test
/// cannot tell "reached the other lane" from "reached the same one twice" --
/// measured: a single-provider version of this fixture passed under a LIFO
/// mutation because both rotations commit the same identity.
///
/// Both lanes must RESOLVE and be fully priced: a lane refused by a precondition
/// never reaches the accounting layer and never requeues, so a starvation test
/// built on one would prove nothing about rotation.
fn two_lane_router(ledger: Arc<dyn PaidProbeLedger>) -> Router {
    let fixture = Fixture::default();
    let mut config = Config::default();
    for provider_name in [PROVIDER, SECOND_PROVIDER] {
        let mut entry = ProviderEntry::anthropic_api("literal:k");
        if let ProviderEntry::AnthropicApi { base_url, .. } = &mut entry {
            *base_url = fixture.base_url.clone();
        }
        config.providers.insert(provider_name.to_string(), entry);
        config
            .fidelity
            .paid_probe_daily_caps
            .insert(provider_name.to_string(), CAP);
    }
    for (nickname, provider_name) in TWO_LANES {
        config.models.insert(
            nickname.to_string(),
            ModelEntry::new(provider_name, "claude-sonnet-4-5"),
        );
    }
    config
        .aliases
        .insert("default".to_string(), AliasValue::Single("m1".to_string()));
    let mut router = Router::new(Arc::new(config));
    let mut models = std::collections::BTreeMap::new();
    for (nickname, provider_name) in TWO_LANES {
        let provider: Arc<dyn Provider> = Arc::new(OkProvider {
            count_calls: AtomicUsize::new(0),
        });
        models.insert(
            nickname.to_string(),
            Arc::new(
                ResolvedModel::new(nickname, provider_name, provider, "claude-sonnet-4-5")
                    .with_effective_row(fully_priced()),
            ),
        );
    }
    router.install_resolved_models(models);
    router.with_paid_probe_ledger(ledger)
}

/// A bounded payload, through the production constructor so no fixture can
/// carry a value the capture path would have refused.
fn payload() -> ProbePayload {
    ProbePayload::new(
        GROUNDED_PATH,
        "summarized".to_string(),
        &["client-flag-1".to_string()],
        &["operator-flag-1".to_string()],
        true,
    )
    .expect("a modeled display token within every retention bound")
}

/// Seed one candidate for `key` at the router's LIVE incarnation, the way an
/// exhausted free plan would have.
///
/// Asserts its own premise: a fixture that silently failed to seed would make
/// every refusal test below pass as `NoCandidate` for the wrong reason.
fn seed_candidate(router: &Router, key: &FieldVerdictKey) {
    seed_candidate_at(router, key, router.probe_incarnation());
}

/// Seed one candidate at an EXPLICIT incarnation, for the staleness case.
fn seed_candidate_at(router: &Router, key: &FieldVerdictKey, incarnation: u64) {
    let before = router.all_recorded_paid_candidates_for_tests().len();
    router
        .paid_probe_candidates
        .lock()
        .push_back(PaidProbeCandidate {
            key: key.clone(),
            incarnation,
            validator: ProbeValidator::PaidCompletion,
            payload: payload(),
        });
    assert_eq!(
        router.all_recorded_paid_candidates_for_tests().len(),
        before + 1,
        "fixture premise: the candidate must actually be on the list",
    );
}

// ---------------------------------------------------------------------------
// The positive control: a fully admissible lane authorizes exactly once
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_fully_admissible_candidate_authorizes_one_call_and_consumes_the_candidate() {
    let ledger = CountingLedger::committed();
    let router = Fixture::default().router_with_ledger(ledger.clone());
    seed_candidate(&router, &key());

    let outcome = router.authorize_paid_probe().await;

    let PaidProbeOutcome::Authorized(auth) = outcome else {
        panic!("a fully admissible candidate must authorize: {outcome:?}");
    };
    assert_eq!(auth.key(), &key());
    assert_eq!(auth.incarnation(), router.probe_incarnation());
    assert_eq!(
        auth.payload(),
        &payload(),
        "the authorization must carry the candidate's OWN payload, so the paid \
         body asks the question the admitted request posed",
    );
    assert_eq!(
        auth.seat().provider_name,
        PROVIDER,
        "the exact seat the reservation was committed for must travel with it",
    );
    assert_eq!(auth.profile().max_tokens(), LEGACY_MIN_VIABLE_MAX_TOKENS);
    assert_eq!(auth.committed().used(), 2);
    assert_eq!(auth.committed().cap(), CAP);
    assert_eq!(ledger.calls(), 1, "exactly one reservation, once");
    assert_eq!(
        ledger.seen(),
        vec![(PROVIDER.to_string(), CAP)],
        "the ledger must be handed the same provider and cap every local check \
         was taken against",
    );
    assert!(
        router.all_recorded_paid_candidates_for_tests().is_empty(),
        "an authorized candidate is CONSUMED, or a second pass would reserve a \
         second unit for one exhausted lane",
    );
}

#[tokio::test]
async fn an_empty_candidate_list_refuses_without_asking_the_ledger() {
    let ledger = CountingLedger::committed();
    let router = Fixture::default().router_with_ledger(ledger.clone());

    let outcome = router.authorize_paid_probe().await;

    assert_eq!(outcome.refusal(), Some(PaidProbeRefusal::NoCandidate));
    assert_eq!(
        ledger.calls(),
        0,
        "with nothing claimed there is nothing to reserve for",
    );
}

// The remaining groups live in sibling files to keep every file under the size
// ceiling. They compile into THIS module via `include!`, so the fixture helpers
// above stay in scope and no test's module path changes.
include!("paid_probe_authorize_claim_tests.rs");
include!("paid_probe_authorize_requeue_tests.rs");
include!("paid_probe_generation_tests.rs");
include!("paid_probe_fairness_tests.rs");
include!("paid_probe_lifecycle_tests.rs");
include!("paid_probe_hostile_claim_tests.rs");
