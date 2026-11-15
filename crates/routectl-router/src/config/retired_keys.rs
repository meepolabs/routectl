//! The retired config-key table and the one raw-text pass that refuses every
//! retired key a config still carries, each naming what replaces it.

use super::validate::message_safe_key;

/// Path segment standing for any one non-empty entry name.
const ANY_ENTRY: &str = "*";

/// One retired key: where it sat and what replaces it.
#[derive(Debug, Clone, Copy)]
pub struct RetiredKey {
    /// Structural key path; each segment is a literal key or `"*"`.
    pub path: &'static [&'static str],
    /// When set, only this string value is retired and the key itself stays
    /// live; any other value, or a non-string, passes.
    pub refused_value: Option<&'static str>,
    /// What replaces the key, rendered after "is retired: ".
    pub replacement: &'static str,
}

/// Every retired key. No two rows may name the same concrete key, counting
/// `*` as any entry and a table as every key under it: that key would be
/// reported twice.
pub const RETIRED_KEYS: &[RetiredKey] = &[
    RetiredKey {
        path: &["bedrock"],
        refused_value: None,
        replacement: "nothing replaces it: client betas and body fields pass through, minus \
                      what the router withholds from the lane",
    },
    RetiredKey {
        path: &["cache_pricing"],
        refused_value: None,
        replacement: "set prices under `[models.X.pricing]`",
    },
    RetiredKey {
        path: &["calibration"],
        refused_value: None,
        replacement: "the learned token-estimate correction always applies",
    },
    RetiredKey {
        path: &["capability", "staleness_hint_days"],
        refused_value: None,
        replacement: "diagnostics use a fixed 14-day hint",
    },
    RetiredKey {
        path: &["fidelity"],
        refused_value: None,
        replacement: "prefix-impacting pre-flight repair is never admitted; class and quorum \
                      are fixed",
    },
    RetiredKey {
        path: &["log"],
        refused_value: None,
        replacement: "set ROUTECTL_TRACE_HEADERS / ROUTECTL_TRACE_BODY_BYTES / \
                      ROUTECTL_LOG_REDACT_PROMPTS in the environment",
    },
    RetiredKey {
        path: &["mitm", "credential_source"],
        refused_value: None,
        replacement: "a forwarded credential is a per-provider choice: add \
                      `[providers.anthropic-forwarded]` with `kind = \"anthropic-api\"`, \
                      `base_url = \"https://api.anthropic.com\"` and \
                      `credential_source = \"forwarded\"`, and no `api_key_ref`",
    },
    RetiredKey {
        path: &["models", ANY_ENTRY, "effort_levels"],
        refused_value: None,
        replacement: "refused effort levels are learned per lane",
    },
    RetiredKey {
        path: &["models", ANY_ENTRY, "max_thinking_budget"],
        refused_value: None,
        replacement: "client thinking budgets are forwarded",
    },
    RetiredKey {
        path: &["models", ANY_ENTRY, "supports_adaptive_thinking"],
        refused_value: None,
        replacement: "client thinking is forwarded; effort alone adds no thinking",
    },
    RetiredKey {
        path: &["models", ANY_ENTRY, "unsupported_features"],
        refused_value: None,
        replacement: "use `[capability.overrides.\"<provider>:<nickname>\"] unsupported`",
    },
    RetiredKey {
        path: &["providers", ANY_ENTRY, "allowed_betas"],
        refused_value: None,
        replacement: "nothing replaces it: client betas pass through, minus the flags the \
                      router withholds from the lane",
    },
    RetiredKey {
        path: &["providers", ANY_ENTRY, "unsupported_features"],
        refused_value: None,
        replacement: "use `[capability.overrides.<provider>] unsupported`",
    },
    RetiredKey {
        path: &["seat_quota"],
        refused_value: None,
        replacement: "quota-aware placement always applies on sticky-least-loaded pools",
    },
    RetiredKey {
        path: &["server", "allow_disable_fallbacks"],
        refused_value: None,
        replacement: "the x-routectl-disable-fallbacks header is always honored",
    },
    RetiredKey {
        path: &["trim"],
        refused_value: None,
        replacement: "the advisory trim uses fixed defaults",
    },
];

/// One retired key found in a config: its concrete key path and the
/// replacement its row names. Never holds the key's value.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RetiredKeyHit {
    path: Vec<String>,
    replacement: &'static str,
}

/// Refusal carried by [`crate::RawConfigPreflightError::RetiredKeys`]: the
/// config carries one or more retired keys. `Display` is one line per key,
/// sorted by key path, each naming the key and what replaces it; values are
/// never rendered.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", render_hits(.hits))]
pub struct RetiredKeysError {
    hits: Vec<RetiredKeyHit>,
}

/// Refuse every key of `table` that `raw_toml` carries, in one pass. TOML
/// that does not parse passes, so the typed deserialize reports the real
/// syntax error.
pub fn preflight_retired_keys(
    raw_toml: &str,
    table: &[RetiredKey],
) -> Result<(), RetiredKeysError> {
    let Ok(root) = toml::from_str::<toml::Value>(raw_toml) else {
        return Ok(());
    };
    let mut hits: Vec<RetiredKeyHit> = table.iter().flat_map(|row| row_hits(&root, row)).collect();
    if hits.is_empty() {
        return Ok(());
    }
    hits.sort_by(|a, b| a.path.cmp(&b.path));
    Err(RetiredKeysError { hits })
}

/// The refusal line for a dotted config path that names a retired key or
/// sits under one, worded exactly as the raw-config preflight words that key
/// found in a file, so an editing command names the replacement instead of a
/// generic unknown-key error. `None` for any other path, and for a
/// value-retired key, which is still a live key.
pub fn retired_key_message(dotted_path: &str) -> Option<String> {
    retired_key_message_in(RETIRED_KEYS, dotted_path)
}

fn retired_key_message_in(table: &[RetiredKey], dotted_path: &str) -> Option<String> {
    let segments: Vec<&str> = dotted_path.split('.').collect();
    table
        .iter()
        .filter(|row| row.refused_value.is_none())
        .find_map(|row| {
            let prefix = segments.get(..row.path.len())?;
            let matches = row
                .path
                .iter()
                .zip(prefix)
                .all(|(pattern, segment)| segment_matches(pattern, segment));
            matches.then(|| render_line(prefix, row.replacement))
        })
}

fn segment_matches(pattern: &str, segment: &str) -> bool {
    if pattern == ANY_ENTRY {
        !segment.is_empty()
    } else {
        pattern == segment
    }
}

fn row_hits(root: &toml::Value, row: &RetiredKey) -> Vec<RetiredKeyHit> {
    let mut found = Vec::new();
    collect_matches(root, row.path, &mut Vec::new(), &mut found);
    found
        .into_iter()
        .filter(|(_, value)| {
            row.refused_value
                .is_none_or(|refused| value.as_str() == Some(refused))
        })
        .map(|(path, _)| RetiredKeyHit {
            path,
            replacement: row.replacement,
        })
        .collect()
}

/// Every concrete path under `node` that `pattern` names, with the value at
/// its last segment. A segment that does not land on a table ends the walk.
fn collect_matches<'v>(
    node: &'v toml::Value,
    pattern: &[&str],
    prefix: &mut Vec<String>,
    out: &mut Vec<(Vec<String>, &'v toml::Value)>,
) {
    let (Some((segment, rest)), Some(table)) = (pattern.split_first(), node.as_table()) else {
        return;
    };
    for (key, child) in matching_entries(table, segment) {
        prefix.push(key.clone());
        if rest.is_empty() {
            out.push((prefix.clone(), child));
        } else {
            collect_matches(child, rest, prefix, out);
        }
        prefix.pop();
    }
}

fn matching_entries<'t>(
    table: &'t toml::Table,
    segment: &str,
) -> Vec<(&'t String, &'t toml::Value)> {
    if segment == ANY_ENTRY {
        let mut entries: Vec<_> = table.iter().filter(|(key, _)| !key.is_empty()).collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        entries
    } else {
        table.get_key_value(segment).into_iter().collect()
    }
}

fn render_hits(hits: &[RetiredKeyHit]) -> String {
    hits.iter()
        .map(|hit| render_line(&hit.path, hit.replacement))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_line<S: AsRef<str>>(path: &[S], replacement: &str) -> String {
    let dotted = path.iter().map(AsRef::as_ref).collect::<Vec<_>>().join(".");
    format!(
        "`{}` is retired: {replacement}; delete it",
        message_safe_key(&dotted)
    )
}

#[cfg(test)]
#[path = "retired_keys_tests.rs"]
mod retired_keys_tests;
