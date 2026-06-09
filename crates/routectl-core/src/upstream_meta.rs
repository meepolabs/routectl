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
//! does NOT reinterpret Anthropic vocabulary. The unified-family field
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
}

impl UpstreamMeta {
    /// Construct an `UpstreamMeta` carrying only the Anthropic unified
    /// quota family. The common shape today; a thin ctor keeps call
    /// sites free of struct-update churn when new sub-structs land.
    pub fn from_anthropic_unified(quota: AnthropicUnifiedQuota) -> Self {
        Self {
            anthropic_unified: Some(quota),
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
///   - `utilization` -> `utilization`
///   - `overage-utilization` -> `overage_utilization`
///   - `representative-claim` -> `representative_claim`
///   - `reset` -> `reset`
///
/// Any other `anthropic-ratelimit-unified-<suffix>` header lands in
/// `extras` as `(suffix, value)` so a future suffix is observable
/// without a code change.
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct AnthropicUnifiedQuota {
    pub status: Option<String>,
    pub overage_status: Option<String>,
    pub utilization: Option<String>,
    pub overage_utilization: Option<String>,
    pub representative_claim: Option<String>,
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
}
