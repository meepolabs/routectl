//! Top-level `system` field on a request.
//!
//! Anthropic accepts either a flat string or an array of `TextBlockParam`
//! objects with optional `cache_control` per block. The hub keeps both
//! shapes so an Anthropic-in / Anthropic-out round-trip preserves the
//! per-block cache markers exactly. The OpenAI ingress emits the flat
//! string form by lifting Role::System messages.

use serde::{Deserialize, Serialize};

use crate::cache_control::CacheControl;

/// `system` field shape. Untagged: a JSON string parses as `Text(s)`,
/// an array parses as `Blocks(blocks)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SystemContent {
    Text(String),
    Blocks(Vec<SystemBlock>),
}

/// One typed text block inside a system array. Mirrors Anthropic's
/// `TextBlockParam` shape (no image/document blocks are valid here per
/// spec; we keep the schema strict).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemBlock {
    /// Always `"text"` on the wire. Defaulted on deserialize so callers
    /// can build a `SystemBlock` programmatically without repeating it.
    #[serde(rename = "type", default = "default_text_type")]
    pub kind: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub citations: Option<serde_json::Value>,
}

fn default_text_type() -> String {
    "text".into()
}

impl SystemContent {
    /// Concatenate all text into a flat string. Used by egresses that
    /// don't support structured system blocks (i.e. OpenAI-compat).
    pub fn flatten(&self) -> String {
        match self {
            SystemContent::Text(s) => s.clone(),
            SystemContent::Blocks(blocks) => blocks
                .iter()
                .map(|b| b.text.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    /// Iterator over `cache_control` markers in the order they appear in
    /// the system position of the cache prefix.
    pub fn cache_controls(&self) -> impl Iterator<Item = &CacheControl> {
        let blocks: &[SystemBlock] = match self {
            SystemContent::Text(_) => &[],
            SystemContent::Blocks(b) => b,
        };
        blocks.iter().filter_map(|b| b.cache_control.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn string_form_round_trips() {
        let v = json!("you are helpful");
        let s: SystemContent = serde_json::from_value(v.clone()).unwrap();
        assert!(matches!(s, SystemContent::Text(_)));
        assert_eq!(serde_json::to_value(&s).unwrap(), v);
    }

    #[test]
    fn blocks_form_with_cache_control_round_trips() {
        let v = json!([
            {"type": "text", "text": "system prompt", "cache_control": {"type": "ephemeral"}}
        ]);
        let s: SystemContent = serde_json::from_value(v.clone()).unwrap();
        assert!(matches!(s, SystemContent::Blocks(_)));
        assert_eq!(serde_json::to_value(&s).unwrap(), v);
    }

    #[test]
    fn flatten_concatenates_blocks_with_newlines() {
        let s = SystemContent::Blocks(vec![
            SystemBlock {
                kind: "text".into(),
                text: "one".into(),
                cache_control: None,
                citations: None,
            },
            SystemBlock {
                kind: "text".into(),
                text: "two".into(),
                cache_control: None,
                citations: None,
            },
        ]);
        assert_eq!(s.flatten(), "one\ntwo");
    }

    #[test]
    fn text_form_flattens_to_itself() {
        let s = SystemContent::Text("hi".into());
        assert_eq!(s.flatten(), "hi");
    }

    #[test]
    fn cache_controls_iter_is_empty_for_text_form() {
        let s = SystemContent::Text("hi".into());
        assert_eq!(s.cache_controls().count(), 0);
    }

    #[test]
    fn cache_controls_iter_yields_blocks_with_marker() {
        let s = SystemContent::Blocks(vec![
            SystemBlock {
                kind: "text".into(),
                text: "a".into(),
                cache_control: Some(CacheControl::ephemeral_5m()),
                citations: None,
            },
            SystemBlock {
                kind: "text".into(),
                text: "b".into(),
                cache_control: None,
                citations: None,
            },
            SystemBlock {
                kind: "text".into(),
                text: "c".into(),
                cache_control: Some(CacheControl::ephemeral_1h()),
                citations: None,
            },
        ]);
        let collected: Vec<&str> = s.cache_controls().map(|c| c.effective_ttl()).collect();
        assert_eq!(collected, vec!["5m", "1h"]);
    }
}
