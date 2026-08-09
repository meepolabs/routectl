use super::*;

#[cfg(test)]
mod class_policy_validation_tests {
    //! Tests for `validate_class_policy` (hard rejects) and
    //! `class_policy_warnings` (advisory) over `[retry.classes]` and
    //! `[providers.X.class_overrides]`.

    use super::{class_policy_warnings, validate_class_policy};
    use crate::class_policy::{ClassPolicy, ConfigFailureClass};
    use crate::config::{Config, ProviderEntry, ProviderRuntimePolicy};
    use std::collections::BTreeMap;

    fn provider_with_overrides(overrides: &[(u16, ConfigFailureClass)]) -> ProviderEntry {
        let class_overrides = overrides.iter().copied().collect::<BTreeMap<_, _>>();
        ProviderEntry::anthropic_api("literal:sk-ant-test").with_runtime(ProviderRuntimePolicy {
            class_overrides,
            ..Default::default()
        })
    }

    #[test]
    fn rejects_reserved_feature_unsupported_classes_block() {
        // Arrange: an operator override on the reserved key.
        let mut cfg = Config::default();
        cfg.retry.classes.insert(
            ConfigFailureClass::FeatureUnsupported,
            ClassPolicy {
                retry: Some(1),
                fallback: None,
            },
        );

        // Act
        let err = validate_class_policy(&cfg).unwrap_err();

        // Assert
        let msg = err.to_string();
        assert!(msg.contains("feature-unsupported"), "msg: {msg}");
        assert!(msg.contains("reserved"), "msg: {msg}");
    }

    #[test]
    fn rejects_class_override_targeting_a_disallowed_class() {
        // Arrange: 400 remapped to `server-error`, a retrying class
        // outside the allowed remap-target set.
        let mut cfg = Config::default();
        cfg.providers.insert(
            "acme".to_string(),
            provider_with_overrides(&[(400, ConfigFailureClass::ServerError)]),
        );

        // Act
        let err = validate_class_policy(&cfg).unwrap_err();

        // Assert: names the provider, the status, and the offending target.
        let msg = err.to_string();
        assert!(msg.contains("acme"), "msg: {msg}");
        assert!(msg.contains("400"), "msg: {msg}");
        assert!(msg.contains("server-error"), "msg: {msg}");
    }

    #[test]
    fn accepts_class_override_targeting_feature_unsupported() {
        // Arrange: 400 remapped to `feature-unsupported`, one of the
        // allowed (less-aggressive) remap targets.
        let mut cfg = Config::default();
        cfg.providers.insert(
            "acme".to_string(),
            provider_with_overrides(&[(400, ConfigFailureClass::FeatureUnsupported)]),
        );

        // Act + Assert
        validate_class_policy(&cfg).expect("an allowed remap target must validate");
    }

    #[test]
    fn warns_when_a_health_status_source_is_remapped() {
        // Arrange: 503 (a health signal) remapped to the allowed
        // `feature-unsupported` target -- valid per `validate_class_policy`,
        // but diverts a breaker-relevant status away from health accounting.
        let mut cfg = Config::default();
        cfg.providers.insert(
            "acme".to_string(),
            provider_with_overrides(&[(503, ConfigFailureClass::FeatureUnsupported)]),
        );

        // Act
        let warnings = class_policy_warnings(&cfg);

        // Assert
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("acme") && w.contains("503") && w.contains("outage-masking")),
            "warnings: {warnings:?}"
        );
    }

    #[test]
    fn warns_on_an_empty_class_policy_block() {
        // Arrange: a `[retry.classes.server-error]` block with both
        // leaves unset -- parses fine and does nothing.
        let mut cfg = Config::default();
        cfg.retry.classes.insert(
            ConfigFailureClass::ServerError,
            ClassPolicy {
                retry: None,
                fallback: None,
            },
        );

        // Act
        let warnings = class_policy_warnings(&cfg);

        // Assert
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("server-error") && w.contains("no effect")),
            "warnings: {warnings:?}"
        );
    }

    #[test]
    fn clean_config_validates_ok_with_no_warnings() {
        // Arrange: a well-formed, non-empty class policy plus a
        // non-health-status, allowed-target remap.
        let mut cfg = Config::default();
        cfg.retry.classes.insert(
            ConfigFailureClass::ServerError,
            ClassPolicy {
                retry: Some(2),
                fallback: Some(true),
            },
        );
        cfg.providers.insert(
            "acme".to_string(),
            provider_with_overrides(&[(400, ConfigFailureClass::BadRequest)]),
        );

        // Act + Assert
        validate_class_policy(&cfg).expect("clean config must validate");
        assert!(
            class_policy_warnings(&cfg).is_empty(),
            "clean config must produce no warnings"
        );
    }

    #[test]
    fn warns_when_bad_request_fallback_is_disabled() {
        // Arrange: [retry.classes.bad-request] fallback = false. Valid
        // config, but it turns off the fallback walk that rescues a
        // capability-filter rejection onto a capable target.
        let mut cfg = Config::default();
        cfg.retry.classes.insert(
            ConfigFailureClass::BadRequest,
            ClassPolicy {
                retry: None,
                fallback: Some(false),
            },
        );

        // Act + Assert: config stays valid, but a warning fires naming
        // the structured-output rescue consequence.
        validate_class_policy(&cfg).expect("disabling bad-request fallback is valid config");
        let warnings = class_policy_warnings(&cfg);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("bad-request") && w.contains("structured-output rescue")),
            "warnings: {warnings:?}"
        );
    }

    #[test]
    fn no_bad_request_fallback_warning_on_default_config() {
        // Arrange: default config -- no [retry.classes] overlay at all.
        let cfg = Config::default();

        // Act + Assert
        let warnings = class_policy_warnings(&cfg);
        assert!(
            !warnings
                .iter()
                .any(|w| w.contains("structured-output rescue")),
            "default config must not warn about bad-request fallback: {warnings:?}"
        );
    }

    #[test]
    fn no_bad_request_fallback_warning_when_a_different_class_disables_fallback() {
        // Arrange: [retry.classes.auth] fallback = false. Disabling a
        // DIFFERENT class's fallback must not trip the bad-request-only
        // structured-output-rescue warning.
        let mut cfg = Config::default();
        cfg.retry.classes.insert(
            ConfigFailureClass::Auth,
            ClassPolicy {
                retry: None,
                fallback: Some(false),
            },
        );

        // Act + Assert
        let warnings = class_policy_warnings(&cfg);
        assert!(
            !warnings
                .iter()
                .any(|w| w.contains("structured-output rescue")),
            "disabling auth fallback must not warn about bad-request rescue: {warnings:?}"
        );
    }
}
#[cfg(test)]
mod base_url_validation_tests {
    use super::validate_base_url_scheme;

    #[test]
    fn https_passes() {
        assert!(validate_base_url_scheme("p", "https://api.openai.com").is_ok());
        assert!(validate_base_url_scheme("p", "https://api.anthropic.com").is_ok());
    }

    #[test]
    fn http_loopback_passes() {
        assert!(validate_base_url_scheme("p", "http://127.0.0.1:8080").is_ok());
        assert!(validate_base_url_scheme("p", "http://localhost:8080").is_ok());
        assert!(validate_base_url_scheme("p", "http://[::1]:8080").is_ok());
        // 127.x range covers any IPv4 loopback alias.
        assert!(validate_base_url_scheme("p", "http://127.0.0.5:8080").is_ok());
    }

    #[test]
    fn http_public_host_rejected() {
        let err = validate_base_url_scheme("acme", "http://api.openai.com").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("acme"), "got: {msg}");
        assert!(msg.contains("cleartext"), "got: {msg}");
        // The raw host is NOT echoed: a base_url can carry embedded userinfo
        // (credentials) or an internal hostname the message must not surface.
        assert!(
            !msg.contains("api.openai.com"),
            "host must not be echoed; got: {msg}"
        );
    }

    /// Pin: a rejected base_url carrying embedded userinfo (credentials in
    /// the `user:pass@host` form) must not echo the raw URL, host, or the
    /// embedded secret into the rejection message -- only the provider name
    /// and the violation class survive.
    #[test]
    fn cleartext_rejection_does_not_echo_userinfo_or_host() {
        let err = validate_base_url_scheme("acme", "http://user:sk-live-LEAKED@internal.example")
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("acme"), "got: {msg}");
        assert!(msg.contains("cleartext"), "got: {msg}");
        assert!(
            !msg.contains("sk-live-LEAKED"),
            "credential must not surface; got: {msg}"
        );
        assert!(
            !msg.contains("internal.example"),
            "host must not surface; got: {msg}"
        );
    }

    /// Pin: AWS / Azure / GCP cloud-instance metadata IP must be
    /// rejected even with https. Link-local egress would forward
    /// signed credentials to the configured endpoint.
    #[test]
    fn https_aws_imds_rejected() {
        let err =
            validate_base_url_scheme("p", "https://169.254.169.254/latest/meta-data/").unwrap_err();
        assert!(err.to_string().contains("link-local"));
    }

    /// Pin: 169.254/16 link-local range rejected wholesale (the IMDS
    /// IP is the obvious target but the whole prefix is unsafe).
    #[test]
    fn https_link_local_ipv4_range_rejected() {
        for host in ["169.254.0.1", "169.254.42.42", "169.254.255.255"] {
            let url = format!("https://{host}/");
            let err = validate_base_url_scheme("p", &url).unwrap_err();
            assert!(
                err.to_string().contains("link-local"),
                "expected link-local rejection for {host}; got: {err}"
            );
        }
    }

    /// Pin: IPv6 fe80::/10 unicast link-local rejected.
    #[test]
    fn https_link_local_ipv6_rejected() {
        for url in [
            "https://[fe80::1]/",
            "https://[febf::1]/",
            "https://[fea0:abcd::1]/",
        ] {
            let err = validate_base_url_scheme("p", url).unwrap_err();
            assert!(
                err.to_string().contains("link-local"),
                "expected link-local rejection for {url}; got: {err}"
            );
        }
    }

    /// Pin: IPv6 addresses just outside the fe80::/10 prefix still pass.
    /// fec0:: is site-local (deprecated but not link-local).
    #[test]
    fn https_non_link_local_ipv6_passes() {
        assert!(validate_base_url_scheme("p", "https://[fec0::1]/").is_ok());
        assert!(validate_base_url_scheme("p", "https://[2001:db8::1]/").is_ok());
    }

    /// Pin: an IPv4-mapped IPv6 form of the cloud-metadata IP
    /// (`::ffff:169.254.169.254`) must be rejected. The raw IPv6
    /// link-local check (fe80::/10 on segment[0]) does not catch this
    /// shape, so it must be canonicalized to its embedded IPv4 first.
    #[test]
    fn https_ipv4_mapped_link_local_rejected() {
        for url in [
            "https://[::ffff:169.254.169.254]/v1",
            "https://[::ffff:169.254.0.1]/",
        ] {
            let err = validate_base_url_scheme("p", url).unwrap_err();
            assert!(
                err.to_string().contains("link-local"),
                "expected link-local rejection for {url}; got: {err}"
            );
        }
    }

    /// Pin: an IPv4-mapped IPv6 form of a loopback IP
    /// (`::ffff:127.0.0.1`) is correctly accepted under http:// just
    /// like the bare `127.0.0.1`, rather than misleadingly rejected.
    #[test]
    fn http_ipv4_mapped_loopback_passes() {
        assert!(validate_base_url_scheme("p", "http://[::ffff:127.0.0.1]/").is_ok());
        assert!(validate_base_url_scheme("p", "http://[::ffff:127.0.0.1]:8080/v1").is_ok());
    }

    /// Pin: an IPv4-COMPATIBLE IPv6 form of the cloud-metadata IP
    /// (`::169.254.169.254`, prefix `::/96`) must be rejected. This is
    /// distinct from the IPv4-MAPPED form (`::ffff:...`): `to_ipv4_mapped`
    /// returns None for it, so the link-local guard must also extract the
    /// embedded IPv4 from the IPv4-compatible form before testing it.
    #[test]
    fn https_ipv4_compatible_link_local_rejected() {
        for url in ["https://[::169.254.169.254]/", "https://[::169.254.0.1]/v1"] {
            let err = validate_base_url_scheme("p", url).unwrap_err();
            assert!(
                err.to_string().contains("link-local"),
                "expected link-local rejection for {url}; got: {err}"
            );
        }
    }

    #[test]
    fn unknown_scheme_rejected() {
        let err = validate_base_url_scheme("p", "ftp://example.com").unwrap_err();
        assert!(err.to_string().contains("not allowed"));
    }

    #[test]
    fn empty_rejected() {
        // A present-but-empty base_url is an operator typo, never a
        // "use the kind default" signal: the OpenaiResponses `None`
        // default is substituted before this fn is called, so every
        // string that reaches here was set explicitly.
        let err = validate_base_url_scheme("acme", "").unwrap_err();
        assert!(err.to_string().contains("acme"), "got: {err}");
        assert!(validate_base_url_scheme("p", "   ").is_err());
    }

    #[test]
    fn unparseable_url_rejected() {
        let err = validate_base_url_scheme("p", "not a url at all").unwrap_err();
        assert!(err.to_string().contains("not a valid URL"));
    }
}

#[cfg(test)]
#[cfg(feature = "bedrock")]
mod bedrock_validation_tests {
    use super::*;
    use crate::config::{
        BedrockApiShapeConfig, BedrockCredsConfig, BedrockGlobalConfig, Config, ProviderEntry,
    };
    use std::collections::BTreeMap;

    fn baseline_betas() -> Vec<String> {
        vec!["context-1m-2025-08-07".into()]
    }

    fn baseline_fields() -> Vec<String> {
        BEDROCK_REQUIRED_BODY_FIELDS
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    }

    fn bedrock_provider_entry() -> ProviderEntry {
        ProviderEntry::Bedrock {
            region: "us-west-2".into(),
            api_shape: BedrockApiShapeConfig::Invoke,
            creds: BedrockCredsConfig::DefaultChain,
            user_agent: None,
            header_extras: BTreeMap::new(),
            payload_extras: None,
            anthropic_beta: Vec::new(),
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            reduction_enabled: None,
            runtime: Default::default(),
        }
    }

    fn bedrock_provider_entry_with_floor_beta() -> ProviderEntry {
        let ProviderEntry::Bedrock {
            region,
            api_shape,
            creds,
            user_agent,
            header_extras,
            payload_extras,
            runtime,
            ..
        } = bedrock_provider_entry()
        else {
            unreachable!();
        };
        ProviderEntry::Bedrock {
            region,
            api_shape,
            creds,
            user_agent,
            header_extras,
            payload_extras,
            anthropic_beta: vec!["future-flag-2026-12-31".into()],
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            reduction_enabled: None,
            runtime,
        }
    }

    fn config_with(bedrock_provider: bool, global: BedrockGlobalConfig) -> Config {
        let mut providers: BTreeMap<String, ProviderEntry> = BTreeMap::new();
        if bedrock_provider {
            providers.insert("primary".into(), bedrock_provider_entry());
        }
        Config {
            providers,
            bedrock: global,
            ..Config::default()
        }
    }

    fn config_with_entry(entry: ProviderEntry, global: BedrockGlobalConfig) -> Config {
        let mut providers: BTreeMap<String, ProviderEntry> = BTreeMap::new();
        providers.insert("primary".into(), entry);
        Config {
            providers,
            bedrock: global,
            ..Config::default()
        }
    }

    #[test]
    fn no_bedrock_provider_short_circuits_ok() {
        // Arrange: no providers reference Bedrock; the [bedrock] section
        // is empty (default).
        let cfg = config_with(false, BedrockGlobalConfig::default());

        // Act
        let result = validate_bedrock_global_config(&cfg);

        // Assert
        assert!(result.is_ok());
    }

    #[test]
    fn bedrock_provider_with_empty_allowlists_is_pass_through() {
        // Discovery mode: operator omits the [bedrock] section entirely.
        // Validation passes; the filters run in pass-through mode so the
        // operator can observe traffic via trace logs and build their
        // list from what they see.
        let cfg = config_with(
            true,
            BedrockGlobalConfig {
                allowed_betas: Vec::new(),
                allowed_body_fields: Vec::new(),
            },
        );

        let result = validate_bedrock_global_config(&cfg);

        assert!(result.is_ok(), "expected pass-through Ok, got {result:?}");
    }

    #[test]
    fn bedrock_provider_with_only_allowed_betas_set_is_ok() {
        // Operator chose to gate betas only; body-fields remain in
        // pass-through mode. Validation should accept this -- the
        // empty body-fields list short-circuits the required-keys
        // check.
        let cfg = config_with(
            true,
            BedrockGlobalConfig {
                allowed_betas: baseline_betas(),
                allowed_body_fields: Vec::new(),
            },
        );

        let result = validate_bedrock_global_config(&cfg);

        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    #[test]
    fn bedrock_provider_missing_required_body_field_errors() {
        // Arrange: the operator omitted `messages` from their list.
        let mut fields = baseline_fields();
        fields.retain(|s| s != "messages");
        let cfg = config_with(
            true,
            BedrockGlobalConfig {
                allowed_betas: baseline_betas(),
                allowed_body_fields: fields,
            },
        );

        // Act
        let err = validate_bedrock_global_config(&cfg).unwrap_err();

        // Assert
        let msg = err.to_string();
        assert!(msg.contains("messages"), "msg: {msg}");
        assert!(msg.contains("routectl-mandatory"), "msg: {msg}");
        assert!(msg.contains("Invoke"), "msg: {msg}");
    }

    #[test]
    fn converse_only_deployment_skips_required_body_field_check() {
        // Arrange: a Converse-only deployment with `allowed_body_fields`
        // that omits `messages`/`anthropic_version`/`max_tokens`. Those
        // keys live at the AWS top level on Converse and never reach
        // `additionalModelRequestFields`, so the missing-required check
        // must NOT fire.
        let cfg = config_with_entry(
            ProviderEntry::Bedrock {
                region: "us-west-2".into(),
                api_shape: BedrockApiShapeConfig::Converse,
                creds: BedrockCredsConfig::DefaultChain,
                user_agent: None,
                header_extras: BTreeMap::new(),
                payload_extras: None,
                anthropic_beta: Vec::new(),
                cache_capability: None,
                auto_emit_top_level_breakpoint: None,
                reduction_enabled: None,
                runtime: Default::default(),
            },
            BedrockGlobalConfig {
                allowed_betas: baseline_betas(),
                allowed_body_fields: vec!["thinking".into(), "anthropic_beta".into()],
            },
        );

        let result = validate_bedrock_global_config(&cfg);

        assert!(
            result.is_ok(),
            "Converse-only deployment should not require Invoke-specific body keys; got {result:?}"
        );
    }

    #[test]
    fn bedrock_provider_floor_beta_requires_anthropic_beta_body_field() {
        let cfg = config_with_entry(
            bedrock_provider_entry_with_floor_beta(),
            BedrockGlobalConfig {
                allowed_betas: baseline_betas(),
                allowed_body_fields: baseline_fields(),
            },
        );

        let err = validate_bedrock_global_config(&cfg).unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("anthropic_beta"), "msg: {msg}");
        assert!(msg.contains("always-send"), "msg: {msg}");
    }

    #[test]
    fn fully_populated_config_is_ok() {
        // Arrange
        let cfg = config_with(
            true,
            BedrockGlobalConfig {
                allowed_betas: baseline_betas(),
                allowed_body_fields: baseline_fields(),
            },
        );

        // Act
        let result = validate_bedrock_global_config(&cfg);

        // Assert
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }
}

#[cfg(test)]
#[cfg(feature = "bedrock")]
mod bedrock_creds_ref_validation_tests {
    //! Tests for `validate_bedrock_creds_refs`: a present-but-empty
    //! required creds ref is a config error on the native Bedrock lane and
    //! on the three `bedrock_mantle` lanes. Configs are parsed from TOML to
    //! exercise the real deserialization path an operator hits.

    use super::validate_bedrock_creds_refs;
    use crate::config::Config;

    fn config_from(toml_text: &str) -> Config {
        toml::from_str(toml_text).expect("config must parse")
    }

    #[test]
    fn empty_key_ref_on_native_bedrock_is_rejected() {
        let cfg = config_from(
            r#"
[providers.native]
kind = "bedrock"
region = "us-west-2"
creds = { kind = "bearer-key", key_ref = "" }
"#,
        );

        let err = validate_bedrock_creds_refs(&cfg).unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("native"), "msg: {msg}");
        assert!(msg.contains("key_ref"), "msg: {msg}");
    }

    #[test]
    fn empty_access_key_ref_on_mantle_lane_is_rejected() {
        let cfg = config_from(
            r#"
[providers.compat-mantle]
kind = "openai-compat"
api_key_ref = ""

[providers.compat-mantle.bedrock_mantle]
region = "us-west-2"

[providers.compat-mantle.bedrock_mantle.creds]
kind = "static"
access_key_ref = ""
secret_key_ref = "env://AWS_SECRET_ACCESS_KEY"
"#,
        );

        let err = validate_bedrock_creds_refs(&cfg).unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("compat-mantle"), "msg: {msg}");
        assert!(msg.contains("access_key_ref"), "msg: {msg}");
    }

    #[test]
    fn present_but_empty_session_token_ref_is_rejected() {
        let cfg = config_from(
            r#"
[providers.native]
kind = "bedrock"
region = "us-west-2"

[providers.native.creds]
kind = "static"
access_key_ref = "env://AWS_ACCESS_KEY_ID"
secret_key_ref = "env://AWS_SECRET_ACCESS_KEY"
session_token_ref = ""
"#,
        );

        let err = validate_bedrock_creds_refs(&cfg).unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("session_token_ref"), "msg: {msg}");
    }

    #[test]
    fn valid_creds_refs_pass_on_native_and_mantle_lanes() {
        let cfg = config_from(
            r#"
[providers.native]
kind = "bedrock"
region = "us-west-2"

[providers.native.creds]
kind = "static"
access_key_ref = "env://AWS_ACCESS_KEY_ID"
secret_key_ref = "env://AWS_SECRET_ACCESS_KEY"

[providers.anthropic-mantle]
kind = "anthropic-api"
bedrock_mantle = { region = "us-west-2", creds = { kind = "bearer-key", key_ref = "file:///tmp/whatever" } }

[providers.profile-native]
kind = "bedrock"
region = "us-west-2"
creds = { kind = "profile", name = "bedrock-prod" }

[providers.chain-native]
kind = "bedrock"
region = "us-west-2"
creds = { kind = "default-chain" }
"#,
        );

        assert!(
            validate_bedrock_creds_refs(&cfg).is_ok(),
            "valid creds refs on every lane must pass"
        );
    }

    #[test]
    fn omitted_session_token_ref_is_valid() {
        let cfg = config_from(
            r#"
[providers.native]
kind = "bedrock"
region = "us-west-2"

[providers.native.creds]
kind = "static"
access_key_ref = "env://AWS_ACCESS_KEY_ID"
secret_key_ref = "env://AWS_SECRET_ACCESS_KEY"
"#,
        );

        assert!(
            validate_bedrock_creds_refs(&cfg).is_ok(),
            "an omitted optional session_token_ref must pass"
        );
    }
}

#[cfg(test)]
mod validate_alias_chain_targets_tests {
    //! Tests for the v0.6.0 alias-chain validator. Each test pins
    //! one validator branch (clean pass, unknown nickname, disabled
    //! nickname, multi-error accumulation) so a regression in any
    //! one branch shows up as a precise test failure.

    use super::validate_alias_chain_targets;
    use crate::config::{AliasValue, Config, ModelEntry};
    use std::collections::BTreeMap;

    fn config_with(models: Vec<(&str, ModelEntry)>, aliases: Vec<(&str, AliasValue)>) -> Config {
        let mut m = BTreeMap::new();
        for (name, e) in models {
            m.insert(name.to_string(), e);
        }
        let mut a = BTreeMap::new();
        for (name, v) in aliases {
            a.insert(name.to_string(), v);
        }
        Config {
            models: m,
            aliases: a,
            ..Config::default()
        }
    }

    #[test]
    fn validate_alias_chain_targets_passes_clean_config() {
        let cfg = config_with(
            vec![
                ("haiku", ModelEntry::new("anthropic", "claude-haiku")),
                ("sonnet", ModelEntry::new("anthropic", "claude-sonnet")),
            ],
            vec![
                ("fast", AliasValue::Single("haiku".into())),
                (
                    "heavy",
                    AliasValue::Chain(vec!["sonnet".into(), "haiku".into()]),
                ),
            ],
        );
        validate_alias_chain_targets(&cfg).expect("clean config must validate");
    }

    #[test]
    fn validate_alias_chain_targets_rejects_unknown_nickname() {
        let cfg = config_with(
            vec![("haiku", ModelEntry::new("anthropic", "claude-haiku"))],
            vec![("fast", AliasValue::Single("missing".into()))],
        );
        let err = validate_alias_chain_targets(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("alias `fast`"), "msg: {msg}");
        assert!(msg.contains("missing"), "msg: {msg}");
        assert!(msg.contains("not a known model nickname"), "msg: {msg}");
    }

    #[test]
    fn validate_alias_chain_targets_rejects_disabled_nickname() {
        let cfg = config_with(
            vec![(
                "shelved",
                ModelEntry::new("anthropic", "claude-shelved").with_selectable(false),
            )],
            vec![("fast", AliasValue::Single("shelved".into()))],
        );
        let err = validate_alias_chain_targets(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("alias `fast`"), "msg: {msg}");
        assert!(msg.contains("shelved"), "msg: {msg}");
        assert!(msg.contains("selectable = false"), "msg: {msg}");
    }

    #[test]
    fn validate_alias_chain_targets_rejects_empty_chain() {
        let cfg = config_with(
            vec![("haiku", ModelEntry::new("anthropic", "claude-haiku"))],
            vec![("fast", AliasValue::Chain(vec![]))],
        );
        let err = validate_alias_chain_targets(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("alias `fast`"), "msg: {msg}");
        assert!(msg.contains("empty"), "msg: {msg}");
    }

    #[test]
    fn validate_alias_chain_targets_accumulates_multiple_errors() {
        // Two unrelated misconfigurations -- one alias references an
        // unknown nickname, another references a disabled one. The
        // validator must surface BOTH in a single error so the
        // operator doesn't fix one and discover the other on the
        // next run.
        let cfg = config_with(
            vec![
                ("haiku", ModelEntry::new("anthropic", "claude-haiku")),
                (
                    "shelved",
                    ModelEntry::new("anthropic", "claude-shelved").with_selectable(false),
                ),
            ],
            vec![
                ("alpha", AliasValue::Single("missing-1".into())),
                ("beta", AliasValue::Single("shelved".into())),
                (
                    "gamma",
                    AliasValue::Chain(vec!["haiku".into(), "missing-2".into()]),
                ),
            ],
        );
        let err = validate_alias_chain_targets(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("missing-1"), "msg: {msg}");
        assert!(msg.contains("missing-2"), "msg: {msg}");
        assert!(msg.contains("shelved"), "msg: {msg}");
    }

    // ----- Recursive alias-chain validation (Task #5) -----
    //
    // Each test pins one slice of the recursive expansion contract:
    // alias-of-alias resolves, dangling refs surface cleanly, cycles
    // are detected with a path-bearing error, and globs follow the
    // same rule as exact aliases.

    #[test]
    fn alias_referencing_another_alias_passes_validation() {
        // A -> B -> model. Pass 1 sees `A`'s "B" as an alias key
        // (skipped, recursion-checked later) and `B`'s "model-x" as
        // a known nickname. Pass 2 walks A -> B -> model-x without
        // hitting a cycle.
        let cfg = config_with(
            vec![("model-x", ModelEntry::new("anthropic", "claude-x"))],
            vec![
                ("a", AliasValue::Single("b".into())),
                ("b", AliasValue::Single("model-x".into())),
            ],
        );
        validate_alias_chain_targets(&cfg).expect("2-deep alias chain must validate");
    }

    #[test]
    fn alias_referencing_three_deep_passes_validation() {
        let cfg = config_with(
            vec![
                ("model-x", ModelEntry::new("anthropic", "claude-x")),
                ("model-y", ModelEntry::new("anthropic", "claude-y")),
            ],
            vec![
                ("a", AliasValue::Single("b".into())),
                ("b", AliasValue::Single("c".into())),
                (
                    "c",
                    AliasValue::Chain(vec!["model-x".into(), "model-y".into()]),
                ),
            ],
        );
        validate_alias_chain_targets(&cfg).expect("3-deep alias chain must validate");
    }

    #[test]
    fn alias_cycle_detected_with_path() {
        // A -> B -> A. Pass 1 sees both entries as alias keys (no
        // dangling-ref errors). Pass 2 catches the back-edge.
        let cfg = config_with(
            vec![("model-x", ModelEntry::new("anthropic", "claude-x"))],
            vec![
                ("a", AliasValue::Single("b".into())),
                ("b", AliasValue::Single("a".into())),
            ],
        );
        let err = validate_alias_chain_targets(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("cycle detected"), "msg: {msg}");
        // The reported path includes both alias keys and closes back
        // on the entry point.
        assert!(
            msg.contains("a -> b -> a") || msg.contains("b -> a -> b"),
            "msg: {msg}"
        );
    }

    #[test]
    fn alias_self_cycle_detected() {
        // The 1-hop degenerate case: A -> A. Pass 1 lets it through
        // (alias key); pass 2 catches the immediate back-edge.
        let cfg = config_with(
            vec![("model-x", ModelEntry::new("anthropic", "claude-x"))],
            vec![("a", AliasValue::Single("a".into()))],
        );
        let err = validate_alias_chain_targets(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("cycle detected"), "msg: {msg}");
        assert!(msg.contains("a -> a"), "msg: {msg}");
    }

    #[test]
    fn external_alias_feeds_cycle_attributes_to_first_in_cycle() {
        // Regression for the cycle-attribution fix: when a non-cycle
        // alias feeds into a cycle, the diagnostic must name the
        // FIRST alias in the cycle, not the DFS root that merely
        // pointed at it. Config: `a -> b -> c -> b`; the cycle is
        // `b <-> c` and `a` is the external feeder. (Root iteration
        // is alphabetical because `config.aliases` is a `BTreeMap`,
        // so `a` is the DFS root that detects the back-edge.)
        //
        // BEFORE the fix this reported `alias `a`: ...` (wrong --
        // operator looks at `a`, finds it just points at `b`, can't
        // see the cycle). AFTER the fix it reports `alias `b`: ...`
        // -- the alias that closes the loop.
        let cfg = config_with(
            vec![("model-x", ModelEntry::new("anthropic", "claude-x"))],
            vec![
                ("a", AliasValue::Single("b".into())),
                ("b", AliasValue::Single("c".into())),
                ("c", AliasValue::Single("b".into())),
            ],
        );
        let err = validate_alias_chain_targets(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("alias `b`:"), "msg: {msg}");
        assert!(msg.contains("b -> c -> b"), "msg: {msg}");
        assert!(
            !msg.contains("alias `a`:"),
            "external feeder `a` must not be the attributed alias; msg: {msg}"
        );
    }

    #[test]
    fn dangling_ref_in_recursive_chain_is_caught() {
        // A -> nonexistent. Neither an alias key nor a model
        // nickname; pass 1 surfaces a dangling-reference error.
        let cfg = config_with(
            vec![("model-x", ModelEntry::new("anthropic", "claude-x"))],
            vec![("a", AliasValue::Single("nonexistent".into()))],
        );
        let err = validate_alias_chain_targets(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("alias `a`"), "msg: {msg}");
        assert!(msg.contains("nonexistent"), "msg: {msg}");
        assert!(
            msg.contains("not a known model nickname") && msg.contains("not an alias key"),
            "msg: {msg}"
        );
    }

    #[test]
    fn glob_alias_referencing_another_alias_passes_validation() {
        // Per architect's verdict F: glob keys follow the same rule
        // as exact keys. `claude-haiku*` -> `a` -> model. The fact
        // that the glob key is a pattern (not a literal) does not
        // change validation semantics.
        let cfg = config_with(
            vec![("model-x", ModelEntry::new("anthropic", "claude-x"))],
            vec![
                ("claude-haiku*", AliasValue::Single("a".into())),
                ("a", AliasValue::Single("model-x".into())),
            ],
        );
        validate_alias_chain_targets(&cfg).expect("glob key into alias must validate");
    }

    #[test]
    fn dry_operator_pattern_passes_validation() {
        // The DRY case from the spec: a single source-of-truth alias
        // `a` plus a discoverability wrapper `claude-a` that just
        // points at it. Both should validate cleanly so the operator
        // can collapse the duplicated `claude-cheap`/`claude-codex-pro`
        // /etc. shapes that currently inline the full chain.
        let cfg = config_with(
            vec![("model-x", ModelEntry::new("anthropic", "claude-x"))],
            vec![
                ("a", AliasValue::Single("model-x".into())),
                ("claude-a", AliasValue::Single("a".into())),
            ],
        );
        validate_alias_chain_targets(&cfg).expect("DRY single-pointer alias must validate");
    }
}

#[cfg(test)]
mod validate_reasoning_defaults_tests {
    //! Unit tests for `validate_reasoning_defaults`.
    //! Covers: valid levels accepted, empty list accepted, invalid level
    //! rejected (error names model and offending token), all six valid
    //! tokens pass individually.

    use super::validate_reasoning_defaults;
    use crate::config::{Config, ModelEntry};

    fn config_with_model(nickname: &str, entry: ModelEntry) -> Config {
        let mut cfg = Config::default();
        cfg.models.insert(nickname.to_string(), entry);
        cfg
    }

    /// Empty effort_levels is valid (pass-through mode).
    #[test]
    fn accepts_empty_effort_levels() {
        let entry = ModelEntry::new("p", "u").with_effort_levels(vec![]);
        let cfg = config_with_model("m", entry);
        assert!(
            validate_reasoning_defaults(&cfg).is_ok(),
            "empty effort_levels should be accepted"
        );
    }

    /// Default effort_levels (["low","medium","high"]) is valid.
    #[test]
    fn accepts_default_effort_levels() {
        let entry = ModelEntry::new("p", "u");
        let cfg = config_with_model("m", entry);
        assert!(
            validate_reasoning_defaults(&cfg).is_ok(),
            "default effort_levels must be valid"
        );
    }

    /// All six valid vocabulary tokens are individually accepted.
    #[test]
    fn accepts_all_six_valid_levels() {
        for level in ["minimal", "low", "medium", "high", "xhigh", "max"] {
            let entry = ModelEntry::new("p", "u").with_effort_levels(vec![level.to_string()]);
            let cfg = config_with_model("single", entry);
            assert!(
                validate_reasoning_defaults(&cfg).is_ok(),
                "level {level:?} should be accepted"
            );
        }
    }

    /// A mix of valid tokens all in one list is accepted.
    #[test]
    fn accepts_mixed_valid_levels() {
        let entry = ModelEntry::new("p", "u").with_effort_levels(vec![
            "minimal".into(),
            "low".into(),
            "medium".into(),
            "high".into(),
            "xhigh".into(),
            "max".into(),
        ]);
        let cfg = config_with_model("m", entry);
        assert!(
            validate_reasoning_defaults(&cfg).is_ok(),
            "all six levels together should be valid"
        );
    }

    /// An unknown token causes rejection with the model name and token in
    /// the error message.
    #[test]
    fn rejects_invalid_level_names_model_and_token() {
        let entry = ModelEntry::new("p", "u")
            .with_effort_levels(vec!["low".into(), "invalid_level".into()]);
        let cfg = config_with_model("my-model", entry);
        let err =
            validate_reasoning_defaults(&cfg).expect_err("invalid effort level should be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("my-model"),
            "error must name the model; got: {msg}"
        );
        assert!(
            msg.contains("invalid_level"),
            "error must name the offending token; got: {msg}"
        );
    }

    /// The validator catches every entry: if multiple models have invalid
    /// levels, the first offender is reported (not silently skipped).
    #[test]
    fn rejects_on_first_invalid_model_encountered() {
        let mut cfg = Config::default();
        cfg.models.insert(
            "good".to_string(),
            ModelEntry::new("p", "u").with_effort_levels(vec!["low".into(), "high".into()]),
        );
        cfg.models.insert(
            "bad".to_string(),
            ModelEntry::new("p", "u").with_effort_levels(vec!["high".into(), "turbo".into()]),
        );
        let err = validate_reasoning_defaults(&cfg)
            .expect_err("should reject the config with an invalid level");
        let msg = err.to_string();
        assert!(
            msg.contains("turbo"),
            "error must name the offending token; got: {msg}"
        );
    }

    /// A config with no models at all passes validation.
    #[test]
    fn accepts_empty_models_table() {
        let cfg = Config::default();
        assert!(
            validate_reasoning_defaults(&cfg).is_ok(),
            "empty models table must be valid"
        );
    }
}

#[cfg(test)]
mod validate_registry_patterns_tests {
    //! Tests for the `[registry]` glob-key validator: a malformed glob
    //! must reject at startup; well-formed exact and trailing-`*` keys
    //! must pass.

    use super::validate_registry_patterns;
    use crate::config::{Config, RegistryEntry};

    fn config_with_keys(keys: &[&str]) -> Config {
        let mut cfg = Config::default();
        for key in keys {
            cfg.registry
                .insert((*key).to_string(), RegistryEntry::default());
        }
        cfg
    }

    #[test]
    fn rejects_embedded_asterisk_key() {
        // Arrange: `a*b` has an asterisk in a non-trailing position.
        let cfg = config_with_keys(&["a*b"]);

        // Act
        let err = validate_registry_patterns(&cfg).unwrap_err();

        // Assert
        let msg = err.to_string();
        assert!(msg.contains("[registry.a*b]"), "msg: {msg}");
        assert!(msg.contains("invalid pattern"), "msg: {msg}");
    }

    #[test]
    fn accepts_exact_and_trailing_star_keys() {
        // Arrange
        let cfg = config_with_keys(&["deepseek-chat", "claude-opus-*"]);

        // Act + Assert
        validate_registry_patterns(&cfg).expect("clean registry keys must validate");
    }
}

#[cfg(test)]
mod validate_alias_patterns_tests {
    //! Tests for the `[aliases]` glob-key validator: a malformed glob
    //! key (bare or embedded `*`) must reject at startup; well-formed
    //! exact and trailing-`*` keys must pass.

    use super::validate_alias_patterns;
    use crate::config::{AliasValue, Config};

    fn config_with_alias_keys(keys: &[&str]) -> Config {
        let mut cfg = Config::default();
        for key in keys {
            cfg.aliases
                .insert((*key).to_string(), AliasValue::Single("some-model".into()));
        }
        cfg
    }

    #[test]
    fn rejects_embedded_asterisk_key() {
        // `foo*bar` has an asterisk in a non-trailing position; the
        // pattern parser rejects it.
        let cfg = config_with_alias_keys(&["foo*bar"]);
        let err = validate_alias_patterns(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("[aliases.foo*bar]"), "msg: {msg}");
        assert!(msg.contains("invalid pattern"), "msg: {msg}");
    }

    #[test]
    fn rejects_bare_asterisk_key() {
        let cfg = config_with_alias_keys(&["*"]);
        let err = validate_alias_patterns(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("[aliases.*]"), "msg: {msg}");
        assert!(msg.contains("invalid pattern"), "msg: {msg}");
    }

    #[test]
    fn accepts_exact_and_trailing_star_keys() {
        let cfg = config_with_alias_keys(&["claude-opus-4-7-20251022", "claude-opus-*"]);
        validate_alias_patterns(&cfg).expect("clean alias keys must validate");
    }
}

#[cfg(test)]
mod validate_mitm_config_tests {
    //! Tests for the `[mitm]` field-coherence validator: absence is
    //! always `Ok`; presence pins `upstream_origin` and `mitm_host` to
    //! EXACTLY first-party `api.anthropic.com` (containment guarantee:
    //! the client's full-scope claude.ai token must never reach a
    //! non-Anthropic egress) and still rejects a `listen_port` that
    //! collides with `[server] port`.

    use super::validate_mitm_config;
    use crate::config::{Config, MitmConfig};

    fn config_with_mitm(mitm: MitmConfig) -> Config {
        Config {
            mitm: Some(mitm),
            ..Config::default()
        }
    }

    #[test]
    fn absent_mitm_block_is_ok() {
        let cfg = Config::default();
        validate_mitm_config(&cfg).expect("None must always validate");
    }

    #[test]
    fn default_mitm_block_validates_clean() {
        let cfg = config_with_mitm(MitmConfig::default());
        validate_mitm_config(&cfg).expect("default [mitm] must validate against default [server]");
    }

    #[test]
    fn rejects_non_anthropic_https_origin() {
        let cfg = config_with_mitm(MitmConfig {
            upstream_origin: "https://evil.example.com".into(),
            ..MitmConfig::default()
        });
        let err = validate_mitm_config(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("upstream_origin"), "msg: {msg}");
        assert!(msg.contains("api.anthropic.com"), "msg: {msg}");
    }

    #[test]
    fn rejects_non_https_upstream_origin() {
        let cfg = config_with_mitm(MitmConfig {
            upstream_origin: "http://api.anthropic.com".into(),
            ..MitmConfig::default()
        });
        let err = validate_mitm_config(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("upstream_origin"), "msg: {msg}");
    }

    #[test]
    fn rejects_malformed_upstream_origin() {
        let cfg = config_with_mitm(MitmConfig {
            upstream_origin: "not a url".into(),
            ..MitmConfig::default()
        });
        let err = validate_mitm_config(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("upstream_origin"), "msg: {msg}");
    }

    /// Userinfo (`user:pass@`) in the origin must be rejected even
    /// though the host itself is the pinned first-party host -- a
    /// stray userinfo component is never legitimate here and could mask
    /// operator error.
    #[test]
    fn rejects_upstream_origin_with_userinfo() {
        let cfg = config_with_mitm(MitmConfig {
            upstream_origin: "https://x@api.anthropic.com".into(),
            ..MitmConfig::default()
        });
        let err = validate_mitm_config(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("upstream_origin"), "msg: {msg}");
    }

    /// An explicit non-default port must be rejected even with the
    /// otherwise-correct host: `https://api.anthropic.com:9999` is a
    /// different egress target from the pinned
    /// `https://api.anthropic.com` (implicit default 443), and a typo'd
    /// or attacker-controlled port is exactly the kind of drift this
    /// containment check exists to catch.
    #[test]
    fn rejects_upstream_origin_with_a_non_default_port() {
        let cfg = config_with_mitm(MitmConfig {
            upstream_origin: "https://api.anthropic.com:9999".into(),
            ..MitmConfig::default()
        });
        let err = validate_mitm_config(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("upstream_origin"), "msg: {msg}");
    }

    /// An explicit but redundant default port (`:443`) is normalized
    /// away by URL parsing and must still validate clean -- this pins
    /// the intent that only a genuinely DIFFERENT port is rejected, not
    /// the spelling of the default one.
    #[test]
    fn accepts_upstream_origin_with_explicit_default_port() {
        let cfg = config_with_mitm(MitmConfig {
            upstream_origin: "https://api.anthropic.com:443".into(),
            ..MitmConfig::default()
        });
        validate_mitm_config(&cfg).expect("an explicit default port must still validate clean");
    }

    #[test]
    fn rejects_upstream_origin_with_a_path() {
        let cfg = config_with_mitm(MitmConfig {
            upstream_origin: "https://api.anthropic.com/v1".into(),
            ..MitmConfig::default()
        });
        let err = validate_mitm_config(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("upstream_origin"), "msg: {msg}");
    }

    #[test]
    fn rejects_upstream_origin_with_a_query() {
        let cfg = config_with_mitm(MitmConfig {
            upstream_origin: "https://api.anthropic.com/?x=1".into(),
            ..MitmConfig::default()
        });
        let err = validate_mitm_config(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("upstream_origin"), "msg: {msg}");
    }

    #[test]
    fn rejects_listen_port_colliding_with_server_port() {
        let default_server_port = Config::default().server.port;
        let cfg = config_with_mitm(MitmConfig {
            listen_port: default_server_port,
            ..MitmConfig::default()
        });
        let err = validate_mitm_config(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("listen_port"), "msg: {msg}");
    }

    #[test]
    fn rejects_empty_mitm_host() {
        let cfg = config_with_mitm(MitmConfig {
            mitm_host: String::new(),
            ..MitmConfig::default()
        });
        let err = validate_mitm_config(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mitm_host"), "msg: {msg}");
    }

    #[test]
    fn rejects_non_anthropic_mitm_host() {
        let cfg = config_with_mitm(MitmConfig {
            mitm_host: "example.com".into(),
            ..MitmConfig::default()
        });
        let err = validate_mitm_config(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mitm_host"), "msg: {msg}");
        assert!(msg.contains("api.anthropic.com"), "msg: {msg}");
    }

    /// A subdomain of the first-party host must be rejected -- this is
    /// an EXACT host match, not a suffix match, so
    /// `evil.api.anthropic.com` never slips through as if it were
    /// `api.anthropic.com`.
    #[test]
    fn rejects_mitm_host_subdomain() {
        let cfg = config_with_mitm(MitmConfig {
            mitm_host: "evil.api.anthropic.com".into(),
            ..MitmConfig::default()
        });
        let err = validate_mitm_config(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mitm_host"), "msg: {msg}");
    }
}

#[cfg(test)]
mod validate_provider_credential_sources_tests {
    //! Config-BOUNDARY tests (parse via `toml::from_str`, not bare-struct
    //! construction only) for the provider-level `credential_source`
    //! field-coherence validator: `forwarded` requires an empty
    //! `api_key_ref` AND a base_url pinned to `api.anthropic.com`; `own`
    //! (the default) requires a non-empty `api_key_ref`, exactly as
    //! every `anthropic-api` provider behaved before this field existed.

    use super::validate_provider_credential_sources;
    use crate::config::Config;

    /// A forwarded provider block with NO `api_key_ref` line at all must
    /// parse cleanly (the field is `#[serde(default)]`) and pass
    /// validation.
    #[test]
    fn forwarded_block_with_no_api_key_ref_parses_and_validates() {
        let toml_text = r#"
[providers.anthropic-forwarded]
kind = "anthropic-api"
base_url = "https://api.anthropic.com"
credential_source = "forwarded"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("forwarded block must parse");
        validate_provider_credential_sources(&cfg).expect("clean forwarded block must validate ok");
    }

    /// `credential_source` omitted entirely defaults to `own` and the
    /// pre-existing `api_key_ref`-required behavior is unchanged.
    #[test]
    fn own_block_with_credential_source_omitted_is_unchanged() {
        let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("own block must parse");
        validate_provider_credential_sources(&cfg)
            .expect("default-own block with a key must validate ok");
    }

    #[test]
    fn rejects_forwarded_on_a_non_anthropic_host() {
        let toml_text = r#"
[providers.sneaky]
kind = "anthropic-api"
base_url = "https://evil.example.com"
credential_source = "forwarded"
"#;
        let cfg: Config = toml::from_str(toml_text)
            .expect("must parse (host pin is validator-time, not parse-time)");
        let err = validate_provider_credential_sources(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("sneaky"), "msg: {msg}");
        assert!(msg.contains("api.anthropic.com"), "msg: {msg}");
    }

    #[test]
    fn rejects_forwarded_carrying_a_nonempty_api_key_ref() {
        let toml_text = r#"
[providers.mixed-up]
kind = "anthropic-api"
base_url = "https://api.anthropic.com"
api_key_ref = "literal:sk-ant-should-not-be-here"
credential_source = "forwarded"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("must parse");
        let err = validate_provider_credential_sources(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mixed-up"), "msg: {msg}");
        assert!(msg.contains("api_key_ref"), "msg: {msg}");
    }

    #[test]
    fn rejects_own_with_an_empty_api_key_ref() {
        let toml_text = r#"
[providers.no-key]
kind = "anthropic-api"
"#;
        let cfg: Config =
            toml::from_str(toml_text).expect("must parse (api_key_ref defaults empty)");
        let err = validate_provider_credential_sources(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no-key"), "msg: {msg}");
        assert!(msg.contains("own"), "msg: {msg}");
    }

    /// Pin: a rejected forwarded `base_url` carrying embedded userinfo must not
    /// echo the credential, the host, the path, or the full URL. Mirrors
    /// `base_url_validation_tests::cleartext_rejection_does_not_echo_userinfo_or_host`,
    /// which pins the same property for the sibling scheme validator.
    ///
    /// Asserts BOTH directions: nothing operator-supplied survives, AND the
    /// diagnostic still names the provider and the required host -- so the test
    /// cannot pass by the message degrading into something useless.
    #[test]
    fn forwarded_host_rejection_does_not_echo_userinfo_or_base_url() {
        let toml_text = r#"
[providers.sneaky]
kind = "anthropic-api"
base_url = "https://user:sk-live-LEAKED@internal.example/v1?token=sk-query-LEAKED"
credential_source = "forwarded"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("must parse");
        let err = validate_provider_credential_sources(&cfg).unwrap_err();
        let msg = err.to_string();

        for secret in [
            "sk-live-LEAKED",
            "sk-query-LEAKED",
            "internal.example",
            "/v1",
            "user:",
            "https://user",
        ] {
            assert!(
                !msg.contains(secret),
                "operator-supplied `{secret}` must not surface; got: {msg}"
            );
        }

        assert!(
            msg.contains("sneaky"),
            "the diagnostic must still name the provider; got: {msg}"
        );
        assert!(
            msg.contains("api.anthropic.com"),
            "the diagnostic must still name the required host; got: {msg}"
        );
        assert!(
            msg.contains("withheld"),
            "the diagnostic must say the configured value is withheld; got: {msg}"
        );
    }

    /// Structural tripwire over the WHOLE of `validate.rs`: no `Error::Config`
    /// message may interpolate the operator-supplied `base_url`. That value can
    /// carry a credential in userinfo, a path, or a query, and every validator
    /// error string reaches `/status/doctor` as a serialized `Finding.detail`.
    ///
    /// This pins all 43 `Error::Config(format!(..))` sites at once rather than
    /// only the forwarded-host one, and needs no production/test slicing because
    /// `validate.rs` keeps its tests in this sibling `#[path]` file.
    ///
    /// The needle is assembled from fragments so this test's own source line
    /// cannot satisfy the scan it performs.
    #[test]
    fn no_validator_message_interpolates_the_raw_base_url() {
        let src = include_str!("validate.rs");
        assert!(
            !src.contains(concat!("{", "base_url")),
            "a validator message interpolates the raw base_url; that string may carry a \
             credential and every validator error reaches /status/doctor as Finding.detail. \
             Name the provider and the violated invariant instead, and withhold the value"
        );
    }
}

#[cfg(test)]
mod collect_config_validation_tests {
    use super::collect_config_validation;
    use crate::config::Config;

    /// Alias pointing at a nickname that is neither a `[models]` entry nor
    /// another alias key -- trips `validate_alias_chain_targets`.
    fn unknown_alias_target_config() -> Config {
        toml::from_str("[aliases]\nfast = \"ghost\"\n").expect("must parse")
    }

    /// The reserved `[retry.classes.feature-unsupported]` override -- trips
    /// `validate_class_policy`.
    fn reserved_class_override_config() -> Config {
        toml::from_str("[retry.classes.feature-unsupported]\nfallback = false\n")
            .expect("must parse")
    }

    #[test]
    fn collects_the_unknown_alias_target_error() {
        let validation = collect_config_validation(&unknown_alias_target_config());
        assert_eq!(
            validation.errors.len(),
            1,
            "exactly one validator should fire: {:?}",
            validation.errors
        );
        assert!(
            validation.errors[0].contains("ghost"),
            "error should name the unknown target: {}",
            validation.errors[0]
        );
    }

    /// A capability override cell carrying contradictory verdicts (a
    /// provider legacy list routes a capability away while a
    /// `force_supported` entry marks it supported) -- trips
    /// `validate_capability_overrides`.
    fn contradictory_capability_override_config() -> Config {
        toml::from_str(
            "[providers.p]\n\
             kind = \"openai-compat\"\n\
             base_url = \"https://x\"\n\
             api_key_ref = \"literal:k\"\n\
             unsupported_features = [\"web_search\"]\n\
             [capability.overrides.p]\n\
             force_supported = [\"web_search\"]\n",
        )
        .expect("must parse")
    }

    #[test]
    fn collects_the_reserved_class_override_error() {
        let validation = collect_config_validation(&reserved_class_override_config());
        assert_eq!(
            validation.errors.len(),
            1,
            "exactly one validator should fire: {:?}",
            validation.errors
        );
        assert!(
            validation.errors[0].contains("feature-unsupported")
                && validation.errors[0].contains("reserved"),
            "error should flag the reserved class: {}",
            validation.errors[0]
        );
    }

    #[test]
    fn collects_the_capability_override_conflict_error() {
        let validation = collect_config_validation(&contradictory_capability_override_config());
        assert_eq!(
            validation.errors.len(),
            1,
            "exactly one validator should fire: {:?}",
            validation.errors
        );
        assert!(
            validation.errors[0].contains("web_search")
                && validation.errors[0].contains("force-supported"),
            "error should name the conflicting cell: {}",
            validation.errors[0]
        );
    }

    #[test]
    fn a_clean_config_produces_no_errors() {
        let validation = collect_config_validation(&Config::default());
        assert!(
            validation.errors.is_empty(),
            "default config must pass the whole suite: {:?}",
            validation.errors
        );
    }

    fn parse(toml_text: &str) -> Config {
        toml::from_str(toml_text).expect("fixture must parse")
    }

    fn has_finite_error(validation: &super::ConfigValidation) -> bool {
        validation.errors.iter().any(|e| e.contains("finite"))
    }

    fn has_base_url_error(validation: &super::ConfigValidation) -> bool {
        validation
            .errors
            .iter()
            .any(|e| e.contains("base_url") && e.contains("kind default"))
    }

    #[test]
    fn rejects_non_finite_backoff_multiplier_from_literal_toml() {
        // The `inf` float literal survives the real TOML parse as a
        // non-finite f64, the one path a constructed `Config` value
        // cannot exercise -- a hand-edited config is how this reaches
        // duration math unchecked.
        let config = parse("[retry]\nbackoff_multiplier = inf\n");
        assert!(
            config.retry.backoff_multiplier.is_infinite(),
            "sanity: parses to inf"
        );
        let validation = collect_config_validation(&config);
        assert!(
            has_finite_error(&validation),
            "inf backoff_multiplier must be rejected: {:?}",
            validation.errors
        );
    }

    #[test]
    fn rejects_negative_backoff_multiplier() {
        let validation = collect_config_validation(&parse("[retry]\nbackoff_multiplier = -1.0\n"));
        assert!(
            !validation.errors.is_empty(),
            "negative backoff_multiplier must be rejected: {:?}",
            validation.errors
        );
    }

    #[test]
    fn rejects_zero_backoff_multiplier() {
        let validation = collect_config_validation(&parse("[retry]\nbackoff_multiplier = 0.0\n"));
        assert!(
            !validation.errors.is_empty(),
            "zero backoff_multiplier must be rejected: {:?}",
            validation.errors
        );
    }

    #[test]
    fn rejects_non_finite_registry_pricing() {
        let validation = collect_config_validation(&parse(
            "[registry.\"gpt-4\".pricing]\ninput_per_mtok = nan\n",
        ));
        assert!(
            has_finite_error(&validation),
            "nan registry pricing must be rejected: {:?}",
            validation.errors
        );
    }

    #[test]
    fn rejects_non_finite_cache_pricing_wm() {
        let validation = collect_config_validation(&parse("[cache_pricing.\"m\"]\nwm = inf\n"));
        assert!(
            has_finite_error(&validation),
            "inf cache_pricing.wm must be rejected: {:?}",
            validation.errors
        );
    }

    #[test]
    fn rejects_non_finite_cache_pricing_rm() {
        let validation = collect_config_validation(&parse("[cache_pricing.\"m\"]\nrm = inf\n"));
        assert!(
            has_finite_error(&validation),
            "inf cache_pricing.rm must be rejected: {:?}",
            validation.errors
        );
    }

    #[test]
    fn rejects_non_finite_cache_pricing_storage_rent() {
        let validation =
            collect_config_validation(&parse("[cache_pricing.\"m\"]\nstorage_rent = nan\n"));
        assert!(
            has_finite_error(&validation),
            "nan cache_pricing.storage_rent must be rejected: {:?}",
            validation.errors
        );
    }

    #[test]
    fn rejects_empty_base_url_openai_compat() {
        let validation = collect_config_validation(&parse(
            "[providers.p]\nkind = \"openai-compat\"\nbase_url = \"\"\napi_key_ref = \"literal:k\"\n",
        ));
        assert!(
            validation
                .errors
                .iter()
                .any(|e| e.contains("base_url") && e.contains("required")),
            "empty openai-compat base_url must be rejected: {:?}",
            validation.errors
        );
    }

    #[test]
    fn rejects_empty_base_url_anthropic_api() {
        let validation = collect_config_validation(&parse(
            "[providers.p]\nkind = \"anthropic-api\"\nbase_url = \"\"\napi_key_ref = \"literal:k\"\n",
        ));
        assert!(
            has_base_url_error(&validation),
            "empty anthropic-api base_url must be rejected: {:?}",
            validation.errors
        );
    }

    #[cfg(feature = "gemini")]
    #[test]
    fn rejects_empty_base_url_gemini() {
        let validation = collect_config_validation(&parse(
            "[providers.p]\nkind = \"gemini\"\nbase_url = \"\"\napi_key_ref = \"literal:k\"\n",
        ));
        assert!(
            has_base_url_error(&validation),
            "empty gemini base_url must be rejected: {:?}",
            validation.errors
        );
    }

    #[cfg(feature = "openai-responses")]
    #[test]
    fn rejects_present_but_empty_base_url_openai_responses() {
        let validation = collect_config_validation(&parse(
            "[providers.p]\nkind = \"openai-responses\"\nauth_kind = \"api-key\"\nbase_url = \"\"\napi_key_ref = \"literal:k\"\n",
        ));
        assert!(
            has_base_url_error(&validation),
            "present-but-empty openai-responses base_url must be rejected: {:?}",
            validation.errors
        );
    }

    #[cfg(feature = "openai-responses")]
    #[test]
    fn omitted_base_url_openai_responses_stays_valid() {
        let validation = collect_config_validation(&parse(
            "[providers.p]\nkind = \"openai-responses\"\nauth_kind = \"api-key\"\napi_key_ref = \"literal:k\"\n",
        ));
        assert!(
            !has_base_url_error(&validation),
            "omitted openai-responses base_url must stay valid: {:?}",
            validation.errors
        );
    }

    /// Drift tripwire: every schema leaf whose `type` includes `number`
    /// (float leaves; integers are out of scope) must be registered in
    /// `validate_float_fields`' covered set. A future f64/f32 leaf added
    /// to the config without registering it here fails this test.
    const COVERED_FLOAT_LEAVES: [&str; 11] = [
        "RetryPolicy.backoff_multiplier",
        "PricingConfig.input_per_mtok",
        "PricingConfig.output_per_mtok",
        "PricingConfig.cache_read_per_mtok",
        "PricingConfig.cache_write_5m_per_mtok",
        "PricingConfig.cache_write_1h_per_mtok",
        "CachePricingOverride.wm",
        "CachePricingOverride.rm",
        "CachePricingOverride.storage_rent",
        "CachePricingOverride.input_cost_per_token",
        "CachePricingOverride.output_cost_per_token",
    ];

    #[test]
    fn float_leaf_coverage_matches_schema() {
        use std::collections::BTreeSet;

        let committed = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../routectl.schema.json"
        ));
        let schema: serde_json::Value =
            serde_json::from_str(committed).expect("committed schema parses");

        fn type_includes_number(field: &serde_json::Value) -> bool {
            match field.get("type") {
                Some(serde_json::Value::String(s)) => s == "number",
                Some(serde_json::Value::Array(items)) => {
                    items.iter().any(|v| v.as_str() == Some("number"))
                }
                _ => false,
            }
        }

        let defs = schema
            .get("$defs")
            .and_then(serde_json::Value::as_object)
            .expect("schema carries $defs");

        let mut schema_number_leaves: BTreeSet<String> = BTreeSet::new();
        for (def_name, def) in defs {
            let Some(props) = def.get("properties").and_then(serde_json::Value::as_object) else {
                continue;
            };
            for (prop_name, prop) in props {
                if type_includes_number(prop) {
                    schema_number_leaves.insert(format!("{def_name}.{prop_name}"));
                }
            }
        }

        let covered: BTreeSet<String> =
            COVERED_FLOAT_LEAVES.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            schema_number_leaves,
            covered,
            "float-leaf coverage diverged from the schema: unregistered={:?}, stale={:?}",
            schema_number_leaves
                .difference(&covered)
                .collect::<Vec<_>>(),
            covered
                .difference(&schema_number_leaves)
                .collect::<Vec<_>>(),
        );
    }
}

#[cfg(all(test, feature = "bedrock"))]
mod validate_provider_bedrock_mantle_tests {
    //! Config-BOUNDARY tests (parse via `toml::from_str`) for the Bedrock
    //! mantle lane's field-coherence validator. The PRESENCE of a
    //! `bedrock_mantle` sub-table selects the lane; every other
    //! credential/endpoint knob must be left at its neutral default, since
    //! the lane derives the endpoint from `region` and the credential from
    //! `creds`.

    use super::{collect_config_validation, validate_provider_bedrock_mantle};
    use crate::config::Config;

    /// Minimal mantle lane on the default-chain credential shape: no
    /// api_key_ref, default auth_kind / credential_source / base_url. Must
    /// parse and pass the whole validation suite (not just the mantle
    /// validator -- the `own`-requires-a-key rule must exempt it).
    #[test]
    fn default_chain_mantle_lane_parses_and_validates() {
        let toml_text = r#"
[providers.mantle]
kind = "anthropic-api"
bedrock_mantle = { region = "us-east-1", creds = { kind = "default-chain" } }
"#;
        let cfg: Config = toml::from_str(toml_text).expect("default-chain mantle must parse");
        validate_provider_bedrock_mantle(&cfg).expect("clean default-chain mantle validates");
        assert!(
            collect_config_validation(&cfg).errors.is_empty(),
            "clean mantle lane must pass the whole suite: {:?}",
            collect_config_validation(&cfg).errors
        );
    }

    /// Mantle lane on the bearer-key credential shape.
    #[test]
    fn bearer_key_mantle_lane_parses_and_validates() {
        let toml_text = r#"
[providers.mantle]
kind = "anthropic-api"
bedrock_mantle = { region = "eu-west-1", creds = { kind = "bearer-key", key_ref = "env://AWS_BEARER_TOKEN_BEDROCK" } }
"#;
        let cfg: Config = toml::from_str(toml_text).expect("bearer-key mantle must parse");
        validate_provider_bedrock_mantle(&cfg).expect("clean bearer-key mantle validates");
        assert!(
            collect_config_validation(&cfg).errors.is_empty(),
            "clean bearer-key mantle must pass the whole suite: {:?}",
            collect_config_validation(&cfg).errors
        );
    }

    #[test]
    fn rejects_mantle_with_oauth_bearer_auth_kind() {
        let toml_text = r#"
[providers.mantle]
kind = "anthropic-api"
auth_kind = "oauth-bearer"
bedrock_mantle = { region = "us-east-1", creds = { kind = "default-chain" } }
"#;
        let cfg: Config = toml::from_str(toml_text).expect("must parse");
        let err = validate_provider_bedrock_mantle(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mantle"), "msg: {msg}");
        assert!(msg.contains("oauth-bearer"), "msg: {msg}");
    }

    #[test]
    fn rejects_mantle_with_forwarded_credential_source() {
        let toml_text = r#"
[providers.mantle]
kind = "anthropic-api"
credential_source = "forwarded"
bedrock_mantle = { region = "us-east-1", creds = { kind = "default-chain" } }
"#;
        let cfg: Config = toml::from_str(toml_text).expect("must parse");
        let err = validate_provider_bedrock_mantle(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mantle"), "msg: {msg}");
        assert!(msg.contains("credential_source"), "msg: {msg}");
    }

    #[test]
    fn rejects_mantle_with_nonempty_api_key_ref() {
        let toml_text = r#"
[providers.mantle]
kind = "anthropic-api"
api_key_ref = "literal:should-not-be-here"
bedrock_mantle = { region = "us-east-1", creds = { kind = "default-chain" } }
"#;
        let cfg: Config = toml::from_str(toml_text).expect("must parse");
        let err = validate_provider_bedrock_mantle(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mantle"), "msg: {msg}");
        assert!(msg.contains("api_key_ref"), "msg: {msg}");
    }

    #[test]
    fn rejects_mantle_with_nondefault_base_url() {
        let toml_text = r#"
[providers.mantle]
kind = "anthropic-api"
base_url = "https://bedrock-runtime.us-east-1.amazonaws.com"
bedrock_mantle = { region = "us-east-1", creds = { kind = "default-chain" } }
"#;
        let cfg: Config = toml::from_str(toml_text).expect("must parse");
        let err = validate_provider_bedrock_mantle(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mantle"), "msg: {msg}");
        assert!(msg.contains("base_url"), "msg: {msg}");
    }

    #[test]
    fn rejects_mantle_with_empty_region() {
        let toml_text = r#"
[providers.mantle]
kind = "anthropic-api"
bedrock_mantle = { region = "   ", creds = { kind = "default-chain" } }
"#;
        let cfg: Config = toml::from_str(toml_text).expect("must parse");
        let err = validate_provider_bedrock_mantle(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mantle"), "msg: {msg}");
        assert!(msg.contains("region"), "msg: {msg}");
    }
}

#[cfg(all(test, feature = "bedrock"))]
mod validate_provider_openai_mantle_tests {
    //! Config-BOUNDARY tests (parse via `toml::from_str`) for the Bedrock
    //! mantle lane on the two OpenAI-shape providers. Mirrors the
    //! `anthropic-api` suite: the PRESENCE of a `bedrock_mantle` sub-table
    //! selects the lane, so every other credential/endpoint knob must be
    //! neutral, and the legacy `auth_kind = "bedrock-mantle"`-alone surface
    //! (Responses only) is closed with a hard error.
    //!
    //! The `openai-compat` cases run under `bedrock` alone (the compat
    //! branch is independent of the `openai-responses` feature); the
    //! `openai-responses` cases are additionally gated on that feature.

    use super::{collect_config_validation, validate_provider_openai_mantle};
    use crate::config::Config;

    // --- openai-compat lane ---

    #[test]
    fn compat_default_chain_mantle_parses_and_validates() {
        let toml_text = r#"
[providers.mantle]
kind = "openai-compat"
api_key_ref = ""
bedrock_mantle = { region = "us-east-1", creds = { kind = "default-chain" } }
"#;
        let cfg: Config = toml::from_str(toml_text).expect("compat default-chain mantle parses");
        validate_provider_openai_mantle(&cfg).expect("clean compat mantle validates");
        assert!(
            collect_config_validation(&cfg).errors.is_empty(),
            "clean compat mantle must pass the whole suite: {:?}",
            collect_config_validation(&cfg).errors
        );
    }

    #[test]
    fn compat_bearer_key_mantle_parses_and_validates() {
        let toml_text = r#"
[providers.mantle]
kind = "openai-compat"
api_key_ref = ""
bedrock_mantle = { region = "eu-west-1", creds = { kind = "bearer-key", key_ref = "env://AWS_BEARER_TOKEN_BEDROCK" } }
"#;
        let cfg: Config = toml::from_str(toml_text).expect("compat bearer-key mantle parses");
        validate_provider_openai_mantle(&cfg).expect("clean compat bearer-key mantle validates");
        assert!(
            collect_config_validation(&cfg).errors.is_empty(),
            "clean compat bearer-key mantle must pass the whole suite: {:?}",
            collect_config_validation(&cfg).errors
        );
    }

    #[test]
    fn compat_rejects_mantle_with_nonempty_api_key_ref() {
        let toml_text = r#"
[providers.mantle]
kind = "openai-compat"
api_key_ref = "literal:should-not-be-here"
bedrock_mantle = { region = "us-east-1", creds = { kind = "default-chain" } }
"#;
        let cfg: Config = toml::from_str(toml_text).expect("must parse");
        let err = validate_provider_openai_mantle(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("api_key_ref"), "msg: {msg}");
    }

    #[test]
    fn compat_rejects_mantle_with_nonempty_base_url() {
        let toml_text = r#"
[providers.mantle]
kind = "openai-compat"
api_key_ref = ""
base_url = "https://example.invalid/v1"
bedrock_mantle = { region = "us-east-1", creds = { kind = "default-chain" } }
"#;
        let cfg: Config = toml::from_str(toml_text).expect("must parse");
        let err = validate_provider_openai_mantle(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("base_url"), "msg: {msg}");
    }

    #[test]
    fn compat_rejects_mantle_with_empty_region() {
        let toml_text = r#"
[providers.mantle]
kind = "openai-compat"
api_key_ref = ""
bedrock_mantle = { region = "   ", creds = { kind = "default-chain" } }
"#;
        let cfg: Config = toml::from_str(toml_text).expect("must parse");
        let err = validate_provider_openai_mantle(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("region"), "msg: {msg}");
    }

    #[test]
    fn non_mantle_compat_requires_base_url() {
        // With `#[serde(default)]` on base_url a non-mantle compat entry may
        // now omit it; the suite must still reject the empty value.
        let toml_text = r#"
[providers.plain]
kind = "openai-compat"
api_key_ref = "env://KEY"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("must parse (base_url defaults empty)");
        let errors = collect_config_validation(&cfg).errors;
        assert!(
            errors.iter().any(|e| e.contains("base_url")),
            "non-mantle compat with no base_url must be rejected: {errors:?}"
        );
    }

    // --- openai-responses lane ---

    #[cfg(feature = "openai-responses")]
    #[test]
    fn responses_mantle_default_auth_kind_parses_and_validates() {
        let toml_text = r#"
[providers.mantle]
kind = "openai-responses"
api_key_ref = ""
bedrock_mantle = { region = "us-east-1", creds = { kind = "default-chain" } }
"#;
        let cfg: Config = toml::from_str(toml_text).expect("responses mantle parses");
        validate_provider_openai_mantle(&cfg).expect("clean responses mantle validates");
        assert!(
            collect_config_validation(&cfg).errors.is_empty(),
            "clean responses mantle must pass the whole suite: {:?}",
            collect_config_validation(&cfg).errors
        );
    }

    #[cfg(feature = "openai-responses")]
    #[test]
    fn responses_mantle_bearer_key_parses_and_validates() {
        let toml_text = r#"
[providers.mantle]
kind = "openai-responses"
api_key_ref = ""
bedrock_mantle = { region = "eu-west-1", creds = { kind = "bearer-key", key_ref = "env://AWS_BEARER_TOKEN_BEDROCK" } }
"#;
        let cfg: Config = toml::from_str(toml_text).expect("responses bearer-key mantle parses");
        validate_provider_openai_mantle(&cfg).expect("clean responses bearer-key mantle validates");
        assert!(
            collect_config_validation(&cfg).errors.is_empty(),
            "clean responses bearer-key mantle must pass the whole suite: {:?}",
            collect_config_validation(&cfg).errors
        );
    }

    #[cfg(feature = "openai-responses")]
    #[test]
    fn responses_mantle_redundant_auth_kind_parses_and_validates() {
        // Stating auth_kind = "bedrock-mantle" alongside the block is
        // redundant but accepted (the factory sets the runtime marker).
        let toml_text = r#"
[providers.mantle]
kind = "openai-responses"
api_key_ref = ""
auth_kind = "bedrock-mantle"
bedrock_mantle = { region = "us-east-1", creds = { kind = "default-chain" } }
"#;
        let cfg: Config = toml::from_str(toml_text).expect("responses mantle parses");
        validate_provider_openai_mantle(&cfg).expect("redundant auth_kind validates");
        assert!(
            collect_config_validation(&cfg).errors.is_empty(),
            "redundant auth_kind must pass the whole suite: {:?}",
            collect_config_validation(&cfg).errors
        );
    }

    #[cfg(feature = "openai-responses")]
    #[test]
    fn responses_legacy_auth_kind_without_block_is_rejected() {
        // The must-fix: bedrock-mantle auth_kind ALONE (no block) is the
        // silent us-east-1 misroute. Hard error naming the block form.
        let toml_text = r#"
[providers.mantle]
kind = "openai-responses"
api_key_ref = "env://AWS_BEARER_TOKEN_BEDROCK"
auth_kind = "bedrock-mantle"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("must parse (enum stays parseable)");
        let err = validate_provider_openai_mantle(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("bedrock_mantle"),
            "must name the block form: {msg}"
        );
        assert!(
            msg.contains("region and creds"),
            "must name the migration path: {msg}"
        );
    }

    #[cfg(feature = "openai-responses")]
    #[test]
    fn responses_legacy_auth_kind_with_explicit_base_url_is_rejected() {
        // The bearer-only lane form (explicit base_url) is closed too --
        // it cannot meet the SigV4 posture. Still errors, regardless of
        // base_url.
        let toml_text = r#"
[providers.mantle]
kind = "openai-responses"
api_key_ref = "env://AWS_BEARER_TOKEN_BEDROCK"
auth_kind = "bedrock-mantle"
base_url = "https://bedrock-mantle.us-east-1.api.aws/openai/v1"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("must parse");
        let err = validate_provider_openai_mantle(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("bedrock_mantle"),
            "must name the block form: {msg}"
        );
    }

    #[cfg(feature = "openai-responses")]
    #[test]
    fn responses_rejects_mantle_with_account_id_ref() {
        let toml_text = r#"
[providers.mantle]
kind = "openai-responses"
api_key_ref = ""
account_id_ref = "acct-123"
bedrock_mantle = { region = "us-east-1", creds = { kind = "default-chain" } }
"#;
        let cfg: Config = toml::from_str(toml_text).expect("must parse");
        let err = validate_provider_openai_mantle(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("account_id_ref"), "msg: {msg}");
    }

    #[cfg(feature = "openai-responses")]
    #[test]
    fn responses_rejects_mantle_with_nonempty_api_key_ref() {
        let toml_text = r#"
[providers.mantle]
kind = "openai-responses"
api_key_ref = "literal:should-not-be-here"
bedrock_mantle = { region = "us-east-1", creds = { kind = "default-chain" } }
"#;
        let cfg: Config = toml::from_str(toml_text).expect("must parse");
        let err = validate_provider_openai_mantle(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("api_key_ref"), "msg: {msg}");
    }

    #[cfg(feature = "openai-responses")]
    #[test]
    fn responses_rejects_mantle_with_nonempty_base_url() {
        let toml_text = r#"
[providers.mantle]
kind = "openai-responses"
api_key_ref = ""
base_url = "https://example.invalid/v1"
bedrock_mantle = { region = "us-east-1", creds = { kind = "default-chain" } }
"#;
        let cfg: Config = toml::from_str(toml_text).expect("must parse");
        let err = validate_provider_openai_mantle(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("base_url"), "msg: {msg}");
    }

    #[cfg(feature = "openai-responses")]
    #[test]
    fn responses_rejects_mantle_with_empty_region() {
        let toml_text = r#"
[providers.mantle]
kind = "openai-responses"
api_key_ref = ""
bedrock_mantle = { region = "   ", creds = { kind = "default-chain" } }
"#;
        let cfg: Config = toml::from_str(toml_text).expect("must parse");
        let err = validate_provider_openai_mantle(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("region"), "msg: {msg}");
    }

    #[cfg(feature = "openai-responses")]
    #[test]
    fn responses_rejects_mantle_with_store_payload_extra() {
        // The Responses `store` flag is forced off on the mantle lane; a
        // `store` key in payload_extras must be rejected at config load.
        let toml_text = r#"
[providers.mantle]
kind = "openai-responses"
api_key_ref = ""
payload_extras = { store = true }
bedrock_mantle = { region = "us-east-1", creds = { kind = "default-chain" } }
"#;
        let cfg: Config = toml::from_str(toml_text).expect("must parse");
        let err = validate_provider_openai_mantle(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("store"), "msg: {msg}");
    }
}

#[cfg(all(test, not(feature = "bedrock")))]
mod bedrock_mantle_feature_off_tests {
    //! With the `bedrock` feature off, the `bedrock_mantle` field does not
    //! exist on the `AnthropicApi` variant. A config carrying the key must
    //! fail to parse via `deny_unknown_fields` -- a clean rejection, never
    //! a silent drop.

    use crate::config::Config;

    #[test]
    fn bedrock_mantle_key_is_rejected_when_feature_off() {
        let toml_text = r#"
[providers.mantle]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
bedrock_mantle = { region = "us-east-1", creds = { kind = "default-chain" } }
"#;
        let err = toml::from_str::<Config>(toml_text)
            .expect_err("bedrock_mantle must be rejected as an unknown field with bedrock off");
        assert!(
            err.to_string().contains("bedrock_mantle"),
            "the unknown-field error should name the key: {err}"
        );
    }

    #[test]
    fn bedrock_mantle_key_on_openai_compat_rejected_when_feature_off() {
        let toml_text = r#"
[providers.mantle]
kind = "openai-compat"
base_url = "https://example.invalid/v1"
api_key_ref = "env://KEY"
bedrock_mantle = { region = "us-east-1", creds = { kind = "default-chain" } }
"#;
        let err = toml::from_str::<Config>(toml_text)
            .expect_err("bedrock_mantle must be rejected as an unknown field with bedrock off");
        assert!(
            err.to_string().contains("bedrock_mantle"),
            "the unknown-field error should name the key: {err}"
        );
    }

    #[cfg(feature = "openai-responses")]
    #[test]
    fn bedrock_mantle_key_on_openai_responses_rejected_when_feature_off() {
        let toml_text = r#"
[providers.mantle]
kind = "openai-responses"
api_key_ref = "env://KEY"
bedrock_mantle = { region = "us-east-1", creds = { kind = "default-chain" } }
"#;
        let err = toml::from_str::<Config>(toml_text)
            .expect_err("bedrock_mantle must be rejected as an unknown field with bedrock off");
        assert!(
            err.to_string().contains("bedrock_mantle"),
            "the unknown-field error should name the key: {err}"
        );
    }
}

#[cfg(all(test, feature = "openai-responses"))]
mod codex_version_validation_tests {
    //! Tests for `validate_codex_version` (conflict + syntax rejects) and
    //! `codex_identity_warnings` (divergent identity-header override).

    use super::{collect_config_validation, resolved_codex_version, validate_codex_version};
    use crate::codex_identity_warnings;
    use crate::config::Config;

    fn parse(toml_text: &str) -> Config {
        toml::from_str(toml_text).expect("fixture must parse")
    }

    #[test]
    fn absent_knob_resolves_to_none_and_passes() {
        let config = parse(
            "[providers.a]\nkind = \"openai-responses\"\napi_key_ref = \"oauth://codex\"\n\
             auth_kind = \"chatgpt-oauth\"\naccount_id_ref = \"env://ACCT\"\n",
        );
        assert_eq!(resolved_codex_version(&config), None);
        assert!(validate_codex_version(&config).is_ok());
    }

    #[test]
    fn single_configured_value_resolves_and_passes() {
        let config = parse(
            "[providers.a]\nkind = \"openai-responses\"\napi_key_ref = \"oauth://codex\"\n\
             auth_kind = \"chatgpt-oauth\"\naccount_id_ref = \"env://ACCT\"\n\
             codex_version = \"0.200.0\"\n",
        );
        assert_eq!(resolved_codex_version(&config).as_deref(), Some("0.200.0"));
        assert!(validate_codex_version(&config).is_ok());
    }

    #[test]
    fn matching_values_across_providers_pass() {
        let config = parse(
            "[providers.a]\nkind = \"openai-responses\"\napi_key_ref = \"oauth://codex\"\n\
             auth_kind = \"chatgpt-oauth\"\naccount_id_ref = \"env://A\"\ncodex_version = \"0.200.0\"\n\
             [providers.b]\nkind = \"openai-responses\"\napi_key_ref = \"oauth://codex2\"\n\
             auth_kind = \"chatgpt-oauth\"\naccount_id_ref = \"env://B\"\ncodex_version = \"0.200.0\"\n",
        );
        assert!(validate_codex_version(&config).is_ok());
    }

    #[test]
    fn divergent_values_error_naming_both_providers() {
        let config = parse(
            "[providers.alpha]\nkind = \"openai-responses\"\napi_key_ref = \"oauth://codex\"\n\
             auth_kind = \"chatgpt-oauth\"\naccount_id_ref = \"env://A\"\ncodex_version = \"0.200.0\"\n\
             [providers.beta]\nkind = \"openai-responses\"\napi_key_ref = \"oauth://codex2\"\n\
             auth_kind = \"chatgpt-oauth\"\naccount_id_ref = \"env://B\"\ncodex_version = \"0.201.0\"\n",
        );
        let err = validate_codex_version(&config).expect_err("divergent versions must error");
        let msg = err.to_string();
        assert!(
            msg.contains("alpha"),
            "error must name first provider: {msg}"
        );
        assert!(
            msg.contains("beta"),
            "error must name second provider: {msg}"
        );
        // Reaches the central suite too.
        assert!(
            collect_config_validation(&config)
                .errors
                .iter()
                .any(|e| e.contains("alpha") && e.contains("beta")),
            "collect_config_validation must surface the conflict"
        );
    }

    fn config_with_version(version: &str) -> Config {
        parse(&format!(
            "[providers.a]\nkind = \"openai-responses\"\napi_key_ref = \"oauth://codex\"\n\
             auth_kind = \"chatgpt-oauth\"\naccount_id_ref = \"env://A\"\ncodex_version = \"{version}\"\n"
        ))
    }

    #[test]
    fn empty_version_rejected() {
        let err = validate_codex_version(&config_with_version("")).expect_err("empty rejected");
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[test]
    fn too_long_version_rejected() {
        let long = "1".repeat(65);
        let err =
            validate_codex_version(&config_with_version(&long)).expect_err("over-long rejected");
        assert!(err.to_string().contains("maximum"), "{err}");
    }

    #[test]
    fn whitespace_version_rejected() {
        let err = validate_codex_version(&config_with_version("0.200 0"))
            .expect_err("whitespace rejected");
        assert!(err.to_string().contains("illegal byte"), "{err}");
    }

    #[test]
    fn non_ascii_version_rejected() {
        // A non-ASCII byte in the version is header-illegal and not the
        // fingerprint the operator asked for.
        let err = validate_codex_version(&config_with_version("0.200.\u{00e9}"))
            .expect_err("non-ascii rejected");
        assert!(err.to_string().contains("illegal byte"), "{err}");
    }

    #[test]
    fn divergent_version_header_override_warns() {
        // A chatgpt-oauth provider overriding `version` in header_extras
        // with a value diverging from the derived identity warns (but the
        // override still wins -- the warning is advisory).
        let config = parse(
            "[providers.a]\nkind = \"openai-responses\"\napi_key_ref = \"oauth://codex\"\n\
             auth_kind = \"chatgpt-oauth\"\naccount_id_ref = \"env://A\"\ncodex_version = \"0.200.0\"\n\
             [providers.a.header_extras]\nversion = \"0.999.0\"\n",
        );
        let warnings = codex_identity_warnings(&config);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("version") && w.contains("0.999.0")),
            "divergent version override must warn: {warnings:?}"
        );
    }

    #[test]
    fn matching_version_header_override_does_not_warn() {
        let config = parse(
            "[providers.a]\nkind = \"openai-responses\"\napi_key_ref = \"oauth://codex\"\n\
             auth_kind = \"chatgpt-oauth\"\naccount_id_ref = \"env://A\"\ncodex_version = \"0.200.0\"\n\
             [providers.a.header_extras]\nversion = \"0.200.0\"\n",
        );
        assert!(
            codex_identity_warnings(&config).is_empty(),
            "a matching override must not warn"
        );
    }

    #[test]
    fn header_override_on_api_key_surface_does_not_warn() {
        // The version override warning is scoped to the chatgpt-oauth
        // surface -- an api-key responses provider emits no codex
        // fingerprint, so a `version` header there is not an identity
        // override.
        let config = parse(
            "[providers.a]\nkind = \"openai-responses\"\napi_key_ref = \"env://KEY\"\n\
             auth_kind = \"api-key\"\n[providers.a.header_extras]\nversion = \"0.999.0\"\n",
        );
        assert!(
            codex_identity_warnings(&config).is_empty(),
            "api-key surface must not warn on a version header"
        );
    }

    #[test]
    fn absent_override_does_not_warn() {
        let config = parse(
            "[providers.a]\nkind = \"openai-responses\"\napi_key_ref = \"oauth://codex\"\n\
             auth_kind = \"chatgpt-oauth\"\naccount_id_ref = \"env://A\"\ncodex_version = \"0.200.0\"\n",
        );
        assert!(codex_identity_warnings(&config).is_empty());
    }
}

#[cfg(test)]
#[cfg(feature = "bedrock")]
mod bedrock_invoke_model_family_tests {
    //! Config-BOUNDARY tests (parse via `toml::from_str`) for the gate
    //! that keeps a non-Anthropic model off the Bedrock InvokeModel lane.
    //! The lane assembles and parses the Anthropic wire shape, so such an
    //! entry cannot work; the Converse lane is vendor-neutral and never
    //! rejected here.

    use super::{collect_config_validation, validate_bedrock_invoke_model_family};
    use crate::config::Config;

    /// A Bedrock provider plus one model, with the provider's `api_shape`
    /// line and the model's `upstream` supplied by the caller. An absent
    /// `shape_line` exercises the defaulted (invoke) shape.
    fn config_with(shape_line: &str, upstream: &str) -> Config {
        let toml_text = format!(
            "[providers.aws]\n\
             kind = \"bedrock\"\n\
             region = \"us-west-2\"\n\
             creds = {{ kind = \"default-chain\" }}\n\
             {shape_line}\n\
             [models.seat]\n\
             provider = \"aws\"\n\
             upstream = \"{upstream}\"\n"
        );
        toml::from_str(&toml_text).expect("config must parse")
    }

    #[test]
    fn rejects_a_non_anthropic_model_on_the_defaulted_invoke_shape() {
        // Arrange
        let config = config_with("", "meta.llama3-70b-instruct-v1:0");

        // Act
        let result = validate_bedrock_invoke_model_family(&config);

        // Assert
        let message = result.expect_err("non-Anthropic invoke seat must be rejected");
        let message = message.to_string();
        assert!(
            message.contains("meta.llama3-70b-instruct-v1:0"),
            "message must name the model id: {message}"
        );
        assert!(
            message.contains("invoke") && message.contains("converse"),
            "message must name both wire shapes: {message}"
        );
    }

    #[test]
    fn rejects_a_non_anthropic_model_on_the_explicit_invoke_shape() {
        let config = config_with("api_shape = \"invoke\"", "mistral.mistral-large-2402-v1:0");

        let result = validate_bedrock_invoke_model_family(&config);

        let message = result
            .expect_err("explicit invoke shape must be gated too")
            .to_string();
        assert!(
            message.contains("mistral.mistral-large-2402-v1:0"),
            "message must name the model id: {message}"
        );
    }

    #[test]
    fn accepts_a_non_anthropic_model_on_the_converse_shape() {
        let config = config_with("api_shape = \"converse\"", "meta.llama3-70b-instruct-v1:0");

        let result = validate_bedrock_invoke_model_family(&config);

        assert!(
            result.is_ok(),
            "the vendor-neutral lane is unaffected by model family: {result:?}"
        );
    }

    #[test]
    fn accepts_region_prefixed_anthropic_models_on_the_invoke_shape() {
        for upstream in [
            "anthropic.claude-haiku-4-5-20251001-v1:0",
            "us.anthropic.claude-haiku-4-5-20251001-v1:0",
            "eu.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "apac.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "global.anthropic.claude-opus-4-7",
            "global.anthropic.claude-opus-4-7[1m]",
        ] {
            let config = config_with("", upstream);

            let result = validate_bedrock_invoke_model_family(&config);

            assert!(result.is_ok(), "{upstream} must pass: {result:?}");
        }
    }

    #[test]
    fn accepts_an_inference_profile_arn_on_the_invoke_shape() {
        // An inference profile is opaque by resource form, so the family
        // is unprovable. The gate is an ergonomics guard, not a proof
        // obligation -- rejecting would break working Claude-on-ARN
        // deployments.
        let config = config_with(
            "",
            "arn:aws:bedrock:us-east-1:123456789012:inference-profile/my-profile",
        );

        let result = validate_bedrock_invoke_model_family(&config);

        assert!(result.is_ok(), "an ARN must pass: {result:?}");
    }

    /// A foundation-model ARN embeds the plain model id, so the vendor IS
    /// provable and the gate must act on it. Without this, a
    /// non-Anthropic model reaches the Anthropic-shaped lane just by
    /// being written as an ARN -- the hole the gate exists to close.
    #[test]
    fn rejects_a_non_anthropic_foundation_model_arn_on_the_invoke_shape() {
        let config = config_with(
            "",
            "arn:aws:bedrock:us-east-1::foundation-model/meta.llama3-70b-instruct-v1:0",
        );

        let result = validate_bedrock_invoke_model_family(&config);

        let message = result.expect_err("a provable non-Anthropic ARN must be rejected");
        let rendered = message.to_string();
        assert!(
            rendered.contains("meta.llama3-70b-instruct-v1:0"),
            "{rendered}"
        );
    }

    #[test]
    fn accepts_an_anthropic_foundation_model_arn_on_the_invoke_shape() {
        let config = config_with(
            "",
            "arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-haiku-4-5",
        );

        let result = validate_bedrock_invoke_model_family(&config);

        assert!(
            result.is_ok(),
            "a provable Anthropic ARN must pass: {result:?}"
        );
    }

    #[test]
    fn ignores_a_model_routed_at_a_non_bedrock_provider() {
        let config: Config = toml::from_str(
            "[providers.oai]\n\
             kind = \"openai-compat\"\n\
             base_url = \"https://x\"\n\
             api_key_ref = \"literal:k\"\n\
             [models.seat]\n\
             provider = \"oai\"\n\
             upstream = \"meta.llama3-70b-instruct-v1:0\"\n",
        )
        .expect("config must parse");

        let result = validate_bedrock_invoke_model_family(&config);

        assert!(result.is_ok(), "non-Bedrock providers are out of scope");
    }

    #[test]
    fn collect_config_validation_reports_the_invoke_family_error() {
        // Proves the validator is wired into the collected suite every
        // config surface routes through, not just callable directly.
        let config = config_with("", "meta.llama3-70b-instruct-v1:0");

        let errors = collect_config_validation(&config).errors;

        assert!(
            errors
                .iter()
                .any(|e| e.contains("meta.llama3-70b-instruct-v1:0")),
            "the collected suite must surface the error: {errors:?}"
        );
    }
}
