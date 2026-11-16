#[cfg(feature = "bedrock")]
use super::Cloud;
#[cfg(any(feature = "bedrock", feature = "gemini"))]
use super::{Auth, Format};
use super::{CacheCapability, Config, ProviderEntry, ReductionConfig};

#[test]
#[should_panic(expected = "with_anthropic_version")]
fn wrong_variant_setter_panics() {
    let _ = ProviderEntry::openai_compat("https://example.com/v1", "literal:test")
        .with_anthropic_version("2023-06-01");
}

#[test]
fn kind_str_returns_stable_config_tokens() {
    assert_eq!(
        ProviderEntry::openai_compat("https://example.com/v1", "literal:k").kind_str(),
        "openai-compat",
    );
    assert_eq!(
        ProviderEntry::anthropic_api("literal:k").kind_str(),
        "anthropic-api",
    );
    #[cfg(feature = "openai-responses")]
    assert_eq!(
        ProviderEntry::openai_responses("literal:k").kind_str(),
        "openai-responses",
    );
    #[cfg(feature = "gemini")]
    assert_eq!(ProviderEntry::gemini("literal:k").kind_str(), "gemini",);
    #[cfg(feature = "bedrock")]
    {
        let bedrock = bedrock_entry(Format::BedrockInvoke, None);
        assert_eq!(bedrock.kind_str(), "bedrock");
    }
}

/// The vocabulary and the family tokens must agree in both directions: every
/// constructible entry's `kind_str()` is a listed token, and every listed
/// token is the family token of some constructible entry. A format added
/// without a vocabulary entry (or vice versa) fails here rather than silently
/// narrowing a caller that gates on the list.
#[test]
fn config_provider_kinds_matches_the_kind_str_discriminants() {
    let mut constructible: Vec<&'static str> = vec![
        ProviderEntry::openai_compat("https://example.com/v1", "literal:k").kind_str(),
        ProviderEntry::anthropic_api("literal:k").kind_str(),
    ];
    #[cfg(feature = "openai-responses")]
    constructible.push(ProviderEntry::openai_responses("literal:k").kind_str());
    #[cfg(feature = "gemini")]
    constructible.push(ProviderEntry::gemini("literal:k").kind_str());
    #[cfg(feature = "bedrock")]
    constructible.push(bedrock_entry(Format::BedrockInvoke, None).kind_str());

    constructible.sort_unstable();
    let mut listed = super::CONFIG_PROVIDER_KINDS.to_vec();
    listed.sort_unstable();
    assert_eq!(constructible, listed);

    for kind in super::CONFIG_PROVIDER_KINDS {
        assert!(
            super::is_config_provider_kind(kind),
            "listed kind {kind:?} must pass the membership predicate",
        );
    }
    assert!(!super::is_config_provider_kind("not-a-real-kind"));
    assert!(!super::is_config_provider_kind("*"));
}

#[cfg(feature = "gemini")]
#[test]
fn gemini_constructor_defaults() {
    let entry = ProviderEntry::gemini("env://GEMINI_API_KEY");
    assert_eq!(entry.kind_str(), "gemini");
    assert_eq!(entry.api_key_ref(), Some("env://GEMINI_API_KEY"));
    assert_eq!(entry.format(), Format::Gemini);
    assert_eq!(entry.auth, Auth::XGoogApiKey);
    assert_eq!(
        entry.base_url.as_deref(),
        Some("https://generativelanguage.googleapis.com/v1beta"),
    );
    assert!(entry.cloud.is_none());
    assert!(entry.header_extras.is_empty());
    assert!(entry.payload_extras.is_none());
}

#[cfg(feature = "gemini")]
#[test]
fn gemini_format_parses_as_the_api_key_surface() {
    let toml_text = r#"
[providers.gemini]
format = "gemini"
base_url = "https://generativelanguage.googleapis.com/v1beta"
credential = "env://GEMINI_API_KEY"
auth = "x-goog-api-key"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse gemini entry");
    let entry = cfg.providers.get("gemini").expect("gemini provider");
    assert_eq!(entry.format(), Format::Gemini);
    assert_eq!(entry.auth, Auth::XGoogApiKey);
    assert_eq!(entry.kind_str(), "gemini");
}

#[cfg(feature = "gemini")]
#[test]
fn gemini_cloud_code_format_parses_into_the_gemini_family() {
    let toml_text = r#"
[providers.gemini]
format = "gemini-cloud-code"
base_url = "https://daily-cloudcode-pa.googleapis.com"
credential = "oauth://antigravity"
auth = "bearer"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse cloud-code entry");
    let entry = cfg.providers.get("gemini").expect("gemini provider");
    assert_eq!(entry.format(), Format::GeminiCloudCode);
    assert_eq!(entry.auth, Auth::Bearer);
    assert_eq!(entry.kind_str(), "gemini");
}

#[test]
fn redact_secrets_redacts_literal_only() {
    let mut entry = ProviderEntry::openai_compat("https://example.com/v1", "literal:sk-test");
    entry.redact_secrets();
    assert_eq!(entry.secret_uris(), vec!["literal:[REDACTED]"]);
}

/// `redact_secrets` reduces `base_url` to its origin: userinfo, path, and
/// query are all credential-carrying positions in practice, so none of them
/// may survive into a displayed config.
#[test]
fn redact_secrets_reduces_base_url_to_origin() {
    let raw = "https://user:sk-live-FAKE@upstream.example:8443/v1?token=sk-query-FAKE";

    let mut compat = ProviderEntry::openai_compat(raw, "literal:sk-test");
    compat.redact_secrets();
    assert_eq!(
        compat.base_url.as_deref(),
        Some("https://upstream.example:8443")
    );

    let mut anthropic = ProviderEntry::anthropic_api("literal:sk-ant-test").with_base_url(raw);
    anthropic.redact_secrets();
    assert_eq!(
        anthropic.base_url.as_deref(),
        Some("https://upstream.example:8443")
    );
}

/// A `gemini` entry parsed from TOML reduces too, not only the code-built
/// entries above.
#[cfg(feature = "gemini")]
#[test]
fn redact_secrets_reduces_base_url_on_the_gemini_arm() {
    let toml_text = r#"
[providers.g]
format = "gemini"
base_url = "https://user:sk-live-FAKE@gw.example/v1beta"
credential = "literal:super-secret"
auth = "x-goog-api-key"
"#;
    let mut cfg: Config = toml::from_str(toml_text).expect("parse");
    let entry = cfg.providers.get_mut("g").expect("gemini provider");
    entry.redact_secrets();
    assert_eq!(entry.base_url.as_deref(), Some("https://gw.example"));
}

/// A set `base_url` reduces; an unset one (a cloud entry, whose endpoint the
/// region derives) stays `None`. Turning `None` into a redaction sentinel
/// would misreport a config that set no URL at all.
#[cfg(feature = "openai-responses")]
#[test]
fn redact_secrets_reduces_a_set_base_url_and_preserves_an_unset_one() {
    let toml_text = r#"
[providers.set]
format = "openai-responses"
base_url = "https://user:sk-live-FAKE@gw.example/v1"
credential = "literal:sk-test"
auth = "bearer"

[providers.unset]
format = "openai-responses"
cloud = { name = "aws", region = "us-east-1" }
credential = "literal:sk-test"
auth = "bearer"
"#;
    let mut cfg: Config = toml::from_str(toml_text).expect("parse");
    for entry in cfg.providers.values_mut() {
        entry.redact_secrets();
    }
    let set = cfg.providers.get("set").expect("set provider");
    assert_eq!(set.base_url.as_deref(), Some("https://gw.example"));
    let unset = cfg.providers.get("unset").expect("unset provider");
    assert!(
        unset.base_url.is_none(),
        "an unset base_url must stay None, never a sentinel; got: {:?}",
        unset.base_url
    );
}

/// An EMPTY `base_url` stays empty: `config show` reports what the operator
/// wrote (the plan refuses the entry, and the operator must see why), and
/// empty stays distinguishable from the withhold sentinel below.
#[test]
fn redact_secrets_leaves_an_empty_base_url_empty() {
    let toml_text = r#"
[providers.empty-base]
format = "anthropic-messages"
base_url = ""
credential = "literal:sk-ant-test"
auth = "x-api-key"
"#;
    let mut cfg: Config = toml::from_str(toml_text).expect("parse");
    let entry = cfg.providers.get_mut("empty-base").expect("provider entry");
    entry.redact_secrets();
    assert_eq!(
        entry.base_url.as_deref(),
        Some(""),
        "an empty base_url must print as written, never as a sentinel"
    );
}

/// A `base_url` the origin projection refuses to reduce becomes the fixed
/// `[REDACTED]` sentinel, never an empty string, so it stays distinguishable
/// from the empty value `redact_secrets_leaves_an_empty_base_url_empty` pins.
#[test]
fn redact_secrets_withholds_an_unprojectable_base_url_as_a_sentinel() {
    // A second `@` demoted past the authority: the projection withholds this
    // whole rather than trusting the parsed host, which would be the secret.
    let mut entry = ProviderEntry::openai_compat(
        "https://x@sk-live-FAKE/y@real.example/v1",
        "literal:sk-test",
    );
    entry.redact_secrets();
    assert_eq!(entry.base_url.as_deref(), Some("[REDACTED]"));
}

/// `forward_client_headers` is unset when the field is omitted from the
/// TOML (secure-by-default: drop every captured `x-claude-code-*`
/// header). Explicit lists round-trip
/// through serialize/deserialize so the operator's allowlist is
/// preserved end-to-end.
#[test]
fn anthropic_api_forward_client_headers_round_trips() {
    // Default: omitted -> empty Vec.
    let toml_text = r#"
[providers.anthropic]
format = "anthropic-messages"
base_url = "https://api.anthropic.com"
credential = "literal:sk-ant-test"
auth = "x-api-key"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse default");
    let entry = cfg.providers.get("anthropic").expect("anthropic provider");
    assert!(
        entry.forward_client_headers.is_none(),
        "default must be unset; got: {:?}",
        entry.forward_client_headers
    );

    // Explicit list of two names.
    let toml_text = r#"
[providers.anthropic]
format = "anthropic-messages"
base_url = "https://api.anthropic.com"
credential = "literal:sk-ant-test"
auth = "x-api-key"
forward_client_headers = ["x-claude-code-session-id", "x-claude-code-agent-id"]
"#;
    let expected = vec![
        "x-claude-code-session-id".to_string(),
        "x-claude-code-agent-id".to_string(),
    ];
    let cfg: Config = toml::from_str(toml_text).expect("parse explicit");
    let entry = cfg.providers.get("anthropic").expect("anthropic provider");
    assert_eq!(
        entry.forward_client_headers.as_ref(),
        Some(&expected),
        "explicit list must parse"
    );

    // Round-trip: serialize, deserialize, compare.
    let cfg_in: Config = toml::from_str(toml_text).expect("parse in");
    let serialized = toml::to_string(&cfg_in).expect("serialize");
    let cfg_out: Config = toml::from_str(&serialized).expect("parse out");
    assert_eq!(
        cfg_out
            .providers
            .get("anthropic")
            .expect("anthropic")
            .forward_client_headers
            .as_ref(),
        Some(&expected),
        "round-trip must preserve list"
    );
}

/// `RetryPolicy::default()` ships with `jitter_ms = 50` so
/// multi-client deployments get retry spread out of the box without
/// any explicit operator configuration.
#[test]
fn retry_policy_default_jitter_is_50() {
    use super::RetryPolicy;
    assert_eq!(
        RetryPolicy::default().jitter_ms,
        50,
        "default jitter_ms must be 50 for out-of-the-box retry spread"
    );
}

/// A `[retry]` block that tunes one knob and omits `jitter_ms` must
/// still resolve jitter to 50, not `u64::default()` (0). The struct
/// `Default` only applies when the whole table is absent, so this
/// parse-boundary case is the one that actually exercises the field's
/// serde default.
#[test]
fn retry_block_present_without_jitter_keeps_50() {
    use super::RetryPolicy;
    let cfg: RetryPolicy = toml::from_str("max_attempts = 5\n").expect("parse [retry] block");
    assert_eq!(cfg.max_attempts, 5);
    assert_eq!(
        cfg.jitter_ms, 50,
        "jitter must stay 50 when [retry] is present but omits it"
    );
}

#[test]
fn cache_capability_per_kind_defaults_are_conservative() {
    let anthropic = CacheCapability::for_provider_kind("anthropic-api");
    assert!(anthropic.supports_top_level_cache_control);
    assert!(anthropic.cache_hit_observable);

    // Bedrock caches only off per-block markers, never a top-level one,
    // so auto-emit must fail-closed -- but hit usage is still reported.
    let bedrock = CacheCapability::for_provider_kind("bedrock");
    assert!(!bedrock.supports_top_level_cache_control);
    assert!(bedrock.cache_hit_observable);

    // OpenAI-shape: no explicit breakpoint, but cached_tokens reported.
    let responses = CacheCapability::for_provider_kind("openai-responses");
    assert!(!responses.supports_top_level_cache_control);
    assert!(responses.cache_hit_observable);

    // Gemini: implicit + explicit context caching; no top-level
    // breakpoint to emit, but cachedContentTokenCount is reported.
    let gemini = CacheCapability::for_provider_kind("gemini");
    assert!(!gemini.supports_top_level_cache_control);
    assert!(gemini.cache_hit_observable);

    let compat = CacheCapability::for_provider_kind("openai-compat");
    assert!(!compat.supports_top_level_cache_control);
    assert!(!compat.cache_hit_observable);

    // Unknown kind: never auto-emit.
    let unknown = CacheCapability::for_provider_kind("some-future-kind");
    assert!(!unknown.supports_top_level_cache_control);
    assert!(!unknown.cache_hit_observable);
}

#[cfg(feature = "gemini")]
#[test]
fn gemini_provider_entry_parses_and_exposes_secret_uri() {
    let toml_text = r#"
[providers.g]
format = "gemini"
base_url = "https://generativelanguage.googleapis.com/v1beta"
credential = "literal:test-key"
auth = "x-goog-api-key"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse minimal");
    let minimal = cfg.providers.get("g").expect("gemini provider");
    assert_eq!(minimal.api_key_ref(), Some("literal:test-key"));
    assert_eq!(
        minimal.base_url.as_deref(),
        Some("https://generativelanguage.googleapis.com/v1beta"),
    );

    // Explicit base_url + header_extras.
    let toml_text = r#"
[providers.g]
format = "gemini"
base_url = "https://example.test/v1beta"
credential = "env://GEMINI_API_KEY"
auth = "x-goog-api-key"

[providers.g.header_extras]
x-custom = "v"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse explicit");
    let entry = cfg.providers.get("g").expect("gemini provider");
    assert_eq!(
        entry.base_url.as_deref(),
        Some("https://example.test/v1beta")
    );
    assert_eq!(
        entry.header_extras.get("x-custom").map(String::as_str),
        Some("v")
    );

    // Family token + secret enumeration / redaction contract.
    assert_eq!(entry.kind_str(), "gemini");
    assert_eq!(entry.secret_uris(), vec!["env://GEMINI_API_KEY"]);
}

#[cfg(feature = "gemini")]
#[test]
fn gemini_redact_secrets_masks_literal_api_key() {
    let toml_text = r#"
[providers.g]
format = "gemini"
base_url = "https://generativelanguage.googleapis.com/v1beta"
credential = "literal:super-secret"
auth = "x-goog-api-key"
"#;
    let mut cfg: Config = toml::from_str(toml_text).expect("parse");
    let entry = cfg.providers.get_mut("g").expect("gemini provider");
    entry.redact_secrets();
    assert!(
        !entry.credential.contains("super-secret"),
        "literal key must be redacted; got: {}",
        entry.credential
    );
    assert_eq!(entry.credential, "literal:[REDACTED]");
}

#[test]
fn cache_capability_falls_back_to_per_kind_default_when_unset() {
    let anthropic = ProviderEntry::anthropic_api("literal:sk-ant-test");
    assert_eq!(
        anthropic.cache_capability(),
        CacheCapability::for_provider_kind("anthropic-api"),
    );

    let compat = ProviderEntry::openai_compat("https://example.com/v1", "literal:k");
    assert_eq!(
        compat.cache_capability(),
        CacheCapability::for_provider_kind("openai-compat"),
    );
}

#[cfg(feature = "bedrock")]
fn bedrock_entry(format: Format, cache_capability: Option<CacheCapability>) -> ProviderEntry {
    let mut entry = ProviderEntry::cloud_aws(format, "us-east-1", "aws://chain", Auth::AwsSigv4);
    entry.cache_capability = cache_capability;
    entry
}

/// The Bedrock Invoke egress lowers a top-level `cache_control`
/// marker to the per-block form Invoke caches on, so auto-emit is
/// safe there: `cache_capability()` derives supports_top_level = true
/// from `format = "bedrock-invoke"`.
#[cfg(feature = "bedrock")]
#[test]
fn cache_capability_bedrock_invoke_supports_top_level() {
    let cap = bedrock_entry(Format::BedrockInvoke, None).cache_capability();
    assert!(cap.supports_top_level_cache_control);
    assert!(cap.cache_hit_observable);
}

/// A top-level marker is inert on Bedrock Converse (no `cachePoint`
/// translation), so it stays fail-closed: supports_top_level = false,
/// hit usage still observable.
#[cfg(feature = "bedrock")]
#[test]
fn cache_capability_bedrock_converse_fails_closed() {
    let cap = bedrock_entry(Format::BedrockConverse, None).cache_capability();
    assert!(!cap.supports_top_level_cache_control);
    assert!(cap.cache_hit_observable);
}

/// An explicit operator override always wins over the format-derived
/// default, even when the format would otherwise enable auto-emit.
#[cfg(feature = "bedrock")]
#[test]
fn cache_capability_bedrock_override_beats_the_format_default() {
    let cap = bedrock_entry(
        Format::BedrockInvoke,
        Some(CacheCapability::new(false, false)),
    )
    .cache_capability();
    assert!(!cap.supports_top_level_cache_control);
    assert!(!cap.cache_hit_observable);
}

#[test]
fn cache_capability_operator_override_beats_per_kind_default() {
    // An anthropic-api entry whose upstream does NOT honor a
    // top-level breakpoint but DOES report cache hits.
    let toml_text = r#"
[providers.anthropic]
format = "anthropic-messages"
base_url = "https://api.anthropic.com"
credential = "literal:sk-ant-test"
auth = "x-api-key"
cache_capability = { supports_top_level_cache_control = false, cache_hit_observable = true }
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse override");
    let entry = cfg.providers.get("anthropic").expect("anthropic provider");
    let cap = entry.cache_capability();
    assert!(!cap.supports_top_level_cache_control);
    assert!(cap.cache_hit_observable);
    // The override beats the per-kind default (which is true/true).
    assert_ne!(cap, CacheCapability::for_provider_kind("anthropic-api"));

    // Round-trips through serialize/deserialize.
    let serialized = toml::to_string(&cfg).expect("serialize");
    let cfg_out: Config = toml::from_str(&serialized).expect("re-parse");
    let cap_out = cfg_out
        .providers
        .get("anthropic")
        .expect("anthropic")
        .cache_capability();
    assert_eq!(cap_out, cap);
}

#[test]
fn cache_capability_omitted_uses_default_and_deny_unknown_fields_holds() {
    let toml_text = r#"
[providers.openai]
format = "openai-chat"
base_url = "https://example.com/v1"
credential = "literal:k"
auth = "bearer"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse default");
    let entry = cfg.providers.get("openai").expect("openai provider");
    assert_eq!(
        entry.cache_capability(),
        CacheCapability::for_provider_kind("openai-compat"),
    );

    // An unknown sub-field inside cache_capability must be rejected.
    let bad = r#"
[providers.openai]
format = "openai-chat"
base_url = "https://example.com/v1"
credential = "literal:k"
auth = "bearer"
cache_capability = { supports_top_level_cache_control = true, bogus = 1 }
"#;
    assert!(
        toml::from_str::<Config>(bad).is_err(),
        "deny_unknown_fields must reject an unknown CacheCapability field",
    );
}

/// An `anthropic-api` entry on the DEFAULT Anthropic base, with no
/// operator override, gets the optimistic per-kind default (true/true)
/// -- the real Anthropic server honors a top-level breakpoint.
#[test]
fn cache_capability_anthropic_default_base_uses_optimistic_default() {
    let toml_text = r#"
[providers.anthropic]
format = "anthropic-messages"
base_url = "https://api.anthropic.com"
credential = "literal:sk-ant-test"
auth = "x-api-key"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse default base");
    let entry = cfg.providers.get("anthropic").expect("anthropic provider");
    let cap = entry.cache_capability();
    assert_eq!(cap, CacheCapability::for_provider_kind("anthropic-api"));
    assert!(cap.supports_top_level_cache_control);
    assert!(cap.cache_hit_observable);
}

/// An `anthropic-api` entry on a NON-default base_url (an Anthropic-
/// compatible third party), with no operator override, fails closed:
/// auto-emit must never break a host that may not honor cache_control.
/// `anthropic_api_base_url` answers only for a lane whose CONFIGURED base is
/// the one it will actually egress to. Its caller is the envelope-field mint
/// suppression, so a wrong answer here is a permanent verdict minted against
/// the wrong upstream.
#[test]
fn anthropic_api_base_url_reports_a_plain_entrys_configured_base() {
    // The positive control: a plain direct-to-Anthropic entry DOES answer, so
    // the refusals below are about their sub-lanes rather than about an accessor
    // that never answers.
    let toml_text = r#"
[providers.direct]
format = "anthropic-messages"
base_url = "https://api.anthropic.com"
credential = "literal:sk-test"
auth = "x-api-key"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse direct anthropic");
    let entry = cfg.providers.get("direct").expect("direct provider");
    assert_eq!(
        entry.anthropic_api_base_url(),
        Some("https://api.anthropic.com"),
        "a plain anthropic-api entry egresses to its configured base, so it is reportable",
    );
}

#[test]
fn anthropic_api_base_url_reports_a_plain_entrys_custom_base() {
    // A custom remote base is still the effective egress, so it is reportable
    // too -- a remote mirror's rejections ARE attributable to it. Included so
    // the cloud refusal cannot be mistaken for "anything non-default refuses".
    let toml_text = r#"
[providers.mirror]
format = "anthropic-messages"
base_url = "https://api.example.com/anthropic"
credential = "literal:sk-test"
auth = "x-api-key"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse custom base");
    let entry = cfg.providers.get("mirror").expect("mirror provider");
    assert_eq!(
        entry.anthropic_api_base_url(),
        Some("https://api.example.com/anthropic"),
        "a custom REMOTE base is the effective egress and stays reportable",
    );
}

#[cfg(feature = "bedrock")]
#[test]
fn anthropic_api_base_url_refuses_an_anthropic_entry_over_the_aws_cloud() {
    // An `anthropic-messages` entry over `cloud` egresses through the AWS
    // managed surface; reporting it would classify a Bedrock upstream as a
    // direct Anthropic one.
    let toml_text = r#"
[providers.mantle]
format = "anthropic-messages"
cloud = { name = "aws", region = "us-west-2" }
credential = "env://AWS_BEARER_TOKEN_BEDROCK"
auth = "bearer"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse cloud entry");
    let entry = cfg.providers.get("mantle").expect("cloud provider");
    // Premise: the entry is the same format as the reportable rows above and
    // does name an egress, so the refusal is about the carrier.
    assert_eq!(entry.format(), Format::AnthropicMessages);
    assert_eq!(
        entry.classifiable_egress_base_url().as_deref(),
        Some("https://bedrock-mantle.us-west-2.api.aws/anthropic"),
    );
    assert_eq!(
        entry.anthropic_api_base_url(),
        None,
        "a cloud entry egresses through Bedrock, so it must not be reported as a direct \
         Anthropic one",
    );
}

/// One `[providers.m]` entry of `format` over the AWS cloud in `region`,
/// parsed from TOML.
#[cfg(feature = "bedrock")]
fn mantle_entry(format: Format, region: &str) -> ProviderEntry {
    toml::from_str::<Config>(&format!(
        "[providers.m]\nformat = \"{}\"\n\
         cloud = {{ name = \"aws\", region = \"{region}\" }}\n\
         credential = \"env://AWS_BEARER_TOKEN_BEDROCK\"\nauth = \"bearer\"\n",
        format.as_str()
    ))
    .expect("parse cloud entry")
    .providers
    .remove("m")
    .expect("cloud provider")
}

/// Each format over the AWS cloud egresses to the base its region derives,
/// the one the factory dials, and a region that fails admission names none.
/// Compared against literals so a drifted derivation is caught here rather
/// than echoed back by it.
///
/// Mutation check: map `Format::AnthropicMessages` to the OpenAI-vocabulary
/// base in `cloud_egress_base` -> red on the anthropic us-west-2 row; replace
/// `base.ok()` with `Some(base.unwrap_or_else(|_| "https://bedrock-mantle.invalid".into()))`
/// -> red on every bad-region row.
#[cfg(feature = "bedrock")]
#[test]
fn classifiable_egress_base_url_answers_each_cloud_format() {
    #[cfg_attr(not(feature = "openai-responses"), allow(unused_mut))]
    let mut rows = vec![
        (
            Format::AnthropicMessages,
            "us-west-2",
            Some("https://bedrock-mantle.us-west-2.api.aws/anthropic"),
        ),
        (Format::AnthropicMessages, "not-a-region", None),
        (
            Format::OpenaiChat,
            "us-west-2",
            Some("https://bedrock-mantle.us-west-2.api.aws/openai/v1"),
        ),
        (Format::OpenaiChat, "not-a-region", None),
        (
            Format::BedrockInvoke,
            "us-west-2",
            Some("https://bedrock-runtime.us-west-2.amazonaws.com"),
        ),
        (Format::BedrockInvoke, "not-a-region", None),
        (
            Format::BedrockConverse,
            "us-west-2",
            Some("https://bedrock-runtime.us-west-2.amazonaws.com"),
        ),
    ];
    #[cfg(feature = "openai-responses")]
    rows.extend([
        (
            Format::OpenaiResponses,
            "us-west-2",
            Some("https://bedrock-mantle.us-west-2.api.aws/openai/v1"),
        ),
        (Format::OpenaiResponses, "not-a-region", None),
    ]);

    for (format, region, expected) in rows {
        let entry = mantle_entry(format, region);
        assert_eq!(
            entry.format(),
            format,
            "premise: {format:?} parsed as itself"
        );
        assert_eq!(
            entry.classifiable_egress_base_url().as_deref(),
            expected,
            "{format:?} over the AWS cloud in {region}",
        );
    }
}

#[test]
fn anthropic_api_base_url_refuses_every_other_kind() {
    let toml_text = r#"
[providers.compat]
format = "openai-chat"
base_url = "https://api.example.com/v1"
credential = "literal:sk-test"
auth = "bearer"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse openai-compat");
    assert_eq!(
        cfg.providers
            .get("compat")
            .expect("compat provider")
            .anthropic_api_base_url(),
        None,
        "a lane this stage has captured no rejection envelope for must fail closed",
    );
}

/// An `openai-responses` entry egresses to its configured `base_url`; the
/// code-built surfaces start on the Codex host or the OpenAI API, and an
/// entry that sets neither `base_url` nor `cloud` (which the plan refuses)
/// names none. The base rows compare against literals so a drifted builder
/// base is caught here, not echoed back by the function under test.
#[cfg(feature = "openai-responses")]
#[test]
fn classifiable_egress_base_url_answers_openai_responses_lanes() {
    use routectl_providers::openai_responses::AuthKind;

    let no_carrier = {
        let mut entry = ProviderEntry::openai_responses("env://OPENAI_KEY");
        entry.base_url = None;
        entry
    };
    let rows: Vec<(&str, ProviderEntry, Option<&str>)> = vec![
        (
            "the chatgpt-oauth builder egresses to the codex host",
            ProviderEntry::openai_responses("env://OPENAI_JWT"),
            Some("https://chatgpt.com/backend-api/codex"),
        ),
        (
            "the api-key builder egresses to the platform host",
            ProviderEntry::openai_responses("env://OPENAI_KEY")
                .with_openai_responses_auth_kind(AuthKind::ApiKey),
            Some("https://api.openai.com/v1"),
        ),
        (
            "a configured base_url is the egress",
            ProviderEntry::openai_responses("env://OPENAI_KEY")
                .with_openai_responses_auth_kind(AuthKind::ApiKey)
                .with_openai_responses_base_url("https://responses.example.com/v1"),
            Some("https://responses.example.com/v1"),
        ),
        (
            "an entry with neither base_url nor cloud answers None, no panic",
            no_carrier,
            None,
        ),
    ];
    #[cfg(feature = "bedrock")]
    let rows = {
        let mut rows = rows;
        rows.push((
            "a cloud entry answers its region's managed base",
            ProviderEntry::cloud_aws(
                Format::OpenaiResponses,
                "us-east-1",
                "env://AWS_BEARER_TOKEN_BEDROCK",
                Auth::Bearer,
            ),
            Some("https://bedrock-mantle.us-east-1.api.aws/openai/v1"),
        ));
        rows
    };

    for (name, entry, expected) in rows {
        assert_eq!(
            entry.classifiable_egress_base_url().as_deref(),
            expected,
            "{name}",
        );
    }
}

/// A `gemini` or `gemini-cloud-code` entry egresses to exactly the
/// `base_url` it writes, whichever host that is. Literals, so a rewritten
/// base is caught here.
#[cfg(feature = "gemini")]
#[test]
fn classifiable_egress_base_url_answers_gemini_lanes() {
    let rows: [(&str, &str, &str, &str); 4] = [
        (
            "api-key on the public host egresses there",
            "gemini",
            "x-goog-api-key",
            "https://generativelanguage.googleapis.com/v1beta",
        ),
        (
            "cloud-code on the daily host egresses there",
            "gemini-cloud-code",
            "bearer",
            "https://daily-cloudcode-pa.googleapis.com",
        ),
        (
            "cloud-code pinned to production egresses there",
            "gemini-cloud-code",
            "bearer",
            "https://cloudcode-pa.googleapis.com",
        ),
        (
            "api-key pinned to a mirror egresses there",
            "gemini",
            "x-goog-api-key",
            "https://gemini.example.com/v1beta",
        ),
    ];
    for (name, format, auth, expected) in rows {
        let cfg: Config = toml::from_str(&format!(
            "[providers.g]\nformat = \"{format}\"\nbase_url = \"{expected}\"\n\
             credential = \"oauth://antigravity\"\nauth = \"{auth}\"\n"
        ))
        .expect("parse gemini entry");
        assert_eq!(
            cfg.providers["g"].classifiable_egress_base_url().as_deref(),
            Some(expected),
            "{name}",
        );
    }
}

#[test]
fn cache_capability_anthropic_custom_base_fails_closed() {
    let toml_text = r#"
[providers.compat]
format = "anthropic-messages"
base_url = "https://api.example.com/anthropic"
credential = "literal:sk-test"
auth = "x-api-key"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse custom base");
    let entry = cfg.providers.get("compat").expect("compat provider");
    let cap = entry.cache_capability();
    assert!(
        !cap.supports_top_level_cache_control,
        "custom-base anthropic-api must fail closed on cache_control"
    );
    assert!(!cap.cache_hit_observable);
    assert_eq!(cap, CacheCapability::new(false, false));
    // It diverges from the optimistic per-kind default precisely
    // because the base_url is not the default Anthropic base.
    assert_ne!(cap, CacheCapability::for_provider_kind("anthropic-api"));
}

/// An explicit operator `cache_capability` override always wins, even
/// on a custom base_url: the operator knows their host supports it.
#[test]
fn cache_capability_anthropic_custom_base_override_wins() {
    let toml_text = r#"
[providers.compat]
format = "anthropic-messages"
base_url = "https://api.example.com/anthropic"
credential = "literal:sk-test"
auth = "x-api-key"
cache_capability = { supports_top_level_cache_control = true, cache_hit_observable = true }
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse custom base override");
    let entry = cfg.providers.get("compat").expect("compat provider");
    let cap = entry.cache_capability();
    assert!(
        cap.supports_top_level_cache_control,
        "explicit override must win over the fail-closed custom-base default"
    );
    assert!(cap.cache_hit_observable);
}

/// The kind-level default for per-block front-marker emission is `true`
/// only for an anthropic-api entry on the default Anthropic base URL.
#[test]
fn per_block_breakpoints_default_true_on_the_first_party_anthropic_host() {
    let toml_text = r#"
[providers.anthropic]
format = "anthropic-messages"
base_url = "https://api.anthropic.com"
credential = "literal:sk-ant-test"
auth = "x-api-key"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse anthropic default base");
    let entry = cfg.providers.get("anthropic").expect("anthropic provider");

    assert!(
        entry.per_block_breakpoints_enabled(),
        "anthropic-api on the default base URL must default to enabled"
    );
}

/// A custom-base anthropic-api entry is an Anthropic-COMPATIBLE third
/// party, so per-block emission stays off there.
#[test]
fn per_block_breakpoints_default_false_on_a_custom_anthropic_host() {
    let toml_text = r#"
[providers.compat]
format = "anthropic-messages"
base_url = "https://example.invalid/v1"
credential = "literal:k"
auth = "x-api-key"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse anthropic custom base");
    let entry = cfg.providers.get("compat").expect("compat provider");

    assert!(
        !entry.per_block_breakpoints_enabled(),
        "custom-base anthropic-api must default to disabled"
    );
}

/// Both Bedrock formats default to disabled.
#[cfg(feature = "bedrock")]
#[test]
fn per_block_breakpoints_default_false_for_both_bedrock_shapes() {
    for format in [Format::BedrockInvoke, Format::BedrockConverse] {
        let entry = bedrock_entry(format, None);
        assert!(
            !entry.per_block_breakpoints_enabled(),
            "{format:?} must default to disabled"
        );
    }
}

/// Every non-anthropic kind defaults to disabled.
#[test]
fn per_block_breakpoints_default_false_for_other_kinds() {
    let compat = ProviderEntry::openai_compat("https://example.com/v1", "literal:k");
    assert!(!compat.per_block_breakpoints_enabled());

    #[cfg(feature = "gemini")]
    assert!(!ProviderEntry::gemini("literal:k").per_block_breakpoints_enabled());

    #[cfg(feature = "openai-responses")]
    assert!(!ProviderEntry::openai_responses("literal:k").per_block_breakpoints_enabled());
}

/// An omitted `[reduction]` block deserializes to the default:
/// reduction enabled.
#[test]
fn reduction_omitted_block_defaults_enabled() {
    // Arrange: a config with no [reduction] table at all.
    let toml_text = r#"
[providers.openai]
format = "openai-chat"
base_url = "https://example.com/v1"
credential = "literal:k"
auth = "bearer"
"#;

    // Act
    let cfg: Config = toml::from_str(toml_text).expect("parse without reduction block");

    // Assert: omitted block == default == enabled.
    assert!(
        cfg.reduction.enabled,
        "omitted [reduction] must default to enabled"
    );
}

/// A present-but-empty `[reduction]` block leaves `enabled` at the field
/// default (true), and an explicit `enabled = false` still deserializes
/// false and survives a serialize/re-parse round trip.
#[test]
fn reduction_empty_block_enabled_and_explicit_false_round_trips() {
    // Arrange + Act: block present, key omitted.
    let empty_block = r"
[reduction]
";
    let cfg: Config = toml::from_str(empty_block).expect("parse empty reduction block");

    // Assert
    assert!(
        cfg.reduction.enabled,
        "empty [reduction] block must leave enabled at the field default (true)"
    );

    // Arrange + Act: explicit opt-out.
    let disabled = r"
[reduction]
enabled = false
";
    let cfg: Config = toml::from_str(disabled).expect("parse disabled reduction block");

    // Assert
    assert!(
        !cfg.reduction.enabled,
        "enabled = false must parse to false"
    );

    // Round-trip: serialize, re-parse, still false.
    let serialized = toml::to_string(&cfg).expect("serialize");
    let cfg_out: Config = toml::from_str(&serialized).expect("re-parse");
    assert!(
        !cfg_out.reduction.enabled,
        "explicit opt-out must survive a serialize/re-parse round trip"
    );
}

/// A `[reduction]` block with `enabled = true` parses, and an unknown
/// field inside it is rejected (deny_unknown_fields, mirroring
/// CacheConfig).
#[test]
fn reduction_block_parses_and_rejects_unknown_fields() {
    // Arrange + Act: explicit enable.
    let toml_text = r"
[reduction]
enabled = true
";
    let cfg: Config = toml::from_str(toml_text).expect("parse enabled reduction block");

    // Assert
    assert!(cfg.reduction.enabled, "enabled = true must parse to true");

    // Arrange: an unknown key inside [reduction].
    let bad = r"
[reduction]
enabled = true
bogus = 1
";

    // Act + Assert: deny_unknown_fields must reject it.
    assert!(
        toml::from_str::<Config>(bad).is_err(),
        "deny_unknown_fields must reject an unknown ReductionConfig field",
    );
}

/// `ReductionConfig::default()` yields enabled (reduction is on unless
/// opted out).
#[test]
fn reduction_config_default_is_enabled() {
    // Arrange + Act
    let cfg = ReductionConfig::default();

    // Assert
    assert!(cfg.enabled, "ReductionConfig::default() must be enabled");
}

/// The per-provider `reduction_enabled()` accessor returns `None` when
/// the override is unset, and the configured `Option<bool>` when a
/// TOML override is present (round-tripping through serialize).
#[test]
fn reduction_enabled_per_provider_accessor() {
    // Arrange: unset -> None.
    let unset = ProviderEntry::openai_compat("https://example.com/v1", "literal:k");

    // Act + Assert
    assert_eq!(
        unset.reduction_enabled(),
        None,
        "unset per-provider override must read as None"
    );

    // Arrange: an explicit per-provider override of false.
    let toml_text = r#"
[providers.openai]
format = "openai-chat"
base_url = "https://example.com/v1"
credential = "literal:k"
auth = "bearer"
reduction_enabled = false
"#;

    // Act
    let cfg: Config = toml::from_str(toml_text).expect("parse override");
    let entry = cfg.providers.get("openai").expect("openai provider");

    // Assert: Some(false) reads back through the accessor.
    assert_eq!(
        entry.reduction_enabled(),
        Some(false),
        "explicit reduction_enabled = false must read as Some(false)"
    );

    // Round-trip: serialize, re-parse, accessor still Some(false).
    let serialized = toml::to_string(&cfg).expect("serialize");
    let cfg_out: Config = toml::from_str(&serialized).expect("re-parse");
    assert_eq!(
        cfg_out
            .providers
            .get("openai")
            .expect("openai")
            .reduction_enabled(),
        Some(false),
        "per-provider override must round-trip through serde"
    );
}

/// The router recording path (`Router::record_would_trim`) trims on
/// `SteadyStateTrimParams::default()`, as the prompt-size path does. The
/// recording path is module-private, so this drives the public dispatch
/// entry point and compares the freed-token count it stamps onto
/// `DispatchMeta` with a direct run of the trimmer on the default params.
/// The request carries a span of tool payload above the default 100k-token
/// trigger, sized so any other trigger, head or tail count changes the
/// outcome or the freed-token count.
#[tokio::test]
async fn dispatch_trims_on_the_default_steady_state_params() {
    use crate::resolved::ResolvedModel;
    use crate::router::{Router, RouterOptions};
    use routectl_core::{
        ChatRequest, ChatResponse, Choice, ContentPart, KnownContentPart, Message, MessageContent,
        Result as CoreResult, Role,
    };
    use std::collections::BTreeMap;
    use std::sync::Arc;

    struct EchoProvider;

    #[async_trait::async_trait]
    impl routectl_core::Provider for EchoProvider {
        fn id(&self) -> &'static str {
            "echo"
        }

        fn normalize_request(&self, _req: &ChatRequest) -> CoreResult<serde_json::Value> {
            Ok(serde_json::json!({}))
        }

        fn normalize_response(&self, _raw: serde_json::Value) -> CoreResult<ChatResponse> {
            Err(routectl_core::Error::normalize_response("echo", "unused"))
        }

        async fn complete(&self, req: ChatRequest) -> CoreResult<ChatResponse> {
            Ok(ChatResponse {
                id: "ok".into(),
                model: req.model,
                choices: vec![Choice {
                    index: 0,
                    message: Message {
                        role: Role::Assistant,
                        content: MessageContent::Text("ok".into()),
                        reasoning: None,
                        reasoning_details: vec![],
                        name: None,
                        tool_call_id: None,
                        tool_calls: None,
                        refusal: None,
                        extras: serde_json::Map::new(),
                    },
                    finish_reason: Some("stop".into()),
                    matched_stop_sequence: None,
                    logprobs: None,
                }],
                usage: Some(routectl_core::Usage::default()),
                ..Default::default()
            })
        }

        async fn stream(
            &self,
            _req: ChatRequest,
        ) -> CoreResult<futures::stream::BoxStream<'static, CoreResult<routectl_core::ChatChunk>>>
        {
            use futures::stream::StreamExt;
            Ok(futures::stream::empty().boxed())
        }
    }

    fn text_msg(role: Role, text: &str) -> Message {
        Message {
            role,
            content: MessageContent::Text(text.into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
            extras: serde_json::Map::new(),
        }
    }

    fn tool_msg(role: Role, tokens: usize) -> Message {
        let payload = serde_json::json!("x".repeat(tokens * 4));
        let part = if matches!(role, Role::Assistant) {
            KnownContentPart::ToolUse {
                id: "toolu_1".into(),
                name: "Tool".into(),
                input: payload,
                cache_control: None,
                extras: serde_json::Map::new(),
            }
        } else {
            KnownContentPart::ToolResult {
                tool_use_id: "toolu_1".into(),
                content: payload,
                is_error: None,
                cache_control: None,
                extras: serde_json::Map::new(),
            }
        };
        Message {
            content: MessageContent::Parts(vec![ContentPart::Known(part)]),
            ..text_msg(role, "")
        }
    }

    // About 105k estimated tokens, just over the 100k trigger. The head's
    // second message carries tool payload a one-message head would elide;
    // exactly two 11k payloads sit between head and tail, so the 20k floor
    // takes both and a seven-message tail leaves too little to elide; the
    // first tail message carries the bulk that lifts the total over the
    // trigger and is never elided.
    fn steady_state_request() -> ChatRequest {
        ChatRequest {
            model: "m".into(),
            messages: vec![
                text_msg(Role::User, "head turn"),
                tool_msg(Role::Assistant, 3_000),
                tool_msg(Role::User, 11_000),
                tool_msg(Role::Assistant, 11_000),
                tool_msg(Role::User, 80_000),
                text_msg(Role::Assistant, "recent 1"),
                text_msg(Role::User, "recent 2"),
                text_msg(Role::Assistant, "recent 3"),
                text_msg(Role::User, "recent 4"),
                text_msg(Role::Assistant, "recent 5"),
            ]
            .into(),
            ..Default::default()
        }
    }

    let defaults = crate::context_trim::SteadyStateTrimParams::default();
    let expected =
        crate::context_trim::propose_steady_state_trim(&steady_state_request(), &defaults)
            .expect("the default params trim this request");
    assert_eq!(
        expected
            .marks
            .iter()
            .map(|m| m.message_index)
            .collect::<Vec<_>>(),
        vec![2, 3],
        "the fixture must elide exactly the two payloads between head and tail"
    );

    // Act: drive the router recording path via the public dispatch entry
    // point (`record_would_trim` is module-private).
    let mut router = Router::new(Arc::new(Config::default()));
    let provider: Arc<dyn routectl_core::Provider> = Arc::new(EchoProvider);
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m".to_string(),
        Arc::new(ResolvedModel::new("m", "p", provider, "upstream-m")),
    );
    router.install_resolved_models(models);
    let dispatched = router
        .complete_with_options(steady_state_request(), RouterOptions::new())
        .await;
    dispatched.result.expect("dispatch succeeds");

    // Assert
    assert_eq!(
        dispatched.meta.would_trim_tokens,
        Some(expected.candidate.d)
    );
}

/// Per-block WIRE SUPPORT is a property of the egress, independent of the
/// operator switch: only anthropic-api and Bedrock Converse can carry a
/// per-block marker to the wire.
#[test]
fn per_block_wire_support_is_limited_to_anthropic_and_converse() {
    assert!(
        ProviderEntry::anthropic_api("literal:k").supports_per_block_breakpoints(),
        "anthropic-api carries per-block cache_control natively",
    );
    // A CUSTOM-base anthropic-api entry is still Anthropic-SHAPED, so the
    // wire can carry the marker; whether to emit is the (fail-closed)
    // operator call, not a wire-support question.
    assert!(
        ProviderEntry::anthropic_api("literal:k")
            .with_base_url("https://example.invalid/v1")
            .supports_per_block_breakpoints(),
    );

    assert!(
        !ProviderEntry::openai_compat("https://example.invalid/v1", "literal:k")
            .supports_per_block_breakpoints(),
        "the openai-compat egress DROPS a per-block marker",
    );

    #[cfg(feature = "gemini")]
    assert!(!ProviderEntry::gemini("literal:k").supports_per_block_breakpoints());

    #[cfg(feature = "openai-responses")]
    assert!(!ProviderEntry::openai_responses("literal:k").supports_per_block_breakpoints());
}

/// Converse translates a per-block marker into a `cachePoint`; Invoke has
/// no front-marker path (it lowers the TOP-LEVEL marker itself).
#[cfg(feature = "bedrock")]
#[test]
fn per_block_wire_support_splits_the_two_bedrock_shapes() {
    assert!(bedrock_entry(Format::BedrockConverse, None).supports_per_block_breakpoints(),);
    assert!(!bedrock_entry(Format::BedrockInvoke, None).supports_per_block_breakpoints(),);
}

/// Every format that rides the AWS cloud with a bearer credential redacts
/// that credential, not only the one the CLI fixture exercises; the cloud
/// itself prints as written.
#[cfg(feature = "bedrock")]
#[test]
fn redact_secrets_redacts_a_cloud_bearer_credential_on_every_format() {
    let mut formats = vec![
        "openai-chat",
        "anthropic-messages",
        "bedrock-invoke",
        "bedrock-converse",
    ];
    if cfg!(feature = "openai-responses") {
        formats.push("openai-responses");
    }
    for format in formats {
        let toml_text = format!(
            "[providers.m]\nformat = \"{format}\"\n\
             cloud = {{ name = \"aws\", region = \"us-west-2\" }}\n\
             credential = \"literal:sk-mantle-FAKE\"\nauth = \"bearer\"\n"
        );
        let mut cfg: Config = toml::from_str(&toml_text).expect("parse cloud entry");
        let entry = cfg.providers.get_mut("m").expect("cloud provider");

        entry.redact_secrets();

        assert_eq!(
            entry.secret_uris(),
            vec!["literal:[REDACTED]"],
            "format `{format}`"
        );
        assert_eq!(
            entry.cloud,
            Some(Cloud::Aws {
                region: "us-west-2".into()
            }),
            "format `{format}`"
        );
    }
}

/// A ref that is not an `env://`, `file://` or `oauth://` pointer is withheld:
/// a bare value is most likely a key pasted into the ref field.
#[test]
fn redact_secrets_withholds_a_ref_off_the_pointer_schemes() {
    let rows = [
        ("bare value", "sk-bare-FAKE", "[REDACTED]"),
        ("unknown scheme", "vault:sk-scheme-FAKE", "[REDACTED]"),
        ("literal value", "literal:sk-lit-FAKE", "literal:[REDACTED]"),
        ("env pointer", "env://KEY", "env://KEY"),
        ("file pointer", "file:///k", "file:///k"),
        (
            "oauth pointer",
            "oauth://anthropic#a",
            "oauth://anthropic#a",
        ),
    ];
    for (name, key_ref, expected) in rows {
        let mut entry = ProviderEntry::openai_compat("https://example.com/v1", key_ref);

        entry.redact_secrets();

        assert_eq!(entry.secret_uris(), vec![expected], "row `{name}`");
    }
}
