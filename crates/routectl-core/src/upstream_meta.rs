//! Transport-internal carrier for non-canonical upstream metadata that
//! rides alongside a `ChatResponse` / `ChatChunk` without ever touching
//! the client-facing wire. Today it carries Anthropic's
//! `anthropic-ratelimit-unified-*` quota/overage observability family
//! parsed off the anthropic-api egress response headers. Skip-serialized
//! (see the `#[serde(skip)]` fields on `ChatResponse` / `ChatChunk`);
//! never on the wire.
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
}

impl UpstreamMeta {
    /// Construct an `UpstreamMeta` carrying only the Anthropic unified
    /// quota family. The common shape today; a thin ctor keeps call
    /// sites free of struct-update churn when new sub-structs land.
    pub const fn from_anthropic_unified(quota: AnthropicUnifiedQuota) -> Self {
        Self {
            anthropic_unified: Some(quota),
            codex: None,
        }
    }

    /// Construct an `UpstreamMeta` carrying only the Codex quota family.
    pub const fn from_codex(quota: CodexQuota) -> Self {
        Self {
            anthropic_unified: None,
            codex: Some(quota),
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
