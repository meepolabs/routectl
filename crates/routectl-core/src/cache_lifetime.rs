//! Cache-write lifetimes as data: the generic duration parser behind
//! [`CacheCreation`]'s wire form.
//!
//! Upstreams encode a cache lifetime in a usage member's NAME
//! (`ephemeral_5m_input_tokens`, `ttl_30m_tokens`) or as a bare duration
//! string (`"5m"`). Both parse to whole seconds here, so a new lifetime
//! tier needs no code change. A name the grammar refuses contributes
//! nothing to the split: its tokens stay inside the reported write total
//! as unknown lifetime.
//!
//! Grammar. A duration is `<positive integer><s|m|h|d>`, lowercase,
//! fitting `u32` seconds; leading zeros are allowed (`05m` is 300). A member name is `_`-separated
//! segments, ends in `tokens`, holds exactly one `<digits><unit>` segment
//! (which must itself be a valid duration), and has no read-side segment
//! (`read`, `cached`).

use std::fmt;

use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::log_safe::sanitize_for_log;
use crate::schema::CacheCreation;

/// Most distinct lifetimes one [`CacheCreation`] holds. Members past the
/// cap contribute nothing to the split, which bounds what one upstream
/// usage object can make routectl store.
pub const MAX_CACHE_LIFETIME_ENTRIES: usize = 16;

const UNITS: [(char, u32); 4] = [('d', 86_400), ('h', 3_600), ('m', 60), ('s', 1)];

/// Parse a bare duration string (`"5m"`, `"24h"`) into seconds. `None`
/// for zero, a sign, a fraction, an uppercase or unknown unit, or a
/// value past `u32` seconds.
pub fn parse_cache_lifetime(duration: &str) -> Option<u32> {
    let unit = duration.chars().last()?;
    let digits = &duration[..duration.len() - unit.len_utf8()];
    let (_, scale) = UNITS.iter().find(|(c, _)| *c == unit)?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let count: u32 = digits.parse().ok()?;
    if count == 0 {
        return None;
    }
    count.checked_mul(*scale)
}

/// Parse a usage member name that encodes a cache lifetime
/// (`ephemeral_1h_input_tokens`, `cache_write_12h_tokens`) into seconds.
/// `None` when the name breaks the module grammar.
pub fn parse_cache_lifetime_member(name: &str) -> Option<u32> {
    let mut segments = name.split('_');
    if segments.next_back() != Some("tokens") {
        return None;
    }
    let mut lifetime = None;
    for segment in segments {
        if segment == "read" || segment == "cached" {
            return None;
        }
        if !is_duration_shaped(segment) {
            continue;
        }
        if lifetime.is_some() {
            return None;
        }
        lifetime = Some(parse_cache_lifetime(segment)?);
    }
    lifetime
}

/// `<digits><unit>`, valid or not: a zero or overflowing duration
/// segment refuses the whole name instead of being read as plain text.
fn is_duration_shaped(segment: &str) -> bool {
    let Some(unit) = segment.chars().last() else {
        return false;
    };
    let digits = &segment[..segment.len() - unit.len_utf8()];
    UNITS.iter().any(|(c, _)| *c == unit)
        && !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
}

/// Render seconds in the largest unit that divides them exactly
/// (`300` -> `"5m"`, `5400` -> `"90m"`, `86400` -> `"1d"`).
pub fn format_cache_lifetime(seconds: u32) -> String {
    let (unit, scale) = UNITS
        .iter()
        .find(|(_, scale)| seconds != 0 && seconds.is_multiple_of(*scale))
        .copied()
        .unwrap_or(('s', 1));
    format!("{}{unit}", seconds / scale)
}

fn wire_member_name(seconds: u32) -> String {
    format!("ephemeral_{}_input_tokens", format_cache_lifetime(seconds))
}

impl Serialize for CacheCreation {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.len()))?;
        for (seconds, tokens) in self.iter() {
            map.serialize_entry(&wire_member_name(seconds), &tokens)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for CacheCreation {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(CacheCreationVisitor)
    }
}

struct CacheCreationVisitor;

impl<'de> Visitor<'de> for CacheCreationVisitor {
    type Value = CacheCreation;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an object of per-lifetime cache-write token counts")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let mut creation = CacheCreation::default();
        while let Some((member, value)) = access.next_entry::<String, serde_json::Value>()? {
            if let Some(reason) = admit_member(&mut creation, &member, &value) {
                tracing::debug!(
                    member = %sanitize_for_log(&member),
                    reason,
                    "cache write member refused; its tokens count as unknown lifetime"
                );
            }
        }
        Ok(creation)
    }
}

/// Add one wire member to `creation`, returning why it was refused.
fn admit_member(
    creation: &mut CacheCreation,
    member: &str,
    value: &serde_json::Value,
) -> Option<&'static str> {
    let Some(seconds) = parse_cache_lifetime_member(member) else {
        return Some("unparsed_name");
    };
    let Some(tokens) = value.as_u64().and_then(|n| u32::try_from(n).ok()) else {
        return Some("non_integer_value");
    };
    if creation.add(seconds, tokens) {
        None
    } else {
        Some("lifetime_cap")
    }
}

#[cfg(test)]
#[path = "cache_lifetime_tests.rs"]
mod tests;
