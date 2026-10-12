//! The live wire check of a successful attempt's request leg: compare what
//! the client sent with what the egress deposited into the attempt's
//! [`WireTap`], and record the outcome on the router's [`WireCheckState`].
//!
//! The check reads only. It runs inline after the provider call returned,
//! returns `()`, and never touches the response, its status or its error. An
//! unadmitted attempt pays one sampler decision at tap install and nothing
//! else: no body is parsed or cloned unless [`WireCheckState::admit`] said
//! yes.

use std::sync::Arc;

use routectl_core::ChatRequest;
use routectl_core::wire_check::{
    AttemptRecord, DeclaredEdit, DiffOutcome, EgressKind, Lane, LaneClass, Leg, WireTap, diff,
};
use serde_json::Value;

use crate::config::ProviderEntry;
use crate::wire_check_state::WireCheckState;

use super::{DispatchTarget, Router};

/// The egress kind a provider entry sends with. Bedrock splits by API shape
/// and every Mantle sub-lane is its own kind, because each sends a different
/// wire body than its standard sibling.
pub(super) const fn egress_kind_of(entry: &ProviderEntry) -> EgressKind {
    match entry {
        #[cfg(feature = "bedrock")]
        ProviderEntry::AnthropicApi {
            bedrock_mantle: Some(_),
            ..
        } => EgressKind::AnthropicApiMantle,
        ProviderEntry::AnthropicApi { .. } => EgressKind::AnthropicApi,
        #[cfg(feature = "bedrock")]
        ProviderEntry::OpenaiCompat {
            bedrock_mantle: Some(_),
            ..
        } => EgressKind::OpenaiCompatMantle,
        ProviderEntry::OpenaiCompat { .. } => EgressKind::OpenaiCompat,
        #[cfg(all(feature = "openai-responses", feature = "bedrock"))]
        ProviderEntry::OpenaiResponses {
            bedrock_mantle: Some(_),
            ..
        } => EgressKind::OpenaiResponsesMantle,
        #[cfg(feature = "openai-responses")]
        ProviderEntry::OpenaiResponses { .. } => EgressKind::OpenaiResponses,
        #[cfg(feature = "bedrock")]
        ProviderEntry::Bedrock { api_shape, .. } => match api_shape {
            crate::config::BedrockApiShapeConfig::Invoke => EgressKind::BedrockInvoke,
            crate::config::BedrockApiShapeConfig::Converse => EgressKind::BedrockConverse,
        },
        #[cfg(feature = "gemini")]
        ProviderEntry::Gemini { .. } => EgressKind::Gemini,
    }
}

impl Router {
    /// Give this provider call a fresh [`WireTap`] and return the router's
    /// handle on it, with the sampler charge the attempt took. A request with
    /// no captured ingress body has nothing to compare against, so it gets no
    /// tap, takes no charge, and its egress records nothing.
    ///
    /// The sampler decides here, before the call, whether this attempt gets
    /// a full check, and the decision rides the tap so the provider can skip
    /// work only an admitted attempt reads. On a FIDELITY lane an admitted
    /// attempt checks its response too; a TRANSLATION lane's response is a
    /// different dialect from the upstream's, so only its request is
    /// checked. Each attempt is charged exactly
    /// once; a target with no known egress kind is never admitted and takes
    /// no charge. The charge is given back when the returned [`SampleCharge`]
    /// drops, unless the attempt succeeded and called [`SampleCharge::keep`].
    ///
    /// Called once per provider call: clones share one slot, so a tap reused
    /// across calls would let an earlier attempt's deposit stand in for a later
    /// attempt that deposited nothing.
    pub(super) fn install_wire_tap(
        &self,
        target: &DispatchTarget,
        attempt_req: &mut ChatRequest,
    ) -> (Option<WireTap>, SampleCharge) {
        let Some(ingress) = attempt_req.routectl_internal.ingress_wire.as_ref() else {
            return (None, SampleCharge(None));
        };
        let charge = target.egress_kind.map(|egress| {
            let lane = Lane::of(ingress.dialect, egress);
            let client_version = ingress.client_version.as_deref();
            let admitted = self
                .wire_check
                .admit(lane, client_version, &self.config.wire_check);
            let charged = Charged {
                state: Arc::clone(&self.wire_check),
                lane,
                client_version: client_version.map(Box::from),
            };
            (charged, admitted.then_some(lane))
        });
        let (charge, admitted) = charge.map_or((None, None), |(charged, admitted)| {
            (Some(charged), admitted)
        });
        let tap = match admitted {
            Some(lane) if lane.class() == LaneClass::Fidelity => {
                WireTap::checking_response(lane.ingress())
            }
            Some(_) => WireTap::admitted(),
            None => WireTap::default(),
        };
        #[cfg(any(test, feature = "test-utils"))]
        let tap = match self.wire_tap_drop {
            Some(pointer) => tap.with_drop_on_deposit(pointer),
            None => tap,
        };
        attempt_req.routectl_internal.wire_tap = Some(tap.clone());
        (Some(tap), SampleCharge(charge))
    }

    /// Check the request leg of a successful attempt on `target` when its
    /// tap was admitted at install.
    pub(super) fn check_request_wire(
        &self,
        target: &DispatchTarget,
        attempt_req: &ChatRequest,
        tap: Option<&WireTap>,
    ) {
        let Some(tap) = tap.filter(|tap| tap.is_admitted()) else {
            return;
        };
        let Some(lane) = attempt_lane(target, attempt_req) else {
            return;
        };
        compare_request_leg(&self.wire_check, lane, attempt_req, tap);
    }
}

/// One attempt's charge against the success sampler. Only successful
/// attempts count toward first-N and one-in-M, so the charge is given back
/// when this drops -- on a failed attempt and on one whose future is
/// dropped before the provider answers alike -- unless [`Self::keep`] was
/// called. Held by the dispatch loop only, never by the tap, so each charge
/// is given back at most once.
#[must_use = "dropping the charge gives it back; a successful attempt calls `keep`"]
#[derive(Debug)]
pub(super) struct SampleCharge(Option<Charged>);

#[derive(Debug)]
struct Charged {
    state: Arc<WireCheckState>,
    lane: Lane,
    client_version: Option<Box<str>>,
}

impl SampleCharge {
    /// Keep the charge: the attempt succeeded, and a success spends its
    /// sampler slot whether or not it was admitted.
    pub(super) fn keep(mut self) {
        self.0 = None;
    }
}

impl Drop for SampleCharge {
    fn drop(&mut self) {
        if let Some(charged) = self.0.take() {
            charged
                .state
                .release(charged.lane, charged.client_version.as_deref());
        }
    }
}

/// The lane an attempt on `target` travels: the client's ingress dialect and
/// the target's egress kind. `None` when either is unknown.
pub(super) fn attempt_lane(target: &DispatchTarget, attempt_req: &ChatRequest) -> Option<Lane> {
    let ingress = attempt_req.routectl_internal.ingress_wire.as_ref()?;
    Some(Lane::of(ingress.dialect, target.egress_kind?))
}

/// Build the request-leg record from the tap's request side and record the
/// outcome on `lane`. Never consults the sampler: the caller has already
/// decided this attempt is checked. The response side of the tap stays for
/// its own check.
pub(super) fn compare_request_leg(
    state: &WireCheckState,
    lane: Lane,
    attempt_req: &ChatRequest,
    tap: &WireTap,
) {
    lend_request_record(attempt_req, tap, |record| match record {
        Some(record) => compare_request(state, lane, &record),
        None => state.record_incomplete(lane),
    });
}

/// Drain the tap's request side, build the attempt's request-leg record and
/// lend it to `use_record` before every body it borrows is dropped. `None`
/// when the request carries no ingress body to build a record from.
pub(super) fn lend_request_record<R>(
    attempt_req: &ChatRequest,
    tap: &WireTap,
    use_record: impl FnOnce(Option<AttemptRecord<'_>>) -> R,
) -> R {
    let Some(ingress) = attempt_req.routectl_internal.ingress_wire.as_ref() else {
        return use_record(None);
    };
    let contents = tap.take_request();
    let edits: Vec<DeclaredEdit> = attempt_req
        .routectl_internal
        .declared_edits
        .iter()
        .chain(contents.edits(Leg::Request))
        .cloned()
        .collect();
    use_record(Some(AttemptRecord::new(
        ingress,
        attempt_req,
        contents.request(),
        &edits,
    )))
}

/// Diff the request leg and record the outcome. A missing or unparseable
/// body on either side is `incomplete`, never `clean`.
pub(super) fn compare_request(state: &WireCheckState, lane: Lane, record: &AttemptRecord<'_>) {
    let Some(sent) = record
        .egress()
        .and_then(|body| serde_json::from_slice::<Value>(body).ok())
    else {
        state.record_incomplete(lane);
        return;
    };
    let edits = record.edits(Leg::Request);
    let outcome = match lane.class() {
        LaneClass::Fidelity => {
            let Ok(received) = serde_json::from_slice::<Value>(&record.ingress().bytes) else {
                state.record_incomplete(lane);
                return;
            };
            diff(&received, &sent, LaneClass::Fidelity, edits)
        }
        LaneClass::Translation => record
            .canonical()
            .provider_extras
            .as_ref()
            .map_or_else(DiffOutcome::default, |extras| {
                diff(extras, &sent, LaneClass::Translation, edits)
            }),
    };
    state.record_compared(
        lane,
        Leg::Request,
        outcome.findings.iter().map(|f| (&f.path, f.class)),
        outcome.truncated,
    );
}

#[cfg(test)]
#[path = "wire_check_tests.rs"]
mod wire_check_tests;
