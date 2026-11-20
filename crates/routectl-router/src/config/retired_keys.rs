//! The retired config-key table and the one raw-text pass that refuses every
//! retired key a config still carries, each naming what replaces it.

use super::validate::{is_bare_key, message_safe_key};

/// Path segment standing for any one non-empty entry name.
const ANY_ENTRY: &str = "*";

/// Shown in place of a path segment that is not a bare key.
const QUOTED_NAME_PLACEHOLDER: &str = "<quoted name>";

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
                      `[providers.anthropic-forwarded]` with `format = \"anthropic-messages\"`, \
                      `base_url = \"https://api.anthropic.com\"`, `credential = \"forwarded\"` \
                      and `auth = \"bearer\"`",
    },
    RetiredKey {
        path: &["mitm", "mitm_host"],
        refused_value: None,
        replacement: "pinned to https://api.anthropic.com",
    },
    RetiredKey {
        path: &["mitm", "tested_cc_version"],
        refused_value: None,
        replacement: "routectl pins the Claude Code version it mints; nothing replaces the check",
    },
    RetiredKey {
        path: &["mitm", "upstream_origin"],
        refused_value: None,
        replacement: "pinned to https://api.anthropic.com",
    },
    RetiredKey {
        path: &["models", ANY_ENTRY, "additional_request_fields"],
        refused_value: None,
        replacement: "use payload_extras",
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
        path: &["models", ANY_ENTRY, "reported_model"],
        refused_value: None,
        replacement: "the response model field echoes the requested model",
    },
    RetiredKey {
        path: &["models", ANY_ENTRY, "selectable"],
        refused_value: None,
        replacement: "delete the block or comment it out",
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
        path: &["models", ANY_ENTRY, "visible_routectl_provider"],
        refused_value: None,
        replacement: "responses carry no provider field",
    },
    RetiredKey {
        path: &["pools", ANY_ENTRY, "seat_selection"],
        refused_value: Some("round-robin"),
        replacement: "use sticky-least-loaded",
    },
    RetiredKey {
        path: &["providers", ANY_ENTRY, "allowed_betas"],
        refused_value: None,
        replacement: "nothing replaces it: client betas pass through, minus the flags the \
                      router withholds from the lane",
    },
    RetiredKey {
        path: &["providers", ANY_ENTRY, "anthropic_beta"],
        refused_value: None,
        replacement: "pin the flag in header_extras \"anthropic-beta\"",
    },
    RetiredKey {
        path: &["providers", ANY_ENTRY, "api_key_ref"],
        refused_value: None,
        replacement: "write the reference as `credential`",
    },
    RetiredKey {
        path: &["providers", ANY_ENTRY, "api_shape"],
        refused_value: None,
        replacement: "the Bedrock API is the format: `invoke` (the default) is `format = \
                      \"bedrock-invoke\"`, `converse` is `format = \"bedrock-converse\"`",
    },
    RetiredKey {
        path: &["providers", ANY_ENTRY, "auth_kind"],
        refused_value: None,
        replacement: "set `auth`: `api-key` is `x-api-key` on `anthropic-messages` over \
                      `base_url` and `bearer` everywhere else; `oauth-bearer` and \
                      `chatgpt-oauth` are `bearer`; `bedrock-mantle` is `cloud = { name = \
                      \"aws\", region = \"<region>\" }` with `auth = \"bearer\"` for a key or \
                      `auth = \"aws-sigv4\"` for an `aws://` credential",
    },
    RetiredKey {
        path: &["providers", ANY_ENTRY, "auth_mode"],
        refused_value: None,
        replacement: "set `format`: `api-key` is `format = \"gemini\"` with `auth = \
                      \"x-goog-api-key\"`, `cloud-code` is `format = \"gemini-cloud-code\"` with \
                      `auth = \"bearer\"` and `credential = \"oauth://antigravity[#label]\"`",
    },
    RetiredKey {
        path: &["providers", ANY_ENTRY, "auto_emit_per_block_breakpoints"],
        refused_value: None,
        replacement: "use the global [cache] auto_emit_top_level_breakpoint switch",
    },
    RetiredKey {
        path: &["providers", ANY_ENTRY, "auto_emit_top_level_breakpoint"],
        refused_value: None,
        replacement: "use the global [cache] auto_emit_top_level_breakpoint switch",
    },
    RetiredKey {
        path: &["providers", ANY_ENTRY, "bedrock_mantle"],
        refused_value: None,
        replacement: "set `cloud = { name = \"aws\", region = \"<region>\" }` in place of \
                      `base_url`; a `bearer-key` `key_ref` becomes `credential` with `auth = \
                      \"bearer\"`, a profile is `credential = \"aws://profile/<name>\"` and the \
                      default chain `credential = \"aws://chain\"`, both with `auth = \
                      \"aws-sigv4\"`",
    },
    RetiredKey {
        path: &["providers", ANY_ENTRY, "class_overrides"],
        refused_value: None,
        replacement: "per-class retry counts live in `[retry.classes]`; a status keeps \
                      its built-in class",
    },
    RetiredKey {
        path: &["providers", ANY_ENTRY, "cloak"],
        refused_value: None,
        replacement: "set `cloak_mode` and `cloak_strict_mode` on the entry itself",
    },
    RetiredKey {
        path: &["providers", ANY_ENTRY, "cloak_sensitive_words"],
        refused_value: None,
        replacement: "the sensitive-word scanner is removed; nothing replaces it",
    },
    RetiredKey {
        path: &["providers", ANY_ENTRY, "cloak_tool_rename"],
        refused_value: None,
        replacement: "tool names are normalized to mcp__ only",
    },
    RetiredKey {
        path: &["providers", ANY_ENTRY, "credential_source"],
        refused_value: None,
        replacement: "`forwarded` is `credential = \"forwarded\"` with `auth = \"bearer\"` on \
                      `format = \"anthropic-messages\"` and `base_url = \
                      \"https://api.anthropic.com\"`; `own` is the entry's own `credential`",
    },
    RetiredKey {
        path: &["providers", ANY_ENTRY, "creds"],
        refused_value: None,
        replacement: "write one `credential`: a `bearer-key` `key_ref` with `auth = \
                      \"bearer\"`, or `aws://profile/<name>` or `aws://chain` with `auth = \
                      \"aws-sigv4\"`; static key pairs move to a profile or the environment \
                      (read by `aws://chain`)",
    },
    RetiredKey {
        path: &["providers", ANY_ENTRY, "kind"],
        refused_value: None,
        replacement: "set `format` and `auth`: `anthropic-api` is `anthropic-messages` with \
                      `x-api-key` (`bearer` for an `oauth://` login or over `cloud`), \
                      `openai-compat` is `openai-chat` with `bearer`, `openai-responses` stays \
                      and takes `bearer`, `gemini` is `gemini` with `x-goog-api-key` (`auth_mode = \
                      \"cloud-code\"`: `gemini-cloud-code` with `bearer`), `bedrock` is \
                      `bedrock-invoke` (`api_shape = \"converse\"`: `bedrock-converse`) over \
                      `cloud` with `aws-sigv4` for an `aws://` credential or `bearer` for a key",
    },
    RetiredKey {
        path: &["providers", ANY_ENTRY, "max_thinking_entry_bytes"],
        refused_value: None,
        replacement: "the thinking-cache entry cap is fixed at 1 MiB",
    },
    RetiredKey {
        path: &["providers", ANY_ENTRY, "region"],
        refused_value: None,
        replacement: "set `cloud = { name = \"aws\", region = \"<region>\" }`",
    },
    RetiredKey {
        path: &["providers", ANY_ENTRY, "unsupported_features"],
        refused_value: None,
        replacement: "use `[capability.overrides.<provider>] unsupported`",
    },
    RetiredKey {
        path: &["registry"],
        refused_value: None,
        replacement: "set prices under `[models.X.pricing]`",
    },
    RetiredKey {
        path: &["retry", "classes", ANY_ENTRY, "fallback"],
        refused_value: None,
        replacement: "chains and pools fall back; an unknown failure stays terminal",
    },
    RetiredKey {
        path: &["retry", "classes", ANY_ENTRY, "retry"],
        refused_value: None,
        replacement: "write the count as `[retry.classes] <class> = <n>`",
    },
    RetiredKey {
        path: &["retry", "retry_on_429"],
        refused_value: None,
        replacement: "set `[retry.classes] rate-limited = <n>`",
    },
    RetiredKey {
        path: &["retry", "retry_on_5xx"],
        refused_value: None,
        replacement: "set `[retry.classes] server-error = <n>` and `overloaded = <n>`",
    },
    RetiredKey {
        path: &["retry", "retry_on_network"],
        refused_value: None,
        replacement: "set `[retry.classes] timeout = <n>` and `network-error = <n>`",
    },
    RetiredKey {
        path: &["seat_quota"],
        refused_value: None,
        replacement: "pace placement always applies on sticky-least-loaded pools",
    },
    RetiredKey {
        path: &["server", "allow_disable_fallbacks"],
        refused_value: None,
        replacement: "the x-routectl-disable-fallbacks header is always honored",
    },
    RetiredKey {
        path: &["server", "strict_translation"],
        refused_value: None,
        replacement: "lossy seams forward and count",
    },
    RetiredKey {
        path: &["trim"],
        refused_value: None,
        replacement: "the advisory trim uses fixed defaults",
    },
    RetiredKey {
        path: &["usage", "enabled"],
        refused_value: None,
        replacement: "usage recording is always on",
    },
    RetiredKey {
        path: &["wire_check"],
        refused_value: None,
        replacement: "sampling is fixed: first 8 per lane and version, then 1 in 100; 24 h \
                      status window; set ROUTECTL_WIRE_CHECK_SAMPLE_ONE_IN in the environment \
                      to change the 1-in-100 rate",
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
    let dotted = path
        .iter()
        .map(|segment| shown_segment(segment.as_ref()))
        .collect::<Vec<_>>()
        .join(".");
    format!(
        "`{}` is retired: {replacement}; delete it",
        message_safe_key(&dotted)
    )
}

fn shown_segment(segment: &str) -> &str {
    if is_bare_key(segment) {
        segment
    } else {
        QUOTED_NAME_PLACEHOLDER
    }
}

#[cfg(test)]
#[path = "retired_keys_tests.rs"]
mod retired_keys_tests;
