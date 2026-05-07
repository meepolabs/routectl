//! Internal helpers shared between the request, response, and SSE
//! normalizers in `openai_compat`. Keep this small -- only utilities
//! that genuinely have more than one caller belong here.

use uuid::Uuid;

use routectl_core::{ReasoningDetail, ReasoningDetailKind};

/// Construct a `reasoning.text`-kind `ReasoningDetail` from a plain string.
/// Generates a fresh UUID per call so detail blocks are individually
/// addressable downstream.
pub(crate) fn build_reasoning_detail(text: &str, format_tag: &str, index: u32) -> ReasoningDetail {
    ReasoningDetail {
        kind: ReasoningDetailKind::Text,
        id: Some(Uuid::new_v4().to_string()),
        format: Some(format_tag.into()),
        index: Some(index),
        payload: serde_json::json!({ "text": text }),
    }
}
