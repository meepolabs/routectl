//! The live wire check of a successful attempt's request leg: compare what
//! the client sent with what the egress deposited into the attempt's
//! [`WireTap`], and record the outcome on the router's [`WireCheckState`].
//!
//! The check reads only. It runs inline after the provider call returned,
//! returns `()`, and never touches the response, its status or its error. An
//! unadmitted success pays one sampler decision and nothing else: no body is
//! parsed or cloned before [`WireCheckState::admit`] says yes.

use routectl_core::ChatRequest;
use routectl_core::wire_check::{
    AttemptRecord, DeclaredEdit, DiffOutcome, EgressKind, Lane, LaneClass, Leg, WireTap, diff,
};
use serde_json::Value;

use crate::config::{ProviderEntry, WireCheckConfig};
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

/// Give this provider call a fresh [`WireTap`] and return the router's
/// handle on it. A request with no captured ingress body has nothing to
/// compare against, so it gets no tap and its egress records nothing.
///
/// Called once per provider call: clones share one slot, so a tap reused
/// across calls would let an earlier attempt's deposit stand in for a later
/// attempt that deposited nothing.
pub(super) fn install_wire_tap(attempt_req: &mut ChatRequest) -> Option<WireTap> {
    attempt_req.routectl_internal.ingress_wire.as_ref()?;
    let tap = WireTap::default();
    attempt_req.routectl_internal.wire_tap = Some(tap.clone());
    Some(tap)
}

impl Router {
    /// Check the request leg of a successful attempt on `target` when the
    /// sampler admits it.
    pub(super) fn check_request_wire(
        &self,
        target: &DispatchTarget,
        attempt_req: &ChatRequest,
        tap: Option<&WireTap>,
    ) {
        let (Some(tap), Some(egress)) = (tap, target.egress_kind) else {
            return;
        };
        check_request_leg(
            &self.wire_check,
            &self.config.wire_check,
            egress,
            attempt_req,
            tap,
        );
    }
}

fn check_request_leg(
    state: &WireCheckState,
    config: &WireCheckConfig,
    egress: EgressKind,
    attempt_req: &ChatRequest,
    tap: &WireTap,
) {
    let Some(ingress) = attempt_req.routectl_internal.ingress_wire.as_ref() else {
        return;
    };
    let lane = Lane::of(ingress.dialect, egress);
    if !state.admit(lane, ingress.client_version.as_deref(), config) {
        return;
    }
    let contents = tap.take();
    let edits: Vec<DeclaredEdit> = attempt_req
        .routectl_internal
        .declared_edits
        .iter()
        .chain(contents.edits(Leg::Request))
        .cloned()
        .collect();
    let record = AttemptRecord::new(ingress, attempt_req, contents.request(), &edits);
    compare_request(state, lane, &record);
}

/// Diff the request leg and record the outcome. A missing or unparseable
/// body on either side is `incomplete`, never `clean`.
fn compare_request(state: &WireCheckState, lane: Lane, record: &AttemptRecord<'_>) {
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
