//! Classifies an upstream model id as Anthropic-family, or not, or
//! not-provably-either.
//!
//! Two independent consumers need the same answer about a Bedrock model
//! id -- the config-validation gate on the InvokeModel lane (which
//! builds an Anthropic-shaped body) and the token-counting seat filter
//! (which needs an Anthropic tokenizer) -- and they want OPPOSITE
//! defaults when the id proves nothing. Hence one classifier with three
//! outcomes rather than a boolean: the gate accepts
//! [`AnthropicFamily::Unknown`], the seat filter refuses it.
//!
//! The classification is purely lexical, and deliberately narrower than
//! either a `starts_with("anthropic.")` (which rejects every
//! region-prefixed id the catalog actually ships) or a
//! `contains("anthropic")` (which decides nothing for an inference
//! profile ARN):
//!
//!   - at most ONE leading routing-prefix segment is stripped
//!     (`us.`, `eu.`, `apac.`, `global.`, `us-gov.`), so
//!     `us.anthropic.claude-haiku-4-5-20251001-v1:0` classifies the same
//!     as bare `anthropic.claude-haiku-4-5-20251001-v1:0`;
//!   - what follows must have `anthropic` as its first dot-separated
//!     segment, which admits suffixed forms such as
//!     `global.anthropic.claude-opus-4-7[1m]`;
//!   - an `arn:`-prefixed id is `Unknown`: an inference-profile ARN may
//!     carry no vendor token at all, so neither answer is provable from
//!     the string.
//!
//! This module holds no provider types and carries no feature gate, so
//! both consumers can reach it without a `#[cfg]` of their own.

/// Whether a model id names an Anthropic-family model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnthropicFamily {
    /// The id names an Anthropic model.
    Yes,
    /// The id names a non-Anthropic model.
    No,
    /// The id proves neither (an inference-profile ARN, which may carry
    /// no vendor token).
    Unknown,
}

/// Region-routing prefixes an upstream id may carry ahead of the vendor
/// segment. Exactly one leading occurrence is stripped before
/// classification -- these are routing scopes, not vendor tokens.
const ROUTING_PREFIXES: [&str; 5] = ["us.", "eu.", "apac.", "global.", "us-gov."];

/// Classify `model_id` per the rules documented on this module.
pub fn anthropic_family(model_id: &str) -> AnthropicFamily {
    let body = ROUTING_PREFIXES
        .iter()
        .find_map(|prefix| model_id.strip_prefix(prefix))
        .unwrap_or(model_id);
    if body.split('.').next() == Some("anthropic") {
        AnthropicFamily::Yes
    } else if model_id.starts_with("arn:") {
        AnthropicFamily::Unknown
    } else {
        AnthropicFamily::No
    }
}

#[cfg(test)]
mod tests {
    use super::{AnthropicFamily, anthropic_family};

    #[test]
    fn classifies_bare_and_region_prefixed_anthropic_ids_as_yes() {
        for id in [
            "anthropic.claude-haiku-4-5-20251001-v1:0",
            "us.anthropic.claude-haiku-4-5-20251001-v1:0",
            "eu.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "apac.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "global.anthropic.claude-opus-4-7",
            "us-gov.anthropic.claude-haiku-4-5-20251001-v1:0",
        ] {
            assert_eq!(anthropic_family(id), AnthropicFamily::Yes, "id: {id}");
        }
    }

    #[test]
    fn classifies_bracket_suffixed_anthropic_id_as_yes() {
        assert_eq!(
            anthropic_family("global.anthropic.claude-opus-4-7[1m]"),
            AnthropicFamily::Yes
        );
    }

    #[test]
    fn classifies_non_anthropic_vendor_ids_as_no() {
        for id in [
            "meta.llama3-70b-instruct-v1:0",
            "us.meta.llama4-scout-17b-instruct-v1:0",
            "mistral.mistral-large-2402-v1:0",
            "amazon.nova-pro-v1:0",
            "anthropicx.claude-fake-v1:0",
            "",
        ] {
            assert_eq!(anthropic_family(id), AnthropicFamily::No, "id: {id}");
        }
    }

    #[test]
    fn classifies_inference_profile_arn_as_unknown() {
        for id in [
            "arn:aws:bedrock:us-east-1:123456789012:inference-profile/my-profile",
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/abcd1234",
        ] {
            assert_eq!(anthropic_family(id), AnthropicFamily::Unknown, "id: {id}");
        }
    }

    #[test]
    fn strips_only_one_leading_routing_prefix() {
        assert_eq!(
            anthropic_family("us.eu.anthropic.claude-opus-4-7"),
            AnthropicFamily::No
        );
    }

    #[test]
    fn does_not_treat_an_arn_path_vendor_token_as_provable() {
        assert_eq!(
            anthropic_family(
                "arn:aws:bedrock:us-east-1:123456789012:inference-profile/us.anthropic.claude-opus-4-7"
            ),
            AnthropicFamily::Unknown
        );
    }
}
