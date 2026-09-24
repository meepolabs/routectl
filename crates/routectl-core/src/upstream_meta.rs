//! Transport-internal carrier for non-canonical upstream metadata that
//! rides alongside a `ChatResponse` / `ChatChunk` without ever touching
//! the client-facing wire. It carries the provider quota families parsed
//! off egress response headers (Anthropic's `anthropic-ratelimit-unified-*`,
//! Codex's `x-codex-*`) and, on an Anthropic-shape stream, the input side
//! of usage the upstream reported in its first event (`OpeningUsage`).
//! Skip-serialized (see the `#[serde(skip)]` fields on `ChatResponse` /
//! `ChatChunk`); never on the wire.
//!
//! The opening usage deliberately does NOT ride canonical
//! `ChatChunk.usage`: that field is client-visible on OpenAI-shape
//! ingresses and usage accounting reads it as the stream's cumulative
//! counters, so an early value there would change a first frame and stamp
//! a truncated stream's ledger row with input-only numbers.
//!
//! Mirrors the transport-internal carrier precedent at
//! `crate::schema_opaque::OpaqueSseEvent`: `#[non_exhaustive]` so future
//! variants ship without breaking downstream library consumers, and a
//! skip-serialized field on the canonical response types.
//!
//! ## Naming choice
//!
//! Sub-structs are provider-NAMESPACED on purpose. `anthropic_unified`
//! names the Anthropic vendor family explicitly rather than hiding it
//! behind a neutral-sounding top-level field like `quota` or
//! `rate_limit`. A future provider with its own quota-header family adds
//! its own optional sub-struct (e.g. `openai_*`) next to this one; it
//! does NOT reinterpret Anthropic vocabulary. `codex` follows that
//! precedent with Codex-native `x-codex-*` field names; each further
//! provider quota family likewise gets its own sub-struct in this file
//! rather than being folded into an existing one. The unified-family field
//! names (`status`, `overage_status`, `utilization`,
//! `overage_utilization`, `representative_claim`, `reset`) are Anthropic
//! wire terms and only ever describe Anthropic data. Generalizing them
//! into neutral names would silently conflate semantics across vendors
//! whose quota models do not actually match.

/// Transport-internal upstream metadata carried beside a canonical
/// response. Provider-namespaced: each vendor's non-canonical metadata
/// gets its own optional sub-struct. `#[non_exhaustive]` so adding a new
/// provider's sub-struct is not a breaking change for downstream library
/// consumers that match on or construct this type.
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct UpstreamMeta {
    /// Anthropic `anthropic-ratelimit-unified-*` quota/overage family,
    /// parsed off the anthropic-api egress response headers. `None` when
    /// no header of the family was present (the common case on the
    /// api-key path, which does not emit the family).
    pub anthropic_unified: Option<AnthropicUnifiedQuota>,
    /// Codex `x-codex-*` quota family, parsed off the Codex egress
    /// response headers. `None` when no header of the family was present.
    pub codex: Option<CodexQuota>,
    /// Input-side usage the upstream reported in its FIRST stream event,
    /// carried on the content-free opening chunk. `None` on every other
    /// chunk, on non-streaming responses, and when the first event carried
    /// no usage. Not a quota family: see [`Self::has_quota_family`].
    pub opening_usage: Option<OpeningUsage>,
}

impl UpstreamMeta {
    /// Construct an `UpstreamMeta` carrying only the Anthropic unified
    /// quota family. The common shape today; a thin ctor keeps call
    /// sites free of struct-update churn when new sub-structs land.
    pub const fn from_anthropic_unified(quota: AnthropicUnifiedQuota) -> Self {
        Self {
            anthropic_unified: Some(quota),
            codex: None,
            opening_usage: None,
        }
    }

    /// Construct an `UpstreamMeta` carrying only the Codex quota family.
    pub const fn from_codex(quota: CodexQuota) -> Self {
        Self {
            anthropic_unified: None,
            codex: Some(quota),
            opening_usage: None,
        }
    }

    /// Construct an `UpstreamMeta` carrying only the opening usage.
    pub const fn from_opening_usage(opening: OpeningUsage) -> Self {
        Self {
            anthropic_unified: None,
            codex: None,
            opening_usage: Some(opening),
        }
    }

    /// True when this carrier holds a provider quota family. Opening usage
    /// alone is not a quota reading, so a one-shot quota consumer must keep
    /// waiting past a carrier for which this is false.
    pub const fn has_quota_family(&self) -> bool {
        self.anthropic_unified.is_some() || self.codex.is_some()
    }

    /// Combine two carriers that describe the same response. Each family is
    /// taken from `self` when present there, otherwise from `other`, so a
    /// response-head quota carrier and a stream-body opening carrier both
    /// survive on the one chunk they share.
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        Self {
            anthropic_unified: self.anthropic_unified.or(other.anthropic_unified),
            codex: self.codex.or(other.codex),
            opening_usage: self.opening_usage.or(other.opening_usage),
        }
    }
}

/// Which upstream wire produced an [`OpeningUsage`]. Names the parser, not
/// the party that counted the tokens: an Anthropic Messages endpoint may be
/// the vendor itself or any Anthropic-compatible proxy in front of it, so
/// [`Self::AnthropicMessages`] alone does not establish that a model-side
/// tokenizer measured the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OpeningUsageOrigin {
    /// The `message_start` event of an Anthropic Messages SSE stream.
    AnthropicMessages,
    /// The `message_start` event nested inside a Bedrock InvokeModel
    /// response stream.
    BedrockInvoke,
}

/// Input-side usage from an upstream stream's first event, preserved exactly
/// as the upstream reported it. The cache fields stay DISJOINT from
/// `input_tokens` (Anthropic semantics); nothing here is summed. It may
/// legitimately differ from the stream's terminal usage, since server-side
/// work can add input mid-response. `#[non_exhaustive]`: construct with
/// [`Self::new`] and assign the optional fields.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OpeningUsage {
    /// Which upstream wire the value was parsed from.
    pub origin: OpeningUsageOrigin,
    /// When the parser saw the first event, for comparing first-event
    /// arrival against first-content arrival.
    pub observed_at: std::time::Instant,
    /// Uncached input tokens.
    pub input_tokens: u32,
    /// Tokens written to the prompt cache, if reported.
    pub cache_creation_input_tokens: Option<u32>,
    /// Tokens read from the prompt cache, if reported.
    pub cache_read_input_tokens: Option<u32>,
    /// Per-TTL cache-write breakdown, if reported.
    pub cache_creation: Option<crate::schema::CacheCreation>,
}

impl OpeningUsage {
    /// An opening reading with only the required fields; the cache fields
    /// start absent.
    pub const fn new(
        origin: OpeningUsageOrigin,
        observed_at: std::time::Instant,
        input_tokens: u32,
    ) -> Self {
        Self {
            origin,
            observed_at,
            input_tokens,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            cache_creation: None,
        }
    }
}

/// Anthropic `anthropic-ratelimit-unified-*` response-header family,
/// parsed tolerantly: every field keeps the RAW string value the
/// upstream sent, and a weird/unexpected value NEVER fails a request.
/// `#[non_exhaustive]` so future named fields ship without breaking
/// downstream library consumers.
///
/// Header-to-field mapping (suffix after `anthropic-ratelimit-unified-`):
///   - `status` -> `status`
///   - `overage-status` -> `overage_status`
///   - `5h-utilization` -> `utilization` (the 5h window is the
///     operational subscription signal; there is no bare `-utilization`)
///   - `overage-utilization` -> `overage_utilization`
///   - `representative-claim` -> `representative_claim`
///   - `reset` -> `reset`
///
/// Any other `anthropic-ratelimit-unified-<suffix>` header (the 7d
/// window, the per-window status/reset suffixes, the fallback-percentage,
/// or any future suffix) lands in `extras` as `(suffix, value)` so it is
/// observable without a code change.
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct AnthropicUnifiedQuota {
    /// Raw `-status` value.
    pub status: Option<String>,
    /// Raw `-overage-status` value.
    pub overage_status: Option<String>,
    /// 5h-window utilization (a decimal fraction string like "0.21"),
    /// the operational subscription signal. Sourced from the
    /// `-5h-utilization` header; there is no bare `-utilization` header.
    pub utilization: Option<String>,
    /// Raw `-overage-utilization` value.
    pub overage_utilization: Option<String>,
    /// Raw `-representative-claim` value.
    pub representative_claim: Option<String>,
    /// Raw `-reset` value.
    pub reset: Option<String>,
    /// Any other `anthropic-ratelimit-unified-<suffix>` header captured
    /// for forward-compat, as `(suffix, value)` pairs in header order.
    pub extras: Vec<(String, String)>,
}

/// The `representative-claim` value that signals the active billing
/// attribution has flipped to overage (pay-as-you-go beyond the included
/// subscription quota).
pub const OVERAGE_CLAIM: &str = "overage";

impl AnthropicUnifiedQuota {
    /// True when the active billing attribution is overage, i.e. the
    /// `representative-claim` header equals `"overage"`. Steady-state
    /// (subscription quota) reports some other claim (e.g. `five_hour`).
    pub fn is_overage(&self) -> bool {
        self.representative_claim.as_deref() == Some(OVERAGE_CLAIM)
    }
}

/// Codex `x-codex-*` response-header quota family, parsed tolerantly:
/// every field keeps the RAW string value the upstream sent, and a
/// weird/unexpected value NEVER fails a request. `#[non_exhaustive]` so
/// future named fields ship without breaking downstream library
/// consumers.
///
/// Codex-NATIVE field names only: this family has its own quota model and
/// does not borrow Anthropic vocabulary.
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct CodexQuota {
    /// Raw `x-codex-active-limit` value.
    pub active_limit: Option<String>,
    /// Raw `x-codex-primary-used-percent` value.
    pub primary_used_percent: Option<String>,
    /// Raw `x-codex-primary-reset-at` value, an epoch timestamp in
    /// SECONDS (not milliseconds).
    pub primary_reset_at: Option<String>,
    /// Any other `x-codex-<suffix>` header captured for forward-compat,
    /// as `(suffix, value)` pairs in header order.
    pub extras: Vec<(String, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_overage_true_when_representative_claim_is_overage() {
        // Arrange
        let quota = AnthropicUnifiedQuota {
            representative_claim: Some("overage".into()),
            ..Default::default()
        };

        // Act + Assert
        assert!(quota.is_overage());
    }

    #[test]
    fn is_overage_false_for_non_overage_claim() {
        // Arrange
        let quota = AnthropicUnifiedQuota {
            representative_claim: Some("five_hour".into()),
            ..Default::default()
        };

        // Act + Assert
        assert!(!quota.is_overage());
    }

    #[test]
    fn is_overage_false_when_claim_absent() {
        // Arrange
        let quota = AnthropicUnifiedQuota::default();

        // Act + Assert
        assert!(!quota.is_overage());
    }

    #[test]
    fn from_anthropic_unified_wraps_quota_in_some() {
        // Arrange
        let quota = AnthropicUnifiedQuota {
            status: Some("allowed".into()),
            ..Default::default()
        };

        // Act
        let meta = UpstreamMeta::from_anthropic_unified(quota.clone());

        // Assert
        assert_eq!(meta.anthropic_unified, Some(quota));
    }

    #[test]
    fn default_upstream_meta_has_no_anthropic_unified() {
        // Arrange + Act
        let meta = UpstreamMeta::default();

        // Assert
        assert!(meta.anthropic_unified.is_none());
    }

    #[test]
    fn from_codex_wraps_quota_in_some_and_leaves_anthropic_unified_none() {
        // Arrange
        let quota = CodexQuota {
            active_limit: Some("weekly".into()),
            ..Default::default()
        };

        // Act
        let meta = UpstreamMeta::from_codex(quota.clone());

        // Assert
        assert_eq!(meta.codex, Some(quota));
        assert!(meta.anthropic_unified.is_none());
    }

    #[test]
    fn default_upstream_meta_has_no_codex() {
        // Arrange + Act
        let meta = UpstreamMeta::default();

        // Assert
        assert!(meta.codex.is_none());
    }

    fn opening(input_tokens: u32) -> OpeningUsage {
        OpeningUsage::new(
            OpeningUsageOrigin::AnthropicMessages,
            std::time::Instant::now(),
            input_tokens,
        )
    }

    #[test]
    fn opening_usage_alone_is_not_a_quota_family() {
        // Arrange
        let opening_only = UpstreamMeta::from_opening_usage(opening(7));
        let quota = UpstreamMeta::from_anthropic_unified(AnthropicUnifiedQuota::default());
        let codex = UpstreamMeta::from_codex(CodexQuota::default());

        // Act + Assert
        assert!(!opening_only.has_quota_family());
        assert!(quota.has_quota_family());
        assert!(codex.has_quota_family());
        assert!(!UpstreamMeta::default().has_quota_family());
    }

    #[test]
    fn merge_keeps_opening_usage_and_quota_from_either_side() {
        // Arrange
        let quota = AnthropicUnifiedQuota {
            status: Some("allowed".into()),
            ..Default::default()
        };
        let opening_side = UpstreamMeta::from_opening_usage(opening(42));
        let quota_side = UpstreamMeta::from_anthropic_unified(quota.clone());

        // Act
        let merged = opening_side.clone().merge(quota_side.clone());
        let reversed = quota_side.merge(opening_side);

        // Assert
        assert_eq!(merged, reversed);
        assert_eq!(merged.anthropic_unified, Some(quota));
        assert_eq!(merged.opening_usage.map(|o| o.input_tokens), Some(42));
    }

    #[test]
    fn merge_prefers_self_when_both_sides_carry_a_family() {
        // Arrange
        let mine = UpstreamMeta::from_opening_usage(opening(1));
        let theirs = UpstreamMeta::from_opening_usage(opening(2));

        // Act
        let merged = mine.merge(theirs);

        // Assert
        assert_eq!(merged.opening_usage.map(|o| o.input_tokens), Some(1));
    }

    #[test]
    fn codex_quota_retains_all_named_fields_and_extras() {
        // Arrange
        let quota = CodexQuota {
            active_limit: Some("weekly".into()),
            primary_used_percent: Some("42.5".into()),
            primary_reset_at: Some("1754179200".into()),
            extras: vec![("secondary-used-percent".into(), "7".into())],
        };

        // Act
        let echoed = quota.clone();

        // Assert
        assert_eq!(echoed.active_limit.as_deref(), Some("weekly"));
        assert_eq!(echoed.primary_used_percent.as_deref(), Some("42.5"));
        assert_eq!(echoed.primary_reset_at.as_deref(), Some("1754179200"));
        assert_eq!(
            echoed.extras,
            vec![("secondary-used-percent".to_string(), "7".to_string())]
        );
        assert_eq!(echoed, quota);
    }
}
