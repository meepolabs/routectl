//! Forwarded-media URL check for the Bedrock Mantle Responses lane.
//!
//! Mantle fetches media URLs server-side under the operator's AWS identity,
//! so every one it could dereference must pass
//! [`crate::mantle::refuse_unforwardable_media_urls`]. The check runs on the
//! serialized body, after translation and the passthrough splice, so every
//! `input` item that reaches the wire is walked once whichever path produced
//! it, at any depth and whatever its item kind.

use routectl_core::Result;
use serde_json::Value;

/// Refuse a Mantle Responses body whose `input` carries a media URL the
/// upstream would fetch, unless it is `https` or `data`. Top-level `tools`
/// sit outside `input` and are never walked; inside `input`, only the
/// `tools` of a direct tool-declaring item is skipped.
pub(super) fn check_forwarded_media_urls(provider_id: &str, body: &Value) -> Result<()> {
    match body.get("input") {
        Some(input) => crate::mantle::refuse_unforwardable_media_urls(
            provider_id,
            crate::mantle::MediaWalkRoot::ResponsesInput,
            input,
            "input",
        ),
        None => Ok(()),
    }
}

#[cfg(test)]
#[path = "mantle_media_tests.rs"]
mod tests;
