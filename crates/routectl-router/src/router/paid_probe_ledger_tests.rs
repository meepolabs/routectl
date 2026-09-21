use super::*;

use std::sync::atomic::AtomicUsize;

use parking_lot::Mutex;

use crate::config::Config;

/// Counting ledger double. Records every reservation call it is asked for
/// and answers with a fixed outcome, so a test can distinguish "the router
/// asked the actor" from "the router answered on its own" -- the one
/// distinction the fail-closed contract rests on.
struct CountingLedger {
    calls: AtomicUsize,
    seen: Mutex<Vec<(String, u32)>>,
    answer: PaidProbeReservation,
}

impl CountingLedger {
    fn answering(answer: PaidProbeReservation) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
            answer,
        })
    }

    fn committed() -> Arc<Self> {
        Self::answering(PaidProbeReservation::Committed { used: 1, cap: 5 })
    }

    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::Acquire)
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
        self.calls.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.seen.lock().push((provider.to_string(), daily_cap));
        self.answer.clone()
    }
}

fn bare_router() -> Router {
    Router::new(Arc::new(Config::default()))
}

/// Every refusal variant, so a test asserting "nothing but a commit permits
/// a dial" enumerates the closed set rather than sampling it. Written as an
/// exhaustive `match` so a new variant cannot be added without landing here.
fn every_refusal() -> Vec<PaidProbeReservation> {
    let all = vec![
        PaidProbeReservation::CapExhausted,
        PaidProbeReservation::MalformedState,
        PaidProbeReservation::WriteFailed,
        PaidProbeReservation::Overloaded,
        PaidProbeReservation::Unavailable,
    ];
    for outcome in &all {
        match outcome {
            PaidProbeReservation::CapExhausted
            | PaidProbeReservation::MalformedState
            | PaidProbeReservation::WriteFailed
            | PaidProbeReservation::Overloaded
            | PaidProbeReservation::Unavailable => {}
            PaidProbeReservation::Committed { .. } => {
                panic!("the refusal set must not contain a commit")
            }
        }
    }
    all
}

/// A Router built by the ordinary constructor holds NO ledger, and therefore
/// refuses -- at every cap value, including a cap an operator could not
/// plausibly exceed.
///
/// The cap sweep is what makes this more than a `None` check: a permissive
/// absent arm would read the cap and commit, so a fixture pinned to cap zero
/// could not tell the fail-closed answer from a cap refusal.
#[tokio::test]
async fn absent_ledger_refuses_at_every_cap() {
    let router = bare_router();

    assert!(
        router.paid_probe_ledger().is_none(),
        "the default Router must carry no ledger",
    );
    for cap in [0_u32, 1, 7, u32::MAX] {
        let outcome = router.reserve_paid_probe_unit("anthropic", cap).await;
        assert_eq!(
            outcome,
            PaidProbeReservation::Unavailable,
            "an absent ledger must answer unavailable at cap {cap}",
        );
        assert!(
            !outcome.permits_paid_call(),
            "an absent ledger must never permit a paid call",
        );
    }
}

/// An INSTALLED ledger is the only thing that can answer, and the router
/// hands it exactly the configured provider identity and cap it was asked
/// with -- unmodified, once per reservation.
#[tokio::test]
async fn installed_ledger_answers_and_is_asked_once_per_reservation() {
    let ledger = CountingLedger::committed();
    let router = bare_router().with_paid_probe_ledger(ledger.clone());

    let outcome = router.reserve_paid_probe_unit("anthropic", 5).await;

    assert_eq!(outcome, PaidProbeReservation::Committed { used: 1, cap: 5 },);
    assert!(outcome.permits_paid_call());
    assert_eq!(ledger.calls(), 1, "exactly one reservation was asked for");
    assert_eq!(ledger.seen(), vec![("anthropic".to_string(), 5)]);
}

/// A refusal from the installed ledger is passed through unchanged and
/// permits nothing. The router owns no retry, no downgrade, and no second
/// opinion: there is no refund or release method to recover with, so a
/// refusal it softened would be spend the operator never authorized.
#[tokio::test]
async fn installed_ledger_refusals_pass_through_and_permit_nothing() {
    for refusal in every_refusal() {
        let ledger = CountingLedger::answering(refusal.clone());
        let router = bare_router().with_paid_probe_ledger(ledger.clone());

        let outcome = router.reserve_paid_probe_unit("anthropic", 5).await;

        assert_eq!(outcome, refusal, "the refusal must reach the caller intact");
        assert!(
            !outcome.permits_paid_call(),
            "{} must not permit a paid call",
            refusal.as_str(),
        );
        assert_eq!(ledger.calls(), 1);
    }
}

/// A commit permits the dial; nothing else does. Paired with
/// `every_refusal` above so the permissive side is asserted against the
/// same closed set the refusing side is.
#[test]
fn only_a_commit_permits_a_paid_call() {
    assert!(
        PaidProbeReservation::Committed { used: 3, cap: 3 }.permits_paid_call(),
        "a commit must permit the dial it was reserved for",
    );
    for refusal in every_refusal() {
        assert!(!refusal.permits_paid_call(), "{}", refusal.as_str());
    }
}

/// Each outcome carries its own crate-internal log token, all distinct, and
/// the set spans EVERY public variant.
///
/// Distinctness is the assertion that matters: two outcomes sharing a token
/// would make the accounting-health line unable to say which refusal a lane is
/// sitting on. The count is asserted alongside it so the test states its own
/// extent -- `every_refusal` is exhaustively matched, so a variant added
/// without a token cannot slip past both.
#[test]
fn every_outcome_has_a_distinct_log_token() {
    let mut tokens = vec![PaidProbeReservation::Committed { used: 1, cap: 1 }.as_str()];
    tokens.extend(every_refusal().iter().map(PaidProbeReservation::as_str));

    assert_eq!(
        tokens.len(),
        6,
        "one token per public variant: commit plus five refusals",
    );
    let distinct: std::collections::BTreeSet<&str> = tokens.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        tokens.len(),
        "every reservation outcome needs its own token: {tokens:?}",
    );
    assert!(
        tokens.contains(&"overloaded"),
        "saturation is reported mechanism-neutrally: {tokens:?}",
    );
}

/// Saturation of the accounting layer authorizes NOTHING.
///
/// Covered by the refusal sweep above too, and named separately because it is
/// the one refusal whose condition is transient: a caller tempted to treat
/// "try again later" as "proceed for now" would spend against a budget nobody
/// checked, and no refund exists to undo it.
#[test]
fn an_overloaded_accounting_layer_permits_nothing() {
    let overloaded = PaidProbeReservation::Overloaded;

    assert!(!overloaded.permits_paid_call());
    assert_eq!(overloaded.as_str(), "overloaded");
}

/// A hot reload ATTACHES the outgoing ledger to the replacement Router, by
/// pointer identity -- so the replacement reserves against the SAME actor
/// seam, and the reload site needs no writer handle in scope to preserve it.
///
/// Pointer identity rather than presence: a replacement holding a different
/// installation would satisfy an `is_some()` check while reserving against
/// something else, which for a never-refunded day counter is a second budget.
#[tokio::test]
async fn carry_over_attaches_the_same_ledger_instance() {
    let ledger = CountingLedger::committed();
    let before = bare_router().with_paid_probe_ledger(ledger.clone());
    let mut after = bare_router();

    after.carry_over_learned_from(&before);

    let carried = after
        .paid_probe_ledger()
        .expect("the replacement Router must carry the outgoing ledger");
    let installed: Arc<dyn PaidProbeLedger> = ledger.clone();
    assert!(
        Arc::ptr_eq(carried, &installed),
        "the replacement must share the outgoing ledger Arc, not a second installation",
    );

    before.reserve_paid_probe_unit("anthropic", 5).await;
    after.reserve_paid_probe_unit("anthropic", 5).await;
    assert_eq!(
        ledger.calls(),
        2,
        "both Router generations must reserve against the one actor seam",
    );
}

/// A carry-over from a Router with NO ledger leaves the replacement fail
/// closed rather than inheriting an absence as permission.
#[tokio::test]
async fn carry_over_from_a_ledgerless_router_stays_fail_closed() {
    let before = bare_router();
    let mut after = bare_router();

    after.carry_over_learned_from(&before);

    assert!(after.paid_probe_ledger().is_none());
    assert_eq!(
        after.reserve_paid_probe_unit("anthropic", 5).await,
        PaidProbeReservation::Unavailable,
    );
}

/// SOURCE-TEXT guard over the Router half of the seam: the router itself must
/// not be able to mint a commit.
///
/// The behavioral tests above pin what the absent path RETURNS today; this
/// pins WHY it cannot return anything else -- the only producer of a commit is
/// an implementation of the trait, never the router's own fail-closed arm.
/// Without it, a change answering `Committed` with no ledger installed would be
/// caught only by a test whose expected value the same pass could edit.
///
/// Scanned region is the `impl Router` block alone: the enum's own inherent
/// methods legitimately MATCH on the commit variant, so scanning the whole file
/// could only be satisfied by forbidding the match too. The module's tests are
/// a sidecar (this file), so nothing test-side is in the scanned text.
#[test]
fn the_router_half_of_the_seam_never_constructs_a_commit() {
    let source = include_str!("paid_probe_ledger.rs");
    let (_, router_half) = source
        .split_once("impl Router {")
        .expect("the seam module must carry exactly one `impl Router` block");
    assert!(
        !router_half.contains("impl Router {"),
        "an ambiguous cut: more than one `impl Router` block",
    );
    let code: String = router_half
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        !code.contains("Committed"),
        "only a ledger implementation may construct a committed reservation",
    );
    assert!(
        code.contains("PaidProbeReservation::Unavailable"),
        "the fail-closed arm must name the unavailable outcome",
    );
}
