//! The one emitter for `[providers.<name>]` blocks the CLI creates.
//!
//! Every provider entry routectl writes or prints is built here, so the
//! block `provider add` commits, the block `login` writes, and the block
//! either prints for the operator to paste are one table. The entry is
//! serialized with serde, so a field the typed entry carries cannot be
//! dropped by a hand-kept key list; the emitter only decides key ORDER and
//! TOML encoding.
//!
//! It builds new blocks only. A block already in the operator's file is
//! never passed through here to be rewritten: its key order and comments
//! are the operator's.

use routectl_core::{Error, Result};
use routectl_router::ProviderEntry;
use serde::Serialize;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value};

/// The keys that say what an entry is, in the order every block is
/// written: what it speaks, where it goes, which secret, how the secret is
/// sent, then the companions. At most one of `base_url` / `cloud` is
/// present on a valid entry.
const IDENTITY_KEYS: [&str; 7] = [
    "format",
    "base_url",
    "cloud",
    "credential",
    "auth",
    "account_id_ref",
    "cloud_project_id",
];

/// Build the `[providers.<name>]` table for `entry`: identity keys first in
/// [`IDENTITY_KEYS`] order, then every other key in the entry's
/// declaration order. Every value is inline, so the block is one header
/// followed by one line per key (`cloud = { name = "aws", region = ... }`).
/// Empty collections are dropped: they carry no operator intent.
///
/// # Errors
///
/// The entry holds a value TOML cannot represent (a JSON `null` inside
/// `payload_extras`).
pub(crate) fn provider_entry_table(entry: &ProviderEntry) -> Result<Table> {
    let fields = serialized_fields(entry)?;
    let mut table = Table::new();
    table.set_implicit(false);
    for key in IDENTITY_KEYS {
        if let Some(value) = fields.get(key) {
            table.insert(key, Item::Value(normalized(value)?));
        }
    }
    for (key, value) in &fields {
        if !table.contains_key(key) && !is_empty_value(value) {
            table.insert(key, Item::Value(normalized(value)?));
        }
    }
    Ok(table)
}

/// Render `table` as a pasteable `[providers.<name>]` block: the header
/// plus one `key = value` line per field, `=` aligned, the same values the
/// written table carries.
pub(crate) fn render_provider_block(name: &str, table: &Table) -> String {
    let width = table.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let mut out = format!("[providers.{}]\n", toml_key(name));
    for (key, item) in table {
        let Some(value) = item.as_value() else {
            continue;
        };
        let mut value = value.clone();
        value.decor_mut().clear();
        out.push_str(&format!("{key:<width$} = {value}\n"));
    }
    out
}

/// The entry serialized as one inline table, in serde's field order. The
/// inline form is what keeps declaration order: the document serializer
/// moves table-valued fields after every plain value.
fn serialized_fields(entry: &ProviderEntry) -> Result<InlineTable> {
    let mut text = String::new();
    entry
        .serialize(toml::ser::ValueSerializer::new(&mut text))
        .map_err(|e| Error::Config(format!("serialize provider entry: {e}")))?;
    let doc: DocumentMut = format!("entry = {text}\n")
        .parse()
        .map_err(|e| Error::Config(format!("reparse serialized provider entry: {e}")))?;
    doc.get("entry")
        .and_then(Item::as_inline_table)
        .cloned()
        .ok_or_else(|| Error::Config("serialized provider entry is not a table".into()))
}

/// `value` with the serializer's spacing reset, so the written table and
/// the rendered block format it the same way, and every string, at any
/// depth, in the escaped one-line form of [`toml_string`]: left to its
/// default, `toml_edit` writes a string holding a newline as a raw
/// multi-line literal, putting the control character itself on screen.
fn normalized(value: &Value) -> Result<Value> {
    let mut value = match value {
        Value::String(s) => toml_string(s.value())
            .parse::<Value>()
            .map_err(|e| Error::Config(format!("encode provider entry string: {e}")))?,
        Value::Array(array) => {
            let mut out = Array::new();
            for item in array {
                out.push_formatted(normalized(item)?);
            }
            out.fmt();
            Value::Array(out)
        }
        Value::InlineTable(inline) => {
            let mut out = InlineTable::new();
            for (key, item) in inline {
                out.insert(key, normalized(item)?);
            }
            out.fmt();
            Value::InlineTable(out)
        }
        other => other.clone(),
    };
    value.decor_mut().clear();
    Ok(value)
}

fn is_empty_value(value: &Value) -> bool {
    value
        .as_array()
        .map(Array::is_empty)
        .or_else(|| value.as_inline_table().map(InlineTable::is_empty))
        .unwrap_or(false)
}

/// Render `name` as a TOML table key: bare when it is made only of
/// characters a bare key permits, quoted otherwise. An operator label is
/// only checked for non-emptiness upstream, so it can carry a space or a
/// dot that would split the key path if written bare.
pub(crate) fn toml_key(name: &str) -> String {
    let is_bare = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if is_bare {
        name.to_string()
    } else {
        toml_string(name)
    }
}

/// Render `value` as a TOML basic string.
///
/// TOML forbids raw control characters inside a basic string outright, not
/// just the quote and backslash that would terminate or reinterpret it. A
/// member or label name carrying a newline is reachable -- pool member names
/// come from operator-written config, and only non-emptiness is checked
/// upstream -- and a raw one would make the PRINTED delta unparseable while
/// the committed file (written through `toml_edit`, which escapes properly)
/// stayed valid. An operator pasting the shown block would then hit a syntax
/// error routectl appeared to have authored.
///
/// The three whitespace controls get their short escapes; every other C0
/// control plus DEL takes the `\uXXXX` form.
pub(crate) fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
#[path = "provider_entry_tests.rs"]
mod tests;
