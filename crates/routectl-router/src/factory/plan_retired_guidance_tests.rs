//! The retired-key messages operators follow to rewrite a provider entry:
//! for every old spelling whose replacement depends on the format, the
//! message states the mapping, and the entry rewritten as it says passes
//! the provider plan.

use super::plan_provider;
use crate::config::{Config, ProviderEntry, retired_key_message};

struct Row {
    /// The old spelling being rewritten.
    name: &'static str,
    /// The retired key whose message carries the guidance.
    retired_path: &'static str,
    /// The part of that message the rewrite follows.
    guidance: &'static str,
    /// The provider body the guidance produces.
    rewritten: &'static str,
}

const ROWS: &[Row] = &[
    Row {
        name: "anthropic api-key auth_kind",
        retired_path: "providers.p.auth_kind",
        guidance: "`api-key` is `x-api-key` on `anthropic-messages` over `base_url`",
        rewritten: "format = \"anthropic-messages\"\nbase_url = \"https://api.anthropic.com\"\n\
                    credential = \"env://ANTHROPIC_KEY\"\nauth = \"x-api-key\"\n",
    },
    Row {
        name: "anthropic oauth-bearer auth_kind",
        retired_path: "providers.p.auth_kind",
        guidance: "`oauth-bearer` and `chatgpt-oauth` are `bearer`",
        rewritten: "format = \"anthropic-messages\"\nbase_url = \"https://api.anthropic.com\"\n\
                    credential = \"oauth://anthropic\"\nauth = \"bearer\"\n",
    },
    Row {
        name: "responses api-key auth_kind",
        retired_path: "providers.p.auth_kind",
        guidance: "and `bearer` everywhere else",
        rewritten: "format = \"openai-responses\"\nbase_url = \"https://api.openai.com/v1\"\n\
                    credential = \"env://OPENAI_KEY\"\nauth = \"bearer\"\n",
    },
    Row {
        name: "responses chatgpt-oauth auth_kind",
        retired_path: "providers.p.auth_kind",
        guidance: "`oauth-bearer` and `chatgpt-oauth` are `bearer`",
        rewritten: "format = \"openai-responses\"\n\
                    base_url = \"https://chatgpt.com/backend-api/codex\"\n\
                    credential = \"oauth://codex\"\nauth = \"bearer\"\n",
    },
    Row {
        name: "responses bedrock-mantle auth_kind with a key",
        retired_path: "providers.p.auth_kind",
        guidance: "`bedrock-mantle` is `cloud = { name = \"aws\", region = \"<region>\" }` with \
                   `auth = \"bearer\"` for a key",
        rewritten: "format = \"openai-responses\"\n\
                    cloud = { name = \"aws\", region = \"us-east-1\" }\n\
                    credential = \"env://MANTLE_KEY\"\nauth = \"bearer\"\n",
    },
    Row {
        name: "responses bedrock-mantle auth_kind with a profile",
        retired_path: "providers.p.auth_kind",
        guidance: "`auth = \"aws-sigv4\"` for an `aws://` credential",
        rewritten: "format = \"openai-responses\"\n\
                    cloud = { name = \"aws\", region = \"us-east-1\" }\n\
                    credential = \"aws://profile/mantle\"\nauth = \"aws-sigv4\"\n",
    },
    Row {
        name: "gemini api-key auth_mode",
        retired_path: "providers.p.auth_mode",
        guidance: "`api-key` is `format = \"gemini\"` with `auth = \"x-goog-api-key\"`",
        rewritten: "format = \"gemini\"\n\
                    base_url = \"https://generativelanguage.googleapis.com/v1beta\"\n\
                    credential = \"env://GEMINI_KEY\"\nauth = \"x-goog-api-key\"\n",
    },
    Row {
        name: "gemini cloud-code auth_mode",
        retired_path: "providers.p.auth_mode",
        guidance: "`cloud-code` is `format = \"gemini-cloud-code\"` with `auth = \"bearer\"` and \
                   `credential = \"oauth://antigravity[#label]\"`",
        rewritten: "format = \"gemini-cloud-code\"\n\
                    base_url = \"https://daily-cloudcode-pa.googleapis.com\"\n\
                    credential = \"oauth://antigravity#seat-a\"\nauth = \"bearer\"\n",
    },
    Row {
        name: "bedrock invoke api_shape",
        retired_path: "providers.p.api_shape",
        guidance: "`invoke` (the default) is `format = \"bedrock-invoke\"`",
        rewritten: "format = \"bedrock-invoke\"\n\
                    cloud = { name = \"aws\", region = \"us-east-1\" }\n\
                    credential = \"aws://chain\"\nauth = \"aws-sigv4\"\n",
    },
    Row {
        name: "bedrock converse api_shape",
        retired_path: "providers.p.api_shape",
        guidance: "`converse` is `format = \"bedrock-converse\"`",
        rewritten: "format = \"bedrock-converse\"\n\
                    cloud = { name = \"aws\", region = \"us-east-1\" }\n\
                    credential = \"aws://chain\"\nauth = \"aws-sigv4\"\n",
    },
    Row {
        name: "bedrock_mantle with a bearer key",
        retired_path: "providers.p.bedrock_mantle",
        guidance: "a `bearer-key` `key_ref` becomes `credential` with `auth = \"bearer\"`",
        rewritten: "format = \"anthropic-messages\"\n\
                    cloud = { name = \"aws\", region = \"us-east-1\" }\n\
                    credential = \"env://MANTLE_KEY\"\nauth = \"bearer\"\n",
    },
    Row {
        name: "bedrock_mantle with a profile",
        retired_path: "providers.p.bedrock_mantle",
        guidance: "a profile is `credential = \"aws://profile/<name>\"`",
        rewritten: "format = \"openai-chat\"\n\
                    cloud = { name = \"aws\", region = \"us-east-1\" }\n\
                    credential = \"aws://profile/mantle\"\nauth = \"aws-sigv4\"\n",
    },
    Row {
        name: "forwarded credential_source",
        retired_path: "providers.p.credential_source",
        guidance: "`forwarded` is `credential = \"forwarded\"` with `auth = \"bearer\"` on \
                   `format = \"anthropic-messages\"` and `base_url = \
                   \"https://api.anthropic.com\"`",
        rewritten: "format = \"anthropic-messages\"\nbase_url = \"https://api.anthropic.com\"\n\
                    credential = \"forwarded\"\nauth = \"bearer\"\n",
    },
    Row {
        name: "bedrock bearer-key creds",
        retired_path: "providers.p.creds",
        guidance: "a `bearer-key` `key_ref` with `auth = \"bearer\"`",
        rewritten: "format = \"bedrock-converse\"\n\
                    cloud = { name = \"aws\", region = \"us-east-1\" }\n\
                    credential = \"env://BEDROCK_KEY\"\nauth = \"bearer\"\n",
    },
    Row {
        name: "bedrock profile creds",
        retired_path: "providers.p.creds",
        guidance: "`aws://profile/<name>` or `aws://chain` with `auth = \"aws-sigv4\"`",
        rewritten: "format = \"bedrock-invoke\"\n\
                    cloud = { name = \"aws\", region = \"us-east-1\" }\n\
                    credential = \"aws://profile/bedrock\"\nauth = \"aws-sigv4\"\n",
    },
    Row {
        name: "anthropic-api kind with a key",
        retired_path: "providers.p.kind",
        guidance: "`anthropic-api` is `anthropic-messages` with `x-api-key`",
        rewritten: "format = \"anthropic-messages\"\nbase_url = \"https://api.anthropic.com\"\n\
                    credential = \"env://ANTHROPIC_KEY\"\nauth = \"x-api-key\"\n",
    },
    Row {
        name: "anthropic-api kind over cloud",
        retired_path: "providers.p.kind",
        guidance: "(`bearer` for an `oauth://` login or over `cloud`)",
        rewritten: "format = \"anthropic-messages\"\n\
                    cloud = { name = \"aws\", region = \"us-east-1\" }\n\
                    credential = \"env://MANTLE_KEY\"\nauth = \"bearer\"\n",
    },
    Row {
        name: "openai-compat kind",
        retired_path: "providers.p.kind",
        guidance: "`openai-compat` is `openai-chat` with `bearer`",
        rewritten: "format = \"openai-chat\"\nbase_url = \"https://api.openai.com/v1\"\n\
                    credential = \"env://OPENAI_KEY\"\nauth = \"bearer\"\n",
    },
    Row {
        name: "openai-responses kind",
        retired_path: "providers.p.kind",
        guidance: "`openai-responses` stays and takes `bearer`",
        rewritten: "format = \"openai-responses\"\nbase_url = \"https://api.openai.com/v1\"\n\
                    credential = \"env://OPENAI_KEY\"\nauth = \"bearer\"\n",
    },
    Row {
        name: "gemini kind",
        retired_path: "providers.p.kind",
        guidance: "`gemini` is `gemini` with `x-goog-api-key`",
        rewritten: "format = \"gemini\"\n\
                    base_url = \"https://generativelanguage.googleapis.com/v1beta\"\n\
                    credential = \"env://GEMINI_KEY\"\nauth = \"x-goog-api-key\"\n",
    },
    Row {
        name: "gemini kind on the cloud-code surface",
        retired_path: "providers.p.kind",
        guidance: "`gemini-cloud-code` with `bearer`",
        rewritten: "format = \"gemini-cloud-code\"\n\
                    base_url = \"https://daily-cloudcode-pa.googleapis.com\"\n\
                    credential = \"oauth://antigravity\"\nauth = \"bearer\"\n",
    },
    Row {
        name: "bedrock kind",
        retired_path: "providers.p.kind",
        guidance: "`bedrock` is `bedrock-invoke` (`api_shape = \"converse\"`: \
                   `bedrock-converse`) over `cloud` with `aws-sigv4` for an `aws://` \
                   credential or `bearer` for a key",
        rewritten: "format = \"bedrock-invoke\"\n\
                    cloud = { name = \"aws\", region = \"us-east-1\" }\n\
                    credential = \"aws://chain\"\nauth = \"aws-sigv4\"\n",
    },
];

fn entry(body: &str) -> ProviderEntry {
    let text = format!("[providers.p]\n{body}");
    let config: Config =
        toml::from_str(&text).unwrap_or_else(|e| panic!("fixture parses: {e}\n{text}"));
    config.providers["p"].clone()
}

#[test]
fn each_format_dependent_replacement_names_an_entry_the_plan_accepts() {
    for row in ROWS {
        // Arrange
        let message = retired_key_message(row.retired_path)
            .unwrap_or_else(|| panic!("{}: `{}` is retired", row.name, row.retired_path));
        let rewritten = entry(row.rewritten);

        // Act
        let verdict = plan_provider("p", &rewritten);

        // Assert
        assert!(
            message.contains(row.guidance),
            "{}: the message no longer states this mapping: {message}",
            row.name
        );
        if let Err(e) = verdict {
            panic!("{}: the rewritten entry is refused: {e}", row.name);
        }
    }
}

/// Control: the plan refuses `x-api-key` on `openai-responses`, so a message
/// giving that mapping for every format would fail the accept rows above.
#[test]
fn a_responses_entry_rewritten_with_x_api_key_is_refused() {
    // Arrange
    let rewritten = entry(
        "format = \"openai-responses\"\nbase_url = \"https://api.openai.com/v1\"\n\
         credential = \"env://OPENAI_KEY\"\nauth = \"x-api-key\"\n",
    );

    // Act
    let verdict = plan_provider("p", &rewritten);

    // Assert
    let err = verdict
        .expect_err("x-api-key is not a Responses auth")
        .to_string();
    assert!(err.contains("use \"bearer\""), "{err}");
}
