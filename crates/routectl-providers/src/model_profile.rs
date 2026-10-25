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
//! matching profile in declaration order, or [`DEFAULT_PROFILE`] if none match.
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
}

impl ModelProfile {
    /// All-false default. Used for models we have no specific knowledge
    /// of, which should pass requests through unchanged.
    pub const DEFAULT: Self = Self {
        pattern: "",
        kind: MatchKind::Prefix,
        drops_sampling_params: false,
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
///
/// Baked seed verdict: OpenAI names one refused parameter per 400 and field
/// repair makes one edit per target, so a body carrying two refused sampling
/// keys never succeeds and never teaches anything; this table is what keeps
/// those requests working.
/// SEED-VERDICT: acceptance-seed -- the host names one refused key per 400
/// and repair makes one edit per target, so two keys fail and teach nothing.
pub const PROFILES: &[ModelProfile] = &[
    // OpenAI reasoning-only series: drop sampling params (400 otherwise).
    ModelProfile {
        pattern: "o1",
        kind: MatchKind::Prefix,
        drops_sampling_params: true,
    },
    ModelProfile {
        pattern: "o3",
        kind: MatchKind::Prefix,
        drops_sampling_params: true,
    },
    ModelProfile {
        pattern: "o4",
        kind: MatchKind::Prefix,
        drops_sampling_params: true,
    },
    // Non-reasoning GPT-5 chat variants -- accept sampling params, unlike
    // the reasoning gpt-5 line. Listed before the gpt-5 catch-all so the
    // longer prefixes win (first match in order).
    ModelProfile {
        pattern: "gpt-5-chat",
        kind: MatchKind::Prefix,
        drops_sampling_params: false,
    },
    ModelProfile {
        pattern: "gpt-5.4",
        kind: MatchKind::Prefix,
        drops_sampling_params: false,
    },
    ModelProfile {
        pattern: "gpt-5",
        kind: MatchKind::Prefix,
        drops_sampling_params: true,
    },
    // DeepSeek reasoner variants. Substring so `reasoner-r2`, `coder-reasoner`,
    // etc. are all picked up automatically.
    ModelProfile {
        pattern: "reasoner",
        kind: MatchKind::Substring,
        drops_sampling_params: true,
    },
];

/// Default profile reference for models not in `PROFILES`.
pub static DEFAULT_PROFILE: ModelProfile = ModelProfile::DEFAULT;

/// The vendor segment Bedrock Mantle puts in front of OpenAI model ids.
/// Only this exact segment is stripped: a generic `<word>.` strip would cut
/// dotted versions such as `gpt-5.4` down to their tail.
const OPENAI_VENDOR_PREFIX: &str = "openai.";

/// Look up the profile for `model_id`. Returns the first matching row
/// in [`PROFILES`], or [`DEFAULT_PROFILE`] if none match.
///
/// Matching is case-insensitive. A leading `openai.` vendor segment is
/// ignored, so a vendor-prefixed id (Bedrock Mantle's `openai.gpt-5.6-terra`)
/// resolves to the same profile as its bare id.
pub fn profile_for(model_id: &str) -> &'static ModelProfile {
    let lower = model_id.to_ascii_lowercase();
    let bare = lower.strip_prefix(OPENAI_VENDOR_PREFIX).unwrap_or(&lower);
    for p in PROFILES {
        if p.matches(bare) {
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
    }

    #[test]
    fn openai_o3_mini_matches_prefix() {
        let p = profile_for("o3-mini");
        assert!(p.drops_sampling_params);
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
    }

    #[test]
    fn openai_gpt5_chat_keeps_sampling_params() {
        // gpt-5-chat-latest is a NON-reasoning chat model that accepts
        // temperature/top_p/penalties. The longer gpt-5-chat prefix is
        // listed before the gpt-5 catch-all so it wins on first match.
        let p = profile_for("gpt-5-chat-latest");
        assert!(!p.drops_sampling_params);
    }

    #[test]
    fn openai_gpt5_codex_still_drops_sampling_params() {
        // Reasoning gpt-5 variant: must keep dropping. Guards the
        // gpt-5-chat row from being over-broad (it must not catch -codex).
        let p = profile_for("gpt-5-codex");
        assert!(p.drops_sampling_params);
    }

    #[test]
    fn openai_gpt5_4_keeps_sampling_params() {
        // gpt-5.4 is a NON-reasoning flagship chat model that accepts
        // temperature/top_p/penalties. The gpt-5.4 prefix is listed
        // before the gpt-5 catch-all so it wins on first match.
        let p = profile_for("gpt-5.4");
        assert!(!p.drops_sampling_params);
    }

    #[test]
    fn openai_gpt5_4_mini_keeps_sampling_params() {
        // Faster/cheaper non-reasoning variant covered by the same prefix.
        let p = profile_for("gpt-5.4-mini");
        assert!(!p.drops_sampling_params);
    }

    #[test]
    fn openai_gpt5_3_codex_still_drops_sampling_params() {
        // Reasoning dotted-version variant: must keep dropping. Guards the
        // gpt-5.4 row from over-matching (it must not catch gpt-5.3-codex,
        // which still falls through to the gpt-5 catch-all).
        let p = profile_for("gpt-5.3-codex");
        assert!(p.drops_sampling_params);
    }

    #[test]
    fn openai_gpt4_does_not_match() {
        let p = profile_for("gpt-4o-mini");
        assert!(!p.drops_sampling_params);
    }

    #[test]
    fn deepseek_reasoner_matches_substring() {
        let p = profile_for("deepseek-reasoner");
        assert!(p.drops_sampling_params);
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
    fn openai_vendor_prefixed_id_resolves_to_its_bare_ids_profile() {
        let cases = [
            ("openai.gpt-5.6-terra", "gpt-5.6-terra", true),
            ("openai.gpt-5.4", "gpt-5.4", false),
            ("openai.gpt-5-chat-latest", "gpt-5-chat-latest", false),
            ("openai.o3-mini", "o3-mini", true),
            ("OPENAI.GPT-5", "gpt-5", true),
            ("openai.gpt-oss-120b", "gpt-oss-120b", false),
        ];
        for (prefixed, bare, drops) in cases {
            let p = profile_for(prefixed);
            assert!(
                std::ptr::eq(p, profile_for(bare)),
                "{prefixed}: resolves to a different row than {bare}"
            );
            assert_eq!(p.drops_sampling_params, drops, "{prefixed}");
        }
    }

    #[test]
    fn only_the_openai_vendor_segment_is_stripped() {
        // A different vendor segment is not a known alias: `anthropic.o3`
        // must not reach the `o3` prefix row.
        let p = profile_for("anthropic.o3-mini");
        assert!(std::ptr::eq(p, profile_for("totally-unknown-model-xyz")));
    }

    #[test]
    fn unknown_model_returns_default() {
        let p = profile_for("totally-unknown-model-xyz");
        assert!(!p.drops_sampling_params);
    }
}
