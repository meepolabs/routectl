use super::*;

mod config_version_tests {
    use super::{
        CONFIG_VERSION_REMEDIATION, CURRENT_CONFIG_VERSION, Config, ConfigVersionError,
        preflight_config_version, preflight_raw_config,
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
            preflight_raw_config(example),
            Ok(()),
            "example config must not carry a retired key"
        );

        #[cfg(all(feature = "bedrock", feature = "openai-responses"))]
        {
            let config: Config =
                toml::from_str(example).expect("example config must parse as Config");
            assert_eq!(config.version, CURRENT_CONFIG_VERSION);
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
        assert_eq!(preflight_raw_config(example), Ok(()));
        let config: Config =
            toml::from_str(example).expect("lean example config must parse as Config");
        assert_eq!(config.version, CURRENT_CONFIG_VERSION);
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

    /// Every `[cache_pricing]` shape, the empty table included, is refused
    /// before the typed parse with the one retired-key line, never its
    /// entry count or contents.
    #[test]
    fn every_cache_pricing_shape_is_refused_naming_its_replacement() {
        let expected =
            "`cache_pricing` is retired: set prices under `[models.X.pricing]`; delete it";
        for (name, body) in [
            ("empty table", "[cache_pricing]\n"),
            (
                "legacy override cell",
                "[cache_pricing.\"openai-compat:grok-*\"]\nwm = 1.5\nrm = 0.1\n",
            ),
            ("empty cell", "[cache_pricing.\"m\"]\n"),
            ("scalar entry", "[cache_pricing]\nm = 1\n"),
            ("string entry", "[cache_pricing]\nm = \"x\"\n"),
            ("array entry", "[cache_pricing]\nm = [1, 2]\n"),
        ] {
            let raw = format!("version = {CURRENT_CONFIG_VERSION}\n{body}");

            let err = preflight_raw_config(&raw).expect_err(&format!("{name} must be refused"));

            assert_eq!(err.to_string(), expected, "{name}");
        }
    }
}

mod raw_config_preflight_tests {
    use super::{RawConfigPreflightError, preflight_config_version, preflight_raw_config};
    use crate::config::retired_keys::{RETIRED_KEYS, preflight_retired_keys};

    const RETIRED_KEY: &str = "[bedrock]\n";

    fn at_version(version: u32, body: &str) -> String {
        format!("version = {version}\n{body}")
    }

    #[test]
    fn a_clean_current_config_passes() {
        let raw = at_version(
            super::CURRENT_CONFIG_VERSION,
            "[mitm]\n[providers.p]\nformat = \"anthropic-messages\"\nbase_url = \"https://api.anthropic.com\"\ncredential = \"env://ANTHROPIC_API_KEY\"\nauth = \"x-api-key\"\n",
        );

        assert_eq!(preflight_raw_config(&raw), Ok(()));
    }

    /// Each refusal class maps to its variant and forwards the owning
    /// check's wording unchanged. The version check runs first, so a file
    /// at another version that also carries a retired key is told about its
    /// version.
    #[test]
    fn each_refusal_class_maps_to_its_variant_with_the_checks_wording() {
        let current = super::CURRENT_CONFIG_VERSION;
        let rows = [
            ("too old", at_version(current - 1, ""), true),
            ("too new", at_version(current + 1, ""), true),
            (
                "too old with a retired key",
                at_version(current - 1, RETIRED_KEY),
                true,
            ),
            ("retired key", at_version(current, RETIRED_KEY), false),
        ];

        for (row, raw, is_version) in rows {
            let err =
                preflight_raw_config(&raw).expect_err(&format!("row `{row}` must be refused"));

            let expected = if is_version {
                assert!(
                    matches!(err, RawConfigPreflightError::Version(_)),
                    "row `{row}`: {err:?}"
                );
                preflight_config_version(&raw).unwrap_err().to_string()
            } else {
                assert!(
                    matches!(err, RawConfigPreflightError::RetiredKeys(_)),
                    "row `{row}`: {err:?}"
                );
                preflight_retired_keys(&raw, RETIRED_KEYS)
                    .unwrap_err()
                    .to_string()
            };
            assert_eq!(err.to_string(), expected, "row `{row}`");
        }
    }

    /// Without the preflight in front of it, the typed parse still refuses
    /// a retired key as an unknown field rather than silently accepting it.
    /// Each row's control is the same fixture with the key removed.
    #[test]
    fn typed_parse_refuses_a_retired_key_as_an_unknown_field() {
        let cases = [
            ("bedrock", "", "[bedrock]\nallowed_betas = [\"x\"]\n"),
            (
                "allowed_betas",
                "[providers.p]\nformat = \"anthropic-messages\"\nbase_url = \"https://api.anthropic.com\"\ncredential = \"literal:k\"\nauth = \"x-api-key\"\n",
                "allowed_betas = [\"x\"]\n",
            ),
        ];

        for (field, base, retired) in cases {
            // Arrange
            let clean = at_version(super::CURRENT_CONFIG_VERSION, base);
            let with_retired =
                at_version(super::CURRENT_CONFIG_VERSION, &format!("{base}{retired}"));

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
    fn unparseable_toml_falls_through_to_the_typed_deserialize() {
        assert_eq!(
            preflight_raw_config("[mitm\ncredential_source = = 1\n"),
            Ok(())
        );
    }
}

mod model_token_limit_tests {
    use super::{validate_model_pricing, validate_model_token_limits};
    use crate::config::{Config, ModelEntry, PricingConfig};

    fn config_with(entries: &[(&str, ModelEntry)]) -> Config {
        let mut config = Config::default();
        for (nickname, entry) in entries {
            config.models.insert((*nickname).to_string(), entry.clone());
        }
        config
    }

    /// Both keys parse from `[models.X]` into the typed entry.
    #[test]
    fn context_window_and_max_output_tokens_parse_from_a_model_entry() {
        let config: Config = toml::from_str(
            "[models.m]\nprovider = \"p\"\nupstream = \"u\"\n\
             context_window = 200000\nmax_output_tokens = 32000\n",
        )
        .expect("parses");

        let entry = &config.models["m"];
        assert_eq!(entry.context_window, Some(200_000));
        assert_eq!(entry.max_output_tokens, Some(32_000));
    }

    /// A zero for either key is refused naming the dotted key; a positive
    /// value or an absent key passes.
    ///
    /// Mutation check: make `validate_model_token_limits` always return
    /// `Ok` -> red on both zero rows.
    #[test]
    fn a_zero_token_limit_is_refused_naming_the_dotted_key() {
        let rows: [(&str, ModelEntry, Option<&str>); 4] = [
            (
                "zero window",
                ModelEntry::new("p", "u").with_context_window(0),
                Some("`models.m.context_window` is 0"),
            ),
            (
                "zero ceiling",
                ModelEntry::new("p", "u").with_max_output_tokens(0),
                Some("`models.m.max_output_tokens` is 0"),
            ),
            (
                "positive values",
                ModelEntry::new("p", "u")
                    .with_context_window(1)
                    .with_max_output_tokens(1),
                None,
            ),
            ("absent keys", ModelEntry::new("p", "u"), None),
        ];

        for (name, entry, refusal) in rows {
            let result = validate_model_token_limits(&config_with(&[("m", entry)]));

            match refusal {
                Some(expected) => {
                    let msg = result.expect_err(name);
                    assert!(msg.starts_with(expected), "{name}: {msg}");
                }
                None => assert_eq!(result, Ok(()), "{name}"),
            }
        }
    }

    /// Every zero across every model is listed, one line each, so an
    /// operator fixes them all at once; a hostile nickname is sanitized.
    #[test]
    fn every_zero_is_listed_and_a_control_byte_never_reaches_the_message() {
        let zeroed = ModelEntry::new("p", "u")
            .with_context_window(0)
            .with_max_output_tokens(0);
        let config = config_with(&[("a", zeroed.clone()), ("b\u{1b}\n", zeroed)]);

        let msg = validate_model_token_limits(&config).expect_err("refused");

        assert_eq!(msg.lines().count(), 4, "{msg:?}");
        assert!(!msg.contains('\u{1b}'), "{msg:?}");
        assert!(msg.contains("`models.a.context_window`"), "{msg}");
        assert!(msg.contains("`models.a.max_output_tokens`"), "{msg}");
    }

    /// The refusal reaches every config-validation surface through the one
    /// ordered suite.
    #[test]
    fn the_shared_validation_suite_carries_the_zero_refusal() {
        let config = config_with(&[("m", ModelEntry::new("p", "u").with_context_window(0))]);

        let errors = crate::factory::collect_config_validation(&config).errors;

        assert!(
            errors
                .iter()
                .any(|e| e.contains("`models.m.context_window` is 0")),
            "{errors:?}"
        );
    }

    /// `[models.X.pricing]` parses from TOML into the typed entry.
    #[test]
    fn model_pricing_parses_from_a_model_entry() {
        let config: Config = toml::from_str(
            "[models.m]\nprovider = \"p\"\nupstream = \"u\"\n\
             [models.m.pricing]\ninput_per_mtok = 3.0\ncache_write_1h_per_mtok = 6.0\n",
        )
        .expect("parses");

        let pricing = config.models["m"].pricing.clone().expect("pricing");
        assert_eq!(pricing.input_per_mtok, Some(3.0));
        assert_eq!(pricing.cache_write_1h_per_mtok, Some(6.0));
        assert_eq!(pricing.output_per_mtok, None);
    }

    /// A non-finite or negative price is refused naming the dotted key; a
    /// known zero, a positive price and an absent block pass.
    #[test]
    fn a_non_finite_or_negative_model_price_is_refused_naming_the_dotted_key() {
        let priced = |pricing: PricingConfig| ModelEntry::new("p", "u").with_pricing(pricing);
        let rows: [(&str, ModelEntry, Option<&str>); 7] = [
            (
                "nan input",
                priced(PricingConfig {
                    input_per_mtok: Some(f64::NAN),
                    ..Default::default()
                }),
                Some("`models.m.pricing.input_per_mtok` is `NaN`; must be a finite number"),
            ),
            (
                "infinite output",
                priced(PricingConfig {
                    output_per_mtok: Some(f64::INFINITY),
                    ..Default::default()
                }),
                Some("`models.m.pricing.output_per_mtok` is `inf`; must be a finite number"),
            ),
            (
                "negative cache read",
                priced(PricingConfig {
                    cache_read_per_mtok: Some(-0.5),
                    ..Default::default()
                }),
                Some("`models.m.pricing.cache_read_per_mtok` is `-0.5`; must not be negative"),
            ),
            (
                "negative 5m write",
                priced(PricingConfig {
                    cache_write_5m_per_mtok: Some(-1.0),
                    ..Default::default()
                }),
                Some("`models.m.pricing.cache_write_5m_per_mtok` is `-1`; must not be negative"),
            ),
            (
                "negative infinite 1h write",
                priced(PricingConfig {
                    cache_write_1h_per_mtok: Some(f64::NEG_INFINITY),
                    ..Default::default()
                }),
                Some(
                    "`models.m.pricing.cache_write_1h_per_mtok` is `-inf`; must be a finite number",
                ),
            ),
            (
                "known zero and positive",
                priced(PricingConfig {
                    input_per_mtok: Some(0.0),
                    output_per_mtok: Some(15.0),
                    ..Default::default()
                }),
                None,
            ),
            ("absent block", ModelEntry::new("p", "u"), None),
        ];

        for (name, entry, refusal) in rows {
            let result = validate_model_pricing(&config_with(&[("m", entry)]));

            match refusal {
                Some(expected) => assert_eq!(result, Err(expected.to_string()), "{name}"),
                None => assert_eq!(result, Ok(()), "{name}"),
            }
        }
    }

    /// The refusal reaches every config-validation surface, including a
    /// value written in TOML, with a hostile nickname sanitized.
    #[test]
    fn the_shared_validation_suite_refuses_a_bad_model_price_from_toml() {
        let config: Config = toml::from_str(
            "[models.\"m\\u001b\"]\nprovider = \"p\"\nupstream = \"u\"\n\
             [models.\"m\\u001b\".pricing]\noutput_per_mtok = nan\n",
        )
        .expect("parses");

        let errors = crate::factory::collect_config_validation(&config).errors;

        let refusal = errors
            .iter()
            .find(|e| e.contains(".pricing.output_per_mtok"))
            .unwrap_or_else(|| panic!("no pricing refusal: {errors:?}"));
        assert!(refusal.contains("must be a finite number"), "{refusal}");
        assert!(!refusal.contains('\u{1b}'), "{refusal:?}");
    }
}

mod cache_write_map_tests {
    use crate::config::Config;
    use crate::factory::collect_config_validation;

    const MODEL_BLOCK: &str =
        "[models.m]\nprovider = \"p\"\nupstream = \"u\"\n[models.m.pricing]\n";

    fn errors_for(block: &str, map: &str) -> Vec<String> {
        let config: Config =
            toml::from_str(&format!("{block}cache_write_per_mtok = {{ {map} }}\n"))
                .expect("fixture parses");
        collect_config_validation(&config).errors
    }

    /// Every refusal names the full dotted key under `[models.X.pricing]`;
    /// a valid map beside each refused one is the positive control.
    #[test]
    fn a_bad_cache_write_key_or_value_is_refused_naming_the_dotted_key() {
        let blocks = [(
            "model",
            MODEL_BLOCK,
            "models.m.pricing.cache_write_per_mtok",
        )];
        let rows: [(&str, &str, &str, &str); 6] = [
            (
                "word key",
                "\"thirty\" = 2.0",
                "thirty",
                "is not a cache lifetime",
            ),
            ("zero key", "\"0m\" = 2.0", "0m", "is not a cache lifetime"),
            (
                "two durations",
                "\"5m5m\" = 2.0",
                "5m5m",
                "is not a cache lifetime",
            ),
            (
                "negative value",
                "\"30m\" = -1.0",
                "30m",
                "is `-1`; must not be negative",
            ),
            (
                "nan value",
                "\"30m\" = nan",
                "30m",
                "is `NaN`; must be a finite number",
            ),
            (
                "infinite value",
                "\"30m\" = inf",
                "30m",
                "is `inf`; must be a finite number",
            ),
        ];
        for (block_name, block, prefix) in blocks {
            for (row, map, key, problem) in rows {
                let errors = errors_for(block, map);

                let expected = format!("`{prefix}.{key}` {problem}");
                assert!(
                    errors.iter().any(|e| e.starts_with(&expected)),
                    "{block_name} {row}: {errors:?}"
                );
            }
            assert_eq!(
                errors_for(block, "\"30m\" = 2.0, \"5m\" = 0.0, \"24h\" = 1.5"),
                Vec::<String>::new(),
                "{block_name} valid map"
            );
        }
    }

    /// Two spellings of one lifetime inside the map leave no rule for
    /// which wins, so the second is refused naming both keys.
    #[test]
    fn two_map_keys_for_one_lifetime_are_refused() {
        let errors = errors_for(MODEL_BLOCK, "\"60m\" = 2.0, \"1h\" = 3.0");

        assert_eq!(
            errors,
            vec![
                "`models.m.pricing.cache_write_per_mtok.60m` names the same cache lifetime as \
                 `1h`; keep one of them"
                    .to_string()
            ]
        );
    }

    /// A legacy key beside a map key for the same lifetime is not a
    /// conflict the loader refuses: the legacy key wins that lifetime.
    #[test]
    fn a_legacy_key_beside_a_map_key_for_its_lifetime_is_accepted() {
        let config: Config = toml::from_str(&format!(
            "{MODEL_BLOCK}cache_write_5m_per_mtok = 3.75\ncache_write_per_mtok = {{ \"5m\" = 9.0 }}\n"
        ))
        .expect("fixture parses");

        assert_eq!(
            collect_config_validation(&config).errors,
            Vec::<String>::new()
        );
    }
}

mod server_body_limit_tests {
    use super::validate_server_body_limit;
    use crate::config::Config;
    use crate::factory::collect_config_validation;

    fn parsed(server_body: &str) -> Config {
        toml::from_str(&format!("[server]\n{server_body}")).expect("parses")
    }

    /// A zero cap is refused with the remediation; a positive cap and an
    /// omitted key (which takes the 32 MiB schema default) pass.
    #[test]
    fn a_zero_body_limit_is_refused_and_any_positive_value_passes() {
        let rows: [(&str, &str, u32, bool); 3] = [
            ("zero", "max_body_bytes = 0\n", 0, true),
            ("one byte", "max_body_bytes = 1\n", 1, false),
            ("omitted", "", 33_554_432, false),
        ];

        for (name, body, expected_value, refused) in rows {
            let config = parsed(body);

            let result = validate_server_body_limit(&config);

            assert_eq!(config.server.max_body_bytes, expected_value, "{name}");
            if refused {
                assert_eq!(
                    result.expect_err(name),
                    "`server.max_body_bytes` is 0; omit it for the 32 MiB default or set a \
                     positive byte count",
                    "{name}"
                );
            } else {
                assert_eq!(result, Ok(()), "{name}");
            }
        }
    }

    /// The refusal reaches every config-validation surface through the one
    /// ordered suite.
    #[test]
    fn the_shared_validation_suite_carries_the_zero_body_limit_refusal() {
        let refused = collect_config_validation(&parsed("max_body_bytes = 0\n")).errors;
        let accepted = collect_config_validation(&parsed("max_body_bytes = 1\n")).errors;

        assert!(
            refused
                .iter()
                .any(|e| e.starts_with("`server.max_body_bytes` is 0")),
            "{refused:?}"
        );
        assert!(
            !accepted.iter().any(|e| e.contains("max_body_bytes")),
            "{accepted:?}"
        );
    }
}
