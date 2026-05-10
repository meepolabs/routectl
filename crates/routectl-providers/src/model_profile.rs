//! Per-model quirk registry.
//!
//! Some upstream models reject params that others accept (`temperature` on
//! OpenAI's o-series), require unique reasoning controls (Anthropic Opus
//! 4.7+ adaptive thinking), or strip request-history fields that 4xx
//! otherwise (DeepSeek's `reasoning_content`). Rather than scatter
//! `model.starts_with("o3") || model.contains("reasoner") || ...` checks
//! across each provider's request normalizer, every quirk lives as one row
//! in the [`PROFILES`] table below.
//!
//! Adding a new model quirk: append a `ModelProfile` row. The compiler
//! enforces the field set; no other file changes needed.
//!
//! Lookup: [`profile_for`] takes a model id string and returns the first
//! matching profile in declaration order, or [`DEFAULT`] if none match.
//! Match semantics are governed by [`MatchKind`].

/// How a model id is matched against [`ModelProfile::pattern`].
///
/// `Prefix` is faster and more specific -- e.g. `o3` to catch `o3-mini`,
/// `o3-pro`, etc. without accidentally matching `o3` substrings inside
/// longer ids.
///
/// `Substring` is permissive -- e.g. `reasoner` to catch
/// `deepseek-reasoner`, `deepseek-reasoner-r2`, or any future
/// `reasoner`-suffixed variant; `opus-4-7` to catch
/// `anthropic/claude-opus-4-7-20260301` or any vendor-prefixed shape.
///
/// Matching is case-insensitive: the lookup lowercases the model id once
/// before comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    Prefix,
    Substring,
}

/// A single per-model row. Each boolean flag controls one specific
/// behavior in a provider's request-shaping path. Defaults are all
/// `false` so adding a flag to the struct without touching the table
/// rows leaves existing models unaffected.
///
/// When adding a flag here, also add a doc comment explaining what it
/// gates and which provider reads it.
///
/// ## Compiled-here vs. TOML-on-the-provider
///
/// Quirks that fit a stable model-name lineage with a reliable string
/// match (e.g. OpenAI o-series `drops_sampling_params`) belong here.
/// Quirks that ship on Anthropic's release cadence with no clean
/// naming pattern (e.g. `adaptive_thinking` for Opus 4.7+) belong as
/// per-provider TOML flags on the relevant `*Config` struct, NOT in
/// this table -- otherwise routectl is racing the model release
/// schedule on every Anthropic update. The current criterion: if the
/// pattern would false-positive on adjacent SKUs (e.g. `opus-4-`
/// would catch `opus-4-5`/`4-6` which still want the legacy shape)
/// AND we expect more variants imminently, prefer TOML.
///
/// `suggests_adaptive_thinking` is the bridge between the two: a
/// COMPILED hint (`opus-4-7` substring, etc.) that does NOT change
/// behavior on its own, but emits a startup/request-time WARN so an
/// operator sees "you probably want to set
/// `adaptive_thinking = true` on this provider" without having to
/// trip the upstream 400 first.
#[derive(Debug, Clone, Copy)]
pub struct ModelProfile {
    /// Model id (or fragment of it) to match.
    pub pattern: &'static str,
    /// Whether `pattern` is matched as a prefix or substring.
    pub kind: MatchKind,

    /// Drop `temperature`, `top_p`, presence/frequency penalties, and
    /// logprobs from the outgoing request body. Set for OpenAI o-series
    /// and DeepSeek `reasoner` variants which 400 on these params.
    /// Read by `openai_compat::request`.
    pub drops_sampling_params: bool,

    /// Translate `reasoning.effort` to the provider's effort param.
    /// Today the openai dialect always does this when `effort` is set;
    /// the flag is here to make the policy explicit and to support a
    /// future provider that needs effort-only-when-allowed.
    /// Read by `openai_compat::request`.
    pub requires_reasoning_effort: bool,

    /// Forward `chat_template_kwargs` (vLLM/DashScope/some NIM endpoints).
    /// Set on a per-model basis when the model is served by a thinking
    /// model that needs `enable_thinking`. Read by `openai_compat::request`.
    pub uses_chat_template_kwargs: bool,

    /// Hint-only: this model id matches a pattern that probably wants
    /// the operator to set `adaptive_thinking = true` on the relevant
    /// `AnthropicApiConfig` / `BedrockConfig` (the new wire shape
    /// Anthropic introduced for Opus 4.7+). Does NOT change request
    /// shape; the actual rewrite is gated by the TOML flag, not by
    /// this hint. Read by `anthropic_api::request::normalize` solely
    /// to emit a one-shot WARN that points the operator at the flag
    /// when their request is about to hit a 400. Set this on rows for
    /// new Claude models that ship the adaptive shape, AS THEY LAND
    /// in the wild -- so an operator who configures the model the
    /// day it ships gets the WARN automatically without waiting on a
    /// behavior-changing release.
    pub suggests_adaptive_thinking: bool,
}

impl ModelProfile {
    /// All-false default. Used for models we have no specific knowledge
    /// of, which should pass requests through unchanged.
    pub const DEFAULT: ModelProfile = ModelProfile {
        pattern: "",
        kind: MatchKind::Prefix,
        drops_sampling_params: false,
        requires_reasoning_effort: false,
        uses_chat_template_kwargs: false,
        suggests_adaptive_thinking: false,
    };

    /// Test the profile's pattern against a (lowercase) model id.
    fn matches(&self, lower_model: &str) -> bool {
        match self.kind {
            MatchKind::Prefix => lower_model.starts_with(self.pattern),
            MatchKind::Substring => lower_model.contains(self.pattern),
        }
    }
}

/// The full registry of per-model quirks, in declaration order. The
/// first match wins, so list more-specific patterns before
/// less-specific ones if collisions are possible.
pub const PROFILES: &[ModelProfile] = &[
    // OpenAI reasoning-only series: drop sampling params, expect reasoning_effort.
    ModelProfile {
        pattern: "o1",
        kind: MatchKind::Prefix,
        drops_sampling_params: true,
        requires_reasoning_effort: true,
        ..ModelProfile::DEFAULT
    },
    ModelProfile {
        pattern: "o3",
        kind: MatchKind::Prefix,
        drops_sampling_params: true,
        requires_reasoning_effort: true,
        ..ModelProfile::DEFAULT
    },
    ModelProfile {
        pattern: "o4",
        kind: MatchKind::Prefix,
        drops_sampling_params: true,
        requires_reasoning_effort: true,
        ..ModelProfile::DEFAULT
    },
    ModelProfile {
        pattern: "gpt-5",
        kind: MatchKind::Prefix,
        drops_sampling_params: true,
        requires_reasoning_effort: true,
        ..ModelProfile::DEFAULT
    },
    // DeepSeek reasoner variants. Substring so `reasoner-r2`, `coder-reasoner`,
    // etc. are all picked up automatically.
    ModelProfile {
        pattern: "reasoner",
        kind: MatchKind::Substring,
        drops_sampling_params: true,
        ..ModelProfile::DEFAULT
    },
    // Anthropic Opus 4.7+ uses the adaptive thinking wire shape
    // (`thinking.type = "adaptive"` + top-level `output_config.effort`).
    // This is a HINT only -- the rewrite itself is gated by the
    // provider's TOML `adaptive_thinking` flag. The hint exists so an
    // operator who configures `model_id =
    // "global.anthropic.claude-opus-4-7-..."` without setting the
    // flag sees a WARN at request-normalize time pointing them at
    // the fix, instead of a cryptic upstream 400. Substring so
    // `claude-opus-4-7-20260301`, `global.anthropic.claude-opus-4-7-v1:0`,
    // and any vendor-prefixed shape all match.
    ModelProfile {
        pattern: "opus-4-7",
        kind: MatchKind::Substring,
        suggests_adaptive_thinking: true,
        ..ModelProfile::DEFAULT
    },
];

/// Default profile reference for models not in `PROFILES`.
pub static DEFAULT_PROFILE: ModelProfile = ModelProfile::DEFAULT;

/// Look up the profile for `model_id`. Returns the first matching row
/// in [`PROFILES`], or [`DEFAULT_PROFILE`] if none match.
///
/// Matching is case-insensitive.
pub fn profile_for(model_id: &str) -> &'static ModelProfile {
    let lower = model_id.to_ascii_lowercase();
    for p in PROFILES {
        if p.matches(&lower) {
            return p;
        }
    }
    &DEFAULT_PROFILE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_o1_matches_prefix() {
        let p = profile_for("o1");
        assert!(p.drops_sampling_params);
        assert!(p.requires_reasoning_effort);
    }

    #[test]
    fn openai_o3_mini_matches_prefix() {
        let p = profile_for("o3-mini");
        assert!(p.drops_sampling_params);
        assert!(p.requires_reasoning_effort);
    }

    #[test]
    fn openai_o4_pro_matches_prefix() {
        let p = profile_for("o4-pro-2026-01-15");
        assert!(p.drops_sampling_params);
    }

    #[test]
    fn openai_gpt5_matches_prefix() {
        let p = profile_for("gpt-5");
        assert!(p.drops_sampling_params);
        assert!(p.requires_reasoning_effort);
    }

    #[test]
    fn openai_gpt4_does_not_match() {
        let p = profile_for("gpt-4o-mini");
        assert!(!p.drops_sampling_params);
        assert!(!p.requires_reasoning_effort);
    }

    #[test]
    fn deepseek_reasoner_matches_substring() {
        let p = profile_for("deepseek-reasoner");
        assert!(p.drops_sampling_params);
        assert!(!p.requires_reasoning_effort);
    }

    #[test]
    fn deepseek_reasoner_r2_matches_substring() {
        // Future variant -- substring rule means no code change needed.
        let p = profile_for("deepseek-reasoner-r2");
        assert!(p.drops_sampling_params);
    }

    #[test]
    fn deepseek_chat_does_not_match() {
        let p = profile_for("deepseek-chat");
        assert!(!p.drops_sampling_params);
    }

    #[test]
    fn case_insensitive_match() {
        let p = profile_for("O3-MINI");
        assert!(p.drops_sampling_params);
    }

    #[test]
    fn unknown_model_returns_default() {
        let p = profile_for("totally-unknown-model-xyz");
        assert!(!p.drops_sampling_params);
        assert!(!p.requires_reasoning_effort);
        assert!(!p.uses_chat_template_kwargs);
        assert!(!p.suggests_adaptive_thinking);
    }

    #[test]
    fn opus_4_7_suggests_adaptive_thinking_via_substring() {
        // Bare model id (claude-code wire shape after prefix-strip).
        let p = profile_for("claude-opus-4-7");
        assert!(p.suggests_adaptive_thinking);
        // Vendor-prefixed Bedrock inference profile id.
        let p = profile_for("global.anthropic.claude-opus-4-7-v1:0");
        assert!(p.suggests_adaptive_thinking);
        // Vendor-prefixed cross-region profile id.
        let p = profile_for("us.anthropic.claude-opus-4-7-20260301");
        assert!(p.suggests_adaptive_thinking);
    }

    #[test]
    fn opus_4_6_does_not_suggest_adaptive_thinking() {
        // Older Claude families still want the legacy thinking shape.
        // The hint must not trigger on them.
        let p = profile_for("claude-opus-4-6");
        assert!(!p.suggests_adaptive_thinking);
        let p = profile_for("global.anthropic.claude-sonnet-4-6");
        assert!(!p.suggests_adaptive_thinking);
        let p = profile_for("claude-haiku-4-5-20251001");
        assert!(!p.suggests_adaptive_thinking);
    }
}
