use super::*;

mod config_version_tests {
    use super::{
        CONFIG_VERSION_REMEDIATION, CURRENT_CONFIG_VERSION, Config, ConfigVersionError,
        preflight_config_version, preflight_retired_capability_keys, preflight_retired_model_knobs,
        validate_cache_pricing_retired,
    };

    #[test]
    fn shipped_example_config_parses_and_passes_preflight() {
        // The example config is shipped verbatim (embedded in `config
        // example`, copied to the config dir by operators). It must carry
        // the CURRENT schema version and parse as a typed Config, or the
        // documented copy-to-config-dir flow is dead on arrival -- pin it so
        // this class of break (a stale/absent `version` stamp) can't recur.
        // Preflight reads only the `version` key, so it holds on every
        // build; the typed deserialize needs the feature-gated provider
        // kinds the example documents (`bedrock`, `openai-responses`).
        let example = include_str!("../../../../examples/config.toml");

        assert_eq!(
            preflight_config_version(example),
            Ok(CURRENT_CONFIG_VERSION),
            "example config must preflight at the current schema version"
        );
        assert_eq!(
            preflight_retired_capability_keys(example),
            Ok(()),
            "example config must not carry a retired key"
        );
        assert_eq!(
            preflight_retired_model_knobs(example),
            Ok(()),
            "example config must not carry a removed model key"
        );

        #[cfg(all(feature = "bedrock", feature = "openai-responses"))]
        {
            let config: Config =
                toml::from_str(example).expect("example config must parse as Config");
            assert_eq!(config.version, CURRENT_CONFIG_VERSION);
            validate_cache_pricing_retired(&config).expect("example must not carry retired tables");
        }
    }

    /// Lean-build counterpart to
    /// `shipped_example_config_parses_and_passes_preflight`: a full config
    /// using only the always-available provider kinds preflights at the
    /// current version, deserializes, and carries no retired tables -- the
    /// same guarantees on a build where the feature-gated example cannot
    /// parse.
    #[test]
    fn lean_example_config_parses_and_passes_preflight() {
        let example = crate::config::LEAN_EXAMPLE_CONFIG;

        assert_eq!(
            preflight_config_version(example),
            Ok(CURRENT_CONFIG_VERSION),
            "lean example config must preflight at the current schema version"
        );
        assert_eq!(preflight_retired_model_knobs(example), Ok(()));
        let config: Config =
            toml::from_str(example).expect("lean example config must parse as Config");
        assert_eq!(config.version, CURRENT_CONFIG_VERSION);
        validate_cache_pricing_retired(&config)
            .expect("lean example must not carry retired tables");
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
        let config: Config = toml::from_str(&format!(
            "version = {CURRENT_CONFIG_VERSION}\n[server]\nhost = \"127.0.0.1\"\n"
        ))
        .expect("parse");

        // Assert
        assert_eq!(config.version, CURRENT_CONFIG_VERSION);
    }

    /// What [`preflight_config_version`] must return for one raw config.
    #[derive(Debug)]
    enum Verdict {
        Accept(u32),
        TooOld(u32),
        Missing,
        TooNew(u32),
    }

    /// One row per input class the preflight distinguishes. Unparseable TOML
    /// and a `version` that is not an integer fitting `u32` fall through as
    /// `Ok` so the typed deserialize reports the precise syntax / type error.
    #[test]
    fn preflight_classifies_each_version_shape() {
        let body = "[server]\nhost = \"x\"\n";
        let current = CURRENT_CONFIG_VERSION;
        let mut rows: Vec<(String, String, Verdict)> = (1..current)
            .map(|n| {
                (
                    format!("explicit v{n}"),
                    format!("version = {n}\n{body}"),
                    Verdict::TooOld(n),
                )
            })
            .collect();
        rows.extend([
            ("absent key".to_string(), body.to_string(), Verdict::Missing),
            (
                "explicit zero".to_string(),
                format!("version = 0\n{body}"),
                Verdict::TooOld(0),
            ),
            (
                "negative version".to_string(),
                format!("version = -1\n{body}"),
                Verdict::Accept(1),
            ),
            (
                "version above u32".to_string(),
                format!("version = 4294967296\n{body}"),
                Verdict::Accept(1),
            ),
            (
                "unparseable toml".to_string(),
                "this is = = not valid toml".to_string(),
                Verdict::Accept(1),
            ),
            (
                "string version".to_string(),
                format!("version = \"two\"\n{body}"),
                Verdict::Accept(1),
            ),
            (
                "current".to_string(),
                format!("version = {current}\n{body}"),
                Verdict::Accept(current),
            ),
            (
                "one above current".to_string(),
                format!("version = {}\n{body}", current + 1),
                Verdict::TooNew(current + 1),
            ),
        ]);

        for (row, raw, expected) in rows {
            // Act
            let got = preflight_config_version(&raw);

            // Assert
            match (expected, got) {
                (Verdict::Accept(want), Ok(found)) => assert_eq!(found, want, "row {row}"),
                (
                    Verdict::TooOld(want),
                    Err(err @ ConfigVersionError::TooOld { found, supported }),
                ) => {
                    assert_eq!((found, supported), (want, current), "row {row}");
                    assert_eq!(
                        err.to_string(),
                        format!(
                            "config version {want} is no longer supported; v{current} \
                             required. {CONFIG_VERSION_REMEDIATION}"
                        ),
                        "row {row}"
                    );
                }
                (Verdict::Missing, Err(err @ ConfigVersionError::Missing { supported })) => {
                    assert_eq!(supported, current, "row {row}");
                    assert_eq!(
                        err.to_string(),
                        format!(
                            "config version missing; v{current} required. \
                             {CONFIG_VERSION_REMEDIATION}"
                        ),
                        "row {row}"
                    );
                }
                (Verdict::TooNew(want), Err(ConfigVersionError::TooNew(inner))) => {
                    assert_eq!((inner.found, inner.supported), (want, current), "row {row}");
                }
                (expected, got) => panic!("row {row}: expected {expected:?}, got {got:?}"),
            }
        }
    }

    /// The shared remediation names the replacement path a refused operator
    /// can actually take, and promises no write happened.
    #[test]
    fn version_remediation_names_init_the_docs_and_no_write() {
        for needle in [
            "aside",
            "`routectl init`",
            "docs/CONFIGURATION.md",
            "Nothing was written.",
        ] {
            assert!(
                CONFIG_VERSION_REMEDIATION.contains(needle),
                "remediation must mention {needle}: {CONFIG_VERSION_REMEDIATION}"
            );
        }
        assert!(
            !CONFIG_VERSION_REMEDIATION.contains("migrate"),
            "remediation must not point at a removed command"
        );
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
        assert!(
            err.contains("remove the [cache_pricing] table by hand"),
            "err: {err}"
        );
        assert!(
            !err.contains("migrate"),
            "err names a removed command: {err}"
        );
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

mod retired_capability_keys_preflight_tests {
    use super::{CURRENT_CONFIG_VERSION, preflight_retired_capability_keys};

    /// A value that must never be echoed back: operators paste anything
    /// into a list, and the error goes to logs and terminals.
    const CANARY: &str = "sk-canary-0123456789abcdef";

    fn current_with(body: &str) -> String {
        format!("version = {CURRENT_CONFIG_VERSION}\n{body}")
    }

    #[test]
    fn each_retired_path_is_named_without_its_value() {
        let cases = [
            (
                "bedrock",
                format!("[bedrock]\nallowed_betas = [\"{CANARY}\"]\n"),
            ),
            (
                "providers.p.allowed_betas",
                format!(
                    "[providers.p]\nkind = \"anthropic-api\"\nallowed_betas = [\"{CANARY}\"]\n"
                ),
            ),
            (
                "providers.p.unsupported_features",
                format!(
                    "[providers.p]\nkind = \"openai-compat\"\n\
                     unsupported_features = [\"{CANARY}\"]\n"
                ),
            ),
            (
                "models.m.unsupported_features",
                format!(
                    "[models.m]\nprovider = \"p\"\nupstream = \"x\"\n\
                     unsupported_features = [\"{CANARY}\"]\n"
                ),
            ),
        ];

        for (path, body) in cases {
            // Act
            let err = preflight_retired_capability_keys(&current_with(&body))
                .expect_err(&format!("`{path}` must be refused"));
            let msg = err.to_string();

            // Assert
            assert_eq!(err.paths, vec![path.to_string()], "row `{path}`");
            assert_eq!(
                msg,
                format!("`{path}` was retired in config version 5; remove it by hand"),
                "row `{path}`"
            );
            assert!(!msg.contains(CANARY), "row `{path}` leaked a value: {msg}");
        }
    }

    #[test]
    fn every_retired_key_is_reported_in_deterministic_order() {
        let body = current_with(
            "[models.zeta]\nunsupported_features = []\n\
             [models.alpha]\nunsupported_features = []\n\
             [providers.b]\nunsupported_features = []\nallowed_betas = []\n\
             [providers.a]\nallowed_betas = []\n\
             [bedrock]\n",
        );

        let err = preflight_retired_capability_keys(&body).expect_err("retired keys present");

        assert_eq!(
            err.paths,
            [
                "bedrock",
                "providers.a.allowed_betas",
                "providers.b.allowed_betas",
                "providers.b.unsupported_features",
                "models.alpha.unsupported_features",
                "models.zeta.unsupported_features",
            ]
        );
        assert_eq!(err.to_string().lines().count(), 6, "one line per path");
    }

    #[test]
    fn a_clean_current_config_passes() {
        let body = current_with(
            "[providers.p]\nkind = \"anthropic-api\"\nanthropic_beta = [\"x\"]\n\
             [models.m]\nprovider = \"p\"\nupstream = \"x\"\n\
             [capability.overrides.p]\nunsupported = [\"web_search\"]\n",
        );

        assert_eq!(preflight_retired_capability_keys(&body), Ok(()));
    }

    /// Without the preflight in front of it, the typed parse still refuses
    /// each retired key as an unknown field rather than silently accepting
    /// it. Each row's control is the same fixture with the key removed.
    #[test]
    fn typed_parse_refuses_each_retired_key_as_an_unknown_field() {
        let cases = [
            ("bedrock", "", "[bedrock]\nallowed_betas = [\"x\"]\n"),
            (
                "allowed_betas",
                "[providers.p]\nkind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\n",
                "allowed_betas = [\"x\"]\n",
            ),
        ];

        for (field, base, retired) in cases {
            // Arrange
            let clean = current_with(base);
            let with_retired = current_with(&format!("{base}{retired}"));

            // Act
            let control = crate::parse_config(&clean);
            let err =
                crate::parse_config(&with_retired).expect_err(&format!("`{field}` must not parse"));

            // Assert
            assert!(control.is_ok(), "control for `{field}`: {control:?}");
            assert!(
                err.contains(&format!("unknown field `{field}`")),
                "row `{field}`: {err}"
            );
        }
    }

    #[test]
    fn a_key_name_with_control_characters_is_filtered_out_of_the_message() {
        let cases = [
            (
                "provider name",
                "[providers.\"p\\u001b[31m\\r\\nx\"]\nallowed_betas = []\n",
                "providers.p?[31m??x.allowed_betas",
            ),
            (
                "model nickname",
                "[models.\"m\\u001b\\r\\n\"]\nunsupported_features = []\n",
                "models.m???.unsupported_features",
            ),
        ];

        for (row, body, rendered) in cases {
            // Act
            let msg = preflight_retired_capability_keys(&current_with(body))
                .expect_err(row)
                .to_string();

            // Assert
            for byte in ['\u{1b}', '\r', '\n'] {
                assert!(!msg.contains(byte), "{row}: {byte:?} survived in {msg:?}");
            }
            assert!(
                msg.starts_with(&format!("`{rendered}` was retired")),
                "{row}: {msg:?}"
            );
        }
    }

    #[test]
    fn malformed_toml_falls_through_to_the_typed_parse() {
        assert_eq!(
            preflight_retired_capability_keys("[bedrock\nallowed_betas = = 1\n"),
            Ok(())
        );
    }
}

mod removed_paid_probe_caps_preflight_tests {
    use super::preflight_removed_paid_probe_caps;

    #[test]
    fn refuses_the_key_by_name_without_naming_a_successor() {
        let err =
            preflight_removed_paid_probe_caps("[fidelity.paid_probe_daily_caps]\nanthropic = 5\n")
                .expect_err("the removed key must be refused");

        assert_eq!(
            err.to_string(),
            "config carries `[fidelity] paid_probe_daily_caps`: paid probes were removed; \
             delete this key"
        );
    }

    #[test]
    fn refuses_an_empty_table_and_an_inline_value() {
        for raw in [
            "[fidelity.paid_probe_daily_caps]\n",
            "[fidelity]\npaid_probe_daily_caps = {}\n",
        ] {
            assert!(preflight_removed_paid_probe_caps(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn allows_a_fidelity_block_without_the_key() {
        for raw in [
            "",
            "[fidelity]\n",
            "[fidelity]\nprefix_impact_opt_in = [\"anthropic\"]\n",
            "[providers.x]\npaid_probe_daily_caps = 1\n",
        ] {
            assert!(preflight_removed_paid_probe_caps(raw).is_ok(), "{raw}");
        }
    }

    #[test]
    fn unparseable_toml_falls_through_to_the_typed_deserialize() {
        assert!(preflight_removed_paid_probe_caps("[fidelity\npaid_probe_daily_caps = 1").is_ok());
    }
}

mod retired_model_knobs_preflight_tests {
    use super::{CURRENT_CONFIG_VERSION, RETIRED_MODEL_KNOBS, preflight_retired_model_knobs};

    fn current_with(body: &str) -> String {
        format!("version = {CURRENT_CONFIG_VERSION}\n{body}")
    }

    /// Every removed key, each set to a value carrying `sentinel`, on one
    /// model entry.
    fn all_knobs(nick: &str, sentinel: &str) -> String {
        format!(
            "[models.{nick}]\nprovider = \"p\"\nupstream = \"u\"\n\
             supports_adaptive_thinking = \"{sentinel}\"\n\
             effort_levels = [\"{sentinel}\"]\n\
             max_thinking_budget = \"{sentinel}\"\n\
             max_output_tokens = \"{sentinel}\"\n"
        )
    }

    #[test]
    fn a_model_without_removed_keys_passes() {
        let body = "[models.m]\nprovider = \"p\"\nupstream = \"u\"\n\
                    reasoning_dialect = \"openai\"\n[providers.p]\nkind = \"openai-compat\"\n";

        assert_eq!(preflight_retired_model_knobs(&current_with(body)), Ok(()));
    }

    #[test]
    fn two_models_carrying_every_key_yield_one_sorted_line_per_path() {
        // Arrange: `zeta` first in the document, so sorting is observable.
        let body = format!("{}{}", all_knobs("zeta", "s1"), all_knobs("alpha", "s2"));

        // Act
        let err = preflight_retired_model_knobs(&current_with(&body)).expect_err("refused");
        let msg = err.to_string();

        // Assert
        let expected_paths: Vec<String> = ["alpha", "zeta"]
            .iter()
            .flat_map(|nick| {
                [
                    "effort_levels",
                    "max_output_tokens",
                    "max_thinking_budget",
                    "supports_adaptive_thinking",
                ]
                .map(|knob| format!("models.{nick}.{knob}"))
            })
            .collect();
        assert_eq!(err.paths, expected_paths);
        let lines: Vec<&str> = msg.lines().collect();
        assert_eq!(lines.len(), 8, "{msg}");
        for (line, path) in lines.iter().zip(&expected_paths) {
            assert!(line.starts_with(&format!("`{path}` ")), "{line}");
        }
    }

    #[test]
    fn each_line_names_its_replacement_and_ends_delete_the_key() {
        let msg = preflight_retired_model_knobs(&current_with(&all_knobs("m", "x")))
            .expect_err("refused")
            .to_string();

        for (knob, replacement) in RETIRED_MODEL_KNOBS {
            let line = msg
                .lines()
                .find(|line| line.starts_with(&format!("`models.m.{knob}`")))
                .unwrap_or_else(|| panic!("no line for {knob}: {msg}"));
            assert!(line.contains(replacement), "{knob}: {line}");
            assert!(line.ends_with("; delete the key"), "{knob}: {line}");
        }
        assert!(
            msg.contains("`routectl catalog set ... max_output_tokens`"),
            "{msg}"
        );
    }

    #[test]
    fn the_message_names_no_version_and_no_migrate() {
        let msg = preflight_retired_model_knobs(&current_with(&all_knobs("m", "x")))
            .expect_err("refused")
            .to_string();

        for word in ["version", "migrate"] {
            assert!(!msg.to_lowercase().contains(word), "{word}: {msg}");
        }
    }

    #[test]
    fn values_never_reach_the_message() {
        // Arrange: a sentinel no path or replacement text contains.
        let sentinel = "SENTINEL-VALUE-7f3a";
        let raw = current_with(&all_knobs("m", sentinel));
        assert!(raw.contains(sentinel), "control: the fixture carries it");

        // Act
        let msg = preflight_retired_model_knobs(&raw)
            .expect_err("refused")
            .to_string();

        // Assert
        assert!(!msg.contains(sentinel), "{msg}");
        assert_eq!(msg.lines().count(), 4, "{msg}");
    }

    #[test]
    fn a_control_byte_in_a_nickname_never_reaches_the_message() {
        let body = "[models.\"m\\u001b\\r\\n\"]\neffort_levels = []\n";

        let msg = preflight_retired_model_knobs(&current_with(body))
            .expect_err("refused")
            .to_string();

        for byte in ['\u{1b}', '\r', '\n'] {
            assert!(!msg.contains(byte), "{byte:?} survived in {msg:?}");
        }
        assert!(
            msg.starts_with("`models.m???.effort_levels` is no longer"),
            "{msg:?}"
        );
    }

    #[test]
    fn malformed_toml_falls_through_to_the_typed_parse() {
        assert_eq!(
            preflight_retired_model_knobs("[models.m\neffort_levels = = 1\n"),
            Ok(())
        );
    }
}

mod raw_config_preflight_tests {
    use super::{
        RawConfigPreflightError, preflight_config_version, preflight_legacy_mitm_credential_source,
        preflight_raw_config, preflight_removed_paid_probe_caps, preflight_retired_capability_keys,
        preflight_retired_model_knobs,
    };

    const LEGACY_MITM: &str = "[mitm]\ncredential_source = \"forwarded\"\n";
    const RETIRED_KEY: &str = "[bedrock]\n";
    const PAID_CAPS: &str = "[fidelity.paid_probe_daily_caps]\nanthropic = 5\n";
    const MODEL_KNOB: &str = "[models.m]\neffort_levels = [\"low\"]\n";

    fn at_version(version: u32, body: &str) -> String {
        format!("version = {version}\n{body}")
    }

    /// One refusal class the driver can return: how to recognise its
    /// variant, and the Display of the individual check that owns it, run on
    /// the same fixture, so the driver is pinned to forward wording unchanged.
    struct Refusal {
        name: &'static str,
        is_variant: fn(&RawConfigPreflightError) -> bool,
        check_message: fn(&str) -> String,
    }

    const VERSION_REFUSAL: Refusal = Refusal {
        name: "Version",
        is_variant: |e| matches!(e, RawConfigPreflightError::Version(_)),
        check_message: |raw| preflight_config_version(raw).unwrap_err().to_string(),
    };

    const LEGACY_MITM_REFUSAL: Refusal = Refusal {
        name: "LegacyMitmCredentialSource",
        is_variant: |e| matches!(e, RawConfigPreflightError::LegacyMitmCredentialSource(_)),
        check_message: |raw| {
            preflight_legacy_mitm_credential_source(raw)
                .unwrap_err()
                .to_string()
        },
    };

    const RETIRED_KEYS_REFUSAL: Refusal = Refusal {
        name: "RetiredCapabilityKeys",
        is_variant: |e| matches!(e, RawConfigPreflightError::RetiredCapabilityKeys(_)),
        check_message: |raw| {
            preflight_retired_capability_keys(raw)
                .unwrap_err()
                .to_string()
        },
    };

    const PAID_CAPS_REFUSAL: Refusal = Refusal {
        name: "RemovedPaidProbeCaps",
        is_variant: |e| matches!(e, RawConfigPreflightError::RemovedPaidProbeCaps(_)),
        check_message: |raw| {
            preflight_removed_paid_probe_caps(raw)
                .unwrap_err()
                .to_string()
        },
    };

    const MODEL_KNOBS_REFUSAL: Refusal = Refusal {
        name: "RetiredModelKnobs",
        is_variant: |e| matches!(e, RawConfigPreflightError::RetiredModelKnobs(_)),
        check_message: |raw| preflight_retired_model_knobs(raw).unwrap_err().to_string(),
    };

    fn assert_refused_as(row: &str, raw: &str, expected: &Refusal) {
        let err = preflight_raw_config(raw).expect_err(&format!("row `{row}` must be refused"));

        assert!(
            (expected.is_variant)(&err),
            "row `{row}`: expected {}, got {err:?}",
            expected.name
        );
        assert_eq!(
            err.to_string(),
            (expected.check_message)(raw),
            "row `{row}`: driver Display must equal the wrapped check's"
        );
    }

    #[test]
    fn a_clean_current_config_passes() {
        let raw = at_version(
            super::CURRENT_CONFIG_VERSION,
            "[mitm]\n[fidelity]\n[providers.p]\nkind = \"anthropic-api\"\n",
        );

        assert_eq!(preflight_raw_config(&raw), Ok(()));
    }

    #[test]
    fn each_refusal_class_maps_to_its_variant_with_the_checks_wording() {
        let current = super::CURRENT_CONFIG_VERSION;
        let rows = [
            ("too old", at_version(current - 1, ""), &VERSION_REFUSAL),
            ("too new", at_version(current + 1, ""), &VERSION_REFUSAL),
            (
                "legacy mitm credential_source",
                at_version(current, LEGACY_MITM),
                &LEGACY_MITM_REFUSAL,
            ),
            (
                "retired capability key",
                at_version(current, RETIRED_KEY),
                &RETIRED_KEYS_REFUSAL,
            ),
            (
                "paid probe caps",
                at_version(current, PAID_CAPS),
                &PAID_CAPS_REFUSAL,
            ),
            (
                "removed model key",
                at_version(current, MODEL_KNOB),
                &MODEL_KNOBS_REFUSAL,
            ),
        ];

        for (row, raw, expected) in rows {
            assert_refused_as(row, &raw, expected);
        }
    }

    #[test]
    fn the_first_refusal_in_check_order_wins() {
        let current = super::CURRENT_CONFIG_VERSION;
        let rows = [
            (
                "too old + legacy mitm",
                at_version(current - 1, LEGACY_MITM),
                &VERSION_REFUSAL,
            ),
            (
                "too old + retired key",
                at_version(current - 1, RETIRED_KEY),
                &VERSION_REFUSAL,
            ),
            (
                "legacy mitm + retired key",
                at_version(current, &format!("{LEGACY_MITM}{RETIRED_KEY}")),
                &LEGACY_MITM_REFUSAL,
            ),
            (
                "retired key + paid caps",
                at_version(current, &format!("{RETIRED_KEY}{PAID_CAPS}")),
                &RETIRED_KEYS_REFUSAL,
            ),
            (
                "too old + removed model key",
                at_version(current - 1, MODEL_KNOB),
                &VERSION_REFUSAL,
            ),
            (
                "paid caps + removed model key",
                at_version(current, &format!("{PAID_CAPS}{MODEL_KNOB}")),
                &PAID_CAPS_REFUSAL,
            ),
        ];

        for (row, raw, expected) in rows {
            assert_refused_as(row, &raw, expected);
        }
    }

    #[test]
    fn unparseable_toml_falls_through_to_the_typed_deserialize() {
        assert_eq!(
            preflight_raw_config("[mitm\ncredential_source = = 1\n"),
            Ok(())
        );
    }
}
