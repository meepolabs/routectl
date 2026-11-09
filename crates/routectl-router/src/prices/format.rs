//! The canonical on-disk serde shapes, the table date, and the normalized
//! provider API base.

use std::fmt;

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize};

use super::{PriceTable, PriceTableError};
use crate::config_effective::normalized_origin;

/// The only schema version this build reads and writes.
pub(super) const SCHEMA_VERSION: u64 = 1;

/// Reads `schema_version` alone, before the typed parse, so a future layout
/// is refused as unsupported rather than as a shape error.
#[derive(Deserialize)]
pub(super) struct SchemaProbe {
    #[serde(default)]
    pub(super) schema_version: Option<ProbedVersion>,
}

/// A scalar `schema_version`: `Some` for a non-negative integer, `None` for
/// any other scalar (unsupported). An object or array is a shape error and
/// is never materialized.
pub(super) struct ProbedVersion(pub(super) Option<u64>);

impl<'de> Deserialize<'de> for ProbedVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ProbedVersionVisitor)
    }
}

struct ProbedVersionVisitor;

impl Visitor<'_> for ProbedVersionVisitor {
    type Value = ProbedVersion;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a scalar schema_version")
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<ProbedVersion, E> {
        Ok(ProbedVersion(Some(v)))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<ProbedVersion, E> {
        Ok(ProbedVersion(u64::try_from(v).ok()))
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<ProbedVersion, E> {
        Ok(ProbedVersion(None))
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<ProbedVersion, E> {
        Ok(ProbedVersion(None))
    }

    fn visit_str<E: de::Error>(self, _: &str) -> Result<ProbedVersion, E> {
        Ok(ProbedVersion(None))
    }

    fn visit_unit<E: de::Error>(self) -> Result<ProbedVersion, E> {
        Ok(ProbedVersion(None))
    }
}

/// The whole file. Read by the bounded seeds, which tolerate unknown
/// fields on every on-disk type.
#[derive(Serialize)]
pub(super) struct RawTable {
    pub(super) schema_version: u64,
    pub(super) generated_at: String,
    pub(super) providers: Vec<RawProvider>,
}

#[derive(Serialize)]
pub(super) struct RawProvider {
    pub(super) id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) api_host: Option<String>,
    pub(super) models: Vec<RawModel>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct RawModel {
    pub(super) id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) input: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) output: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) cache_write_5m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) cache_write_1h: Option<f64>,
}

pub(super) fn to_canonical_json(table: &PriceTable) -> Result<Vec<u8>, PriceTableError> {
    let raw = RawTable {
        schema_version: SCHEMA_VERSION,
        generated_at: table.generated_at.to_string(),
        providers: table
            .providers
            .iter()
            .map(|provider| RawProvider {
                id: provider.id.clone(),
                api_host: provider.api.as_ref().map(ApiBase::canonical),
                models: provider
                    .models
                    .iter()
                    .map(|(id, prices)| RawModel {
                        id: id.clone(),
                        input: prices.input_per_mtok,
                        output: prices.output_per_mtok,
                        cache_read: prices.cache_read_per_mtok,
                        cache_write_5m: prices.cache_write_5m_per_mtok,
                        cache_write_1h: prices.cache_write_1h_per_mtok,
                    })
                    .collect(),
            })
            .collect(),
    };
    let mut bytes = serde_json::to_vec_pretty(&raw).map_err(|_| PriceTableError::Serialize)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// A calendar date, `YYYY-MM-DD`. Rendered from its parsed numbers, never
/// from the text it was parsed from.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TableDate {
    year: u16,
    month: u8,
    day: u8,
}

impl TableDate {
    /// Parses exactly `YYYY-MM-DD` (four, two and two ASCII digits) naming a
    /// real calendar day; anything else, including a timestamp, is `None`.
    pub fn parse(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
            return None;
        }
        let year = u16::try_from(digits(&bytes[0..4])?).ok()?;
        let month = u8::try_from(digits(&bytes[5..7])?).ok()?;
        let day = u8::try_from(digits(&bytes[8..10])?).ok()?;
        if year == 0 || !(1..=12).contains(&month) || day == 0 {
            return None;
        }
        (day <= days_in_month(year, month)).then_some(Self { year, month, day })
    }
}

fn digits(bytes: &[u8]) -> Option<u32> {
    bytes.iter().try_fold(0u32, |acc, &b| {
        b.is_ascii_digit().then(|| acc * 10 + u32::from(b - b'0'))
    })
}

const fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

impl fmt::Display for TableDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

impl fmt::Debug for TableDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// A provider API base URL reduced to what selection compares: the
/// normalized origin and the non-empty path segments.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct ApiBase {
    pub(super) origin: String,
    pub(super) segments: Vec<String>,
}

impl ApiBase {
    /// `None` for any URL the shared origin normalizer withholds (non-http(s),
    /// userinfo ambiguity, no host), so a table entry and a config target are
    /// reduced by the same rule.
    pub(super) fn parse(raw: &str) -> Option<Self> {
        let origin = normalized_origin(raw)?;
        let url = url::Url::parse(raw.trim()).ok()?;
        let segments = url
            .path_segments()
            .map(|parts| {
                parts
                    .filter(|part| !part.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Some(Self { origin, segments })
    }

    /// The written form: the origin without its scheme's default port, then
    /// the path segments. Reading it back normalizes to the same origin.
    pub(super) fn canonical(&self) -> String {
        // An explicit `:443` before a path reads to secret scanners as a key
        // (`443/anthropic/v1`), and the table is committed to the repo.
        let mut out = without_default_port(&self.origin).to_owned();
        for segment in &self.segments {
            out.push('/');
            out.push_str(segment);
        }
        out
    }
}

fn without_default_port(origin: &str) -> &str {
    let default = if origin.starts_with("https://") {
        ":443"
    } else if origin.starts_with("http://") {
        ":80"
    } else {
        return origin;
    };
    origin.strip_suffix(default).unwrap_or(origin)
}
