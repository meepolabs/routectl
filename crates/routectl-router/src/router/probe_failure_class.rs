//! What a probe-request failure means for the free PLAN.
//!
//! The single question here is whether an outcome SPENT a free step. Every
//! class that did not actually put the question to the upstream must not, or a
//! lane walks toward paid candidacy on requests that never asked -- and with
//! `count_tokens` the only executable free validator in this build, one
//! wrongly-spent step is the whole plan.
//!
//! FAILS CLOSED: the `#[non_exhaustive]` catch-all refuses, so a failure class
//! added upstream cannot spend a step by nobody's decision.

use crate::probe_scheduler::FreeValidatorOutcome;

/// [`classify_probe_failure`], for the tests whose subject IS the
/// classification.
///
/// Calls the production function directly rather than re-deriving its match,
/// so a mutation to a single arm is observable here. The unmodelled-class arm
/// in particular has no reachable upstream shape to drive it end-to-end.
#[cfg(test)]
pub(super) fn classify_probe_failure_for_tests(err: &routectl_core::Error) -> FreeValidatorOutcome {
    classify_probe_failure(err)
}

/// [`probe_outcome_for_class`], for the arms no error fixture can reach.
///
/// `FeatureUnsupported` is not in the anthropic lane's token table (it arrives
/// only through the reasoning-replay path) and the `#[non_exhaustive]`
/// catch-all has no shape at all, so driving those arms from an `Error` is
/// impossible. Calls the production mapping, so a mutation to any arm is
/// observable here.
#[cfg(test)]
pub(super) fn probe_outcome_for_class_for_tests(
    class: routectl_core::failure_class::FailureClass,
) -> FreeValidatorOutcome {
    probe_outcome_for_class(class)
}

/// What a PAID probe's failure means for the settlement stage.
///
/// Closed, and deliberately COARSER than [`routectl_core::failure_class::FailureClass`]
/// while preserving the one distinction the paid path exists to draw: a
/// capability/feature rejection is evidence ABOUT THE FIELD under test, and
/// everything else is evidence about the lane, the credential, or the load.
/// Collapsing them would force the settlement stage to replay a paid call to
/// recover what the first one already established -- and a replay is a second
/// never-refundable unit.
///
/// Derived from the SHARED classifier rather than from a second response parser:
/// this carries no upstream text, no status, and no body, so a settlement reads
/// the router's own vocabulary and a log line cannot leak an error message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PaidProbeFailure {
    /// The upstream says the capability is not supported HERE. The one class
    /// that is evidence about the FIELD, which is why it may not be folded in
    /// with the rest.
    CapabilityRejected,
    /// The credential was rejected. Says nothing about the field, and re-asking
    /// under the same credential cannot answer differently.
    AuthRejected,
    /// Rate-limited or overloaded. LOAD, which passes -- but not on this unit.
    Throttled,
    /// A server-side or transport fault. The question never got a verdict.
    Transient,
    /// Routectl's OWN request was refused as malformed, out of window, or
    /// policy-blocked. A defect in what routectl sent rather than evidence
    /// about the field.
    RequestRefused,
    /// No confident classification, including every class added upstream later.
    /// FAILS CLOSED as unclassified rather than inheriting any of the meanings
    /// above -- a class nobody audited must not read as evidence about a field.
    Unclassified,
}

impl PaidProbeFailure {
    /// Closed-set log token. Carries no upstream text, status, or body.
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::CapabilityRejected => "capability_rejected",
            Self::AuthRejected => "auth_rejected",
            Self::Throttled => "throttled",
            Self::Transient => "transient",
            Self::RequestRefused => "request_refused",
            Self::Unclassified => "unclassified",
        }
    }
}

/// Classify a PAID probe's failure into the closed settlement vocabulary.
///
/// Routes through the SAME `routectl_core::failure_class::classify` call the free
/// path uses, with the same provider kind, so the two cannot disagree about one
/// upstream rejection -- and so no second response parser exists to drift.
pub(super) fn classify_paid_probe_failure(err: &routectl_core::Error) -> PaidProbeFailure {
    paid_failure_for_class(
        routectl_core::failure_class::classify(err, Some(super::field_repair::ANTHROPIC_API_KIND))
            .class,
    )
}

/// What one failure class means for a paid settlement. ONE ARM PER VARIANT.
///
/// Every current [`routectl_core::failure_class::FailureClass`] variant has its
/// OWN arm, none grouped, and that is the point rather than verbosity: a grouped
/// arm makes several classes indistinguishable to a mutation, so removing one
/// class's decision from a group of three reds nothing. With one arm each, the
/// table below IS the mapping, and `PAID_FAILURE_TABLE` in the sidecar pins every
/// row -- so mutating any single arm to
/// [`PaidProbeFailure::Unclassified`] reds exactly that row.
///
/// THE CATCH-ALL CANNOT BE REMOVED, and its presence is not slack.
/// `FailureClass` is `#[non_exhaustive]`, so a match in this crate is refused
/// without one -- the compiler will not let any downstream crate enumerate that
/// enum. The sidecar's `paid_failure_table` and its coverage test pin every
/// variant THIS BUILD KNOWS, catching a mapping dropped, regrouped, or changed
/// within that inventory.
///
/// NEITHER THE COMPILER NOR THOSE TESTS ANNOUNCE A NEW CORE VARIANT. The
/// sidecar's enumerations are hand-written, so a variant added upstream appears
/// in none of them, stays green, and silently takes the arm below as
/// `Unclassified` -- fail-closed (never a capability verdict), but unannounced.
/// Auditing such an addition is a manual step against this file.
pub(super) fn paid_failure_for_class(
    class: routectl_core::failure_class::FailureClass,
) -> PaidProbeFailure {
    use routectl_core::failure_class::FailureClass;
    match class {
        // THE distinction the paid class exists to draw: this says the FIELD is
        // unsupported here, while every other 4xx class says the request or the
        // credential was wrong.
        FailureClass::FeatureUnsupported { .. } => PaidProbeFailure::CapabilityRejected,
        FailureClass::Auth => PaidProbeFailure::AuthRejected,
        FailureClass::RateLimited => PaidProbeFailure::Throttled,
        FailureClass::Overloaded => PaidProbeFailure::Throttled,
        FailureClass::ServerError => PaidProbeFailure::Transient,
        FailureClass::NetworkError => PaidProbeFailure::Transient,
        // Never produced by `classify` today (it is reserved for a later
        // configuration key set), and mapped anyway rather than left to the
        // catch-all: a deadline is a fault that got no verdict, which is the
        // transient reading, and recording that decision here is what keeps it
        // from silently becoming `Unclassified` the day the classifier starts
        // producing it.
        FailureClass::Timeout => PaidProbeFailure::Transient,
        FailureClass::BadRequest => PaidProbeFailure::RequestRefused,
        // Routectl's own body asked for more than the window allows. A defect in
        // what this build sent, not evidence about the field -- so it reads with
        // the malformed class rather than with the capability one.
        FailureClass::ContextWindow => PaidProbeFailure::RequestRefused,
        // The probe's own minimal turn was policy-blocked. Also a property of
        // what routectl sent, and deliberately NOT a capability verdict: the
        // field under test is an envelope field, so a content refusal says
        // nothing about whether the upstream supports it.
        FailureClass::ContentPolicy => PaidProbeFailure::RequestRefused,
        FailureClass::Unknown => PaidProbeFailure::Unclassified,
        // REQUIRED by `#[non_exhaustive]`, and fail-closed by choice: a class
        // this build has not audited must not read as a capability verdict.
        _ => PaidProbeFailure::Unclassified,
    }
}

/// [`paid_failure_for_class`], for the arms no error fixture can reach.
///
/// `Timeout` is never produced by `classify`, the anthropic token table carries
/// an empty `feature_unsupported` set, and the `#[non_exhaustive]` catch-all has
/// no shape at all -- so driving those arms from an `Error` is impossible. Calls
/// the production mapping, so a mutation to any arm is observable here.
#[cfg(test)]
pub(super) fn paid_failure_for_class_for_tests(
    class: routectl_core::failure_class::FailureClass,
) -> PaidProbeFailure {
    paid_failure_for_class(class)
}

/// Classify a failure of ROUTECTL'S OWN probe request.
///
/// Splits the classifier's verdict into what it means for the free PLAN: a
/// transient fault is worth re-running, and everything else this stage can
/// act on is a refusal that spends no step. Counting a 400 on a body routectl
/// built as evidence about the capability would walk a lane toward paid
/// eligibility on requests that never asked the question.
pub(super) fn classify_probe_failure(err: &routectl_core::Error) -> FreeValidatorOutcome {
    probe_outcome_for_class(
        routectl_core::failure_class::classify(err, Some(super::field_repair::ANTHROPIC_API_KIND))
            .class,
    )
}

/// What one failure class means for the free plan.
///
/// FAILS CLOSED on anything this build does not model.
/// [`routectl_core::failure_class::FailureClass`] is
/// `#[non_exhaustive]`, so the catch-all here receives every variant added
/// later, and the default must be the SAFE side of the paid boundary: a
/// refusal spends no free step and advances nothing toward paid candidacy. A
/// default that spent a step would let a class nobody had audited walk a lane
/// to the paid class purely by existing.
pub(super) fn probe_outcome_for_class(
    class: routectl_core::failure_class::FailureClass,
) -> FreeValidatorOutcome {
    use routectl_core::failure_class::FailureClass;
    match class {
        FailureClass::RateLimited
        | FailureClass::Overloaded
        | FailureClass::Timeout
        | FailureClass::ServerError
        | FailureClass::NetworkError => FreeValidatorOutcome::Transient,
        FailureClass::BadRequest
        | FailureClass::Auth
        | FailureClass::ContextWindow
        | FailureClass::ContentPolicy => FreeValidatorOutcome::ProbeRequestRefused,
        // The upstream says the capability is not supported HERE. That is an
        // answer about the lane, but not one this stage may spend a step on:
        // a lane whose `count_tokens` is unsupported has no free evidence to
        // gather, and spending the step would exhaust the plan and make the
        // lane a PAID candidate on the strength of a question the upstream
        // refused to take.
        FailureClass::FeatureUnsupported { .. } => FreeValidatorOutcome::ProbeRequestRefused,
        // `Unknown` plus every future variant. See the fail-closed note above.
        _ => FreeValidatorOutcome::ProbeRequestRefused,
    }
}
