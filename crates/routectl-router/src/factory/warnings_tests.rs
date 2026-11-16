//! Unit tests for the non-fatal config warning collectors in
//! `factory/warnings.rs`.

use crate::config::Config;

fn parse(toml_text: &str) -> Config {
    toml::from_str(toml_text).expect("fixture must parse")
}

#[cfg(feature = "gemini")]
mod cloud_code {
    use super::parse;
    use crate::config::Config;
    use crate::factory::warnings::{cloudcode_host_warnings, cloudcode_model_warnings};

    /// A `gemini-cloud-code` entry on `base_url`; `None` writes the daily
    /// host, the base the login writes.
    fn cloud_code_config(base_url: Option<&str>) -> Config {
        let base = base_url.unwrap_or("https://daily-cloudcode-pa.googleapis.com");
        parse(&format!(
            "version = 2\n\
             [providers.g]\n\
             format = \"gemini-cloud-code\"\n\
             base_url = \"{base}\"\n\
             credential = \"oauth://antigravity\"\n\
             auth = \"bearer\"\n"
        ))
    }

    #[test]
    fn warns_when_a_cloud_code_entry_pins_the_production_host() {
        // Arrange
        let config = cloud_code_config(Some(routectl_providers::gemini::PROD_BASE_URL));

        // Act
        let warnings = cloudcode_host_warnings(&config);

        // Assert
        assert_eq!(
            warnings.len(),
            1,
            "an explicit production pin must warn exactly once: {warnings:?}"
        );
        assert!(
            warnings[0].contains("[providers.g]"),
            "the warning must name the provider key: {}",
            warnings[0]
        );
        assert!(
            warnings[0].contains("base_url"),
            "the warning must name the recovery knob: {}",
            warnings[0]
        );
        assert!(
            warnings[0].contains("daily"),
            "the warning must name the daily lane default: {}",
            warnings[0]
        );
    }

    #[test]
    fn warns_on_a_production_pin_carrying_a_path_port_or_credentials() {
        // POSITIVE CONTROL for the negative cases below: the predicate matches
        // the parsed host, so these three real production pins must all fire --
        // otherwise the lookalike test below would pass vacuously.
        for pin in [
            "https://cloudcode-pa.googleapis.com/v1internal",
            "https://cloudcode-pa.googleapis.com:443",
            "https://CloudCode-PA.GoogleAPIs.Com",
            "https://user:pass@cloudcode-pa.googleapis.com",
        ] {
            let warnings = cloudcode_host_warnings(&cloud_code_config(Some(pin)));
            assert_eq!(
                warnings.len(),
                1,
                "pin {pin} egresses to the production host and must warn: {warnings:?}"
            );
        }
    }

    #[test]
    fn silent_for_a_lookalike_host() {
        // A sibling-domain suffix, the host inside a path, and a
        // credentials-suffix smuggle all egress somewhere other than the
        // production host: a substring test would false-positive on each.
        for pin in [
            "https://cloudcode-pa.googleapis.com.example.test",
            "https://proxy.example.test/cloudcode-pa.googleapis.com",
            "https://cloudcode-pa.googleapis.com@other.example.test",
        ] {
            let warnings = cloudcode_host_warnings(&cloud_code_config(Some(pin)));
            assert!(
                warnings.is_empty(),
                "pin {pin} is not the production host and must stay silent: {warnings:?}"
            );
        }
    }

    #[test]
    fn silent_for_the_daily_host() {
        assert!(
            cloudcode_host_warnings(&cloud_code_config(Some(
                routectl_providers::gemini::DAILY_BASE_URL
            )))
            .is_empty(),
            "the daily host is the login-written base, not a divergence"
        );
    }

    #[test]
    fn silent_for_an_api_key_gemini_entry_even_pinned_at_the_production_host() {
        // The Cloud Code hosts are not on the api-key lane at all, so a pin
        // there says nothing about cloud-code lane serving.
        let config = parse(&format!(
            "version = 2\n\
             [providers.g]\n\
             format = \"gemini\"\n\
             base_url = \"{}\"\n\
             credential = \"env://GEMINI_API_KEY\"\n\
             auth = \"x-goog-api-key\"\n",
            routectl_providers::gemini::PROD_BASE_URL
        ));
        assert!(
            cloudcode_host_warnings(&config).is_empty(),
            "an api-key entry must never produce a cloud-code host warning"
        );
    }

    #[test]
    fn collect_config_validation_surfaces_the_pin_as_a_warning_not_an_error() {
        // Arrange
        let config = cloud_code_config(Some(routectl_providers::gemini::PROD_BASE_URL));

        // Act
        let validation = crate::factory::validate::collect_config_validation(&config);

        // Assert
        assert!(
            validation
                .warnings
                .iter()
                .any(|w| w.contains("[providers.g]") && w.contains("production cloud-code host")),
            "the host pin line must reach .warnings: {:?}",
            validation.warnings
        );
        assert!(
            validation.errors.is_empty(),
            "a production pin is advisory and must never fail the load: {:?}",
            validation.errors
        );
    }

    /// One `[models.m]` entry bound to a single provider of the given Gemini
    /// `format`, pinning `upstream`.
    fn model_config(format: &str, upstream: &str) -> Config {
        let (base_url, auth) = match format {
            "gemini-cloud-code" => ("https://daily-cloudcode-pa.googleapis.com", "bearer"),
            _ => (
                "https://generativelanguage.googleapis.com/v1beta",
                "x-goog-api-key",
            ),
        };
        parse(&format!(
            "version = 2\n\
             [providers.g]\n\
             format = \"{format}\"\n\
             base_url = \"{base_url}\"\n\
             credential = \"oauth://antigravity\"\n\
             auth = \"{auth}\"\n\
             [models.m]\n\
             provider = \"g\"\n\
             upstream = \"{upstream}\"\n"
        ))
    }

    #[test]
    fn warns_on_a_cloud_code_entry_pinning_a_deprecated_alias() {
        for (upstream, replacement) in [
            ("gemini-2.5-flash", "gemini-3.1-flash-lite"),
            ("gemini-2.5-pro", "gemini-3.1-pro-low"),
        ] {
            // Arrange
            let config = model_config("gemini-cloud-code", upstream);

            // Act
            let warnings = cloudcode_model_warnings(&config);

            // Assert
            assert_eq!(
                warnings.len(),
                1,
                "{upstream} is a deprecated cloud-code id and must warn once: {warnings:?}"
            );
            assert!(
                warnings[0].contains("[models.m]"),
                "the warning must name the model entry key: {}",
                warnings[0]
            );
            assert!(
                warnings[0].contains(upstream),
                "the warning must name the deprecated id: {}",
                warnings[0]
            );
            assert!(
                warnings[0].contains(replacement),
                "the warning must name the verified replacement: {}",
                warnings[0]
            );
        }
    }

    #[test]
    fn silent_for_an_api_key_gemini_entry_on_the_same_upstream_id() {
        // The load-bearing negative: gemini-2.5-flash is a LIVE id on the
        // native api-key REST lane, so that config is correct as written and a
        // WARN there would be a false positive telling the operator to break it.
        // The positive above (same id, cloud-code mode) proves this is not
        // vacuous -- only the lane differs.
        for upstream in ["gemini-2.5-flash", "gemini-2.5-pro"] {
            let warnings = cloudcode_model_warnings(&model_config("gemini", upstream));
            assert!(
                warnings.is_empty(),
                "{upstream} is current on the api-key lane and must stay silent: {warnings:?}"
            );
        }
    }

    #[test]
    fn silent_for_a_cloud_code_entry_pinning_a_verified_id() {
        for upstream in ["gemini-3.1-flash-lite", "gemini-3.1-pro-low"] {
            let warnings = cloudcode_model_warnings(&model_config("gemini-cloud-code", upstream));
            assert!(
                warnings.is_empty(),
                "{upstream} is verified on the cloud-code lane: {warnings:?}"
            );
        }
    }

    #[test]
    fn collect_config_validation_surfaces_the_deprecated_alias_as_a_warning_not_an_error() {
        // Arrange
        let config = model_config("gemini-cloud-code", "gemini-2.5-flash");

        // Act
        let validation = crate::factory::validate::collect_config_validation(&config);

        // Assert
        assert!(
            validation
                .warnings
                .iter()
                .any(|w| w.contains("[models.m]") && w.contains("gemini-3.1-flash-lite")),
            "the deprecated-alias line must reach .warnings: {:?}",
            validation.warnings
        );
        assert!(
            validation.errors.is_empty(),
            "a deprecated alias is advisory and must never fail the load: {:?}",
            validation.errors
        );
    }
}

#[test]
fn auth_reserved_header_warnings_name_each_reserved_model_header() {
    // Arrange: one row per reserved key, in mixed case, beside a plain
    // header that must not be reported.
    for reserved in ["Authorization", "x-api-key", "ANTHROPIC-VERSION"] {
        let config = parse(&format!(
            "version = 5\n\
             [providers.p]\nformat = \"openai-chat\"\nbase_url = \"https://example.invalid\"\ncredential = \"env://ROUTECTL_TEST_ABSENT\"\nauth = \"bearer\"\n\
             [models.m]\nprovider = \"p\"\nupstream = \"u\"\n\
             [models.m.header_extras]\n\"{reserved}\" = \"secret-value\"\nx-app = \"ok\"\n"
        ));

        // Act
        let warnings = super::auth_reserved_header_warnings(&config);

        // Assert
        assert_eq!(warnings.len(), 1, "{reserved}: {warnings:?}");
        assert!(
            warnings[0].contains("[models.m.header_extras]") && warnings[0].contains(reserved),
            "{reserved}: {warnings:?}"
        );
        assert!(!warnings[0].contains("secret-value"), "{warnings:?}");
    }
}

#[test]
fn auth_reserved_header_warnings_skip_a_config_without_reserved_model_headers() {
    let config = parse(
        "version = 5\n\
         [providers.p]\nformat = \"openai-chat\"\nbase_url = \"https://example.invalid\"\ncredential = \"env://ROUTECTL_TEST_ABSENT\"\nauth = \"bearer\"\n\
         [models.m]\nprovider = \"p\"\nupstream = \"u\"\n\
         [models.m.header_extras]\nhost = \"x\"\nx-app = \"ok\"\n",
    );

    assert!(super::auth_reserved_header_warnings(&config).is_empty());
}
