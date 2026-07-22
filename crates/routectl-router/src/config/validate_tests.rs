use super::*;

mod config_version_tests {
    use super::{
        CURRENT_CONFIG_VERSION, Config, ConfigVersionError, preflight_config_version,
        validate_cache_pricing_retired,
    };

    #[test]
    fn shipped_example_config_parses_and_passes_preflight() {
        // The example config is shipped verbatim (embedded in `config
        // example`, copied to the config dir by operators). It must carry
        // the CURRENT schema version and parse as a typed Config, or the
        // documented copy-to-config-dir flow is dead on arrival -- pin it so
        // this class of break (a stale/absent `version` stamp) can't recur.
        let example = include_str!("../../../../examples/config.toml");

        assert_eq!(
            preflight_config_version(example),
            Ok(CURRENT_CONFIG_VERSION),
            "example config must preflight at the current schema version"
        );
        let config: Config = toml::from_str(example).expect("example config must parse as Config");
        assert_eq!(config.version, CURRENT_CONFIG_VERSION);
        validate_cache_pricing_retired(&config).expect("example must not carry retired tables");
    }

    #[test]
    fn absent_version_key_deserializes_to_one() {
        // Arrange / Act
        let config: Config = toml::from_str("[server]\nhost = \"127.0.0.1\"\n").expect("parse");

        // Assert
        assert_eq!(config.version, 1);
    }

    #[test]
    fn explicit_current_version_round_trips() {
        // Arrange / Act
        let config: Config =
            toml::from_str("version = 3\n[server]\nhost = \"127.0.0.1\"\n").expect("parse");

        // Assert
        assert_eq!(config.version, CURRENT_CONFIG_VERSION);
    }

    #[test]
    fn preflight_rejects_absent_version_as_too_old() {
        // An absent `version` key defaults to `1`, which predates the
        // current schema, so the loader must reject it rather than migrate.
        let err = preflight_config_version("[server]\nhost = \"x\"\n")
            .expect_err("absent version defaults below current and must be rejected");

        match err {
            ConfigVersionError::TooOld { found, supported } => {
                assert_eq!(found, 1);
                assert_eq!(supported, CURRENT_CONFIG_VERSION);
            }
            other => panic!("expected TooOld, got {other:?}"),
        }
        assert!(err.to_string().contains("config migrate"), "err: {err}");
    }

    #[test]
    fn preflight_does_not_mask_malformed_toml_as_too_old() {
        // Unparseable TOML must fall through so the typed deserialize can
        // report the real syntax error -- never a spurious `config migrate`
        // hint.
        assert_eq!(
            preflight_config_version("this is = = not valid toml"),
            Ok(1)
        );
    }

    #[test]
    fn preflight_does_not_mask_non_integer_version_as_too_old() {
        // A `version` that is present but the wrong type falls through to the
        // typed deserialize, which reports the precise type error.
        assert_eq!(
            preflight_config_version("version = \"two\"\n[server]\nhost = \"x\"\n"),
            Ok(1)
        );
    }

    #[test]
    fn preflight_rejects_version_older_than_current() {
        // A stale explicit `version` is rejected with the migrate pointer,
        // never silently upgraded on load.
        let err = preflight_config_version("version = 1\n[server]\nhost = \"x\"\n")
            .expect_err("version 1 predates current and must be rejected");

        match err {
            ConfigVersionError::TooOld { found, supported } => {
                assert_eq!(found, 1);
                assert_eq!(supported, CURRENT_CONFIG_VERSION);
            }
            other => panic!("expected TooOld, got {other:?}"),
        }
        assert!(err.to_string().contains("config migrate"), "err: {err}");
    }

    #[test]
    fn preflight_accepts_current_version() {
        assert_eq!(
            preflight_config_version("version = 3\n[server]\nhost = \"x\"\n"),
            Ok(3)
        );
    }

    #[test]
    fn preflight_rejects_version_newer_than_current() {
        // Act
        let err = preflight_config_version("version = 4\n[server]\nhost = \"x\"\n")
            .expect_err("version 4 must be rejected");

        // Assert
        match err {
            ConfigVersionError::TooNew(inner) => {
                assert_eq!(inner.found, 4);
                assert_eq!(inner.supported, CURRENT_CONFIG_VERSION);
            }
            other => panic!("expected TooNew, got {other:?}"),
        }
    }

    /// Preflight must catch a too-new version BEFORE the full deserialize
    /// runs, so a newer routectl's unknown fields never reach
    /// `deny_unknown_fields` and mask the version error behind a
    /// confusing "unknown field" message.
    #[test]
    fn preflight_rejects_newer_version_even_with_fields_this_build_does_not_know() {
        let raw = "version = 99\nsome_field_from_the_future = true\n[server]\nhost = \"x\"\n";

        let err = preflight_config_version(raw).expect_err("version 99 must be rejected");
        match err {
            ConfigVersionError::TooNew(inner) => assert_eq!(inner.found, 99),
            other => panic!("expected TooNew, got {other:?}"),
        }

        // The typed deserialize is never reached for this input in the
        // real load path; confirm it WOULD have failed with the confusing
        // unknown-field error preflight exists to avoid.
        let deny_unknown_err = toml::from_str::<Config>(raw).expect_err("must fail to parse");
        assert!(
            !deny_unknown_err.to_string().contains("newer"),
            "sanity: the raw deserialize error must NOT already read like a version message"
        );
    }

    #[test]
    fn validate_cache_pricing_retired_allows_nonempty_at_v1() {
        let mut config = Config {
            version: 1,
            ..Config::default()
        };
        config.cache_pricing.insert(
            "openai-compat:grok-*".to_string(),
            crate::catalog::CachePricingOverride::default(),
        );

        assert!(validate_cache_pricing_retired(&config).is_ok());
    }

    #[test]
    fn validate_cache_pricing_retired_allows_empty_at_current_version() {
        let config = Config {
            version: CURRENT_CONFIG_VERSION,
            ..Config::default()
        };

        assert!(validate_cache_pricing_retired(&config).is_ok());
    }

    #[test]
    fn validate_cache_pricing_retired_rejects_nonempty_at_current_version() {
        let mut config = Config {
            version: CURRENT_CONFIG_VERSION,
            ..Config::default()
        };
        config.cache_pricing.insert(
            "openai-compat:grok-*".to_string(),
            crate::catalog::CachePricingOverride::default(),
        );

        let err = validate_cache_pricing_retired(&config).expect_err("must reject");
        assert!(err.contains("config_migrate"), "err: {err}");
        assert!(err.contains('1'), "err should name the entry count: {err}");
    }
}

mod legacy_mitm_credential_source_preflight_tests {
    use super::{
        LEGACY_MITM_CREDENTIAL_SOURCE_REPLACEMENT_BLOCK, preflight_legacy_mitm_credential_source,
    };

    #[test]
    fn rejects_forwarded_value() {
        let err =
            preflight_legacy_mitm_credential_source("[mitm]\ncredential_source = \"forwarded\"\n")
                .expect_err("legacy key must be rejected regardless of its value");
        let msg = err.to_string();
        assert!(msg.contains("credential_source"), "msg: {msg}");
        assert!(
            msg.contains(LEGACY_MITM_CREDENTIAL_SOURCE_REPLACEMENT_BLOCK),
            "msg must name the exact replacement block: {msg}"
        );
    }

    #[test]
    fn rejects_own_value() {
        // Arrange / Act
        let result =
            preflight_legacy_mitm_credential_source("[mitm]\ncredential_source = \"own\"\n");

        // Assert: the key itself is the problem, not the value.
        assert!(result.is_err());
    }

    /// The replacement block is the actionable payload of the error --
    /// it must be the exact 4-line shape the provider-level schema
    /// accepts (kind, base_url, credential_source, no api_key_ref), not
    /// a paraphrase.
    #[test]
    fn error_names_the_exact_provider_replacement_shape() {
        let err =
            preflight_legacy_mitm_credential_source("[mitm]\ncredential_source = \"forwarded\"\n")
                .expect_err("legacy key must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("kind = \"anthropic-api\""), "msg: {msg}");
        assert!(
            msg.contains("base_url = \"https://api.anthropic.com\""),
            "msg: {msg}"
        );
        assert!(
            msg.contains("credential_source = \"forwarded\""),
            "msg: {msg}"
        );
        assert!(
            !msg.contains("api_key_ref ="),
            "msg must not suggest an api_key_ref: {msg}"
        );
    }

    #[test]
    fn allows_transport_only_mitm_block() {
        assert!(preflight_legacy_mitm_credential_source("[mitm]\n").is_ok());
    }

    #[test]
    fn allows_absent_mitm_block() {
        assert!(preflight_legacy_mitm_credential_source("").is_ok());
        assert!(
            preflight_legacy_mitm_credential_source("[server]\nhost = \"127.0.0.1\"\n").is_ok()
        );
    }

    /// Sanity mirror of `preflight_config_version`'s own sanity test: the
    /// raw `deny_unknown_fields` deserialize error for this exact input
    /// does NOT already carry the actionable replacement text -- this
    /// preflight is the reason the operator sees more than "unknown
    /// field `credential_source`".
    #[test]
    fn raw_deserialize_error_alone_lacks_the_actionable_replacement() {
        let raw = "[mitm]\ncredential_source = \"forwarded\"\n";
        let deny_unknown_err = toml::from_str::<crate::config::Config>(raw)
            .expect_err("legacy key must still fail the typed deserialize too");
        assert!(
            !deny_unknown_err
                .to_string()
                .contains("[providers.anthropic-forwarded]"),
            "sanity: the raw deserialize error must NOT already name the replacement block"
        );
    }
}
