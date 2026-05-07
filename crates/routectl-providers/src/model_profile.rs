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

    /// Use Anthropic's `adaptive` thinking type instead of fixed
    /// `budget_tokens`. Set for Opus 4.7+. Read by `anthropic_api::request`.
    pub supports_adaptive_thinking: bool,

    /// Forward `chat_template_kwargs` (vLLM/DashScope/some NIM endpoints).
    /// Set on a per-model basis when the model is served by a thinking
    /// model that needs `enable_thinking`. Read by `openai_compat::request`.
    pub uses_chat_template_kwargs: bool,
}

impl ModelProfile {
    /// All-false default. Used for models we have no specific knowledge
    /// of, which should pass requests through unchanged.
    pub const DEFAULT: ModelProfile = ModelProfile {
        pattern: "",
        kind: MatchKind::Prefix,
        drops_sampling_params: false,
        requires_reasoning_effort: false,
        supports_adaptive_thinking: false,
        uses_chat_template_kwargs: false,
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
    // Anthropic Opus 4.7+: adaptive thinking. Substring so vendor-prefixed
    // model names like `anthropic/claude-opus-4-7-20260301` still match.
    ModelProfile {
        pattern: "opus-4-7",
        kind: MatchKind::Substring,
        supports_adaptive_thinking: true,
        ..ModelProfile::DEFAULT
    },
    ModelProfile {
        pattern: "opus-4-8",
        kind: MatchKind::Substring,
        supports_adaptive_thinking: true,
        ..ModelProfile::DEFAULT
    },
    ModelProfile {
        pattern: "opus-4-9",
        kind: MatchKind::Substring,
        supports_adaptive_thinking: true,
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
    fn anthropic_opus_4_7_matches() {
        let p = profile_for("claude-opus-4-7");
        assert!(p.supports_adaptive_thinking);
    }

    #[test]
    fn anthropic_vendor_prefixed_opus_4_7_matches() {
        // OpenRouter-style id with vendor prefix.
        let p = profile_for("anthropic/claude-opus-4-7-20260301");
        assert!(p.supports_adaptive_thinking);
    }

    #[test]
    fn anthropic_opus_4_5_does_not_match() {
        let p = profile_for("claude-opus-4-5");
        assert!(!p.supports_adaptive_thinking);
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
        assert!(!p.supports_adaptive_thinking);
        assert!(!p.uses_chat_template_kwargs);
    }
}
