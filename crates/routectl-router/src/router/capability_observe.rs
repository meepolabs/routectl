//! Response-evidence observation on the terminal successful non-streaming
//! dispatch: the read-only mirror of [`Router::observe_for_learning`].
//!
//! Where the learn path reads a dispatch ERROR arm and mints negatives, this
//! path reads the terminal SUCCESS arm and admits positive (VerifiedWorking)
//! and suspected-absence (F3) observations. Both co-locate the kill switch
//! (`config.capability.enabled`), per-request dedupe, the registry, and the
//! WARN/counter observability floor, reusing the shipped pattern.
//!
//! Two-stage (R2) contract: live-only detection (stage one) is the pure
//! [`crate::capability_detect`] slice; admission (stage two, `now`-
//! parameterized, `EvidenceSource::Live`) happens here and is EXACTLY what a
//! later warm-rebuild replays. No ledger write -- acting observations ride out
//! on [`DispatchMeta`] as the in-memory persistence hook, mirroring
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
use serde_json::Value;

use super::{DispatchMeta, DispatchTarget, Router};
use crate::capability_detect::{
    self, CapabilityObservation, DetectorContext, ObservationDirection,
};
use crate::learned_capability::{ObserveOutcome, PositiveOutcome};

/// A single response-evidence observation captured on the terminal
/// successful non-streaming dispatch, riding out on [`DispatchMeta`] to the
/// usage-capture layer. The router does not depend on the ledger writer, so
/// observations travel on the dispatch meta rather than being written here.
///
/// Carries the columns a later warm-rebuild replays (stage two): the
/// capability key, the pinned evidence class, the observation direction,
/// tier, and source, plus the routing `state_key`, `provider_kind`, and the
/// request's derived feature set. No request body, prompt, or response text
/// ever enters this struct.
#[derive(Debug, Clone)]
pub struct CapabilityObserveEvent {
    /// Routing state key (nickname-or-provider) of the served target.
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
    /// Fixed to `Live` in this milestone.
    pub source: EvidenceSource,
    /// The request's derived feature set at observation time. Replay
    /// verifies the observed capability was actually in flight.
    pub request_features: Vec<String>,
}

impl Router {
    /// Observe response evidence on the terminal successful non-streaming
    /// dispatch -- the read-only mirror of [`Router::observe_for_learning`].
    /// Runs the pure detectors over the assembled response, then admits each
    /// observation into the registry (`now`-parameterized, `Live` source) and
    /// rides an acting observation out on `meta`.
    ///
    /// Short-circuits with ZERO detector runs / writes / ride-alongs when the
    /// kill switch is off (`!config.capability.enabled`) or the served target
    /// carries no provider-kind (a legacy / direct-construction target -- fail
    /// closed, exactly as the learn path does).
    pub(super) fn observe_capabilities(
        &self,
        req: &ChatRequest,
        resp: &ChatResponse,
        target: &DispatchTarget,
        meta: &mut DispatchMeta,
        now: Instant,
    ) {
        if !self.config.capability.enabled {
            return;
        }
        let Some(provider_kind) = target.provider_kind else {
            return;
        };
        let request_features = crate::feature_keys::derive_feature_keys(
            req.tools.as_deref().unwrap_or(&[]),
            req.provider_extras.as_ref(),
        );
        let ctx = detector_context(req, &request_features);
        let observations = capability_detect::detect(&ctx, resp);
        if observations.is_empty() {
            return;
        }
        let state_key = target.state_key.as_str();
        // One observation per `(state_key, capability)` per request. The
        // success arm is terminal (it returns on first success), so this call
        // fires once per request and `state_key` is constant across the loop;
        // the set therefore dedupes on the capability key alone.
        let mut dedupe: HashSet<&'static str> = HashSet::new();
        for obs in observations {
            if !dedupe.insert(obs.capability_key) {
                continue;
            }
            self.admit_observation(&obs, state_key, provider_kind, &request_features, meta, now);
        }
    }

    /// Admit one observation into the registry (stage two) and, when it acts,
    /// emit the structured WARN, bump the dedicated counter, and ride the
    /// observation out on `meta`. A VerifiedWorking positive acts on its first
    /// observation; a suspected-absence F3 negative acts only once corroborated
    /// within the inferred window (a passive positive suppressed by a resident
    /// negative, or a still-pending single inferred observation, does neither).
    fn admit_observation(
        &self,
        obs: &CapabilityObservation,
        state_key: &str,
        provider_kind: &'static str,
        request_features: &[String],
        meta: &mut DispatchMeta,
        now: Instant,
    ) {
        let acting = match obs.direction {
            ObservationDirection::Verified => {
                let outcome = self.learned_capabilities.observe_positive(
                    state_key,
                    obs.capability_key,
                    provider_kind,
                    now,
                );
                if matches!(outcome, PositiveOutcome::Recorded) {
                    self.metrics.incr_verified_working();
                    true
                } else {
                    false
                }
            }
            ObservationDirection::SuspectAbsence => {
                let outcome = self.learned_capabilities.observe(
                    state_key,
                    obs.capability_key,
                    provider_kind,
                    obs.tier,
                    FailurePhase::F3,
                    now,
                );
                if matches!(outcome, ObserveOutcome::Acting) {
                    self.metrics.incr_f3_suspect();
                    true
                } else {
                    false
                }
            }
        };
        if !acting {
            return;
        }
        tracing::warn!(
            event = "observe",
            state_key = %state_key,
            capability_key = obs.capability_key,
            provider_kind,
            evidence_class = obs.evidence_class,
            direction = direction_token(obs.direction),
            signal_tier = obs.tier.as_str(),
            source = EvidenceSource::Live.as_str(),
            "response-evidence capability observation acted",
        );
        meta.capability_observations.push(CapabilityObserveEvent {
            state_key: state_key.to_string(),
            capability_key: obs.capability_key.to_string(),
            provider_kind: provider_kind.to_string(),
            evidence_class: obs.evidence_class.to_string(),
            direction: obs.direction,
            signal_tier: obs.tier,
            source: EvidenceSource::Live,
            request_features: request_features.to_vec(),
        });
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
/// output rides the same `derive_feature_keys` membership the act side uses;
/// the schema keys, forced-search directive, reasoning intent, and cache
/// markers are bounded, syntactic reads of the request in hand.
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
    let Some(tool_choice) = tool_choice else {
        return false;
    };
    match tool_choice {
        Value::String(s) => matches!(s.as_str(), "required" | "any"),
        Value::Object(_) => match tool_choice.get("type").and_then(Value::as_str) {
            Some("any" | "required") => true,
            Some("tool") => names_web_search(tool_choice.get("name")),
            Some("function") => {
                names_web_search(tool_choice.get("function").and_then(|f| f.get("name")))
            }
            _ => false,
        },
        _ => false,
    }
}

/// Whether a `tool_choice` name field names the web-search tool, tolerating a
/// dated builtin id (`web_search_20250305`).
fn names_web_search(name: Option<&Value>) -> bool {
    name.and_then(Value::as_str)
        .map(crate::feature_keys::strip_date_suffix)
        == Some(WEB_SEARCH)
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
