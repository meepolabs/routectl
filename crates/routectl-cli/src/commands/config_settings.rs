//! The `settings` block of `routectl config show --effective`: every scalar
//! setting with the layer it came from.
//!
//! A leaf reads `(file)` when `config.toml` writes its key and `(default)`
//! when the value is the one in code. A key the runtime resolves from
//! another config entry reads `(from <path>)`. The values come from the redacted tree `config show`
//! serializes, so this block can never print a secret the plain dump
//! withholds; the raw text is parsed only to test key presence and none of
//! its values are printed.
//!
//! An `Option` key that is unset is absent from the serialized tree. The
//! ones whose unset value is a concrete code default are listed in
//! [`CODE_DEFAULTS`] and printed with the value their owner resolves; the
//! others are either resolved by another section of the effective view or
//! mean "off" or "inherit" when unset. A census test over the config JSON
//! Schema keeps that classification complete.

use std::collections::BTreeMap;

use routectl_core::{Error, Result};
use routectl_router::Config;
use toml::{Table, Value};

use super::config::redacted_config;

/// Where a printed value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SettingSource {
    /// `config.toml` writes the key.
    File,
    /// Nothing writes the key; the value is the one in code.
    Default,
    /// Another entry's key, at this rendered path, supplies the value.
    From(String),
}

impl SettingSource {
    fn render(&self) -> String {
        match self {
            Self::File => "file".to_string(),
            Self::Default => "default".to_string(),
            Self::From(path) => format!("from {path}"),
        }
    }
}

/// The table an unset code-default key belongs to.
#[derive(Debug, Clone, Copy)]
pub enum DefaultScope {
    /// A top-level section such as `[retry]`.
    Section(&'static str),
    /// Every `[providers.X]` entry of one `kind`.
    ProviderKind(&'static str),
}

/// How the runtime resolves one [`CODE_DEFAULTS`] key.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    /// The key as `config.toml` writes it applies.
    File,
    /// The code default applies.
    Default(Value),
    /// Another entry's key, at this dotted path, supplies the value.
    From(Value, Vec<String>),
}

/// What a [`CodeDefault`] resolves against: the loaded config and whether
/// the config sets the key in the table being resolved.
pub struct ResolveCx<'a> {
    pub config: &'a Config,
    pub is_set: bool,
}

/// One `Option` key whose unset value is a concrete default in code.
pub struct CodeDefault {
    pub scope: DefaultScope,
    pub key: &'static str,
    /// The value in force and where it comes from, read from the key's
    /// runtime owner.
    pub resolve: fn(&ResolveCx<'_>) -> Resolution,
}

/// The `Option` keys printed with their code default when unset.
pub const CODE_DEFAULTS: &[CodeDefault] = &[
    CodeDefault {
        scope: DefaultScope::Section("retry"),
        key: "max_honored_retry_after_ms",
        resolve: |cx| {
            if cx.is_set {
                return Resolution::File;
            }
            let ceiling = routectl_router::RetryPolicy::default().max_honored_retry_after();
            Resolution::Default(Value::Integer(
                i64::try_from(ceiling.as_millis()).unwrap_or(i64::MAX),
            ))
        },
    },
    CodeDefault {
        scope: DefaultScope::ProviderKind("anthropic-api"),
        key: "max_thinking_entry_bytes",
        resolve: |cx| {
            if cx.is_set {
                return Resolution::File;
            }
            Resolution::Default(usize_value(
                routectl_providers::anthropic_api::AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
            ))
        },
    },
    CodeDefault {
        scope: DefaultScope::ProviderKind("openai-responses"),
        key: "codex_version",
        resolve: codex_version_resolution,
    },
];

/// An entry's own `codex_version` applies as written; an unset one runs
/// under the config-wide value another entry sets, else the pinned build.
fn codex_version_resolution(cx: &ResolveCx<'_>) -> Resolution {
    if cx.is_set {
        return Resolution::File;
    }
    match routectl_router::configured_codex_version(cx.config) {
        Some((name, version)) => Resolution::From(
            Value::String(version.to_string()),
            vec![
                "providers".to_string(),
                name.to_string(),
                "codex_version".to_string(),
            ],
        ),
        None => Resolution::Default(Value::String(
            routectl_core::identity::codex::PINNED_CODEX_VERSION.to_string(),
        )),
    }
}

fn usize_value(n: usize) -> Value {
    Value::Integer(i64::try_from(n).unwrap_or(i64::MAX))
}

/// One printed setting: its key path, value and source.
type Settings = BTreeMap<Vec<String>, (Value, SettingSource)>;

/// [`settings_lines`] over `config` as `config show` prints it: secrets
/// redacted, each provider `base_url` reduced to its origin.
pub fn redacted_settings_lines(config: &Config, raw_text: &str) -> Result<Vec<String>> {
    let tree = Table::try_from(redacted_config(config))
        .map_err(|e| Error::Internal(format!("serialize: {e}")))?;
    // The parse error quotes the offending source line, which may hold a
    // secret; the loader has already parsed this text, so name no detail.
    settings_lines(config, &tree, raw_text)
        .map_err(|_| Error::Config("config.toml does not parse as TOML".to_string()))
}

/// The settings block, one `  path=value(source)` line per setting, sorted
/// by key path.
///
/// `tree` is `config`, redacted, as `config show` serializes it; `raw_text`
/// is the config file it was loaded from. Returns `Err` only when
/// `raw_text` does not parse as TOML.
fn settings_lines(
    config: &Config,
    tree: &Table,
    raw_text: &str,
) -> std::result::Result<Vec<String>, toml::de::Error> {
    let raw: Table = toml::from_str(raw_text)?;
    let mut settings = Settings::new();
    collect_leaves(tree, &mut Vec::new(), &raw, &mut settings);
    apply_code_defaults(config, tree, &mut settings);
    Ok(settings
        .into_iter()
        .map(|(path, (value, source))| {
            format!("  {}={value}({})", render_path(&path), source.render())
        })
        .collect())
}

/// Record every non-table value under `table`. An array is one setting.
fn collect_leaves(table: &Table, path: &mut Vec<String>, raw: &Table, out: &mut Settings) {
    for (key, value) in table {
        path.push(key.clone());
        if let Value::Table(child) = value {
            collect_leaves(child, path, raw, out);
        } else {
            let source = if tree_has_path(raw, path) {
                SettingSource::File
            } else {
                SettingSource::Default
            };
            out.insert(path.clone(), (value.clone(), source));
        }
        path.pop();
    }
}

/// Whether `table` holds a key at `path`. Over the parsed config file,
/// dotted keys, inline tables and `[section]` headers all parse to the same
/// table, so every spelling of a key answers alike.
fn tree_has_path(raw: &Table, path: &[String]) -> bool {
    let Some((last, parents)) = path.split_last() else {
        return false;
    };
    let mut table = raw;
    for segment in parents {
        match table.get(segment) {
            Some(Value::Table(child)) => table = child,
            _ => return false,
        }
    }
    table.contains_key(last)
}

/// Resolve each [`CODE_DEFAULTS`] key in every table its scope names. A
/// key the runtime takes from the code default or another entry prints that
/// value and source in place of any file leaf; a key it takes from the file
/// keeps the leaf.
fn apply_code_defaults(config: &Config, tree: &Table, out: &mut Settings) {
    for default in CODE_DEFAULTS {
        for table in scope_tables(tree, default.scope) {
            let mut path = table;
            path.push(default.key.to_string());
            let cx = ResolveCx {
                config,
                is_set: tree_has_path(tree, &path),
            };
            let resolution = (default.resolve)(&cx);
            let resolved = match resolution {
                Resolution::File => continue,
                Resolution::Default(value) => (value, SettingSource::Default),
                Resolution::From(value, from) => (value, SettingSource::From(render_path(&from))),
            };
            out.insert(path, resolved);
        }
    }
}

/// The key path of every table `scope` names in `tree`.
fn scope_tables(tree: &Table, scope: DefaultScope) -> Vec<Vec<String>> {
    match scope {
        DefaultScope::Section(name) => vec![vec![name.to_string()]],
        DefaultScope::ProviderKind(kind) => {
            let Some(Value::Table(providers)) = tree.get("providers") else {
                return Vec::new();
            };
            providers
                .iter()
                .filter(|(_, entry)| entry.get("kind").and_then(Value::as_str) == Some(kind))
                .map(|(name, _)| vec!["providers".to_string(), name.clone()])
                .collect()
        }
    }
}

/// A dotted key path, quoting any segment that is not a bare TOML key.
fn render_path(path: &[String]) -> String {
    path.iter()
        .map(|segment| {
            let is_bare = !segment.is_empty()
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
            if is_bare {
                segment.clone()
            } else {
                Value::String(segment.clone()).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

#[cfg(test)]
#[path = "config_settings_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "config_settings_resolution_tests.rs"]
mod resolution_tests;
