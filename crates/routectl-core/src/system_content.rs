//! Top-level `system` field on a request.
//!
//! Anthropic accepts either a flat string or an array of `TextBlockParam`
//! objects with optional `cache_control` per block. The hub keeps both
//! shapes so an Anthropic-in / Anthropic-out round-trip preserves the
//! per-block cache markers exactly. The OpenAI ingress emits the flat
//! string form by lifting Role::System messages.

use std::fmt;

use serde::de::value::SeqAccessDeserializer;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::cache_control::CacheControl;
use crate::member_slots::{MemberKey, next_member_key};

/// `system` field shape. Untagged: a JSON string parses as `Text(s)`,
/// an array parses as `Blocks(blocks)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SystemContent {
    /// Flat system-prompt string.
    Text(String),
    /// Array of typed system blocks with per-block cache markers.
    Blocks(Vec<SystemBlock>),
}

/// One typed text block inside a system array. Mirrors Anthropic's
/// `TextBlockParam` shape (no image/document blocks are valid here per
/// spec; we keep the schema strict).
#[derive(Debug, Clone, Serialize)]
pub struct SystemBlock {
    /// Always `"text"` on the wire. Defaulted on deserialize so callers
    /// can build a `SystemBlock` programmatically without repeating it.
    #[serde(rename = "type", default = "default_text_type")]
    pub kind: String,
    /// The block's text content.
    pub text: String,
    /// Optional cache breakpoint marker on this block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
    /// Optional citation metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub citations: Option<Value>,
    /// Unmodeled wire members of this node, filled by the deserializer for
    /// a same-dialect egress to re-emit. Never written by canonical
    /// serialization.
    #[serde(skip)]
    pub extras: Map<String, Value>,
}

fn default_text_type() -> String {
    "text".into()
}

/// Wire member names [`SystemBlock`] models, in field order. Every other
/// member of a system block object is an extra.
pub(crate) const SYSTEM_BLOCK_MEMBERS: [&str; 4] = ["type", "text", "cache_control", "citations"];

const KIND: usize = 0;
const TEXT: usize = 1;
const CACHE_CONTROL: usize = 2;
const CITATIONS: usize = 3;

/// The positional form serde derives for a struct (fields in order), under
/// the same type name so its error text names `SystemBlock`.
mod positional {
    use serde::Deserialize;
    use serde_json::Value;

    use super::{CacheControl, default_text_type};

    #[derive(Deserialize)]
    pub(super) struct SystemBlock {
        #[serde(rename = "type", default = "default_text_type")]
        pub(super) kind: String,
        pub(super) text: String,
        #[serde(default)]
        pub(super) cache_control: Option<CacheControl>,
        #[serde(default)]
        pub(super) citations: Option<Value>,
    }
}

/// Reads typed members straight into their fields as they arrive, so
/// defaults, null handling, error text and error positions are those of the
/// derived struct; every other member is kept in `extras`.
struct SystemBlockVisitor;

impl<'de> Visitor<'de> for SystemBlockVisitor {
    type Value = SystemBlock;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("struct SystemBlock")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<SystemBlock, A::Error> {
        let mut kind: Option<String> = None;
        let mut text: Option<String> = None;
        let mut cache_control: Option<Option<CacheControl>> = None;
        let mut citations: Option<Option<Value>> = None;
        let mut extras = Map::new();
        while let Some(key) = next_member_key(&mut map, &SYSTEM_BLOCK_MEMBERS)? {
            match key {
                MemberKey::Slot(KIND) => set_once(&mut kind, KIND, &mut map)?,
                MemberKey::Slot(TEXT) => set_once(&mut text, TEXT, &mut map)?,
                MemberKey::Slot(CACHE_CONTROL) => {
                    set_once(&mut cache_control, CACHE_CONTROL, &mut map)?;
                }
                MemberKey::Slot(CITATIONS) => set_once(&mut citations, CITATIONS, &mut map)?,
                MemberKey::Slot(_) => unreachable!("slot outside SYSTEM_BLOCK_MEMBERS"),
                MemberKey::Other(name) => {
                    extras.insert(name, map.next_value()?);
                }
            }
        }
        Ok(SystemBlock {
            kind: kind.unwrap_or_else(default_text_type),
            text: text.ok_or_else(|| de::Error::missing_field("text"))?,
            cache_control: cache_control.flatten(),
            citations: citations.flatten(),
            extras,
        })
    }

    fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<SystemBlock, A::Error> {
        let block = positional::SystemBlock::deserialize(SeqAccessDeserializer::new(seq))?;
        Ok(SystemBlock {
            kind: block.kind,
            text: block.text,
            cache_control: block.cache_control,
            citations: block.citations,
            extras: Map::new(),
        })
    }
}

/// Read the value of typed member `slot` into `field`; a second occurrence
/// is serde's duplicate-field error.
fn set_once<'de, T: Deserialize<'de>, A: MapAccess<'de>>(
    field: &mut Option<T>,
    slot: usize,
    map: &mut A,
) -> Result<(), A::Error> {
    if field.is_some() {
        return Err(de::Error::duplicate_field(SYSTEM_BLOCK_MEMBERS[slot]));
    }
    *field = Some(map.next_value()?);
    Ok(())
}

impl<'de> Deserialize<'de> for SystemBlock {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_struct("SystemBlock", &SYSTEM_BLOCK_MEMBERS, SystemBlockVisitor)
    }
}

impl SystemContent {
    /// Concatenate all text into a flat string. Used by egresses that
    /// don't support structured system blocks (i.e. OpenAI-compat).
    pub fn flatten(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Blocks(blocks) => blocks
                .iter()
                .map(|b| b.text.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    /// True when this carries no meaningful system text: an empty or
    /// whitespace-only `Text`, or `Blocks` in which every block's text is
    /// empty or whitespace-only (including an empty block array).
    ///
    /// Egresses use this to decide that a canonical `system` supplied on the
    /// wire is equivalent to none at all, so no upstream receives a
    /// meaningless empty system value.
    pub fn is_blank(&self) -> bool {
        match self {
            Self::Text(s) => s.trim().is_empty(),
            Self::Blocks(blocks) => blocks.iter().all(|b| b.text.trim().is_empty()),
        }
    }

    /// Iterator over `cache_control` markers in the order they appear in
    /// the system position of the cache prefix.
    pub fn cache_controls(&self) -> impl Iterator<Item = &CacheControl> {
        let blocks: &[SystemBlock] = match self {
            Self::Text(_) => &[],
            Self::Blocks(b) => b,
        };
        blocks.iter().filter_map(|b| b.cache_control.as_ref())
    }
}

#[cfg(test)]
#[path = "system_content_extras_tests.rs"]
mod extras_tests;

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
                extras: Map::new(),
            },
            SystemBlock {
                kind: "text".into(),
                text: "two".into(),
                cache_control: None,
                citations: None,
                extras: Map::new(),
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
    fn empty_and_whitespace_text_are_blank() {
        assert!(SystemContent::Text(String::new()).is_blank());
        assert!(SystemContent::Text("   \n\t ".into()).is_blank());
        assert!(!SystemContent::Text("you are helpful".into()).is_blank());
    }

    #[test]
    fn blocks_are_blank_when_every_block_text_is_blank() {
        let blank = SystemContent::Blocks(vec![
            SystemBlock {
                kind: "text".into(),
                text: String::new(),
                cache_control: None,
                citations: None,
                extras: Map::new(),
            },
            SystemBlock {
                kind: "text".into(),
                text: "  \n".into(),
                cache_control: None,
                citations: None,
                extras: Map::new(),
            },
        ]);
        assert!(blank.is_blank());
        assert!(SystemContent::Blocks(vec![]).is_blank());
    }

    #[test]
    fn blocks_are_not_blank_when_one_block_has_text() {
        let mixed = SystemContent::Blocks(vec![
            SystemBlock {
                kind: "text".into(),
                text: "  ".into(),
                cache_control: None,
                citations: None,
                extras: Map::new(),
            },
            SystemBlock {
                kind: "text".into(),
                text: "real prompt".into(),
                cache_control: None,
                citations: None,
                extras: Map::new(),
            },
        ]);
        assert!(!mixed.is_blank());
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
                extras: Map::new(),
            },
            SystemBlock {
                kind: "text".into(),
                text: "b".into(),
                cache_control: None,
                citations: None,
                extras: Map::new(),
            },
            SystemBlock {
                kind: "text".into(),
                text: "c".into(),
                cache_control: Some(CacheControl::ephemeral_1h()),
                citations: None,
                extras: Map::new(),
            },
        ]);
        let collected: Vec<&str> = s.cache_controls().map(|c| c.effective_ttl()).collect();
        assert_eq!(collected, vec!["5m", "1h"]);
    }
}
