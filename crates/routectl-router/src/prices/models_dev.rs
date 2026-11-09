//! The models.dev `api.json` converter: a pure mapping onto the canonical
//! format, validated by the same step as a parsed table.
//!
//! The source is an object keyed by provider id; each provider carries an
//! optional `api` base URL and a `models` object keyed by model id, whose
//! `cost` holds per-Mtok `input`, `output`, `cache_read` and `cache_write`.
//! `cache_write` is the 5-minute write tier; the source has no 1-hour tier.
//! Every other field is ignored.

use std::fmt;
use std::marker::PhantomData;

use serde::Deserialize;
use serde::de::{Deserializer, MapAccess, Visitor};

use super::format::{ApiBase, RawModel, RawProvider, RawTable, SCHEMA_VERSION, TableDate};
use super::validate::{check_size, validate};
use super::{PriceTable, PriceTableError};

/// Converts a models.dev `api.json` document into a validated table dated
/// `generated_at`.
///
/// A provider `api` that the shared origin normalizer withholds (not a URL,
/// a non-http(s) scheme) is dropped, leaving that provider reachable only
/// through the first-party fallback. A placeholder inside an otherwise valid
/// URL is kept as written. A model with no cost field is omitted. A provider
/// or model id repeated in the source is a duplicate row, not a silent
/// overwrite.
pub fn from_models_dev(
    bytes: &[u8],
    generated_at: TableDate,
) -> Result<PriceTable, PriceTableError> {
    check_size(bytes)?;
    let source: Entries<DevProvider> =
        serde_json::from_slice(bytes).map_err(|e| PriceTableError::syntax(&e))?;
    let providers = source
        .0
        .into_iter()
        .map(|(id, provider)| RawProvider {
            id,
            api_host: provider
                .api
                .as_deref()
                .and_then(ApiBase::parse)
                .map(|api| api.canonical()),
            models: provider
                .models
                .0
                .into_iter()
                .filter_map(|(id, model)| model.cost.map(|cost| cost.into_row(id)))
                .collect(),
        })
        .collect();
    validate(RawTable {
        schema_version: SCHEMA_VERSION,
        generated_at: generated_at.to_string(),
        providers,
    })
}

#[derive(Deserialize)]
struct DevProvider {
    #[serde(default)]
    api: Option<String>,
    models: Entries<DevModel>,
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

/// A JSON object read as its entries in source order, keeping a repeated key
/// as a second entry so validation can refuse it.
struct Entries<T>(Vec<(String, T)>);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Entries<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EntriesVisitor<T>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>> Visitor<'de> for EntriesVisitor<T> {
            type Value = Entries<T>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry::<String, T>()? {
                    entries.push(entry);
                }
                Ok(Entries(entries))
            }
        }

        deserializer.deserialize_map(EntriesVisitor(PhantomData))
    }
}
