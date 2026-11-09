//! Bounds counted while a table is deserialized, and the canonical-format
//! seeds that count them, so an oversized document is refused before its
//! rows are materialized.

use std::cell::Cell;
use std::fmt;

use serde::de::{self, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};

use super::PriceTableError;
use super::format::{RawModel, RawProvider, RawTable, SCHEMA_VERSION};
use super::validate::{MAX_PROVIDERS, MAX_ROWS};

/// Running provider and model-row counts for one parse, shared by every
/// provider in the document. This is the only place the row and provider
/// bounds are enforced.
#[derive(Default)]
pub(super) struct Bounds {
    rows: Cell<usize>,
    providers: Cell<usize>,
    refusal: Cell<Option<PriceTableError>>,
}

impl Bounds {
    /// Counts one source model entry, priced or not.
    pub(super) fn count_row<E: de::Error>(&self) -> Result<(), E> {
        self.bump(
            &self.rows,
            MAX_ROWS,
            PriceTableError::TooManyRows { limit: MAX_ROWS },
        )
    }

    pub(super) fn count_provider<E: de::Error>(&self) -> Result<(), E> {
        self.bump(
            &self.providers,
            MAX_PROVIDERS,
            PriceTableError::TooManyProviders {
                limit: MAX_PROVIDERS,
            },
        )
    }

    fn bump<E: de::Error>(
        &self,
        counter: &Cell<usize>,
        limit: usize,
        refusal: PriceTableError,
    ) -> Result<(), E> {
        let next = counter.get().saturating_add(1);
        counter.set(next);
        if next > limit {
            return Err(self.refuse(refusal));
        }
        Ok(())
    }

    /// Stops the parse with `refusal` as its outcome.
    pub(super) fn refuse<E: de::Error>(&self, refusal: PriceTableError) -> E {
        let message = refusal.to_string();
        self.refusal.set(Some(refusal));
        E::custom(message)
    }

    /// The bound a failed parse stopped at, or the syntax error at `err`.
    pub(super) fn refusal_or_syntax(&self, err: &serde_json::Error) -> PriceTableError {
        self.refusal
            .take()
            .unwrap_or_else(|| PriceTableError::syntax(err))
    }
}

/// Refuses a second occurrence of a field before its value is read.
pub(super) fn not_seen<T, E: de::Error>(slot: Option<&T>, field: &'static str) -> Result<(), E> {
    match slot {
        Some(_) => Err(E::duplicate_field(field)),
        None => Ok(()),
    }
}

/// Parses a canonical table body (its schema version already checked).
pub(super) fn parse_canonical(bytes: &[u8]) -> Result<RawTable, PriceTableError> {
    let bounds = Bounds::default();
    let mut de = serde_json::Deserializer::from_slice(bytes);
    TableSeed(&bounds)
        .deserialize(&mut de)
        .and_then(|raw| de.end().map(|()| raw))
        .map_err(|e| bounds.refusal_or_syntax(&e))
}

struct TableSeed<'a>(&'a Bounds);

impl<'de> DeserializeSeed<'de> for TableSeed<'_> {
    type Value = RawTable;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<RawTable, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for TableSeed<'_> {
    type Value = RawTable;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a price table object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<RawTable, A::Error> {
        let mut generated_at: Option<String> = None;
        let mut providers = None;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "generated_at" => {
                    not_seen(generated_at.as_ref(), "generated_at")?;
                    generated_at = Some(map.next_value()?);
                }
                "providers" => {
                    not_seen(providers.as_ref(), "providers")?;
                    providers = Some(map.next_value_seed(ProvidersSeed(self.0))?);
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(RawTable {
            schema_version: SCHEMA_VERSION,
            generated_at: generated_at.ok_or_else(|| de::Error::missing_field("generated_at"))?,
            providers: providers.ok_or_else(|| de::Error::missing_field("providers"))?,
        })
    }
}

struct ProvidersSeed<'a>(&'a Bounds);

impl<'de> DeserializeSeed<'de> for ProvidersSeed<'_> {
    type Value = Vec<RawProvider>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for ProvidersSeed<'_> {
    type Value = Vec<RawProvider>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a list of providers")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut out = Vec::new();
        while let Some(provider) = seq.next_element_seed(ProviderSeed(self.0))? {
            self.0.count_provider()?;
            out.push(provider);
        }
        Ok(out)
    }
}

struct ProviderSeed<'a>(&'a Bounds);

impl<'de> DeserializeSeed<'de> for ProviderSeed<'_> {
    type Value = RawProvider;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<RawProvider, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for ProviderSeed<'_> {
    type Value = RawProvider;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a provider object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<RawProvider, A::Error> {
        let mut id: Option<String> = None;
        let mut api_host: Option<Option<String>> = None;
        let mut models = None;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "id" => {
                    not_seen(id.as_ref(), "id")?;
                    id = Some(map.next_value()?);
                }
                "api_host" => {
                    not_seen(api_host.as_ref(), "api_host")?;
                    api_host = Some(map.next_value()?);
                }
                "models" => {
                    not_seen(models.as_ref(), "models")?;
                    models = Some(map.next_value_seed(ModelsSeed(self.0))?);
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(RawProvider {
            id: id.ok_or_else(|| de::Error::missing_field("id"))?,
            api_host: api_host.flatten(),
            models: models.ok_or_else(|| de::Error::missing_field("models"))?,
        })
    }
}

struct ModelsSeed<'a>(&'a Bounds);

impl<'de> DeserializeSeed<'de> for ModelsSeed<'_> {
    type Value = Vec<RawModel>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for ModelsSeed<'_> {
    type Value = Vec<RawModel>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a list of models")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut out = Vec::new();
        while let Some(model) = seq.next_element::<RawModel>()? {
            self.0.count_row()?;
            out.push(model);
        }
        Ok(out)
    }
}
