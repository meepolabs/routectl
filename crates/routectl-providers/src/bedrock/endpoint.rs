//! Region -> bedrock-runtime endpoint resolution.
//!
//! Bedrock follows the standard AWS regional endpoint pattern:
//! `https://bedrock-runtime.<region>.<partition dns suffix>`
//! (`amazonaws.com`, or `amazonaws.com.cn` in the China partition).
//! Cross-region inference profiles (`us.`, `eu.`, `apac.`, `global.`)
//! are encoded in the *model id*, not the endpoint -- so the endpoint
//! always uses the regional hostname even for `global.` model ids.
//!
//! Every builder parses the region through the crate's canonical region
//! parser (the one behind [`crate::validate_aws_region`]) and returns
//! [`routectl_core::Error::Config`] for a non-canonical value, so no URL
//! is ever produced from a region that could alter the parsed host.

use routectl_core::Result;

/// Build the base URL for the Bedrock-runtime endpoint in `region`,
/// refusing a non-canonical region.
///
/// Example: `bedrock_runtime_url("us-west-2")` -> `"https://bedrock-runtime.us-west-2.amazonaws.com"`.
pub fn bedrock_runtime_url(region: &str) -> Result<String> {
    let region = crate::aws_region::require_aws_region(region)?;
    Ok(format!(
        "https://bedrock-runtime.{}.{}",
        region.as_str(),
        region.partition().dns_suffix()
    ))
}

/// Whether `endpoint` is exactly the Bedrock-runtime endpoint for `region`:
/// `https`, no userinfo or explicit port, and a host equal to the one
/// [`bedrock_runtime_url`] derives. A region that fails admission matches
/// nothing.
pub(crate) fn is_bedrock_runtime_endpoint(region: &str, endpoint: &reqwest::Url) -> bool {
    let Ok(expected) = bedrock_runtime_url(region) else {
        return false;
    };
    let Ok(expected) = reqwest::Url::parse(&expected) else {
        return false;
    };
    endpoint.scheme() == expected.scheme()
        && endpoint.username().is_empty()
        && endpoint.password().is_none()
        && endpoint.port().is_none()
        && endpoint.host_str().is_some()
        && endpoint.host_str() == expected.host_str()
}

/// Build the InvokeModel URL for `model_id` in `region`. `streaming`
/// switches between `/invoke` and `/invoke-with-response-stream`.
pub fn invoke_url(region: &str, model_id: &str, streaming: bool) -> Result<String> {
    let suffix = if streaming {
        "invoke-with-response-stream"
    } else {
        "invoke"
    };
    let encoded = urlencoded(model_id);
    Ok(format!(
        "{}/model/{encoded}/{suffix}",
        bedrock_runtime_url(region)?
    ))
}

/// Build the Converse URL for `model_id` in `region`.
pub fn converse_url(region: &str, model_id: &str, streaming: bool) -> Result<String> {
    let suffix = if streaming {
        "converse-stream"
    } else {
        "converse"
    };
    let encoded = urlencoded(model_id);
    Ok(format!(
        "{}/model/{encoded}/{suffix}",
        bedrock_runtime_url(region)?
    ))
}

/// Build the CountTokens URL for `model_id` in `region`.
///
/// Wire reference:
/// <https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_CountTokens.html>
pub fn count_tokens_url(region: &str, model_id: &str) -> Result<String> {
    let encoded = urlencoded(model_id);
    Ok(format!(
        "{}/model/{encoded}/count-tokens",
        bedrock_runtime_url(region)?
    ))
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
            invoke_url("us-west-2", "anthropic.claude-opus-4-7", true).unwrap(),
            "https://bedrock-runtime.us-west-2.amazonaws.com/model/anthropic.claude-opus-4-7/invoke-with-response-stream",
        );
        assert_eq!(
            invoke_url("us-west-2", "anthropic.claude-opus-4-7", false).unwrap(),
            "https://bedrock-runtime.us-west-2.amazonaws.com/model/anthropic.claude-opus-4-7/invoke",
        );
    }

    #[test]
    fn converse_url_handles_inference_profile_prefix() {
        assert_eq!(
            converse_url("us-west-2", "global.anthropic.claude-opus-4-7", false).unwrap(),
            "https://bedrock-runtime.us-west-2.amazonaws.com/model/global.anthropic.claude-opus-4-7/converse",
        );
    }

    #[test]
    fn url_encodes_arn_colons_and_slashes() {
        // Cross-account inference profile ARNs are a real Bedrock
        // shape; their colons/slashes MUST be percent-encoded so the
        // AWS endpoint sees one path segment rather than several.
        let arn = "arn:aws:bedrock:us-west-2:123456789012:inference-profile/abc";
        let url = invoke_url("us-west-2", arn, false).unwrap();
        assert!(
            url.contains("/model/arn%3Aaws%3Abedrock%3Aus-west-2%3A123456789012%3Ainference-profile%2Fabc/invoke"),
            "got: {url}"
        );
    }

    #[test]
    fn count_tokens_url_uses_the_documented_path() {
        assert_eq!(
            count_tokens_url("us-west-2", "anthropic.claude-opus-4-7").unwrap(),
            "https://bedrock-runtime.us-west-2.amazonaws.com/model/anthropic.claude-opus-4-7/count-tokens",
        );
    }

    #[test]
    fn count_tokens_url_encodes_arn_and_bracket_model_ids() {
        let arn = "arn:aws:bedrock:us-west-2:123456789012:inference-profile/abc";
        let url = count_tokens_url("us-west-2", arn).unwrap();
        assert!(
            url.ends_with(
                "/model/arn%3Aaws%3Abedrock%3Aus-west-2%3A123456789012%3Ainference-profile%2Fabc/count-tokens"
            ),
            "got: {url}"
        );
        assert!(
            count_tokens_url("us-west-2", "global.anthropic.claude-opus-4-7[1m]")
                .unwrap()
                .ends_with("claude-opus-4-7%5B1m%5D/count-tokens"),
        );
    }

    #[test]
    fn url_encodes_brackets_in_1m_suffix() {
        // The bedrock-access-gateway exposes "global.anthropic.claude-opus-4-7[1m]"
        // for the 1M-context inference-profile variant. The brackets
        // would otherwise need URL-encoding in the path segment.
        let url = invoke_url("us-west-2", "global.anthropic.claude-opus-4-7[1m]", true).unwrap();
        assert!(url.ends_with("claude-opus-4-7%5B1m%5D/invoke-with-response-stream"));
    }

    #[test]
    fn runtime_host_follows_the_region_partition_dns_suffix() {
        let cases = [
            ("us-west-2", "bedrock-runtime.us-west-2.amazonaws.com"),
            ("eu-central-1", "bedrock-runtime.eu-central-1.amazonaws.com"),
            (
                "us-gov-west-1",
                "bedrock-runtime.us-gov-west-1.amazonaws.com",
            ),
            ("cn-north-1", "bedrock-runtime.cn-north-1.amazonaws.com.cn"),
            (
                "cn-northwest-1",
                "bedrock-runtime.cn-northwest-1.amazonaws.com.cn",
            ),
        ];

        for (region, host) in cases {
            let base = bedrock_runtime_url(region).unwrap();
            let parsed = reqwest::Url::parse(&base).unwrap();

            assert_eq!(parsed.scheme(), "https", "{region}");
            assert_eq!(parsed.host_str(), Some(host), "{region}");
            assert_eq!(parsed.port(), None, "{region}");
            assert_eq!(base, format!("https://{host}"), "{region}");
        }
    }

    /// Region values that, interpolated unchecked, parse to a host other
    /// than `bedrock-runtime.<region>.<suffix>` or smuggle a path/query.
    const HOST_ALTERING_REGIONS: &[&str] = &[
        "x@127.0.0.1:9/",
        "us-west-2.evil.example/",
        "evil.example#",
        "evil.example?",
        "us-west-2.amazonaws.com.evil.example/x",
        "127.0.0.1:9/x",
        "us-west-2 ",
        "US-WEST-2",
        "",
    ];

    #[test]
    fn every_builder_refuses_a_host_altering_region() {
        for region in HOST_ALTERING_REGIONS {
            let results = [
                bedrock_runtime_url(region),
                invoke_url(region, "anthropic.claude-opus-4-7", false),
                invoke_url(region, "anthropic.claude-opus-4-7", true),
                converse_url(region, "anthropic.claude-opus-4-7", false),
                converse_url(region, "anthropic.claude-opus-4-7", true),
                count_tokens_url(region, "anthropic.claude-opus-4-7"),
            ];

            for result in results {
                match result {
                    Err(routectl_core::Error::Config(msg)) => {
                        assert!(msg.contains("region"), "{region:?}: {msg}");
                    }
                    other => panic!("{region:?} must be refused, got {other:?}"),
                }
            }
        }
    }
}
