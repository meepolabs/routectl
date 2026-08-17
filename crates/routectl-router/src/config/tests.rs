use super::{CacheCapability, Config, ProviderEntry, ReductionConfig};
#[cfg(feature = "gemini")]
use routectl_providers::gemini::GeminiAuthMode;

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
        let bedrock = ProviderEntry::Bedrock {
            region: "us-east-1".into(),
            api_shape: super::BedrockApiShapeConfig::default(),
            creds: super::BedrockCredsConfig::DefaultChain,
            user_agent: None,
            header_extras: std::collections::BTreeMap::new(),
            payload_extras: None,
            anthropic_beta: Vec::new(),
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            auto_emit_per_block_breakpoints: None,
            reduction_enabled: None,
            runtime: Default::default(),
        };
        assert_eq!(bedrock.kind_str(), "bedrock");
    }
}

#[cfg(feature = "gemini")]
#[test]
fn gemini_constructor_defaults() {
    let entry = ProviderEntry::gemini("env://GEMINI_API_KEY");
    assert_eq!(entry.kind_str(), "gemini");
    assert_eq!(entry.api_key_ref(), Some("env://GEMINI_API_KEY"));
    match entry {
        ProviderEntry::Gemini {
            base_url,
            header_extras,
            payload_extras,
            ..
        } => {
            assert_eq!(base_url, "https://generativelanguage.googleapis.com/v1beta",);
            assert!(header_extras.is_empty());
            assert!(payload_extras.is_none());
        }
        other => panic!("expected Gemini entry; got {other:?}"),
    }
}

#[cfg(feature = "gemini")]
#[test]
fn gemini_auth_mode_defaults_to_api_key_when_omitted() {
    let toml_text = r#"
[providers.gemini]
kind = "gemini"
api_key_ref = "env://GEMINI_API_KEY"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse omitted auth_mode");
    let entry = cfg.providers.get("gemini").expect("gemini provider");
    match entry {
        ProviderEntry::Gemini { auth_mode, .. } => {
            assert_eq!(*auth_mode, GeminiAuthMode::ApiKey);
        }
        other => panic!("expected Gemini entry; got {other:?}"),
    }
}

#[cfg(feature = "gemini")]
#[test]
fn gemini_auth_mode_parses_cloud_code() {
    let toml_text = r#"
[providers.gemini]
kind = "gemini"
api_key_ref = "oauth://antigravity"
auth_mode = "cloud-code"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse cloud-code auth_mode");
    let entry = cfg.providers.get("gemini").expect("gemini provider");
    match entry {
        ProviderEntry::Gemini { auth_mode, .. } => {
            assert_eq!(*auth_mode, GeminiAuthMode::CloudCode);
        }
        other => panic!("expected Gemini entry; got {other:?}"),
    }
}

#[test]
fn redact_secrets_redacts_literal_only() {
    let mut entry = ProviderEntry::openai_compat("https://example.com/v1", "literal:sk-test");
    entry.redact_secrets();
    assert_eq!(entry.secret_uris(), vec!["literal:[REDACTED]"]);
}

/// `redact_secrets` reduces `base_url` to its origin on the `String`-valued
/// variants: userinfo, path, and query are all credential-carrying positions in
/// practice, so none of them may survive into a displayed config.
#[test]
fn redact_secrets_reduces_base_url_to_origin_on_string_variants() {
    let raw = "https://user:sk-live-FAKE@upstream.example:8443/v1?token=sk-query-FAKE";

    let mut compat = ProviderEntry::openai_compat(raw, "literal:sk-test");
    compat.redact_secrets();
    match &compat {
        ProviderEntry::OpenaiCompat { base_url, .. } => {
            assert_eq!(base_url, "https://upstream.example:8443");
        }
        other => panic!("expected OpenaiCompat; got {other:?}"),
    }

    let mut anthropic = ProviderEntry::anthropic_api("literal:sk-ant-test").with_base_url(raw);
    anthropic.redact_secrets();
    match &anthropic {
        ProviderEntry::AnthropicApi { base_url, .. } => {
            assert_eq!(base_url, "https://upstream.example:8443");
        }
        other => panic!("expected AnthropicApi; got {other:?}"),
    }
}

/// The Gemini arm reduces too -- every `base_url`-bearing variant is covered,
/// not just the two api-backed ones.
#[cfg(feature = "gemini")]
#[test]
fn redact_secrets_reduces_base_url_on_the_gemini_arm() {
    let toml_text = r#"
[providers.g]
kind = "gemini"
api_key_ref = "literal:super-secret"
base_url = "https://user:sk-live-FAKE@gw.example/v1beta"
"#;
    let mut cfg: Config = toml::from_str(toml_text).expect("parse");
    let entry = cfg.providers.get_mut("g").expect("gemini provider");
    entry.redact_secrets();
    match entry {
        ProviderEntry::Gemini { base_url, .. } => {
            assert_eq!(base_url, "https://gw.example");
        }
        other => panic!("expected Gemini entry; got {other:?}"),
    }
}

/// `OpenaiResponses` carries `Option<String>`: `Some` reduces, `None` stays
/// `None`. `None` means "the factory picks the default endpoint" -- turning it
/// into a redaction sentinel would misreport a config that set nothing at all.
#[cfg(feature = "openai-responses")]
#[test]
fn redact_secrets_reduces_some_base_url_and_preserves_none_on_openai_responses() {
    let toml_text = r#"
[providers.set]
kind = "openai-responses"
api_key_ref = "literal:sk-test"
base_url = "https://user:sk-live-FAKE@gw.example/v1"

[providers.unset]
kind = "openai-responses"
api_key_ref = "literal:sk-test"
"#;
    let mut cfg: Config = toml::from_str(toml_text).expect("parse");
    for entry in cfg.providers.values_mut() {
        entry.redact_secrets();
    }
    match cfg.providers.get("set").expect("set provider") {
        ProviderEntry::OpenaiResponses { base_url, .. } => {
            assert_eq!(base_url.as_deref(), Some("https://gw.example"));
        }
        other => panic!("expected OpenaiResponses; got {other:?}"),
    }
    match cfg.providers.get("unset").expect("unset provider") {
        ProviderEntry::OpenaiResponses { base_url, .. } => {
            assert!(
                base_url.is_none(),
                "an unset base_url must stay None, never a sentinel; got: {base_url:?}"
            );
        }
        other => panic!("expected OpenaiResponses; got {other:?}"),
    }
}

/// An EMPTY `base_url` must stay empty. This is a correctness constraint, not a
/// formatting preference: the bedrock-mantle lane REQUIRES an empty `base_url`
/// (the factory asserts on it, because `region` is the single source of truth
/// for the mantle endpoint), so rewriting `""` to a sentinel would render a
/// valid mantle config as broken in `config show`.
#[test]
fn redact_secrets_leaves_an_empty_base_url_empty() {
    let toml_text = r#"
[providers.mantle-shaped]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
base_url = ""
"#;
    let mut cfg: Config = toml::from_str(toml_text).expect("parse");
    let entry = cfg
        .providers
        .get_mut("mantle-shaped")
        .expect("provider entry");
    entry.redact_secrets();
    match entry {
        ProviderEntry::AnthropicApi { base_url, .. } => {
            assert_eq!(
                base_url, "",
                "the mantle lane requires an empty base_url; a sentinel here would \
                 misreport a valid config as broken"
            );
        }
        other => panic!("expected AnthropicApi entry; got {other:?}"),
    }
}

/// A `base_url` the origin projection refuses to reduce becomes the fixed
/// `[REDACTED]` sentinel, never an empty string -- empty means the mantle lane
/// per `redact_secrets_leaves_an_empty_base_url_empty`, so the two outcomes must
/// stay distinguishable.
#[test]
fn redact_secrets_withholds_an_unprojectable_base_url_as_a_sentinel() {
    // A second `@` demoted past the authority: the projection withholds this
    // whole rather than trusting the parsed host, which would be the secret.
    let mut entry = ProviderEntry::openai_compat(
        "https://x@sk-live-FAKE/y@real.example/v1",
        "literal:sk-test",
    );
    entry.redact_secrets();
    match &entry {
        ProviderEntry::OpenaiCompat { base_url, .. } => {
            assert_eq!(base_url, "[REDACTED]");
            assert!(
                !base_url.is_empty(),
                "the withhold sentinel must never be empty: empty is a meaningful \
                 mantle-lane value"
            );
        }
        other => panic!("expected OpenaiCompat; got {other:?}"),
    }
}

/// `forward_client_headers` defaults to an empty list when the
/// field is omitted from the TOML (secure-by-default: drop every
/// captured `x-claude-code-*` header). Explicit lists round-trip
/// through serialize/deserialize so the operator's allowlist is
/// preserved end-to-end.
#[test]
fn anthropic_api_forward_client_headers_round_trips() {
    // Default: omitted -> empty Vec.
    let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse default");
    let entry = cfg.providers.get("anthropic").expect("anthropic provider");
    match entry {
        ProviderEntry::AnthropicApi {
            forward_client_headers,
            ..
        } => assert!(
            forward_client_headers.is_empty(),
            "default must be empty; got: {forward_client_headers:?}"
        ),
        other => panic!("expected AnthropicApi entry; got {other:?}"),
    }

    // Explicit list of two names.
    let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
forward_client_headers = ["x-claude-code-session-id", "x-claude-code-agent-id"]
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse explicit");
    let entry = cfg.providers.get("anthropic").expect("anthropic provider");
    match entry {
        ProviderEntry::AnthropicApi {
            forward_client_headers,
            ..
        } => assert_eq!(
            forward_client_headers,
            &vec![
                "x-claude-code-session-id".to_string(),
                "x-claude-code-agent-id".to_string(),
            ],
            "explicit list must round-trip"
        ),
        other => panic!("expected AnthropicApi entry; got {other:?}"),
    }

    // Round-trip: serialize, deserialize, compare.
    let cfg_in: Config = toml::from_str(toml_text).expect("parse in");
    let serialized = toml::to_string(&cfg_in).expect("serialize");
    let cfg_out: Config = toml::from_str(&serialized).expect("parse out");
    match cfg_out.providers.get("anthropic").expect("anthropic") {
        ProviderEntry::AnthropicApi {
            forward_client_headers,
            ..
        } => assert_eq!(
            forward_client_headers,
            &vec![
                "x-claude-code-session-id".to_string(),
                "x-claude-code-agent-id".to_string(),
            ],
            "round-trip must preserve list"
        ),
        other => panic!("expected AnthropicApi entry; got {other:?}"),
    }
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
    // Minimal: only api_key_ref -> base_url defaults to v1beta.
    let toml_text = r#"
[providers.g]
kind = "gemini"
api_key_ref = "literal:test-key"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse minimal");
    match cfg.providers.get("g").expect("gemini provider") {
        ProviderEntry::Gemini {
            api_key_ref,
            base_url,
            ..
        } => {
            assert_eq!(api_key_ref, "literal:test-key");
            assert_eq!(
                base_url, "https://generativelanguage.googleapis.com/v1beta",
                "base_url must default to the v1beta endpoint"
            );
        }
        other => panic!("expected Gemini entry; got {other:?}"),
    }

    // Explicit base_url + header_extras.
    let toml_text = r#"
[providers.g]
kind = "gemini"
api_key_ref = "env://GEMINI_API_KEY"
base_url = "https://example.test/v1beta"

[providers.g.header_extras]
x-custom = "v"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse explicit");
    let entry = cfg.providers.get("g").expect("gemini provider");
    match entry {
        ProviderEntry::Gemini {
            base_url,
            header_extras,
            ..
        } => {
            assert_eq!(base_url, "https://example.test/v1beta");
            assert_eq!(header_extras.get("x-custom").map(String::as_str), Some("v"));
        }
        other => panic!("expected Gemini entry; got {other:?}"),
    }

    // kind discriminator + secret enumeration / redaction contract.
    assert_eq!(entry.kind_str(), "gemini");
    assert_eq!(entry.secret_uris(), vec!["env://GEMINI_API_KEY"]);
}

#[cfg(feature = "gemini")]
#[test]
fn gemini_redact_secrets_masks_literal_api_key() {
    let toml_text = r#"
[providers.g]
kind = "gemini"
api_key_ref = "literal:super-secret"
"#;
    let mut cfg: Config = toml::from_str(toml_text).expect("parse");
    let entry = cfg.providers.get_mut("g").expect("gemini provider");
    entry.redact_secrets();
    match entry {
        ProviderEntry::Gemini { api_key_ref, .. } => assert!(
            !api_key_ref.contains("super-secret"),
            "literal key must be redacted; got: {api_key_ref}"
        ),
        other => panic!("expected Gemini entry; got {other:?}"),
    }
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
fn bedrock_entry(
    api_shape: super::BedrockApiShapeConfig,
    cache_capability: Option<CacheCapability>,
) -> ProviderEntry {
    ProviderEntry::Bedrock {
        region: "us-east-1".into(),
        api_shape,
        creds: super::BedrockCredsConfig::DefaultChain,
        user_agent: None,
        header_extras: std::collections::BTreeMap::new(),
        payload_extras: None,
        anthropic_beta: Vec::new(),
        cache_capability,
        auto_emit_top_level_breakpoint: None,
        auto_emit_per_block_breakpoints: None,
        reduction_enabled: None,
        runtime: Default::default(),
    }
}

/// The Bedrock Invoke egress lowers a top-level `cache_control`
/// marker to the per-block form Invoke caches on, so auto-emit is
/// safe there: `cache_capability()` derives supports_top_level = true
/// from `api_shape = Invoke`.
#[cfg(feature = "bedrock")]
#[test]
fn cache_capability_bedrock_invoke_supports_top_level() {
    let cap = bedrock_entry(super::BedrockApiShapeConfig::Invoke, None).cache_capability();
    assert!(cap.supports_top_level_cache_control);
    assert!(cap.cache_hit_observable);
}

/// A top-level marker is inert on Bedrock Converse (no `cachePoint`
/// translation), so it stays fail-closed: supports_top_level = false,
/// hit usage still observable.
#[cfg(feature = "bedrock")]
#[test]
fn cache_capability_bedrock_converse_fails_closed() {
    let cap = bedrock_entry(super::BedrockApiShapeConfig::Converse, None).cache_capability();
    assert!(!cap.supports_top_level_cache_control);
    assert!(cap.cache_hit_observable);
}

/// An explicit operator override always wins over the api_shape-
/// derived default, even when the shape would otherwise enable
/// auto-emit.
#[cfg(feature = "bedrock")]
#[test]
fn cache_capability_bedrock_override_beats_api_shape() {
    let cap = bedrock_entry(
        super::BedrockApiShapeConfig::Invoke,
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
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
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
kind = "openai-compat"
base_url = "https://example.com/v1"
api_key_ref = "literal:k"
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
kind = "openai-compat"
base_url = "https://example.com/v1"
api_key_ref = "literal:k"
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
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
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
#[test]
fn cache_capability_anthropic_custom_base_fails_closed() {
    let toml_text = r#"
[providers.compat]
kind = "anthropic-api"
api_key_ref = "literal:sk-test"
base_url = "https://api.example.com/anthropic"
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
kind = "anthropic-api"
api_key_ref = "literal:sk-test"
base_url = "https://api.example.com/anthropic"
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
fn per_block_breakpoints_default_true_for_anthropic_default_base() {
    let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse anthropic default base");
    let entry = cfg.providers.get("anthropic").expect("anthropic provider");

    assert_eq!(
        entry.auto_emit_per_block_breakpoints(),
        None,
        "an omitted key must read as None through the accessor"
    );
    assert!(
        entry.per_block_breakpoints_enabled(),
        "anthropic-api on the default base URL must default to enabled"
    );
}

/// A custom-base anthropic-api entry is an Anthropic-COMPATIBLE third
/// party, so per-block emission stays opt-in there.
#[test]
fn per_block_breakpoints_default_false_for_anthropic_custom_base() {
    let toml_text = r#"
[providers.compat]
kind = "anthropic-api"
api_key_ref = "literal:k"
base_url = "https://example.invalid/v1"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse anthropic custom base");
    let entry = cfg.providers.get("compat").expect("compat provider");

    assert!(
        !entry.per_block_breakpoints_enabled(),
        "custom-base anthropic-api must default to disabled"
    );
}

/// Both Bedrock shapes default to disabled: Converse per-block emission
/// stays opt-in, and the knob is inert on Invoke.
#[cfg(feature = "bedrock")]
#[test]
fn per_block_breakpoints_default_false_for_both_bedrock_shapes() {
    for shape in [
        super::BedrockApiShapeConfig::Invoke,
        super::BedrockApiShapeConfig::Converse,
    ] {
        let entry = bedrock_entry(shape, None);
        assert_eq!(entry.auto_emit_per_block_breakpoints(), None);
        assert!(
            !entry.per_block_breakpoints_enabled(),
            "bedrock {shape:?} must default to disabled"
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

/// An explicit per-provider override beats the kind-level default in both
/// directions and round-trips through serde.
#[test]
fn per_block_breakpoints_override_beats_kind_default() {
    let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
auto_emit_per_block_breakpoints = false

[providers.openai]
kind = "openai-compat"
base_url = "https://example.com/v1"
api_key_ref = "literal:k"
auto_emit_per_block_breakpoints = true
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse overrides");

    let anthropic = cfg.providers.get("anthropic").expect("anthropic");
    assert_eq!(anthropic.auto_emit_per_block_breakpoints(), Some(false));
    assert!(!anthropic.per_block_breakpoints_enabled());

    let openai = cfg.providers.get("openai").expect("openai");
    assert_eq!(openai.auto_emit_per_block_breakpoints(), Some(true));
    assert!(openai.per_block_breakpoints_enabled());

    let serialized = toml::to_string(&cfg).expect("serialize");
    let cfg_out: Config = toml::from_str(&serialized).expect("re-parse");
    assert_eq!(
        cfg_out
            .providers
            .get("anthropic")
            .expect("anthropic")
            .auto_emit_per_block_breakpoints(),
        Some(false),
        "per-provider override must round-trip through serde"
    );
}

/// An omitted `[reduction]` block deserializes to the default:
/// reduction enabled.
#[test]
fn reduction_omitted_block_defaults_enabled() {
    // Arrange: a config with no [reduction] table at all.
    let toml_text = r#"
[providers.openai]
kind = "openai-compat"
base_url = "https://example.com/v1"
api_key_ref = "literal:k"
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
kind = "openai-compat"
base_url = "https://example.com/v1"
api_key_ref = "literal:k"
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

/// A missing `[trim]` block must resolve, via `to_params()`, to params
/// byte-identical to `SteadyStateTrimParams::default()` -- the whole
/// point of driving both the per-field serde defaults and the struct's
/// own `Default` impl off the SAME consts in `context_trim.rs`.
#[test]
fn trim_omitted_block_matches_steady_state_trim_params_default() {
    // Arrange: a config with no [trim] table at all.
    let toml_text = r#"
[providers.openai]
kind = "openai-compat"
base_url = "https://example.com/v1"
api_key_ref = "literal:k"
"#;

    // Act
    let cfg: Config = toml::from_str(toml_text).expect("parse without trim block");
    let resolved = cfg.trim.to_params();

    // Assert: byte-identical to the trimmer's own Default.
    assert_eq!(
        resolved,
        crate::context_trim::SteadyStateTrimParams::default(),
        "missing [trim] must resolve to SteadyStateTrimParams::default()"
    );
}

/// A `[trim]` block with explicit knobs parses and resolves through
/// `to_params()`; an unknown key inside it is rejected
/// (deny_unknown_fields, mirroring `reduction_block_parses_and_rejects_unknown_fields`).
#[test]
fn trim_block_parses_and_rejects_unknown_fields() {
    // Arrange + Act: explicit knobs.
    let toml_text = r"
[trim]
trigger_tokens = 50000
clear_at_least_tokens = 10000
head_keep_messages = 1
keep_recent_messages = 3
";
    let cfg: Config = toml::from_str(toml_text).expect("parse explicit trim block");

    // Assert
    let params = cfg.trim.to_params();
    assert_eq!(params.trigger_tokens, 50_000);
    assert_eq!(params.clear_at_least_tokens, 10_000);
    assert_eq!(params.head_keep_messages, 1);
    assert_eq!(params.keep_recent_messages, 3);

    // Arrange: an unknown key inside [trim].
    let bad = r"
[trim]
trigger_tokens = 50000
bogus = 1
";

    // Act + Assert: deny_unknown_fields must reject it.
    assert!(
        toml::from_str::<Config>(bad).is_err(),
        "deny_unknown_fields must reject an unknown TrimConfig field",
    );
}

/// PARITY: the router recording path (`Router::record_would_trim`) and
/// the prompt-size path (`prompt_size::build_steady_state_economics`)
/// both resolve `SteadyStateTrimParams` via `TrimConfig::to_params()`.
/// Neither is directly callable from here -- `record_would_trim` is
/// module-private to `router.rs`, and `build_steady_state_economics`
/// lives in `routectl-cli`, which depends on this crate (not the other
/// way around). So this test drives the router's PUBLIC dispatch entry
/// point end-to-end and reads the OBSERVABLE it stamps onto
/// `DispatchMeta`, then cross-checks it against a local recomputation
/// using the prompt-size path's exact two-call shape (`trim.to_params()`
/// then `propose_steady_state_trim`). Calling `to_params()` twice in
/// isolation can never fail -- it is a pure deterministic mapping -- so
/// that alone would prove nothing; this version fails if
/// `record_would_trim` ever stops resolving params via
/// `self.config.trim.to_params()` (e.g. a revert to
/// `SteadyStateTrimParams::default()`), because the custom trigger
/// below is tuned to fire ONLY under the configured value, not the
/// stock default.
#[tokio::test]
async fn trim_to_params_is_identical_across_both_consumers() {
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
        }
    }

    // A request sized to cross a LOW custom trigger but stay well
    // under the stock default trigger (100_000 tokens): a reversion to
    // the default in either consumer flips the observable outcome from
    // Some to None.
    fn parity_request() -> ChatRequest {
        let payload = "x".repeat(400);
        ChatRequest {
            model: "m".into(),
            messages: vec![
                text_msg(Role::User, "head turn"),
                Message {
                    role: Role::User,
                    content: MessageContent::Parts(vec![ContentPart::Known(
                        KnownContentPart::ToolResult {
                            tool_use_id: "toolu_1".into(),
                            content: serde_json::json!(payload),
                            is_error: None,
                            cache_control: None,
                        },
                    )]),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    refusal: None,
                },
                text_msg(Role::User, "recent turn"),
            ]
            .into(),
            ..Default::default()
        }
    }

    // Arrange: one Config with an explicit, non-default [trim] block.
    let toml_text = r"
[trim]
trigger_tokens = 50
clear_at_least_tokens = 20
head_keep_messages = 1
keep_recent_messages = 1
";
    let cfg: Config = toml::from_str(toml_text).expect("parse trim block");
    let params = cfg.trim.to_params();
    assert_ne!(
        params,
        crate::context_trim::SteadyStateTrimParams::default(),
        "sanity: the explicit block must differ from the stock default"
    );

    // Act: drive the REAL router recording path via the public
    // dispatch entry point (`record_would_trim` is module-private).
    let mut router = Router::new(Arc::new(cfg));
    let provider: Arc<dyn routectl_core::Provider> = Arc::new(EchoProvider);
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m".to_string(),
        Arc::new(ResolvedModel::new("m", "p", provider, "upstream-m")),
    );
    router.install_resolved_models(models);
    let dispatched = router
        .complete_with_options(parity_request(), RouterOptions::new())
        .await;
    dispatched.result.expect("dispatch succeeds");
    let router_observed_d = dispatched
        .meta
        .would_trim_tokens
        .expect("router path must propose a trim for this request under the custom trim block");

    // Act: recompute the prompt-size path's exact call shape --
    // `trim.to_params()` then `propose_steady_state_trim` -- since
    // `build_steady_state_economics` itself lives in a crate that
    // depends on this one and is not callable from here.
    let prompt_size_plan =
        crate::context_trim::propose_steady_state_trim(&parity_request(), &params).expect(
            "prompt-size path must propose a trim for this request under the custom trim block",
        );

    // Assert: both paths agree on the freed-token count, proving they
    // resolved the SAME params from the SAME Config. A reverted
    // consumer would either fail the `.expect(...)` calls above (no
    // trim proposed under the stock default) or, if it transformed the
    // params instead of reverting them outright, land here with a
    // mismatched `d`.
    assert_eq!(
        router_observed_d, prompt_size_plan.candidate.d,
        "router and prompt-size paths must resolve identical SteadyStateTrimParams from the same Config"
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
        "the openai-compat egress DROPS a per-block marker (and 400s under strict_translation)",
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
    assert!(
        bedrock_entry(super::BedrockApiShapeConfig::Converse, None)
            .supports_per_block_breakpoints(),
    );
    assert!(
        !bedrock_entry(super::BedrockApiShapeConfig::Invoke, None).supports_per_block_breakpoints(),
    );
}

/// The two predicates are ORTHOGONAL: an explicit operator `true` sets
/// intent on any kind, but never manufactures wire support.
#[test]
fn explicit_opt_in_does_not_confer_per_block_wire_support() {
    let toml_text = r#"
[providers.openai]
kind = "openai-compat"
base_url = "https://example.com/v1"
api_key_ref = "literal:k"
auto_emit_per_block_breakpoints = true
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse opted-in openai-compat");
    let entry = cfg.providers.get("openai").expect("openai provider");

    assert!(
        entry.per_block_breakpoints_enabled(),
        "the operator switch reads true (intent is recorded)",
    );
    assert!(
        !entry.supports_per_block_breakpoints(),
        "but the egress still cannot carry a per-block marker",
    );
}
