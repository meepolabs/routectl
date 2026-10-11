//! What a probe-request failure means for the free PLAN.
//!
//! The single question here is whether an outcome SPENT a free step. Every
//! class that did not actually put the question to the upstream must not, or a
//! lane's plan runs out on requests that never asked -- and with `count_tokens`
//! the only validator in this build, one wrongly-spent step is the whole plan.
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

/// Classify a failure of ROUTECTL'S OWN probe request.
///
/// Splits the classifier's verdict into what it means for the free PLAN: a
/// transient fault is worth re-running, and everything else this stage can
/// act on is a refusal that spends no step. Counting a 400 on a body routectl
/// built as evidence about the capability would run a lane's plan out on
/// requests that never asked the question.
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
/// later, and the default must be the SAFE side: a refusal spends no free step.
/// A default that spent a step would let a class nobody had audited run a
/// lane's plan out purely by existing.
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
        // gather, and spending the step would exhaust the plan on the strength
        // of a question the upstream refused to take.
        FailureClass::FeatureUnsupported { .. } => FreeValidatorOutcome::ProbeRequestRefused,
        // `Unknown` plus every future variant. See the fail-closed note above.
        _ => FreeValidatorOutcome::ProbeRequestRefused,
    }
}

/// The attribution tier when `err` rejects the very field a re-check carried
/// at `field_path`, as an UNSUPPORTED field, or `None` for any other failure.
///
/// Read through the same resolver the reactive repair uses, on the NATIVE
/// class, so a probe re-confirms a negative on exactly the evidence that could
/// have minted it. A refused VALUE does not re-confirm: it says the field is
/// accepted, and the negative claims it is not.
pub(super) fn rechecked_field_rejection(
    table: super::field_repair::FieldRepairTable,
    err: &routectl_core::Error,
    provider_kind: &str,
    field_path: &str,
) -> Option<routectl_core::capability::SignalTier> {
    let row = table.row_for_path(field_path)?;
    let native = routectl_core::failure_class::classify(err, Some(provider_kind)).class;
    let named = super::Router::rejection_names_field_surface(
        table,
        row.surface,
        &native,
        err,
        provider_kind,
    )?;
    (named.kind == super::rejection_parser::RejectionKind::UnsupportedField).then_some(named.tier)
}
