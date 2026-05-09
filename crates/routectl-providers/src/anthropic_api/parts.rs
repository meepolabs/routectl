//! Per-content-block helpers shared across Anthropic request normalization.
//!
//! - `parse_image_url_source` translates an OpenAI-style `image_url.url`
//!   into the Anthropic `source` block (RFC 2397 data URI -> base64 form,
//!   everything else -> URL form). Includes a defense-in-depth media-type
//!   allowlist and case normalization so a crafted `Image/PNG;base64,...`
//!   can't bypass the filter and ship a non-allowlisted MIME type
//!   verbatim to upstream.
//!
//! - `strip_text_after_tool_use` trims trailing `Text` blocks that follow
//!   the last `ToolUse` in an assistant message. Bedrock + Anthropic both
//!   reject this shape on echo with "tool_use ids were found without
//!   tool_result blocks immediately after"; real claude-code drops it
//!   before resending.

use serde_json::{json, Value};

use routectl_core::{ContentPart, KnownContentPart};

/// Anthropic + Bedrock accept these MIME types as image sources today.
/// We allowlist them defensively so a malicious / typo'd `media_type`
/// (e.g. `application/x-script` from a crafted data URI) never lands
/// in the Anthropic request body. Anything outside the allowlist falls
/// back to URL-source form, which the upstream rejects with a clean
/// error.
///
/// All entries are lowercase; the allowlist comparison normalizes
/// caller input before lookup so `Image/PNG` matches `image/png`.
const ALLOWED_IMAGE_MEDIA_TYPES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Translate an OpenAI-style `image_url.url` value into the Anthropic
/// `source` block. Detects RFC 2397 data URIs (`data:<mt>;base64,<b64>`)
/// and rewrites them to `{type: "base64", media_type, data}` because
/// Bedrock + Anthropic both reject `{type: "url"}` for data URIs with
/// "URL sources are not supported". HTTPS / gs:// / etc. flow through
/// as URL sources unchanged. Malformed data URIs (no `;base64,`
/// separator), unsupported media types, and empty URLs fall back to
/// URL form so the upstream surfaces a clean error rather than us
/// silently dropping the block or shipping a crafted media_type.
///
/// Defenses:
/// - RFC 2397 parameters between media-type and `;base64,` (e.g.
///   `data:image/png;charset=utf-8;base64,XXX`) are stripped from
///   `media_type` before allowlist comparison. Without this, the
///   composite `image/png;charset=utf-8` would either reach Bedrock
///   verbatim (-> 400) or fail allowlist match.
/// - Case normalization: media types are matched case-insensitively
///   against the allowlist (RFC 2045 says MIME types are
///   case-insensitive). A request carrying `Image/PNG` is treated as
///   `image/png` and the LOWERCASED form is what flows to upstream;
///   without this, `Image/PNG;base64,...` would either bypass the
///   allowlist (allowing arbitrary `media_type` to ship verbatim) or
///   fail allowlist match while the original mixed-case string passes.
/// - Allowlist of standard image MIME types prevents a client-supplied
///   `data:application/x-script;base64,...` from echoing into the
///   Anthropic body's `media_type` field (defense-in-depth; Anthropic
///   would reject it but we don't rely on that).
/// - Empty URL emits a `tracing::warn!` so misbehaving callers don't
///   produce silent Bedrock 400s.
pub(crate) fn parse_image_url_source(url: &str) -> Value {
    if url.is_empty() {
        tracing::warn!("empty image_url.url -- upstream will reject");
        return json!({"type": "url", "url": ""});
    }
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some((mt_with_params, b64)) = rest.split_once(";base64,") {
            // RFC 2397 allows `;<param>` between media-type and the
            // `;base64` flag (browser tooling sometimes emits
            // `;charset=utf-8`). Take the bare media-type for the
            // allowlist check + emission.
            let raw_media_type = mt_with_params.split(';').next().unwrap_or(mt_with_params);
            let media_type_lc = raw_media_type.to_ascii_lowercase();
            if ALLOWED_IMAGE_MEDIA_TYPES.contains(&media_type_lc.as_str()) {
                if b64.is_empty() {
                    // Truncated upload / racey image picker on the
                    // client side. Bedrock + Anthropic both 400 on
                    // empty data with a vague "invalid base64"
                    // message; surface the cause here so operators
                    // see WHY their request died.
                    tracing::warn!(
                        media_type = %media_type_lc,
                        "data: URI with empty base64 payload -- falling back to URL form (upstream will reject)",
                    );
                    return json!({"type": "url", "url": url});
                }
                // Emit the lowercased media type even if the caller
                // sent a mixed-case form. Avoids forwarding a
                // non-canonical string and keeps the wire body
                // deterministic across casing variations.
                return json!({
                    "type": "base64",
                    "media_type": media_type_lc,
                    "data": b64,
                });
            }
            tracing::warn!(
                media_type = %media_type_lc,
                "data: URI with non-allowlisted media_type -- falling back to URL form (upstream will reject)",
            );
        }
    }
    json!({"type": "url", "url": url})
}

/// Strip `Text` content parts that appear AFTER the last `ToolUse`
/// part in an assistant message. Required for Bedrock + Anthropic
/// upstream compatibility: when an assistant turn has stop_reason
/// `tool_use`, claude 4 occasionally emits a transition text block
/// after the tool_use (e.g. "Sure, let me run that for you!"). On
/// echo, both upstreams reject this shape with the error
/// `"tool_use ids were found without tool_result blocks immediately
/// after"`. Real claude-code drops the trailing text before resending;
/// we mirror that here so any client gets the same correctness for
/// free.
///
/// Behavior:
/// - No `tool_use` block: returns the parts unchanged (allocation
///   only because the caller needs an owned `Vec<ContentPart>`).
/// - `tool_use` present: drops any `Text` block whose index is past
///   the last `tool_use` index. Other block types after `tool_use`
///   (`Image`, `Document`, etc.) are extremely unusual but pass
///   through unchanged -- only the specific text-after-tool_use case
///   trips the upstream validator.
/// - Emits a `tracing::warn!` per stripped block with
///   `dropped_text_len` so operators can correlate strip events to
///   model behavior. `request_id` is inherited from the parent span.
pub(crate) fn strip_text_after_tool_use(parts: &[ContentPart]) -> Vec<ContentPart> {
    let last_tool_use = parts
        .iter()
        .rposition(|p| matches!(p, ContentPart::Known(KnownContentPart::ToolUse { .. })));
    let Some(last_idx) = last_tool_use else {
        return parts.to_vec();
    };
    let mut out: Vec<ContentPart> = Vec::with_capacity(parts.len());
    for (i, p) in parts.iter().enumerate() {
        if i > last_idx {
            if let ContentPart::Known(KnownContentPart::Text { text, .. }) = p {
                tracing::warn!(
                    dropped_text_len = text.len(),
                    "stripped text block after tool_use in assistant content (Bedrock/Anthropic reject this shape on echo)",
                );
                continue;
            }
        }
        out.push(p.clone());
    }
    out
}

#[cfg(test)]
mod parse_image_url_source_tests {
    use super::parse_image_url_source;
    use serde_json::json;

    #[test]
    fn data_uri_with_base64_payload_emits_anthropic_base64_source() {
        // OpenAI multimodal clients embed images as
        // data:image/png;base64,XXX. Anthropic + Bedrock require the
        // base64 source form for these; URL form is rejected with
        // "URL sources are not supported".
        let url = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAE=";
        let got = parse_image_url_source(url);
        assert_eq!(
            got,
            json!({
                "type": "base64",
                "media_type": "image/png",
                "data": "iVBORw0KGgoAAAANSUhEUgAAAAE=",
            })
        );
    }

    #[test]
    fn data_uri_with_other_media_types_round_trips() {
        let cases = [
            ("data:image/jpeg;base64,QUJDREU=", "image/jpeg", "QUJDREU="),
            ("data:image/webp;base64,V0VCUA==", "image/webp", "V0VCUA=="),
            ("data:image/gif;base64,R0lGODlh", "image/gif", "R0lGODlh"),
        ];
        for (url, media, b64) in cases {
            let got = parse_image_url_source(url);
            assert_eq!(got["type"], "base64", "type for {url}");
            assert_eq!(got["media_type"], media, "media_type for {url}");
            assert_eq!(got["data"], b64, "data for {url}");
        }
    }

    #[test]
    fn https_url_passes_through_as_url_source() {
        let url = "https://example.com/image.png";
        let got = parse_image_url_source(url);
        assert_eq!(got, json!({"type": "url", "url": url}));
    }

    #[test]
    fn malformed_data_uri_falls_back_to_url_source() {
        // No `;base64,` separator -- can't parse safely. Fall back
        // to URL source so upstream surfaces a clean error rather
        // than us silently dropping the block.
        let url = "data:image/png,not-base64";
        let got = parse_image_url_source(url);
        assert_eq!(got, json!({"type": "url", "url": url}));
    }

    #[test]
    fn empty_url_passes_through_as_url_source() {
        let got = parse_image_url_source("");
        assert_eq!(got, json!({"type": "url", "url": ""}));
    }

    #[test]
    fn data_uri_with_charset_param_strips_param_from_media_type() {
        // RFC 2397 allows `;<param>` between media-type and `;base64`.
        // Browser tooling (Chrome DevTools "copy as cURL", some
        // Electron clients) emits `data:image/png;charset=utf-8;base64,
        // XXX`. Without the param-strip, media_type would be
        // `image/png;charset=utf-8` and Bedrock would 400.
        let url = "data:image/png;charset=utf-8;base64,iVBORw0KGgoAAAA=";
        let got = parse_image_url_source(url);
        assert_eq!(got["type"], "base64");
        assert_eq!(got["media_type"], "image/png");
        assert_eq!(got["data"], "iVBORw0KGgoAAAA=");
    }

    #[test]
    fn data_uri_with_empty_base64_payload_falls_back_to_url() {
        // Empty data after `;base64,` is a real client-bug shape
        // (truncated upload, racey image picker). Send-as-base64
        // would just produce a vague upstream 400 with no
        // explanation; URL fallback at least lets the operator
        // see the empty URL string in logs.
        let url = "data:image/png;base64,";
        let got = parse_image_url_source(url);
        assert_eq!(got, json!({"type": "url", "url": url}));
    }

    #[test]
    fn data_uri_with_unsupported_media_type_falls_back_to_url() {
        // Defense-in-depth: a malicious or typo'd media_type
        // (`application/x-script`, `text/html`, etc.) must NOT land
        // in the Anthropic body verbatim. Fall back to URL form so
        // upstream rejects with a clean error.
        for url in [
            "data:application/x-script;base64,YWxlcnQoMSk=",
            "data:text/html;base64,PGltZyBzcmM9eD4=",
            "data:image/svg+xml;base64,PHN2Zz4=",
        ] {
            let got = parse_image_url_source(url);
            assert_eq!(got["type"], "url", "expected URL fallback for {url}");
        }
    }

    #[test]
    fn data_uri_with_mixed_case_media_type_normalizes_to_lowercase() {
        // RFC 2045 says MIME types are case-insensitive. A client
        // sending `Image/PNG` or `IMAGE/JPEG` MUST be treated as
        // the canonical lowercase form. Without case normalization,
        // mixed-case input would either:
        //   (a) bypass the allowlist (allowing arbitrary `media_type`
        //       through to upstream verbatim), OR
        //   (b) fail the allowlist while the upstream still accepts
        //       the underlying media type.
        // Either outcome contradicts the defense-in-depth comment on
        // the allowlist constant. We must normalize, allowlist
        // against the lowercase form, AND emit the lowercase form
        // to upstream so the wire body is deterministic.
        let cases = [
            ("data:Image/PNG;base64,iVBORw0KGgo=", "image/png"),
            ("data:IMAGE/JPEG;base64,QUJD=", "image/jpeg"),
            ("data:Image/Webp;base64,V0VCUA==", "image/webp"),
        ];
        for (url, expected_lc) in cases {
            let got = parse_image_url_source(url);
            assert_eq!(got["type"], "base64", "type for {url}");
            assert_eq!(
                got["media_type"], expected_lc,
                "media_type for {url} must be lowercased"
            );
        }
    }
}

#[cfg(test)]
mod strip_text_after_tool_use_tests {
    use super::strip_text_after_tool_use;
    use routectl_core::{ContentPart, KnownContentPart};

    fn text(s: &str) -> ContentPart {
        ContentPart::Known(KnownContentPart::Text {
            text: s.into(),
            cache_control: None,
        })
    }

    fn tool_use(id: &str) -> ContentPart {
        ContentPart::Known(KnownContentPart::ToolUse {
            id: id.into(),
            name: "calc".into(),
            input: serde_json::json!({}),
            cache_control: None,
        })
    }

    #[test]
    fn no_tool_use_returns_unchanged() {
        let parts = vec![text("hello"), text("world")];
        let got = strip_text_after_tool_use(&parts);
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn drops_text_after_last_tool_use() {
        // The exact claude 4 multi-block pattern: thinking-equivalent
        // text + tool_use + transition text. Trailing text dropped.
        let parts = vec![
            text("Let me calculate."),
            tool_use("toolu_1"),
            text("Sure! On it."),
        ];
        let got = strip_text_after_tool_use(&parts);
        assert_eq!(got.len(), 2);
        assert!(matches!(
            got[0],
            ContentPart::Known(KnownContentPart::Text { .. })
        ));
        assert!(matches!(
            got[1],
            ContentPart::Known(KnownContentPart::ToolUse { .. })
        ));
    }

    #[test]
    fn keeps_text_before_tool_use() {
        let parts = vec![text("preface"), tool_use("toolu_1")];
        let got = strip_text_after_tool_use(&parts);
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn handles_multiple_tool_uses_dropping_only_post_last() {
        // Multiple tool_uses in one assistant turn (rare but legal).
        // Text between tool_uses stays; text after the LAST tool_use
        // is dropped.
        let parts = vec![
            tool_use("toolu_1"),
            text("intermediate"),
            tool_use("toolu_2"),
            text("trailing -- dropped"),
        ];
        let got = strip_text_after_tool_use(&parts);
        assert_eq!(got.len(), 3);
        // Verify the trailing text was the one dropped (intermediate kept).
        assert!(matches!(
            got[1],
            ContentPart::Known(KnownContentPart::Text { ref text, .. }) if text == "intermediate"
        ));
    }
}
