//! The edit register: content-free declarations of the body changes routectl
//! makes on purpose, plus the closed label sets a finding carries.

use std::borrow::Cow;

use super::path::{KeyScope, MAX_WIRE_PATH_DEPTH, MAX_WIRE_PATH_LEN, WILDCARD_SEGMENT, WirePath};

/// Patterns that would exempt a whole container from detection. A transform
/// that broke anything under one of these would otherwise go unseen.
const BLANKET_PATTERNS: [&str; 9] = [
    "",
    "/",
    "/*",
    "/messages",
    "/messages/*",
    "/system",
    "/system/*",
    "/tools",
    "/tools/*",
];

/// Kind of change a declared edit makes at its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EditKind {
    /// The value at the path is removed.
    Remove,
    /// A value is added at the path.
    Add,
    /// The value at the path is replaced.
    Rewrite,
    /// The value at the path is moved to another location.
    Relocate,
}

impl EditKind {
    /// Every kind, in declaration order.
    pub const ALL: [Self; 4] = [Self::Remove, Self::Add, Self::Rewrite, Self::Relocate];

    /// Static label for logs and counters.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Remove => "remove",
            Self::Add => "add",
            Self::Rewrite => "rewrite",
            Self::Relocate => "relocate",
        }
    }
}

/// The site in routectl that made a declared edit. Closed: a new transform
/// that changes a body gets its own member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EditSite {
    /// Router: the alias target's upstream model id replaces the client's.
    ModelRename,
    /// Router: operator `payload_extras` merged into the body.
    OperatorPayloadExtras,
    /// Router: lossless context reduction.
    ContextReduction,
    /// Router: prompt-cache breakpoint placement.
    CachePlacement,
    /// Router: repair of a field the upstream rejected on this request.
    FieldRepair,
    /// Router: a repair learned from an earlier rejection.
    LearnedRepair,
    /// Router: pre-flight removal or rewrite of a field known to be refused.
    Preflight,
    /// Router: canary probe of a previously refused field.
    Canary,
    /// Router: removal of replay artifacts the target cannot accept.
    ReplayStrip,
    /// Ingress: a deprecated request field folded into its current
    /// location.
    LegacyFieldFold,
    /// Ingress: system-role messages lifted into the system prompt, which
    /// the egress sends back as one leading system message.
    SystemMessageLift,
    /// Provider: Claude Code cloak transforms.
    Cloak,
    /// Provider: Claude sampling-parameter normalization.
    ClaudeSamplingNormalize,
    /// Provider: reconciliation of mutually exclusive sampling parameters.
    SamplingReconcile,
    /// Provider: sampling parameters pinned to the values thinking mode
    /// requires.
    ThinkingSamplingClamp,
    /// Provider: thinking removed because `tool_choice` forces tool use.
    ThinkingStripForcedToolUse,
    /// Provider: a client's `enabled` thinking turned into `adaptive` for a
    /// model the operator marks as supporting adaptive thinking, its budget
    /// dropped.
    AdaptiveThinkingOverride,
    /// Provider: a client's thinking budget lowered to the operator's
    /// per-model cap.
    ThinkingBudgetCap,
    /// Provider: the body `stream` flag removed.
    StreamStrip,
    /// Provider: `anthropic_beta` removed from the body.
    AnthropicBetaStrip,
    /// Provider: reasoning effort injected.
    EffortInjection,
    /// Provider: tool-call ids rewritten to the upstream's id grammar.
    ToolIdSanitize,
    /// Provider: thinking signatures removed.
    SignatureStrip,
    /// Provider: top-level cache markers lowered onto blocks.
    CacheMarkerLowering,
    /// Provider: body fields moved into the request URL.
    UrlFieldRelocate,
    /// Provider: a hosted tool withheld from the upstream.
    HostedToolWithhold,
    /// Provider: `stream_options` injected.
    StreamOptionsInjection,
    /// Provider: a field gated off for the upstream's auth surface.
    AuthSurfaceGate,
    /// Provider: preserved Responses input items spliced back in.
    ResponsesPassthroughSplice,
    /// Provider: the body `stream` flag set to the transport mode the
    /// provider drives.
    StreamFlagForce,
    /// Provider: prompt-cache markers removed for an upstream with no cache
    /// surface.
    CacheMarkerStrip,
    /// Provider: a field the upstream's dialect does not accept removed.
    DialectFieldStrip,
    /// Provider: sampling parameters removed for a model that rejects them.
    SamplingParamsStrip,
    /// Provider: `max_tokens` renamed to `max_completion_tokens`.
    MaxTokensRename,
    /// Provider: reasoning removed from echoed assistant history.
    HistoryReasoningStrip,
    /// Provider: a forwarded extra withheld because routectl manages its key
    /// or the upstream rejects it.
    ManagedExtrasStrip,
    /// Provider: a field the upstream requires written with routectl's own
    /// value.
    RequiredFieldDefault,
    /// Provider: the default reasoning summary mode added.
    ReasoningSummaryDefault,
    /// Provider: the encrypted-reasoning `include` entry added.
    ReasoningIncludeInjection,
    /// Provider: the per-installation id stamped into `client_metadata`.
    InstallationIdStamp,
    /// Provider: the Claude Code billing/attribution block withheld from
    /// the system prompt sent to a third-party upstream.
    ClientFingerprintStrip,
    /// Response: a finish reason mapped to the client dialect's value.
    FinishReasonRewrite,
    /// Response: streaming envelope fields stamped onto each chunk.
    StreamEnvelopeStamp,
    /// Response: cached-token usage added.
    CachedTokensAdd,
    /// Response: an upstream `reasoning_content` moved into the canonical
    /// reasoning fields.
    ReasoningContentLift,
    /// Response: envelope fields the client dialect requires stamped onto a
    /// complete body (the Chat Completions envelope, the Anthropic nullable
    /// `stop_sequence`).
    ResponseEnvelopeStamp,
    /// Response: the upstream model id replaced by the client-visible label.
    ResponseModelRelabel,
    /// Response: the serving provider's name added to a complete body.
    ProviderAttributionAdd,
    /// Response: the Anthropic `stop_details` and `upstream_meta` top-level
    /// members withheld from the client.
    AnthropicResponseExtrasStrip,
    /// Response: missing token counts filled on the closing Anthropic
    /// `message_delta` usage.
    AnthropicStreamUsageFill,
    /// Response: the Responses `status` and `incomplete_details` rebuilt
    /// from the canonical finish reason, and a message output item's
    /// `status` written by the render's own item lifecycle.
    ResponsesStatusRewrite,
    /// Response: stream frames arriving after the terminal frame, or a
    /// repeated stop reason, withheld from the client.
    StreamStragglerDrop,
}

impl EditSite {
    /// Every site, in declaration order.
    pub const ALL: [Self; 52] = [
        Self::ModelRename,
        Self::OperatorPayloadExtras,
        Self::ContextReduction,
        Self::CachePlacement,
        Self::FieldRepair,
        Self::LearnedRepair,
        Self::Preflight,
        Self::Canary,
        Self::ReplayStrip,
        Self::LegacyFieldFold,
        Self::SystemMessageLift,
        Self::Cloak,
        Self::ClaudeSamplingNormalize,
        Self::SamplingReconcile,
        Self::ThinkingSamplingClamp,
        Self::ThinkingStripForcedToolUse,
        Self::AdaptiveThinkingOverride,
        Self::ThinkingBudgetCap,
        Self::StreamStrip,
        Self::AnthropicBetaStrip,
        Self::EffortInjection,
        Self::ToolIdSanitize,
        Self::SignatureStrip,
        Self::CacheMarkerLowering,
        Self::UrlFieldRelocate,
        Self::HostedToolWithhold,
        Self::StreamOptionsInjection,
        Self::AuthSurfaceGate,
        Self::ResponsesPassthroughSplice,
        Self::StreamFlagForce,
        Self::CacheMarkerStrip,
        Self::DialectFieldStrip,
        Self::SamplingParamsStrip,
        Self::MaxTokensRename,
        Self::HistoryReasoningStrip,
        Self::ManagedExtrasStrip,
        Self::RequiredFieldDefault,
        Self::ReasoningSummaryDefault,
        Self::ReasoningIncludeInjection,
        Self::InstallationIdStamp,
        Self::ClientFingerprintStrip,
        Self::FinishReasonRewrite,
        Self::StreamEnvelopeStamp,
        Self::CachedTokensAdd,
        Self::ReasoningContentLift,
        Self::ResponseEnvelopeStamp,
        Self::ResponseModelRelabel,
        Self::ProviderAttributionAdd,
        Self::AnthropicResponseExtrasStrip,
        Self::AnthropicStreamUsageFill,
        Self::ResponsesStatusRewrite,
        Self::StreamStragglerDrop,
    ];

    /// Static label for logs and counters.
    pub const fn label(self) -> &'static str {
        match self {
            Self::ModelRename => "model_rename",
            Self::OperatorPayloadExtras => "operator_payload_extras",
            Self::ContextReduction => "context_reduction",
            Self::CachePlacement => "cache_placement",
            Self::FieldRepair => "field_repair",
            Self::LearnedRepair => "learned_repair",
            Self::Preflight => "preflight",
            Self::Canary => "canary",
            Self::ReplayStrip => "replay_strip",
            Self::LegacyFieldFold => "legacy_field_fold",
            Self::SystemMessageLift => "system_message_lift",
            Self::Cloak => "cloak",
            Self::ClaudeSamplingNormalize => "claude_sampling_normalize",
            Self::SamplingReconcile => "sampling_reconcile",
            Self::ThinkingSamplingClamp => "thinking_sampling_clamp",
            Self::ThinkingStripForcedToolUse => "thinking_strip_forced_tool_use",
            Self::AdaptiveThinkingOverride => "adaptive_thinking_override",
            Self::ThinkingBudgetCap => "thinking_budget_cap",
            Self::StreamStrip => "stream_strip",
            Self::AnthropicBetaStrip => "anthropic_beta_strip",
            Self::EffortInjection => "effort_injection",
            Self::ToolIdSanitize => "tool_id_sanitize",
            Self::SignatureStrip => "signature_strip",
            Self::CacheMarkerLowering => "cache_marker_lowering",
            Self::UrlFieldRelocate => "url_field_relocate",
            Self::HostedToolWithhold => "hosted_tool_withhold",
            Self::StreamOptionsInjection => "stream_options_injection",
            Self::AuthSurfaceGate => "auth_surface_gate",
            Self::ResponsesPassthroughSplice => "responses_passthrough_splice",
            Self::StreamFlagForce => "stream_flag_force",
            Self::CacheMarkerStrip => "cache_marker_strip",
            Self::DialectFieldStrip => "dialect_field_strip",
            Self::SamplingParamsStrip => "sampling_params_strip",
            Self::MaxTokensRename => "max_tokens_rename",
            Self::HistoryReasoningStrip => "history_reasoning_strip",
            Self::ManagedExtrasStrip => "managed_extras_strip",
            Self::RequiredFieldDefault => "required_field_default",
            Self::ReasoningSummaryDefault => "reasoning_summary_default",
            Self::ReasoningIncludeInjection => "reasoning_include_injection",
            Self::InstallationIdStamp => "installation_id_stamp",
            Self::ClientFingerprintStrip => "client_fingerprint_strip",
            Self::FinishReasonRewrite => "finish_reason_rewrite",
            Self::StreamEnvelopeStamp => "stream_envelope_stamp",
            Self::CachedTokensAdd => "cached_tokens_add",
            Self::ReasoningContentLift => "reasoning_content_lift",
            Self::ResponseEnvelopeStamp => "response_envelope_stamp",
            Self::ResponseModelRelabel => "response_model_relabel",
            Self::ProviderAttributionAdd => "provider_attribution_add",
            Self::AnthropicResponseExtrasStrip => "anthropic_response_extras_strip",
            Self::AnthropicStreamUsageFill => "anthropic_stream_usage_fill",
            Self::ResponsesStatusRewrite => "responses_status_rewrite",
            Self::StreamStragglerDrop => "stream_straggler_drop",
        }
    }
}

/// Why a pointer pattern was refused as a [`DeclaredEdit`] path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EditPatternError {
    /// The pattern covers a whole container, which would hide every change
    /// made anywhere inside it.
    #[error("edit pattern exempts a whole container")]
    Blanket,
    /// The pattern is not an RFC 6901 pointer (it does not start with `/`).
    #[error("edit pattern is not a JSON pointer")]
    NotAPointer,
    /// The pattern can never match a [`WirePath`]: a segment that is
    /// neither an index, the wildcard nor a key a path renders verbatim, or
    /// more segments or bytes than a path carries.
    #[error("edit pattern can never match a wire path")]
    Unmatchable,
}

/// One body change routectl made on purpose, declared at the site that made
/// it. Carries no body content: only the site, a pointer pattern and the
/// kind of change.
///
/// The pattern is an RFC 6901 pointer whose segments are spelled the way a
/// [`WirePath`] renders them; a `*` segment matches exactly one array index
/// or masked key, never a verbatim object key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeclaredEdit {
    site: EditSite,
    path: Cow<'static, str>,
    kind: EditKind,
}

impl DeclaredEdit {
    /// Declare an edit, refusing a pattern that blankets a whole container
    /// or that could never match a [`WirePath`].
    pub fn new(
        site: EditSite,
        path: impl Into<Cow<'static, str>>,
        kind: EditKind,
    ) -> Result<Self, EditPatternError> {
        let path = path.into();
        validate_pattern(&path)?;
        Ok(Self { site, path, kind })
    }

    /// The site that made the edit.
    pub const fn site(&self) -> EditSite {
        self.site
    }

    /// The pointer pattern.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The kind of change.
    pub const fn kind(&self) -> EditKind {
        self.kind
    }

    /// True if the pattern matches `path`: the same number of segments, each
    /// pattern segment equal to the path's segment, or `*` where the path's
    /// segment is an array index or a masked key. A `*` never matches a
    /// verbatim object key. A coarsened path matches nothing: it names
    /// an ancestor, and a declaration there would hide a deeper change.
    pub fn matches(&self, path: &WirePath) -> bool {
        pattern_matches(&self.path, path)
    }
}

/// [`DeclaredEdit::matches`] over a bare pattern.
pub(super) fn pattern_matches(pattern: &str, path: &WirePath) -> bool {
    if path.is_coarsened() {
        return false;
    }
    let mut pattern = pattern.split('/').skip(1);
    let mut actual = path.segments().enumerate();
    loop {
        match (pattern.next(), actual.next()) {
            (None, None) => return true,
            (Some(WILDCARD_SEGMENT), Some((position, _))) if path.is_wildcard_segment(position) => {
            }
            (Some(want), Some((_, got))) if want != WILDCARD_SEGMENT && want == got => {}
            _ => return false,
        }
    }
}

/// True when `pattern` has no `*` segment, so it matches exactly the one
/// path that renders as the pattern itself.
pub(super) fn is_concrete_pattern(pattern: &str) -> bool {
    !pattern
        .split('/')
        .any(|segment| segment == WILDCARD_SEGMENT)
}

fn validate_pattern(pattern: &str) -> Result<(), EditPatternError> {
    if BLANKET_PATTERNS.contains(&pattern) {
        return Err(EditPatternError::Blanket);
    }
    let Some(rest) = pattern.strip_prefix('/') else {
        return Err(EditPatternError::NotAPointer);
    };
    let matchable = pattern.len() <= MAX_WIRE_PATH_LEN
        && rest.split('/').count() <= MAX_WIRE_PATH_DEPTH
        && segments_can_match(rest);
    if matchable {
        Ok(())
    } else {
        Err(EditPatternError::Unmatchable)
    }
}

/// A [`WirePath`] segment is an index, the wildcard or a verbatim key: an
/// identifier key outside client-keyed objects and schemas, or a JSON Schema
/// keyword inside a schema. A pattern segment that no path can render is
/// unmatchable. A `*` may stand for an index or a masked key, which open
/// different scopes, so every scope the pattern can be in is tracked.
fn segments_can_match(rest: &str) -> bool {
    let mut ancestors: Vec<&str> = Vec::new();
    let mut scopes: Vec<KeyScope> = vec![KeyScope::Open];
    for segment in rest.split('/') {
        let is_index = !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_digit());
        if segment == WILDCARD_SEGMENT {
            scopes = KeyScope::ALL
                .into_iter()
                .filter(|next| {
                    scopes
                        .iter()
                        .any(|scope| scope == next || scope.masked_key() == *next)
                })
                .collect();
        } else if !is_index {
            scopes = KeyScope::ALL
                .into_iter()
                .filter(|next| {
                    scopes
                        .iter()
                        .any(|scope| scope.key(&ancestors, segment) == (true, *next))
                })
                .collect();
            if scopes.is_empty() {
                return false;
            }
        }
        ancestors.push(segment);
    }
    true
}

/// How a path differs between the two bodies a check compares. Closed; it is
/// the `path_class` label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PathClass {
    /// Present in the input, absent from the output.
    Missing,
    /// The same value at another index of the same array.
    Moved,
    /// Present in both with a different value.
    Changed,
    /// Absent from the input, present in the output.
    Added,
    /// A translation-lane input key the egress did not carry.
    TranslationUnconsumed,
}

impl PathClass {
    /// Every class, in declaration order.
    pub const ALL: [Self; 5] = [
        Self::Missing,
        Self::Moved,
        Self::Changed,
        Self::Added,
        Self::TranslationUnconsumed,
    ];

    /// Static label for logs and counters.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Moved => "moved",
            Self::Changed => "changed",
            Self::Added => "added",
            Self::TranslationUnconsumed => "translation_unconsumed",
        }
    }
}

/// Where a path named by an upstream rejection came from. Closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RejectionProvenance {
    /// The client sent it and routectl forwarded it unchanged.
    ClientUnchanged,
    /// The client sent it and routectl changed it.
    ClientChanged,
    /// The client did not send it; routectl added it.
    RoutectlAdded,
    /// The rejection named no path that could be located.
    Unlocalized,
    /// Not applicable: the lane translates, so client and egress paths do
    /// not correspond.
    NotApplicable,
}

impl RejectionProvenance {
    /// Every provenance, in declaration order.
    pub const ALL: [Self; 5] = [
        Self::ClientUnchanged,
        Self::ClientChanged,
        Self::RoutectlAdded,
        Self::Unlocalized,
        Self::NotApplicable,
    ];

    /// Static label for logs and counters.
    pub const fn label(self) -> &'static str {
        match self {
            Self::ClientUnchanged => "client_unchanged",
            Self::ClientChanged => "client_changed",
            Self::RoutectlAdded => "routectl_added",
            Self::Unlocalized => "unlocalized",
            Self::NotApplicable => "n_a",
        }
    }
}

#[cfg(test)]
#[allow(clippy::panic)]
#[path = "edits_tests.rs"]
mod tests;
