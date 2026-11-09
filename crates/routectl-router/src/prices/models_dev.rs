//! The models.dev `api.json` converter: a pure mapping onto the canonical
//! format, validated by the same step as a parsed table.
//!
//! The source is an object keyed by provider id; each provider carries an
//! optional `api` base URL and a `models` object keyed by model id, whose
//! `cost` holds per-Mtok `input`, `output`, `cache_read` and `cache_write`.
//! `cache_write` is the 5-minute write tier; the source has no 1-hour tier.
//! Every other field is ignored.

use std::collections::HashSet;
use std::fmt;

use serde::Deserialize;
use serde::de::{self, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, Visitor};

use super::bounded::{Bounds, not_seen};
use super::format::{ApiBase, RawModel, RawProvider, RawTable, SCHEMA_VERSION, TableDate};
use super::validate::{check_size, validate};
use super::{PriceTable, PriceTableError};

/// Converts a models.dev `api.json` document into a validated table dated
/// `generated_at`.
///
/// A provider `api` that the shared origin normalizer withholds (not a URL,
/// a non-http(s) scheme) is dropped, leaving that provider reachable only
/// through the first-party fallback. A placeholder inside an otherwise valid
/// URL is kept as written. A model with no cost field is omitted, though it
/// still counts toward the row bound. A provider or model id repeated in the
/// source is a duplicate row, not a silent overwrite.
pub fn from_models_dev(
    bytes: &[u8],
    generated_at: TableDate,
) -> Result<PriceTable, PriceTableError> {
    check_size(bytes)?;
    let bounds = Bounds::default();
    let mut de = serde_json::Deserializer::from_slice(bytes);
    let providers = ProvidersSeed(&bounds)
        .deserialize(&mut de)
        .and_then(|providers| de.end().map(|()| providers))
        .map_err(|e| bounds.refusal_or_syntax(&e))?;
    validate(RawTable {
        schema_version: SCHEMA_VERSION,
        generated_at: generated_at.to_string(),
        providers,
    })
}

#[derive(Deserialize)]
struct DevModel {
    #[serde(default)]
    cost: Option<DevCost>,
}

#[derive(Deserialize)]
struct DevCost {
    #[serde(default)]
    input: Option<f64>,
    #[serde(default)]
    output: Option<f64>,
    #[serde(default)]
    cache_read: Option<f64>,
    #[serde(default)]
    cache_write: Option<f64>,
}

impl DevCost {
    const fn into_row(self, id: String) -> RawModel {
        RawModel {
            id,
            input: self.input,
            output: self.output,
            cache_read: self.cache_read,
            cache_write_5m: self.cache_write,
            cache_write_1h: None,
        }
    }
}

/// The top-level object keyed by provider id, read in source order so a
/// repeated key stays a second entry for validation to refuse.
struct ProvidersSeed<'a>(&'a Bounds);

impl<'de> DeserializeSeed<'de> for ProvidersSeed<'_> {
    type Value = Vec<RawProvider>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for ProvidersSeed<'_> {
    type Value = Vec<RawProvider>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an object of providers")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut out = Vec::new();
        while let Some(id) = map.next_key::<String>()? {
            self.0.count_provider()?;
            out.push(map.next_value_seed(ProviderSeed { id, bounds: self.0 })?);
        }
        Ok(out)
    }
}

struct ProviderSeed<'a> {
    id: String,
    bounds: &'a Bounds,
}

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
        let mut api: Option<Option<String>> = None;
        let mut models = None;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "api" => {
                    not_seen(api.as_ref(), "api")?;
                    api = Some(map.next_value()?);
                }
                "models" => {
                    not_seen(models.as_ref(), "models")?;
                    models = Some(map.next_value_seed(ModelsSeed(self.bounds))?);
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(RawProvider {
            id: self.id,
            api_host: api
                .flatten()
                .as_deref()
                .and_then(ApiBase::parse)
                .map(|api| api.canonical()),
            models: models.ok_or_else(|| de::Error::missing_field("models"))?,
        })
    }
}

/// A provider's `models` object; every entry counts toward the row bound
/// and only the entries carrying a cost become rows. A key repeated within
/// one provider is a duplicate row whether or not either copy has a cost.
struct ModelsSeed<'a>(&'a Bounds);

impl<'de> DeserializeSeed<'de> for ModelsSeed<'_> {
    type Value = Vec<RawModel>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for ModelsSeed<'_> {
    type Value = Vec<RawModel>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an object of models")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        while let Some(id) = map.next_key::<String>()? {
            self.0.count_row()?;
            if !seen.insert(id.clone()) {
                return Err(self.0.refuse(PriceTableError::DuplicateModel));
            }
            if let Some(cost) = map.next_value::<DevModel>()?.cost {
                out.push(cost.into_row(id));
            }
        }
        Ok(out)
    }
}
