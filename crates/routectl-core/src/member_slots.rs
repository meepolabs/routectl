//! Splitting one JSON object's members into the typed members a canonical
//! node models and the leftover members its `extras` carrier keeps.
//!
//! [`MemberSlots`] is the streaming form for hand-written deserializers: it
//! drains a serde map (or the positional sequence form serde also accepts
//! for structs) in one pass. A typed member name is matched without
//! allocating, and a member outside the slot list moves into a leftover map
//! that is allocated only when such a member exists. A slot seen twice is
//! remembered, so a caller can reproduce serde's duplicate-field behavior on
//! input that keeps duplicate keys (a `from_str` body).
//!
//! [`take_members_outside`] is the in-place form for a node already buffered
//! as a `serde_json::Value`.

use std::fmt;

use serde::de::{self, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

/// A member key: the index of a named slot, or any other name.
pub enum MemberKey {
    Slot(usize),
    Other(String),
}

struct MemberKeySeed<'n> {
    names: &'n [&'static str],
}

impl MemberKeySeed<'_> {
    fn slot_of(&self, key: &str) -> Option<usize> {
        self.names.iter().position(|name| *name == key)
    }
}

impl<'de> DeserializeSeed<'de> for MemberKeySeed<'_> {
    type Value = MemberKey;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<MemberKey, D::Error> {
        deserializer.deserialize_identifier(self)
    }
}

impl Visitor<'_> for MemberKeySeed<'_> {
    type Value = MemberKey;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a member name")
    }

    fn visit_str<E: de::Error>(self, key: &str) -> Result<MemberKey, E> {
        Ok(self
            .slot_of(key)
            .map_or_else(|| MemberKey::Other(key.to_owned()), MemberKey::Slot))
    }

    fn visit_string<E: de::Error>(self, key: String) -> Result<MemberKey, E> {
        Ok(self
            .slot_of(&key)
            .map_or(MemberKey::Other(key), MemberKey::Slot))
    }
}

/// Read the next member key of `map`, matched against `names` without
/// allocating; `None` at the end of the object.
pub fn next_member_key<'de, A: MapAccess<'de>>(
    map: &mut A,
    names: &[&'static str],
) -> Result<Option<MemberKey>, A::Error> {
    map.next_key_seed(MemberKeySeed { names })
}

/// Read and discard every element left in `seq`.
pub fn drain_seq<'de, A: SeqAccess<'de>>(seq: &mut A) -> Result<(), A::Error> {
    while seq.next_element::<IgnoredAny>()?.is_some() {}
    Ok(())
}

/// The members of one JSON object, split into `N` named slots and the rest.
pub struct MemberSlots<const N: usize> {
    names: &'static [&'static str; N],
    values: [Option<Value>; N],
    duplicated: u64,
    rest: Map<String, Value>,
}

impl<const N: usize> MemberSlots<N> {
    /// Slots named by `names`, all empty.
    pub fn new(names: &'static [&'static str; N]) -> Self {
        const { assert!(N <= 64, "duplicate tracking holds 64 slots") };
        Self {
            names,
            values: std::array::from_fn(|_| None),
            duplicated: 0,
            rest: Map::new(),
        }
    }

    /// Drain `map` into slots named by `names`. A repeated member keeps its
    /// last value, as `serde_json::Value` does.
    pub fn collect<'de, A: MapAccess<'de>>(
        names: &'static [&'static str; N],
        mut map: A,
    ) -> Result<Self, A::Error> {
        let mut slots = Self::new(names);
        while let Some(key) = next_member_key(&mut map, names)? {
            let value: Value = map.next_value()?;
            match key {
                MemberKey::Slot(i) => {
                    if slots.values[i].replace(value).is_some() {
                        slots.duplicated |= 1 << i;
                    }
                }
                MemberKey::Other(name) => {
                    slots.rest.insert(name, value);
                }
            }
        }
        Ok(slots)
    }

    /// Fill the slots in `order` from consecutive elements of `seq`, the
    /// positional form serde accepts for a struct, and consume the rest of
    /// `seq`. Elements may run out early. Returns false when an element
    /// remained past `order`.
    pub fn fill_positional<'de, A: SeqAccess<'de>>(
        &mut self,
        order: impl IntoIterator<Item = usize>,
        seq: &mut A,
    ) -> Result<bool, A::Error> {
        for slot in order {
            match seq.next_element::<Value>()? {
                Some(value) => self.values[slot] = Some(value),
                None => return Ok(true),
            }
        }
        let fits = seq.next_element::<IgnoredAny>()?.is_none();
        if !fits {
            drain_seq(seq)?;
        }
        Ok(fits)
    }

    pub const fn get(&self, slot: usize) -> Option<&Value> {
        self.values[slot].as_ref()
    }

    pub const fn take(&mut self, slot: usize) -> Option<Value> {
        self.values[slot].take()
    }

    pub const fn is_duplicated(&self, slot: usize) -> bool {
        self.duplicated & (1 << slot) != 0
    }

    /// The leftover members, with every slot not taken moved back in under
    /// its own name.
    pub fn into_rest(self) -> Map<String, Value> {
        let Self {
            names,
            values,
            mut rest,
            ..
        } = self;
        for (value, name) in values.into_iter().zip(names) {
            if let Some(value) = value {
                rest.insert((*name).to_owned(), value);
            }
        }
        rest
    }
}

/// Move every member of `obj` whose name is not in `keep` into a new map.
/// Allocates nothing when every member is kept.
pub fn take_members_outside(obj: &mut Map<String, Value>, keep: &[&str]) -> Map<String, Value> {
    let mut taken = Map::new();
    let is_kept = |name: &str| keep.contains(&name);
    if obj.keys().all(|name| is_kept(name)) {
        return taken;
    }
    for (name, value) in std::mem::take(obj) {
        if is_kept(&name) {
            obj.insert(name, value);
        } else {
            taken.insert(name, value);
        }
    }
    taken
}
