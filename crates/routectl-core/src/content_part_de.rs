//! Single-pass `Deserialize` for [`ContentPart`].
//!
//! The block's members are drained once into named slots. The `type` tag
//! picks a [`KnownContentPart`] shape; when every typed member of that shape
//! is present and well-typed, the block becomes that variant and every
//! other member moves into the variant's `extras`. Otherwise the block
//! becomes `ContentPart::Other`, which keeps `type`, `cache_control` and the
//! remaining members verbatim.
//!
//! Accepted inputs and their outcomes are exactly those of the derived
//! `#[serde(untagged)]` pair this replaces (an internally tagged
//! `KnownContentPart`, then `Other`): a numeric tag selects a shape by
//! declaration index, a JSON array is read positionally as `[tag, fields...]`
//! and never yields `Other`, a malformed known block falls back to `Other`,
//! and a repeated typed member (possible only from a text body) counts as
//! malformed. Every rejection carries the derived error text. One known
//! difference, reachable only from a text body: a repeated member nested
//! inside `cache_control` is collapsed (last wins) instead of rejected.

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

use crate::cache_control::CacheControl;
use crate::content_part::{ContentPart, KnownContentPart};
use crate::member_slots::{MemberSlots, drain_seq};

const TYPE: usize = 0;
const CACHE_CONTROL: usize = 1;
const TEXT: usize = 2;
const CITATIONS: usize = 3;
const SOURCE: usize = 4;
const IMAGE_URL: usize = 5;
const FILE: usize = 6;
const TITLE: usize = 7;
const ID: usize = 8;
const NAME: usize = 9;
const INPUT: usize = 10;
const TOOL_USE_ID: usize = 11;
const CONTENT: usize = 12;
const IS_ERROR: usize = 13;
const THINKING: usize = 14;
const SIGNATURE: usize = 15;
const DATA: usize = 16;

/// Every member name any known shape types, indexed by the slot constants.
const MEMBER_NAMES: [&str; 17] = [
    "type",
    "cache_control",
    "text",
    "citations",
    "source",
    "image_url",
    "file",
    "title",
    "id",
    "name",
    "input",
    "tool_use_id",
    "content",
    "is_error",
    "thinking",
    "signature",
    "data",
];

const NO_MATCH: &str = "data did not match any variant of untagged enum ContentPart";

/// How a typed member is read: the canonical field type it fills.
#[derive(Clone, Copy)]
enum Field {
    /// `String`, required.
    Str,
    /// `Option<String>`; absent or null is `None`.
    OptStr,
    /// `Value`, required (a present null is kept).
    Json,
    /// `Option<Value>`; absent or null is `None`.
    OptJson,
    /// `Option<bool>`; absent or null is `None`.
    OptBool,
    /// `Option<CacheControl>`; absent or null is `None`.
    OptCacheControl,
}

/// A [`KnownContentPart`] variant, as the deserializer selects it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Text,
    Image,
    ImageUrl,
    File,
    Document,
    ToolUse,
    ToolResult,
    Thinking,
    RedactedThinking,
}

/// Wire tag of each shape, in `KnownContentPart` declaration order (a
/// numeric tag indexes this list).
pub const SHAPES: [(&str, Shape); 9] = [
    ("text", Shape::Text),
    ("image", Shape::Image),
    ("image_url", Shape::ImageUrl),
    ("file", Shape::File),
    ("document", Shape::Document),
    ("tool_use", Shape::ToolUse),
    ("tool_result", Shape::ToolResult),
    ("thinking", Shape::Thinking),
    ("redacted_thinking", Shape::RedactedThinking),
];

/// The wire tags alone, for error messages that list the expected tags.
const SHAPE_TAGS: [&str; SHAPES.len()] = {
    let mut tags = [""; SHAPES.len()];
    let mut i = 0;
    while i < SHAPES.len() {
        tags[i] = SHAPES[i].0;
        i += 1;
    }
    tags
};

impl Shape {
    fn of_tag(tag: &Value) -> Option<Self> {
        let index = match tag {
            Value::String(name) => SHAPES.iter().position(|(wire, _)| wire == name),
            Value::Number(n) => n.as_u64().and_then(|i| usize::try_from(i).ok()),
            _ => None,
        }?;
        SHAPES.get(index).map(|(_, shape)| *shape)
    }

    /// Typed members of this shape, in field declaration order (the order a
    /// positional array supplies them).
    const fn fields(self) -> &'static [(usize, Field)] {
        match self {
            Self::Text => &[
                (TEXT, Field::Str),
                (CITATIONS, Field::OptJson),
                (CACHE_CONTROL, Field::OptCacheControl),
            ],
            Self::Image => &[
                (SOURCE, Field::Json),
                (CACHE_CONTROL, Field::OptCacheControl),
            ],
            Self::ImageUrl => &[
                (IMAGE_URL, Field::Json),
                (CACHE_CONTROL, Field::OptCacheControl),
            ],
            Self::File => &[(FILE, Field::Json), (CACHE_CONTROL, Field::OptCacheControl)],
            Self::Document => &[
                (SOURCE, Field::Json),
                (TITLE, Field::OptStr),
                (CITATIONS, Field::OptJson),
                (CACHE_CONTROL, Field::OptCacheControl),
            ],
            Self::ToolUse => &[
                (ID, Field::Str),
                (NAME, Field::Str),
                (INPUT, Field::Json),
                (CACHE_CONTROL, Field::OptCacheControl),
            ],
            Self::ToolResult => &[
                (TOOL_USE_ID, Field::Str),
                (CONTENT, Field::Json),
                (IS_ERROR, Field::OptBool),
                (CACHE_CONTROL, Field::OptCacheControl),
            ],
            Self::Thinking => &[(THINKING, Field::Str), (SIGNATURE, Field::OptStr)],
            Self::RedactedThinking => &[(DATA, Field::Str)],
        }
    }

    /// The shape `part` was deserialized as.
    pub const fn of_part(part: &KnownContentPart) -> Self {
        match part {
            KnownContentPart::Text { .. } => Self::Text,
            KnownContentPart::Image { .. } => Self::Image,
            KnownContentPart::ImageUrl { .. } => Self::ImageUrl,
            KnownContentPart::File { .. } => Self::File,
            KnownContentPart::Document { .. } => Self::Document,
            KnownContentPart::ToolUse { .. } => Self::ToolUse,
            KnownContentPart::ToolResult { .. } => Self::ToolResult,
            KnownContentPart::Thinking { .. } => Self::Thinking,
            KnownContentPart::RedactedThinking { .. } => Self::RedactedThinking,
        }
    }

    /// Member names this shape types, the `type` tag excluded: the names a
    /// carried member of this shape can never take.
    pub fn typed_keys(self) -> impl Iterator<Item = &'static str> {
        self.fields().iter().map(|(slot, _)| MEMBER_NAMES[*slot])
    }

    /// Member names this shape types, `type` included.
    #[cfg(test)]
    pub fn typed_member_names(self) -> impl Iterator<Item = &'static str> {
        std::iter::once(MEMBER_NAMES[TYPE]).chain(self.typed_keys())
    }
}

type Slots = MemberSlots<{ MEMBER_NAMES.len() }>;

/// The parsed `cache_control` member: `Err` when present and malformed.
type CacheControlSlot = Result<Option<CacheControl>, ()>;

fn parse_cache_control(slots: &Slots) -> CacheControlSlot {
    match slots.get(CACHE_CONTROL) {
        None | Some(Value::Null) => Ok(None),
        Some(raw) => CacheControl::deserialize(raw).map(Some).map_err(drop),
    }
}

const fn field_fits(field: Field, value: Option<&Value>, cache_control: &CacheControlSlot) -> bool {
    match field {
        Field::Str => matches!(value, Some(Value::String(_))),
        Field::OptStr => matches!(value, None | Some(Value::Null | Value::String(_))),
        Field::Json => value.is_some(),
        Field::OptJson => true,
        Field::OptBool => matches!(value, None | Some(Value::Null | Value::Bool(_))),
        Field::OptCacheControl => cache_control.is_ok(),
    }
}

fn fits(shape: Shape, slots: &Slots, cache_control: &CacheControlSlot) -> bool {
    !slots.is_duplicated(TYPE)
        && shape.fields().iter().all(|&(slot, field)| {
            !slots.is_duplicated(slot) && field_fits(field, slots.get(slot), cache_control)
        })
}

fn take_str(slots: &mut Slots, slot: usize) -> String {
    match slots.take(slot) {
        Some(Value::String(s)) => s,
        _ => String::new(),
    }
}

fn take_opt_str(slots: &mut Slots, slot: usize) -> Option<String> {
    match slots.take(slot) {
        Some(Value::String(s)) => Some(s),
        _ => None,
    }
}

fn take_json(slots: &mut Slots, slot: usize) -> Value {
    slots.take(slot).unwrap_or(Value::Null)
}

fn take_opt_json(slots: &mut Slots, slot: usize) -> Option<Value> {
    slots.take(slot).filter(|v| !v.is_null())
}

fn take_opt_bool(slots: &mut Slots, slot: usize) -> Option<bool> {
    slots.take(slot).and_then(|v| v.as_bool())
}

/// Build `shape` from slots that [`fits`] accepted. Typed members are taken
/// first; every member left becomes `extras`.
fn build_known(
    shape: Shape,
    mut slots: Slots,
    cache_control: Option<CacheControl>,
) -> KnownContentPart {
    slots.take(TYPE);
    if shape
        .fields()
        .iter()
        .any(|(slot, _)| *slot == CACHE_CONTROL)
    {
        slots.take(CACHE_CONTROL);
    }
    let s = &mut slots;
    match shape {
        Shape::Text => KnownContentPart::Text {
            text: take_str(s, TEXT),
            citations: take_opt_json(s, CITATIONS),
            cache_control,
            extras: slots.into_rest(),
        },
        Shape::Image => KnownContentPart::Image {
            source: take_json(s, SOURCE),
            cache_control,
            extras: slots.into_rest(),
        },
        Shape::ImageUrl => KnownContentPart::ImageUrl {
            image_url: take_json(s, IMAGE_URL),
            cache_control,
            extras: slots.into_rest(),
        },
        Shape::File => KnownContentPart::File {
            file: take_json(s, FILE),
            cache_control,
            extras: slots.into_rest(),
        },
        Shape::Document => KnownContentPart::Document {
            source: take_json(s, SOURCE),
            title: take_opt_str(s, TITLE),
            citations: take_opt_json(s, CITATIONS),
            cache_control,
            extras: slots.into_rest(),
        },
        Shape::ToolUse => KnownContentPart::ToolUse {
            id: take_str(s, ID),
            name: take_str(s, NAME),
            input: take_json(s, INPUT),
            cache_control,
            extras: slots.into_rest(),
        },
        Shape::ToolResult => KnownContentPart::ToolResult {
            tool_use_id: take_str(s, TOOL_USE_ID),
            content: take_json(s, CONTENT),
            is_error: take_opt_bool(s, IS_ERROR),
            cache_control,
            extras: slots.into_rest(),
        },
        Shape::Thinking => KnownContentPart::Thinking {
            thinking: take_str(s, THINKING),
            signature: take_opt_str(s, SIGNATURE),
            extras: slots.into_rest(),
        },
        Shape::RedactedThinking => KnownContentPart::RedactedThinking {
            data: take_str(s, DATA),
            extras: slots.into_rest(),
        },
    }
}

/// The forward-compat reading: a string `type`, a well-formed or absent
/// `cache_control`, everything else verbatim.
fn build_other(mut slots: Slots, cache_control: CacheControlSlot) -> Option<ContentPart> {
    if slots.is_duplicated(TYPE) || slots.is_duplicated(CACHE_CONTROL) {
        return None;
    }
    let cache_control = cache_control.ok()?;
    let Some(Value::String(type_tag)) = slots.take(TYPE) else {
        return None;
    };
    slots.take(CACHE_CONTROL);
    Some(ContentPart::Other {
        type_tag,
        cache_control,
        extras: slots.into_rest(),
    })
}

fn from_members(slots: Slots) -> Option<ContentPart> {
    let cache_control = parse_cache_control(&slots);
    let shape = slots.get(TYPE).and_then(Shape::of_tag);
    match shape {
        Some(shape) if fits(shape, &slots, &cache_control) => Some(ContentPart::Known(
            build_known(shape, slots, cache_control.unwrap_or_default()),
        )),
        _ => build_other(slots, cache_control),
    }
}

fn from_positional<'de, A: SeqAccess<'de>>(mut seq: A) -> Result<Option<ContentPart>, A::Error> {
    let Some(shape) = seq
        .next_element::<Value>()?
        .as_ref()
        .and_then(Shape::of_tag)
    else {
        drain_seq(&mut seq)?;
        return Ok(None);
    };
    let mut slots = Slots::new(&MEMBER_NAMES);
    let order = shape.fields().iter().map(|(slot, _)| *slot);
    if !slots.fill_positional(order, &mut seq)? {
        return Ok(None);
    }
    let cache_control = parse_cache_control(&slots);
    if !fits(shape, &slots, &cache_control) {
        return Ok(None);
    }
    Ok(Some(ContentPart::Known(build_known(
        shape,
        slots,
        cache_control.unwrap_or_default(),
    ))))
}

/// Reads one block; `None` when the derived pair would have rejected it.
/// The rejection itself is raised by the caller, after the input is
/// consumed, so it carries no input position, as the derived pair's did.
struct ContentPartVisitor;

impl<'de> Visitor<'de> for ContentPartVisitor {
    type Value = Option<ContentPart>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a content block object")
    }

    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
        MemberSlots::collect(&MEMBER_NAMES, map).map(from_members)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
        from_positional(seq)
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_bytes<E: de::Error>(self, _: &[u8]) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }
}

impl<'de> Deserialize<'de> for ContentPart {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer
            .deserialize_any(ContentPartVisitor)?
            .ok_or_else(|| de::Error::custom(NO_MATCH))
    }
}

impl<'de> Deserialize<'de> for KnownContentPart {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match ContentPart::deserialize(deserializer)? {
            ContentPart::Known(known) => Ok(known),
            ContentPart::Other { type_tag, .. } if SHAPE_TAGS.contains(&type_tag.as_str()) => Err(
                de::Error::custom(format_args!("malformed `{type_tag}` content block")),
            ),
            ContentPart::Other { type_tag, .. } => {
                Err(de::Error::unknown_variant(&type_tag, &SHAPE_TAGS))
            }
        }
    }
}

#[cfg(test)]
#[path = "content_part_de_tests.rs"]
mod tests;
