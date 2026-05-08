//! Region -> bedrock-runtime endpoint resolution.
//!
//! Bedrock follows the standard AWS regional endpoint pattern:
//! `https://bedrock-runtime.<region>.amazonaws.com`. Cross-region
//! inference profiles (`us.`, `eu.`, `apac.`, `global.`) are encoded
//! in the *model id*, not the endpoint -- so the endpoint always uses
//! the regional hostname even for `global.` model ids.

/// Build the base URL for the Bedrock-runtime endpoint in `region`.
///
/// Example: `bedrock_runtime_url("us-west-2")` -> `"https://bedrock-runtime.us-west-2.amazonaws.com"`.
pub fn bedrock_runtime_url(region: &str) -> String {
    format!("https://bedrock-runtime.{region}.amazonaws.com")
}

/// Build the InvokeModel URL for `model_id` in `region`. `streaming`
/// switches between `/invoke` and `/invoke-with-response-stream`.
pub fn invoke_url(region: &str, model_id: &str, streaming: bool) -> String {
    let suffix = if streaming {
        "invoke-with-response-stream"
    } else {
        "invoke"
    };
    let encoded = urlencoded(model_id);
    format!("{}/model/{encoded}/{suffix}", bedrock_runtime_url(region))
}

/// Build the Converse URL for `model_id` in `region`.
pub fn converse_url(region: &str, model_id: &str, streaming: bool) -> String {
    let suffix = if streaming {
        "converse-stream"
    } else {
        "converse"
    };
    let encoded = urlencoded(model_id);
    format!("{}/model/{encoded}/{suffix}", bedrock_runtime_url(region))
}

/// Bedrock model ids may contain bracket-suffixed inference profile
/// markers (`[1m]`) or full ARN forms
/// (`arn:aws:bedrock:us-west-2:123456789012:inference-profile/...`).
/// Both shapes need careful path-segment encoding: brackets are
/// reserved in URI path components, and ARN colons/slashes must be
/// percent-encoded so the AWS endpoint doesn't reinterpret the path.
///
/// We don't pull in a full URL crate just for this -- the encoding
/// surface is small and bounded.
fn urlencoded(model_id: &str) -> String {
    let mut out = String::with_capacity(model_id.len());
    for c in model_id.chars() {
        match c {
            '[' => out.push_str("%5B"),
            ']' => out.push_str("%5D"),
            ':' => out.push_str("%3A"),
            '/' => out.push_str("%2F"),
            ' ' => out.push_str("%20"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invoke_url_includes_streaming_suffix() {
        assert_eq!(
            invoke_url("us-west-2", "anthropic.claude-opus-4-7", true),
            "https://bedrock-runtime.us-west-2.amazonaws.com/model/anthropic.claude-opus-4-7/invoke-with-response-stream",
        );
        assert_eq!(
            invoke_url("us-west-2", "anthropic.claude-opus-4-7", false),
            "https://bedrock-runtime.us-west-2.amazonaws.com/model/anthropic.claude-opus-4-7/invoke",
        );
    }

    #[test]
    fn converse_url_handles_inference_profile_prefix() {
        assert_eq!(
            converse_url("us-west-2", "global.anthropic.claude-opus-4-7", false),
            "https://bedrock-runtime.us-west-2.amazonaws.com/model/global.anthropic.claude-opus-4-7/converse",
        );
    }

    #[test]
    fn url_encodes_arn_colons_and_slashes() {
        // Cross-account inference profile ARNs are a real Bedrock
        // shape; their colons/slashes MUST be percent-encoded so the
        // AWS endpoint sees one path segment rather than several.
        let arn = "arn:aws:bedrock:us-west-2:123456789012:inference-profile/abc";
        let url = invoke_url("us-west-2", arn, false);
        assert!(
            url.contains("/model/arn%3Aaws%3Abedrock%3Aus-west-2%3A123456789012%3Ainference-profile%2Fabc/invoke"),
            "got: {url}"
        );
    }

    #[test]
    fn url_encodes_brackets_in_1m_suffix() {
        // The bedrock-access-gateway exposes "global.anthropic.claude-opus-4-7[1m]"
        // for the 1M-context inference-profile variant. The brackets
        // would otherwise need URL-encoding in the path segment.
        let url = invoke_url("us-west-2", "global.anthropic.claude-opus-4-7[1m]", true);
        assert!(url.ends_with("claude-opus-4-7%5B1m%5D/invoke-with-response-stream"));
    }
}
