//! The shape rules one `[providers.X]` entry must meet on its own, before
//! any route, coherence or containment question is asked of it: exactly one
//! carrier, a plain http(s) `base_url`, and no key its format does not take.
//!
//! [`shape_refusals`] reports every broken rule of an entry at once, so an
//! operator fixing a hand-edited file sees all of them in one pass. Each
//! refusal names the provider and the key, never a configured value.

use crate::config::ProviderEntry;
use crate::config::provider_descriptor::Format;

/// A key only some formats take: its name, the one format that takes it,
/// and whether the entry sets it. An explicitly written `false`, empty
/// string or empty list counts as set.
struct FormatKey {
    key: &'static str,
    format: Format,
    is_set: fn(&ProviderEntry) -> bool,
}

/// Every key that applies to one format only. A key absent here applies to
/// every format.
const FORMAT_KEYS: &[FormatKey] = &[
    FormatKey {
        key: "anthropic_version",
        format: Format::AnthropicMessages,
        is_set: |e| e.anthropic_version.is_some(),
    },
    FormatKey {
        key: "forward_client_headers",
        format: Format::AnthropicMessages,
        is_set: |e| e.forward_client_headers.is_some(),
    },
    FormatKey {
        key: "context_management",
        format: Format::AnthropicMessages,
        is_set: |e| e.context_management.is_some(),
    },
    FormatKey {
        key: "cloak_mode",
        format: Format::AnthropicMessages,
        is_set: |e| e.cloak_mode.is_some(),
    },
    FormatKey {
        key: "cloak_strict_mode",
        format: Format::AnthropicMessages,
        is_set: |e| e.cloak_strict_mode.is_some(),
    },
    FormatKey {
        key: "codex_version",
        format: Format::OpenaiResponses,
        is_set: |e| e.codex_version.is_some(),
    },
    FormatKey {
        key: "account_id_ref",
        format: Format::OpenaiResponses,
        is_set: |e| e.account_id_ref.is_some(),
    },
    FormatKey {
        key: "cloud_project_id",
        format: Format::GeminiCloudCode,
        is_set: |e| e.cloud_project_id.is_some(),
    },
];

/// Every shape rule `entry` (configured as provider `name`) breaks, in a
/// fixed order: the carrier, then `base_url`, then each key its format does
/// not take. Empty when the entry is well formed.
pub fn shape_refusals(name: &str, entry: &ProviderEntry) -> Vec<String> {
    let mut refusals = Vec::new();
    if !has_one_carrier(entry) {
        refusals.push(carrier_refusal(name, entry));
    }
    if let Some(base) = &entry.base_url
        && let Err(refusal) = check_plain_base_url(name, base)
    {
        refusals.push(refusal);
    }
    refusals.extend(
        FORMAT_KEYS
            .iter()
            .filter(|row| row.format != entry.format && (row.is_set)(entry))
            .map(|row| {
                format!(
                    "provider `{name}`: `{}` does not apply to format `{}`; delete it",
                    row.key,
                    entry.format.as_str()
                )
            }),
    );
    refusals
}

/// True when the entry sets exactly one of `base_url` and `cloud`.
const fn has_one_carrier(entry: &ProviderEntry) -> bool {
    entry.base_url.is_some() != entry.cloud.is_some()
}

/// The refusal for an entry that sets both carriers or neither.
pub fn carrier_refusal(name: &str, entry: &ProviderEntry) -> String {
    let detail = if entry.base_url.is_some() {
        ", not both"
    } else {
        ""
    };
    format!("provider `{name}`: set exactly one of `base_url` or `cloud`{detail}")
}

/// Parse a configured base URL and require an http(s) URL with a host. The
/// value is trimmed first. Refusals name the provider and `base_url`, never
/// the value: it can carry a credential.
pub fn parse_base_url(name: &str, base_url: &str) -> Result<url::Url, String> {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        return Err(format!(
            "provider `{name}`: base_url is set but empty; set an explicit endpoint"
        ));
    }
    let url = url::Url::parse(trimmed)
        .map_err(|e| format!("provider `{name}`: base_url is not a valid URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "provider `{name}`: base_url scheme is not allowed; use https:// (or http:// for \
             loopback only)"
        ));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(format!(
            "provider `{name}`: base_url has no host; set an explicit http(s) endpoint with a \
             hostname"
        ));
    }
    Ok(url)
}

/// Require a plain base URL: [`parse_base_url`], plus no userinfo, no
/// query and no fragment. Request paths are appended to the base, so a query
/// or fragment there would end up inside the path.
fn check_plain_base_url(name: &str, base_url: &str) -> Result<(), String> {
    let url = parse_base_url(name, base_url)?;
    let part = if !url.username().is_empty() || url.password().is_some() {
        "userinfo (`user@`); put the secret in `credential`"
    } else if url.query().is_some() {
        "a query (`?`)"
    } else if url.fragment().is_some() {
        "a fragment (`#`)"
    } else {
        return Ok(());
    };
    Err(format!(
        "provider `{name}`: base_url carries {part}. A base_url is a plain http(s) URL: a \
         host and an optional path prefix, nothing else. The configured base_url is withheld"
    ))
}

#[cfg(test)]
#[path = "entry_shape_tests.rs"]
mod tests;
