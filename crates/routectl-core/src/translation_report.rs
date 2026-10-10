//! Per-attempt slot through which an egress reports the tool translations it
//! did not carry out faithfully.
//!
//! An egress may withhold a hosted tool the upstream cannot receive, or drop
//! a builtin its wire shape cannot express. The upstream then answers a
//! request that differs from the one the client sent, so its success or
//! rejection is not evidence about the withheld capability. The egress
//! records the reason here; the router reads it after the call returns.
//!
//! A reason carries only the canonical capability key -- never a tool label,
//! server URL, header, or any other caller-supplied value.

use std::sync::{Arc, Mutex, PoisonError};

use crate::safe_token::is_safe_token;

/// Canonical capability key for a tool type: the type with a trailing
/// `-YYYYMMDD` / `_YYYYMMDD` suffix removed, so `web_search_20250305` and
/// `web_search` share one key. Constructible only from a tool type, so it can
/// never hold anything else the caller sent alongside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslationKey(String);

impl TranslationKey {
    /// The key for a tool `type` tag, or `None` when the tag is not
    /// token-shaped or is nothing but a date suffix.
    pub fn from_tool_type(tool_type: &str) -> Option<Self> {
        if !is_safe_token(tool_type) {
            return None;
        }
        let key = strip_date_suffix(tool_type);
        (!key.is_empty()).then(|| Self(key.to_string()))
    }

    /// The canonical key string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Must match the router's feature-key derivation, which reduces a
/// `tools[]` entry's type the same way; otherwise a reason would name a key
/// no request-derived feature set contains.
fn strip_date_suffix(s: &str) -> &str {
    const SUFFIX_LEN: usize = 9;
    let Some(suffix_start) = s.len().checked_sub(SUFFIX_LEN) else {
        return s;
    };
    let bytes = s.as_bytes();
    let has_separator = matches!(bytes[suffix_start], b'-' | b'_');
    if has_separator && bytes[suffix_start + 1..].iter().all(u8::is_ascii_digit) {
        &s[..suffix_start]
    } else {
        s
    }
}

/// Why an attempt's wire body does not carry a capability the request asked
/// for. Closed set; each reason carries only the canonical key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranslationReason {
    /// The egress withheld or dropped a hosted / builtin tool, and the
    /// request was sent without it.
    HostedToolWithheld(TranslationKey),
    /// The egress refused to translate a tool without failing the request.
    TranslationRejected(TranslationKey),
}

/// Shared, clone-cheap slot of translation reasons. Every clone shares one
/// slot, so the copy the router keeps observes what the egress records on
/// the copy that rode the request.
#[derive(Debug, Clone, Default)]
pub struct TranslationReport {
    reasons: Arc<Mutex<Vec<TranslationReason>>>,
}

impl TranslationReport {
    /// Append `reason` unless it is already recorded, preserving first-seen
    /// order.
    pub fn record(&self, reason: TranslationReason) {
        let mut slot = self.reasons.lock().unwrap_or_else(PoisonError::into_inner);
        if !slot.contains(&reason) {
            slot.push(reason);
        }
    }

    /// Return the recorded reasons in first-seen order and empty the slot.
    pub fn take(&self) -> Vec<TranslationReason> {
        let mut slot = self.reasons.lock().unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut *slot)
    }
}

#[cfg(test)]
mod tests {
    use super::{TranslationKey, TranslationReason, TranslationReport};

    fn withheld(tool_type: &str) -> TranslationReason {
        TranslationReason::HostedToolWithheld(
            TranslationKey::from_tool_type(tool_type).expect("token-shaped tool type"),
        )
    }

    fn rejected(tool_type: &str) -> TranslationReason {
        TranslationReason::TranslationRejected(
            TranslationKey::from_tool_type(tool_type).expect("token-shaped tool type"),
        )
    }

    #[test]
    fn record_dedupes_and_keeps_first_seen_order() {
        let report = TranslationReport::default();

        report.record(withheld("web_search_20250305"));
        report.record(withheld("mcp"));
        report.record(withheld("web_search_20251102"));
        report.record(rejected("mcp"));

        assert_eq!(
            report.take(),
            vec![withheld("web_search"), withheld("mcp"), rejected("mcp")]
        );
    }

    #[test]
    fn take_empties_the_slot() {
        let report = TranslationReport::default();
        report.record(withheld("mcp"));

        let first = report.take();
        let second = report.take();

        assert_eq!(first, vec![withheld("mcp")]);
        assert!(second.is_empty());
    }

    #[test]
    fn clones_share_one_slot() {
        let router_copy = TranslationReport::default();
        let provider_copy = router_copy.clone();

        provider_copy.record(withheld("mcp"));
        router_copy.record(withheld("mcp"));
        router_copy.record(withheld("web_search"));

        assert_eq!(
            router_copy.take(),
            vec![withheld("mcp"), withheld("web_search")]
        );
        assert!(provider_copy.take().is_empty());
    }

    #[test]
    fn take_recovers_from_a_poisoned_lock() {
        let report = TranslationReport::default();
        report.record(withheld("mcp"));
        let poisoner = report.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.reasons.lock().unwrap();
            panic!("poison the slot");
        })
        .join();

        assert_eq!(report.take(), vec![withheld("mcp")]);
    }

    #[test]
    fn key_strips_a_trailing_date_suffix() {
        for (tool_type, key) in [
            ("web_search_20250305", "web_search"),
            ("computer-20241022", "computer"),
            ("mcp", "mcp"),
            ("web_search", "web_search"),
            ("tool_2025030x", "tool_2025030x"),
            ("x20250305", "x20250305"),
        ] {
            let derived = TranslationKey::from_tool_type(tool_type).expect("token-shaped");
            assert_eq!(derived.as_str(), key, "{tool_type}");
        }
    }

    #[test]
    fn key_refuses_a_non_token_or_date_only_type() {
        for tool_type in ["", "two words", "line\nbreak", "_20250305"] {
            assert!(
                TranslationKey::from_tool_type(tool_type).is_none(),
                "{tool_type:?} must yield no key"
            );
        }
    }
}
