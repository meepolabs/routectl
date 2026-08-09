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
//!   - at most ONE leading routing-prefix segment is stripped, so
//!     `us.anthropic.claude-haiku-4-5-20251001-v1:0` classifies the same
//!     as bare `anthropic.claude-haiku-4-5-20251001-v1:0`;
//!   - what follows must have `anthropic` as its first dot-separated
//!     segment, which admits suffixed forms such as
//!     `global.anthropic.claude-opus-4-7[1m]`;
//!   - an ARN is classified from its RESOURCE form, not refused
//!     wholesale. A `foundation-model/<model-id>` ARN embeds the plain
//!     model id, so the vendor IS provable and the embedded id is
//!     classified as above -- otherwise a non-Anthropic model reached
//!     the Anthropic-shaped lane through an ARN, which is the exact hole
//!     the gate exists to close. Resource forms that genuinely prove
//!     nothing (`inference-profile`, `application-inference-profile`,
//!     provisioned / custom / imported models) stay `Unknown`, as does
//!     any resource form not recognized here -- an unrecognized form
//!     must not become a false `No`, because that rejects a working
//!     config at startup.
//!
//! The routing-prefix set is MEASURED, not guessed: it is the set of
//! regional prefixes appearing on Anthropic Bedrock ids this binary
//! already recognizes elsewhere. Re-derive with
//! `grep -rhoE '"[a-z-]+\.anthropic\.' crates/routectl-router/catalog_data/*.json | sort -u`.
//! Widening it needs that kind of evidence -- an invented prefix is a
//! fabricated constraint, and a missing one rejects a working config.
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
    /// The id proves neither -- an opaque ARN resource form (an
    /// inference profile, or a provisioned / custom / imported model),
    /// which may front any vendor.
    Unknown,
}

/// Region-routing prefixes an upstream id may carry ahead of the vendor
/// segment. Exactly one leading occurrence is stripped before
/// classification -- these are routing scopes, not vendor tokens.
const ROUTING_PREFIXES: [&str; 7] = ["us.", "eu.", "apac.", "global.", "us-gov.", "au.", "jp."];

/// ARN resource types whose path embeds the plain model id, so the
/// vendor is provable from the ARN alone.
const VENDOR_BEARING_ARN_RESOURCES: [&str; 1] = ["foundation-model"];

/// Classify `model_id` per the rules documented on this module.
pub fn anthropic_family(model_id: &str) -> AnthropicFamily {
    if model_id.starts_with("arn:") {
        return arn_family(model_id);
    }
    bare_id_family(model_id)
}

/// Classify a non-ARN id: strip at most one routing prefix, then require
/// `anthropic` as the first dot-separated segment.
fn bare_id_family(model_id: &str) -> AnthropicFamily {
    let body = ROUTING_PREFIXES
        .iter()
        .find_map(|prefix| model_id.strip_prefix(prefix))
        .unwrap_or(model_id);
    if body.split('.').next() == Some("anthropic") {
        AnthropicFamily::Yes
    } else {
        AnthropicFamily::No
    }
}

/// Classify an ARN from its resource segment. A vendor-bearing resource
/// form defers to [`bare_id_family`] on the embedded id; anything else
/// -- including a malformed or truncated ARN -- is `Unknown`.
fn arn_family(arn: &str) -> AnthropicFamily {
    // `arn:<partition>:<service>:<region>:<account>:<type>/<id>`. Parse
    // FORWARD past exactly the five leading colon-delimited fields: the
    // resource id itself routinely contains colons (a trailing version
    // suffix such as `-v1:0`), so splitting at the last colon would cut
    // inside the model id rather than before the resource.
    let mut fields = arn.splitn(6, ':');
    let resource = match (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
    ) {
        (Some("arn"), Some(_), Some(_), Some(_), Some(_), Some(resource)) => resource,
        _ => return AnthropicFamily::Unknown,
    };
    let Some((resource_type, resource_id)) = resource.split_once('/') else {
        return AnthropicFamily::Unknown;
    };
    if VENDOR_BEARING_ARN_RESOURCES.contains(&resource_type) && !resource_id.is_empty() {
        bare_id_family(resource_id)
    } else {
        AnthropicFamily::Unknown
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
            // Australian and Japanese routing scopes: this binary
            // already recognizes ids carrying them, so classifying them
            // `No` would reject a working config at startup.
            "au.anthropic.claude-opus-4-7",
            "jp.anthropic.claude-opus-5",
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

    /// An inference profile is opaque BY RESOURCE FORM, so a vendor token
    /// appearing in its resource id proves nothing -- the profile may
    /// front any model. Contrast the foundation-model case below, where
    /// the resource path is the model id itself.
    #[test]
    fn does_not_treat_an_inference_profile_resource_id_as_provable() {
        assert_eq!(
            anthropic_family(
                "arn:aws:bedrock:us-east-1:123456789012:inference-profile/us.anthropic.claude-opus-4-7"
            ),
            AnthropicFamily::Unknown
        );
    }

    #[test]
    fn classifies_a_foundation_model_arn_from_its_embedded_model_id() {
        for id in [
            "arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-haiku-4-5",
            "arn:aws:bedrock:us-west-2::foundation-model/us.anthropic.claude-opus-4-7",
        ] {
            assert_eq!(anthropic_family(id), AnthropicFamily::Yes, "id: {id}");
        }
        // The hole this closes: a non-Anthropic model reaching the
        // Anthropic-shaped lane because its id happened to be an ARN.
        for id in [
            "arn:aws:bedrock:us-east-1::foundation-model/meta.llama3-70b-instruct-v1:0",
            "arn:aws:bedrock:us-east-1::foundation-model/mistral.mistral-large-2402-v1:0",
        ] {
            assert_eq!(anthropic_family(id), AnthropicFamily::No, "id: {id}");
        }
    }

    #[test]
    fn treats_malformed_or_unrecognized_arn_forms_as_unknown() {
        for id in [
            // No resource separator at all.
            "arn:aws:bedrock:us-east-1:123456789012",
            // Recognized type, empty resource id.
            "arn:aws:bedrock:us-east-1::foundation-model/",
            // Unrecognized resource type: must NOT become a false `No`,
            // which would reject a working config at startup.
            "arn:aws:bedrock:us-east-1:123456789012:provisioned-model/abcd1234",
            "arn:aws:bedrock:us-east-1:123456789012:custom-model/my-tune",
            "arn:aws:bedrock:us-east-1:123456789012:imported-model/abcd1234",
            // Truncated to the scheme.
            "arn:",
        ] {
            assert_eq!(anthropic_family(id), AnthropicFamily::Unknown, "id: {id}");
        }
    }
}
