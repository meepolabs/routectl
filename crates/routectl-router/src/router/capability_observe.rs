//! Response-evidence observation on a successful dispatch: the read-only
//! mirror of [`Router::observe_for_learning`].
//!
//! Where the learn path reads a dispatch ERROR arm and mints negatives, this
//! path reads a successful response and admits positive (VerifiedWorking)
//! and suspected-absence (F3) observations. A successful attempt yields an
//! [`EvidenceScope`]; [`Router::observe_response_evidence`] reads a response
//! against it, under an [`EvidencePolicy`]. Both paths co-locate the kill switch
//! (`config.capability.enabled`), per-request dedupe, the registry, and the
//! WARN/counter observability floor, reusing the shipped pattern.
//!
//! Two-stage contract: live-only detection (stage one) is the pure
//! [`crate::capability_detect`] slice; admission (stage two, `now`-
//! parameterized, `EvidenceSource::Live`) happens here and is EXACTLY what a
//! later warm-rebuild replays. No ledger write -- acting observations ride out
//! as [`CapabilityObserveEvent`]s (on [`super::DispatchMeta`] for a complete
//! dispatch) as the in-memory persistence hook, mirroring
//! [`super::CapabilityLearnEvent`].
//!
//! Log hygiene: only the closed-set capability key, the pinned evidence-class
//! token, and the routing state key reach the log -- never a request body,
//! prompt, or response text.

use std::collections::HashSet;
use std::time::Instant;

use routectl_core::ToolDef;
use routectl_core::capability::{
    EvidenceSource, FailurePhase, STRUCTURED_OUTPUT, SignalTier, WEB_SEARCH,
};
use routectl_core::{ChatRequest, ChatResponse, ReasoningConfig};
use routectl_core::{TranslationReason, TranslationReport};
use serde_json::Value;

use super::{DispatchTarget, Router};
use crate::capability_detect::{
    self, CapabilityObservation, DetectorContext, ObservationDirection,
};
use crate::feature_keys::ToolChoiceForce;
use crate::learned_capability::{GenerationOutcome, ObserveOutcome, PositiveOutcome};

/// Install a fresh, empty translation report on the request about to go
/// upstream and return the handle the router reads after the call.
///
/// Called once per provider call, as the beta repair report is: clones share
/// one slot, so a slot reused across calls would carry an earlier attempt's
/// withhold into a later attempt whose egress sent the tool.
pub(super) fn install_translation_report(attempt_req: &mut ChatRequest) -> TranslationReport {
    let report = TranslationReport::default();
    attempt_req.routectl_internal.translation_report = Some(report.clone());
    report
}

/// The capabilities one attempt's egress did not carry onto the wire, read
/// once after the call returns. The upstream answered a request without
/// them, so that attempt's success or rejection is no evidence about them,
/// and neither learner takes any.
#[derive(Debug, Clone, Default)]
pub(super) struct UntranslatedCapabilities(Vec<TranslationReason>);

impl UntranslatedCapabilities {
    /// Drain `report` into the attempt's untranslated set.
    pub(super) fn take_from(report: &TranslationReport) -> Self {
        Self(report.take())
    }

    /// Whether the attempt's egress withheld or refused `capability_key`.
    pub(super) fn names(&self, capability_key: &str) -> bool {
        self.0.iter().any(|reason| match reason {
            TranslationReason::HostedToolWithheld(key)
            | TranslationReason::TranslationRejected(key) => key.as_str() == capability_key,
        })
    }
}

/// A single acting response-evidence observation, riding out (on
/// [`super::DispatchMeta`] for a complete dispatch) to the usage-capture
/// layer. The router does not depend on the ledger writer, so
/// observations travel on the dispatch meta rather than being written here.
///
/// Carries the columns a later warm-rebuild replays (stage two): the
/// capability key, the pinned evidence class, the observation direction,
/// tier, and source, plus the learned-registry key, `provider_kind`, and the
/// request's derived feature set. No request body, prompt, or response text
/// ever enters this struct.
#[derive(Debug, Clone)]
pub struct CapabilityObserveEvent {
    /// The EFFECTIVE persistence generation this event must be stamped with.
    ///
    /// Taken from the registry operation that produced the event, atomically
    /// under the same guard as its read or mutation -- never sampled before or
    /// after. A separate read could be taken across a boundary and stamp the
    /// event with a generation that does not describe the state it reports. A
    /// single request legitimately spans a boundary, so events on one request
    /// may carry DIFFERENT generations.
    pub persistence_generation: u64,
    /// The INCARNATION of the key's state this event describes, from the same
    /// guarded mutation.
    ///
    /// What the generation cannot express: a purge and a later relearn of ONE key
    /// both happen inside one generation, so a stale event queued before the
    /// purge and a genuine post-purge relearn are indistinguishable by generation
    /// alone. The writer compares this against the key's purge floor and drops
    /// only the superseded one.
    pub incarnation: u64,

    /// Learned-registry key of the served target: its serialized lane
    /// (`provider_entry#upstream`), persisted verbatim as the ledger
    /// `lane_key`.
    pub state_key: String,
    /// Canonical capability key the observation attests to.
    pub capability_key: String,
    /// Stable provider-kind token of the served target.
    pub provider_kind: String,
    /// Pinned evidence-class token attributing the observation.
    pub evidence_class: String,
    /// Which side of the capability the observation attests to.
    pub direction: ObservationDirection,
    /// Confidence tier of the observation.
    pub signal_tier: SignalTier,
    /// Whether the evidence came from live traffic or an out-of-band probe.
    /// Fixed to `Live` today.
    pub source: EvidenceSource,
    /// The request's derived feature set at observation time. Replay
    /// verifies the observed capability was actually in flight.
    pub request_features: Vec<String>,
}

/// What a successful attempt hands the response-evidence path: an owned
/// snapshot of everything a detector run and its admission need, taken when
/// the attempt succeeded, so the evidence can be read later (a stream's
/// terminal point) without holding the request or the dispatch target.
///
/// Carries the router generation it was taken under. Admission submits that
/// generation, so a scope that outlived a reload which dropped the catalog it
/// describes admits nothing, whichever Router reads it.
#[derive(Debug, Clone)]
pub struct EvidenceScope {
    /// The served target's routing state key (the WARN's `state_key`).
    state_key: String,
    /// Learned-registry key per detectable capability; a capability absent
    /// here has no lane on the served target and is never admitted.
    learned_keys: Vec<(&'static str, String)>,
    provider_kind: &'static str,
    request_features: Vec<String>,
    detector_context: DetectorContext,
    untranslated: UntranslatedCapabilities,
    generation: u64,
}

/// How much response evidence a reader may admit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvidencePolicy {
    /// Admit suspected-absence (F3) observations as well as verified ones.
    /// A reader that may have missed an evidence shape sets this false, so a
    /// gap in its reading cannot become a false negative.
    pub admit_suspect: bool,
}

impl Router {
    /// The evidence scope of a successful attempt on `target`, or `None` when
    /// no response evidence may be read for it: the kill switch is off
    /// (`!config.capability.enabled`), the target dispatches on a forwarded
    /// credential (the wire model is the client's, so the target's lane does
    /// not describe what served; the learn path gates its negatives the same
    /// way), or the target carries no provider kind (a legacy or
    /// direct-construction target, fail closed as the learn path does).
    pub(super) fn evidence_scope(
        &self,
        req: &ChatRequest,
        target: &DispatchTarget,
        untranslated: &UntranslatedCapabilities,
    ) -> Option<EvidenceScope> {
        if !self.config.capability.enabled || target.use_forwarded_credential {
            return None;
        }
        let provider_kind = target.provider_kind?;
        let request_features =
            super::field_repair::request_feature_keys(self.field_repair_table(), req);
        let detector_context = detector_context(req, &request_features);
        let learned_keys = capability_detect::DETECTABLE_CAPABILITIES
            .iter()
            .filter_map(|&key| target.learned_key(key).map(|lane| (key, lane.to_string())))
            .collect();
        Some(EvidenceScope {
            state_key: target.state_key.clone(),
            learned_keys,
            provider_kind,
            request_features,
            detector_context,
            untranslated: untranslated.clone(),
            generation: self.registry_generation(),
        })
    }

    /// Observe response evidence for a successful attempt: the read-only
    /// mirror of the error-arm learn observer. Runs the pure detectors
    /// over `resp`, admits each observation `policy` allows into the registry
    /// (`Live` source, at the scope's generation), and pushes an acting
    /// observation onto `events`.
    ///
    /// An observation about a capability the attempt's egress kept off the
    /// wire is dropped: the response says nothing about it.
    pub fn observe_response_evidence(
        &self,
        scope: &EvidenceScope,
        resp: &ChatResponse,
        policy: EvidencePolicy,
        events: &mut Vec<CapabilityObserveEvent>,
    ) {
        self.observe_response_evidence_at(scope, resp, policy, events, Instant::now());
    }

    /// [`Self::observe_response_evidence`] at an explicit `now`.
    pub(super) fn observe_response_evidence_at(
        &self,
        scope: &EvidenceScope,
        resp: &ChatResponse,
        policy: EvidencePolicy,
        events: &mut Vec<CapabilityObserveEvent>,
        now: Instant,
    ) {
        let observations = capability_detect::detect(&scope.detector_context, resp);
        // One observation per capability per scope: a scope covers one served
        // target, so the capability key alone dedupes.
        let mut dedupe: HashSet<&'static str> = HashSet::new();
        for obs in observations {
            if !policy.admit_suspect && obs.direction == ObservationDirection::SuspectAbsence {
                continue;
            }
            if scope.untranslated.names(obs.capability_key) || !dedupe.insert(obs.capability_key) {
                continue;
            }
            let Some(learned_key) = scope.learned_key(obs.capability_key) else {
                continue;
            };
            self.admit_observation(&obs, learned_key, scope, events, now);
        }
    }

    /// The pre-scope entry the observer tests drive: scope the attempt, then
    /// read its response with suspects admitted, as the complete path does.
    #[cfg(test)]
    pub(super) fn observe_capabilities(
        &self,
        req: &ChatRequest,
        resp: &ChatResponse,
        target: &DispatchTarget,
        untranslated: &UntranslatedCapabilities,
        meta: &mut super::DispatchMeta,
        now: Instant,
    ) {
        let Some(scope) = self.evidence_scope(req, target, untranslated) else {
            return;
        };
        self.observe_response_evidence_at(
            &scope,
            resp,
            EvidencePolicy {
                admit_suspect: true,
            },
            &mut meta.capability_observations,
            now,
        );
    }

    /// Admit one observation into the registry (stage two) and, when it acts,
    /// emit the structured WARN, bump the dedicated counter, and ride the
    /// observation out on `events`. A VerifiedWorking positive acts on its first
    /// observation; a suspected-absence F3 negative acts only once corroborated
    /// within the inferred window (a passive positive suppressed by a resident
    /// negative, or a still-pending single inferred observation, does neither).
    /// A same-verdict refresh of a resident positive bumps the counter only:
    /// no WARN and no ride-along, so it never reaches the ledger.
    fn admit_observation(
        &self,
        obs: &CapabilityObservation,
        learned_key: &str,
        scope: &EvidenceScope,
        events: &mut Vec<CapabilityObserveEvent>,
        now: Instant,
    ) {
        let provider_kind = scope.provider_kind;
        // Both arms submit the scope's generation through the barrier. A
        // `Stale` outcome means the scope was taken under a superseded
        // generation and the key is catalog-scoped: the observation is neither
        // recorded nor counted, and nothing rides out on `events`, so no ledger
        // event is produced either. That is what stops a request that began on
        // the pre-swap Router from repopulating the catalog-scoped state the
        // boundary just evicted.
        let acting = match obs.direction {
            ObservationDirection::Verified => {
                let outcome = self.observe_verified_capability_in(
                    scope.generation,
                    learned_key,
                    obs.capability_key,
                    provider_kind,
                    EvidenceSource::Live,
                    // Retained on the resident entry, not merely logged: the
                    // warm rebuild fails closed on a `verified` row without a
                    // recognized class, so an entry that cannot restate its
                    // class would be evicted at the next boundary.
                    Some(obs.evidence_class),
                    now,
                );
                match outcome {
                    GenerationOutcome::Applied {
                        value: value @ (PositiveOutcome::Recorded | PositiveOutcome::Refreshed),
                        generation,
                        incarnation,
                    } => {
                        self.metrics.incr_verified_working();
                        // Only a verdict transition produces an event. A
                        // same-verdict refresh restates nothing a warm rebuild
                        // needs, and persisting it would let steady positive
                        // traffic crowd the bounded rebuild window, pushing
                        // out the negatives a rebuild must restate.
                        (value == PositiveOutcome::Recorded).then_some((generation, incarnation))
                    }
                    // Suppressed by a negative, refused as stale, or refused
                    // by a purge lease: no metric, no event.
                    _ => None,
                }
            }
            ObservationDirection::SuspectAbsence => {
                let outcome = self.observe_learned_capability_in(
                    scope.generation,
                    learned_key,
                    obs.capability_key,
                    provider_kind,
                    obs.tier,
                    FailurePhase::F3,
                    EvidenceSource::Live,
                    // As above: a `suspect` row also requires a recognized
                    // class on replay.
                    Some(obs.evidence_class),
                    now,
                );
                match outcome {
                    GenerationOutcome::Applied {
                        value: ObserveOutcome::Acting,
                        generation,
                        incarnation,
                    } => {
                        self.metrics.incr_f3_suspect();
                        Some((generation, incarnation))
                    }
                    _ => None,
                }
            }
        };
        // `Some(..)` means the observation acted AND carries the generation and
        // incarnation its own mutation ran under.
        let Some((persistence_generation, incarnation)) = acting else {
            return;
        };
        tracing::warn!(
            event = "observe",
            state_key = %routectl_core::sanitize_for_log(&scope.state_key),
            lane = %routectl_core::sanitize_for_log(learned_key),
            capability_key = obs.capability_key,
            provider_kind,
            evidence_class = obs.evidence_class,
            direction = direction_token(obs.direction),
            signal_tier = obs.tier.as_str(),
            source = EvidenceSource::Live.as_str(),
            "response-evidence capability observation acted",
        );
        events.push(CapabilityObserveEvent {
            persistence_generation,
            incarnation,
            state_key: learned_key.to_string(),
            capability_key: obs.capability_key.to_string(),
            provider_kind: provider_kind.to_string(),
            evidence_class: obs.evidence_class.to_string(),
            direction: obs.direction,
            signal_tier: obs.tier,
            source: EvidenceSource::Live,
            request_features: scope.request_features.clone(),
        });
    }
}

impl EvidenceScope {
    /// The learned-registry key the served target holds for `capability_key`.
    fn learned_key(&self, capability_key: &str) -> Option<&str> {
        self.learned_keys
            .iter()
            .find(|(key, _)| *key == capability_key)
            .map(|(_, lane)| lane.as_str())
    }
}

/// Stable log token for an observation direction. Not a persisted contract
/// (the ledger keys off `evidence_class` + phase); a readable log discriminant
/// only.
const fn direction_token(direction: ObservationDirection) -> &'static str {
    match direction {
        ObservationDirection::Verified => "verified",
        ObservationDirection::SuspectAbsence => "suspect_absence",
    }
}

/// Build the per-capability [`DetectorContext`] from the request. Strict
/// output rides the same `super::field_repair::request_feature_keys`
/// catalog+field membership the act side uses; the schema keys,
/// forced-search directive, reasoning intent, and cache markers are bounded,
/// syntactic reads of the request in hand.
fn detector_context(req: &ChatRequest, request_features: &[String]) -> DetectorContext {
    let has_web_search = request_features.iter().any(|k| k == WEB_SEARCH);
    DetectorContext {
        strict_output_requested: request_features.iter().any(|k| k == STRUCTURED_OUTPUT),
        requested_schema_required_keys: schema_required_keys(req),
        forced_web_search: has_web_search && forces_web_search(req.tool_choice.as_ref()),
        reasoning_requested: reasoning_requested(req.reasoning.as_ref()),
        cache_requested: cache_requested(req),
    }
}

/// Top-level required-property names of the requested output schema. Primary
/// source is the Anthropic structured-outputs schema at
/// `provider_extras.output_config.format.schema` (the shape the detector
/// verifies against message-text JSON); the fallback is the first strict
/// tool's `input_schema`. Empty when no `required` array is declared -- the
/// detector's shape check then passes on any parseable body.
fn schema_required_keys(req: &ChatRequest) -> Vec<String> {
    if let Some(keys) = req
        .provider_extras
        .as_ref()
        .and_then(|v| v.get("output_config"))
        .and_then(|oc| oc.get("format"))
        .and_then(|fmt| fmt.get("schema"))
        .and_then(top_level_required)
    {
        return keys;
    }
    req.tools
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .find_map(|tool| match tool {
            ToolDef::Custom(c) if c.strict == Some(true) => top_level_required(&c.input_schema),
            ToolDef::Other(v) if v.get("strict").and_then(Value::as_bool) == Some(true) => {
                v.get("input_schema").and_then(top_level_required)
            }
            _ => None,
        })
        .unwrap_or_default()
}

/// The string entries of a JSON-schema top-level `required` array, or `None`
/// when the value carries no `required` array. A bounded, syntactic read: no
/// recursion into nested schemas.
fn top_level_required(schema: &Value) -> Option<Vec<String>> {
    let entries = schema.get("required")?.as_array()?;
    Some(
        entries
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
    )
}

/// Whether `tool_choice` forces a web-search call. A general force
/// (Anthropic `any`, OpenAI `required`) counts because web search is among the
/// offered tools; a specific-tool force counts only when it names web search.
/// `auto`, `none`, and unknown directives never force. The caller has already
/// confirmed web search is a requested feature.
fn forces_web_search(tool_choice: Option<&Value>) -> bool {
    match crate::feature_keys::tool_choice_force(tool_choice) {
        Some(ToolChoiceForce::AnyTool) => true,
        // A dated builtin id (`web_search_20250305`) names web search too.
        Some(ToolChoiceForce::Named(name)) => {
            crate::feature_keys::strip_date_suffix(name) == WEB_SEARCH
        }
        None => false,
    }
}

/// Whether extended thinking / reasoning was requested. An explicit
/// `enabled: false` disables regardless of the other fields; otherwise a
/// budget, a non-`none` effort, or `enabled: true` requests it.
fn reasoning_requested(reasoning: Option<&ReasoningConfig>) -> bool {
    let Some(reasoning) = reasoning else {
        return false;
    };
    if reasoning.enabled == Some(false) {
        return false;
    }
    reasoning.enabled == Some(true)
        || reasoning.max_tokens.is_some()
        || reasoning.effort.as_deref().is_some_and(|e| e != "none")
}

/// Whether the request carries any prompt-cache breakpoint: the top-level
/// auto-cache marker, a tool-definition marker, or a message content-part
/// marker. Bounded, syntactic scan of the request in hand.
fn cache_requested(req: &ChatRequest) -> bool {
    if req.cache_control.is_some() {
        return true;
    }
    if req
        .tools
        .as_deref()
        .is_some_and(|tools| tools.iter().any(|t| t.cache_control().is_some()))
    {
        return true;
    }
    req.messages.iter().any(message_has_cache_control)
}

/// Whether a message carries a cache breakpoint on any typed content part.
fn message_has_cache_control(message: &routectl_core::Message) -> bool {
    matches!(
        &message.content,
        routectl_core::MessageContent::Parts(parts)
            if parts.iter().any(|p| p.cache_control().is_some())
    )
}

#[cfg(test)]
#[path = "capability_observe_tests.rs"]
mod capability_observe_tests;
