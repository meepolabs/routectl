//! The OpenAI chat-lane request shape.
//!
//! One rule decides which model ids get the modern OpenAI chat-completions
//! shape (sampling parameters dropped, `max_tokens` sent as
//! `max_completion_tokens`): [`uses_modern_chat_shape`]. Only the OpenAI and
//! DeepSeek reasoning dialects consult it; vLLM, OpenRouter and passthrough
//! leave the body as the client sent it.

/// The vendor segment Bedrock Mantle puts in front of OpenAI model ids.
/// Only this exact segment is stripped, and only once: a generic `<word>.`
/// strip would cut dotted versions such as `gpt-5.4` down to their tail.
const OPENAI_VENDOR_PREFIX: &str = "openai.";

/// Id prefixes, on the lowercased bare id, of the OpenAI families served the
/// modern chat shape.
const MODERN_CHAT_PREFIXES: [&str; 4] = ["gpt-", "o1", "o3", "o4"];

/// An id fragment, anywhere in the lowercased id, naming a reasoner model.
const REASONER_FRAGMENT: &str = "reasoner";

/// Whether `model_id` gets the modern chat shape: true when the id,
/// lowercased with at most one leading `openai.` stripped, starts with
/// `gpt-`, `o1`, `o3` or `o4`, or when the lowercased id contains
/// `reasoner`.
///
/// The rule is deliberately coarse: every `gpt-` id matches, including chat
/// models that would accept sampling parameters. Lanes that need those
/// parameters on a newer OpenAI model belong on the Responses API.
///
/// Baked seed verdict: the shape is fixed before send rather than learned.
/// SEED-VERDICT: structural -- no learned verdict renames a field and no
/// learned row covers logprobs, so the modern shape is set before send; the
/// rule is static by operator decision.
pub fn uses_modern_chat_shape(model_id: &str) -> bool {
    let lower = model_id.to_ascii_lowercase();
    let bare = lower.strip_prefix(OPENAI_VENDOR_PREFIX).unwrap_or(&lower);
    MODERN_CHAT_PREFIXES
        .iter()
        .any(|prefix| bare.starts_with(prefix))
        || lower.contains(REASONER_FRAGMENT)
}

#[cfg(test)]
mod tests {
    use super::uses_modern_chat_shape;

    #[test]
    fn modern_chat_shape_matches_the_openai_families_and_reasoners() {
        let rows = [
            ("bare gpt-5", "gpt-5"),
            ("uppercase bare", "GPT-5"),
            ("vendor-prefixed", "openai.gpt-5.6-terra"),
            ("uppercase vendor-prefixed", "OPENAI.GPT-5"),
            ("o1 family", "o1-mini"),
            ("o3 family", "o3"),
            ("o4 family", "o4-mini"),
            ("older gpt-4o", "gpt-4o"),
            ("gpt-5 chat line", "gpt-5-chat-latest"),
            ("gpt-5.4", "gpt-5.4"),
            ("gpt-5.5", "gpt-5.5"),
            ("gpt-6 line", "gpt-6-sol"),
            ("open-weight gpt-oss", "gpt-oss-120b"),
            ("deepseek reasoner", "deepseek-reasoner"),
        ];
        for (name, id) in rows {
            assert!(uses_modern_chat_shape(id), "{name}: {id} must match");
        }
    }

    #[test]
    fn modern_chat_shape_refuses_other_ids() {
        let rows = [
            ("no dash after gpt", "gpt5"),
            ("gpt- not at the start", "xgpt-5"),
            ("vendor segment twice", "openai.openai.gpt-5"),
            ("slash vendor separator", "openai/gpt-5"),
            ("anthropic model", "claude-opus-4-7"),
            ("deepseek chat", "deepseek-chat"),
            ("open-weight llama", "llama-3.3-70b"),
            ("empty id", ""),
        ];
        for (name, id) in rows {
            assert!(!uses_modern_chat_shape(id), "{name}: {id} must not match");
        }
    }
}
