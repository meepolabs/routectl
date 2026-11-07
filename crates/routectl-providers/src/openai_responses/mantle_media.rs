//! Forwarded-media URL check for the Bedrock Mantle Responses lane.
//!
//! Mantle fetches `input_image.image_url` and `input_file.file_url`
//! server-side under the operator's AWS identity, so a URL it would
//! dereference must be `https` or `data` (see
//! [`crate::mantle::is_forwardable_media_url`]). The check runs on the
//! serialized body, after translation and the passthrough splice, so every
//! item that reaches the wire is seen once whichever path produced it.

use routectl_core::{Error, RefusalReason, Result};
use serde_json::Value;

/// Item fields whose arrays hold content blocks. Function-call `arguments`
/// and top-level `tools` are never walked: they are not fetched upstream.
const BLOCK_ARRAY_KEYS: [&str; 2] = ["content", "output"];

/// Refuse a Mantle request body carrying an image or file URL the upstream
/// would fetch, unless it is `https` or `data`. An absent or null URL field
/// passes (nothing to fetch); an empty or non-string one refuses.
pub(super) fn check_forwarded_media_urls(provider_id: &str, body: &Value) -> Result<()> {
    let Some(input) = body.get("input").and_then(Value::as_array) else {
        return Ok(());
    };
    for (item_index, item) in input.iter().enumerate() {
        for key in BLOCK_ARRAY_KEYS {
            let Some(blocks) = item.get(key).and_then(Value::as_array) else {
                continue;
            };
            for (block_index, block) in blocks.iter().enumerate() {
                let Some(field) = media_url_field(block) else {
                    continue;
                };
                if !is_forwardable(block.get(field)) {
                    return Err(refusal(provider_id, field, item_index, key, block_index));
                }
            }
        }
    }
    Ok(())
}

/// The URL field Mantle fetches for this block's type, if any.
fn media_url_field(block: &Value) -> Option<&'static str> {
    match block.get("type").and_then(Value::as_str)? {
        "input_image" => Some("image_url"),
        "input_file" => Some("file_url"),
        _ => None,
    }
}

fn is_forwardable(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::String(url)) => crate::mantle::is_forwardable_media_url(url),
        Some(_) => false,
    }
}

/// The refusal names the field and its position, never the URL or any part
/// of it: a client URL can carry a bucket, host or signed query.
fn refusal(
    provider_id: &str,
    field: &str,
    item_index: usize,
    key: &str,
    block_index: usize,
) -> Error {
    Error::normalize_request(
        provider_id,
        format!(
            "`{field}` at input[{item_index}].{key}[{block_index}] is not an https or data \
             URL; bedrock-mantle fetches media URLs itself and accepts only https and data"
        ),
        RefusalReason::Unrepresentable,
    )
}

#[cfg(test)]
#[path = "mantle_media_tests.rs"]
mod tests;
