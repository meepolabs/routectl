use super::*;

#[cfg(test)]
mod class_policy_validation_tests {
    //! Tests for `validate_class_policy` (hard rejects) over
    //! `[retry.classes]`.

    use super::validate_class_policy;
    use crate::class_policy::ConfigFailureClass;
    use crate::config::Config;

    #[test]
    fn rejects_a_reserved_feature_unsupported_count() {
        // Arrange: `[retry.classes] feature-unsupported = 1`.
        let cfg: Config = toml::from_str("[retry.classes]\nfeature-unsupported = 1\n")
            .expect("the count shape parses");

        // Act
        let err = validate_class_policy(&cfg).unwrap_err();

        // Assert
        let msg = err.to_string();
        assert!(
            msg.contains("[retry.classes] feature-unsupported"),
            "msg: {msg}"
        );
        assert!(msg.contains("reserved"), "msg: {msg}");
    }

    #[test]
    fn every_other_class_count_validates() {
        // Arrange: a count on every class but the reserved one.
        let mut cfg = Config::default();
        for class in [
            ConfigFailureClass::RateLimited,
            ConfigFailureClass::Auth,
            ConfigFailureClass::BadRequest,
            ConfigFailureClass::ContentPolicy,
            ConfigFailureClass::ContextWindow,
            ConfigFailureClass::ServerError,
            ConfigFailureClass::Timeout,
            ConfigFailureClass::NetworkError,
            ConfigFailureClass::Overloaded,
        ] {
            cfg.retry.classes.insert(class, 2);
        }

        // Act + Assert
        validate_class_policy(&cfg).expect("a count on a non-reserved class validates");
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

    /// DNS names that merely look like loopback. Each is asserted to parse as
    /// `Host::Domain` so the case exercises the cleartext guard rather than a
    /// parser rejection or an IP-literal path.
    const LOOPBACK_LOOKALIKE_DOMAINS: [&str; 7] = [
        "http://127.evil.example/",
        "http://127.0.0.1.nip.io/v1",
        "http://127.0.0.x/",
        "http://127../",
        "http://127.0.0.1%2eevil.example/",
        "http://localhost./",
        "http://api.localhost/",
    ];

    fn assert_parses_as_domain(url: &str) {
        let parsed = url::Url::parse(url).expect("fixture must parse");
        assert!(
            matches!(parsed.host(), Some(url::Host::Domain(_))),
            "fixture {url} must parse as a DNS domain; got {:?}",
            parsed.host()
        );
    }

    fn assert_cleartext_rejected(url: &str) {
        let result = validate_base_url_scheme("p", url);
        assert!(
            result
                .as_ref()
                .is_err_and(|e| e.to_string().contains("cleartext")),
            "expected cleartext rejection for {url}; got: {result:?}"
        );
    }

    #[test]
    fn http_loopback_lookalike_domains_rejected() {
        for url in LOOPBACK_LOOKALIKE_DOMAINS {
            // Arrange
            assert_parses_as_domain(url);

            // Act + Assert
            assert_cleartext_rejected(url);
        }
    }

    #[test]
    fn https_loopback_lookalike_domains_pass() {
        for url in LOOPBACK_LOOKALIKE_DOMAINS {
            // Arrange
            let https = url.replacen("http://", "https://", 1);

            // Act
            let result = validate_base_url_scheme("p", &https);

            // Assert
            assert!(result.is_ok(), "expected {https} to pass; got: {result:?}");
        }
    }

    #[test]
    fn http_ipv4_loopback_literals_pass() {
        for url in [
            "http://127.0.0.1:8080/",
            "http://127.0.0.5/",
            "http://127.255.255.254/",
            "http://127.1/",
            "http://2130706433/",
            "http://user:pass@127.0.0.1:8080/v1",
        ] {
            let parsed = url::Url::parse(url).expect("fixture must parse");
            assert!(
                matches!(parsed.host(), Some(url::Host::Ipv4(_))),
                "fixture {url} must parse as IPv4; got {:?}",
                parsed.host()
            );

            let result = validate_base_url_scheme("p", url);

            assert!(result.is_ok(), "expected {url} to pass; got: {result:?}");
        }
    }

    #[test]
    fn http_non_loopback_ipv4_rejected() {
        for url in ["http://128.0.0.1/", "http://10.0.0.1/", "http://0.0.0.0/"] {
            assert_cleartext_rejected(url);
        }
    }

    #[test]
    fn http_native_ipv6_loopback_passes() {
        for url in [
            "http://[::1]/",
            "http://[0:0:0:0:0:0:0:1]:8080/v1",
            "http://user:pass@[::1]/",
        ] {
            let result = validate_base_url_scheme("p", url);

            assert!(result.is_ok(), "expected {url} to pass; got: {result:?}");
        }
    }

    #[test]
    fn http_ipv4_mapped_loopback_alias_passes() {
        for url in ["http://[::ffff:127.0.0.5]/", "http://[::ffff:7f00:1]/"] {
            let result = validate_base_url_scheme("p", url);

            assert!(result.is_ok(), "expected {url} to pass; got: {result:?}");
        }
    }

    /// The deprecated IPv4-compatible form (`::/96`) is not a native IPv6
    /// loopback and can follow an ordinary IPv6 route.
    #[test]
    fn http_ipv4_compatible_loopback_rejected() {
        for url in ["http://[::127.0.0.1]/", "http://[::7f00:1]:8080/"] {
            assert_cleartext_rejected(url);
        }
    }

    #[test]
    fn http_non_loopback_ipv6_rejected() {
        for url in [
            "http://[::2]/",
            "http://[::ffff:10.0.0.1]/",
            "http://[2001:db8::1]/",
        ] {
            assert_cleartext_rejected(url);
        }
    }

    #[test]
    fn http_exact_localhost_passes_case_insensitively() {
        for url in [
            "http://localhost/",
            "http://LOCALHOST:8080/",
            "http://LocalHost/v1",
            "http://user:pass@localhost:8080/",
        ] {
            let result = validate_base_url_scheme("p", url);

            assert!(result.is_ok(), "expected {url} to pass; got: {result:?}");
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

    /// NOTE what this test does and does NOT exercise. Every input here is
    /// rejected UPSTREAM of the host guard, by `url::Url::parse` (an empty
    /// authority under a special scheme is a parse error) -- so these
    /// assertions pin the rejection, not the guard, and they still pass with
    /// the guard removed. The guard is a deliberate belt-and-braces backstop:
    /// `host_str()` can only be `None` or empty under a NON-special scheme,
    /// which the scheme check already rejects, so no input can reach it
    /// today. It stays for the day `url` changes its parse behavior.
    #[test]
    fn hostless_authority_rejected() {
        let err = validate_base_url_scheme("acme", "https://").unwrap_err();
        assert!(err.to_string().contains("acme"), "got: {err}");

        for url in ["http://", "https://:443/v1", "https://@/v1"] {
            assert!(
                validate_base_url_scheme("p", url).is_err(),
                "expected rejection for hostless base_url {url}"
            );
        }
    }
}

#[cfg(test)]
#[cfg(feature = "bedrock")]
mod cloud_credential_validation_tests {
    //! A cloud entry's credential is one string the provider plan parses
    //! before anything is built: an empty or malformed AWS credential is a
    //! config error naming the provider and the `credential` key, and each
    //! accepted AWS form passes. Configs are parsed from TOML to exercise the
    //! real deserialization path an operator hits, then run through the
    //! validation suite the daemon runs.

    use crate::config::Config;
    use crate::factory::collect_config_validation;

    fn errors_of(toml_text: &str) -> Vec<String> {
        let cfg: Config = toml::from_str(toml_text).expect("config must parse");
        collect_config_validation(&cfg).errors
    }

    fn cloud_entry(name: &str, format: &str, credential: &str, auth: &str) -> String {
        format!(
            "[providers.{name}]\nformat = \"{format}\"\n\
             cloud = {{ name = \"aws\", region = \"us-west-2\" }}\n\
             credential = \"{credential}\"\nauth = \"{auth}\"\n"
        )
    }

    #[test]
    fn an_empty_or_malformed_cloud_credential_is_refused_naming_the_key() {
        let rows = [
            (
                "empty bearer on native bedrock",
                "bedrock-invoke",
                "",
                "bearer",
            ),
            (
                "empty bearer on the managed surface",
                "openai-chat",
                "",
                "bearer",
            ),
            ("empty sigv4", "bedrock-converse", "", "aws-sigv4"),
            (
                "profile with no name",
                "bedrock-invoke",
                "aws://profile/",
                "aws-sigv4",
            ),
            (
                "unknown aws form",
                "bedrock-invoke",
                "aws://static",
                "aws-sigv4",
            ),
        ];
        for (label, format, credential, auth) in rows {
            let errors = errors_of(&cloud_entry("cloudy", format, credential, auth));

            assert_eq!(errors.len(), 1, "{label}: {errors:?}");
            assert!(
                errors[0].contains("provider `cloudy`"),
                "{label}: {errors:?}"
            );
            assert!(errors[0].contains("`credential`"), "{label}: {errors:?}");
        }
    }

    /// Positive control for the rows above: every accepted AWS credential
    /// form, on the native and managed formats, validates clean.
    #[test]
    fn each_accepted_cloud_credential_form_passes() {
        let text = [
            cloud_entry(
                "bearer-native",
                "bedrock-invoke",
                "env://AWS_BEARER_TOKEN_BEDROCK",
                "bearer",
            ),
            cloud_entry(
                "bearer-managed",
                "anthropic-messages",
                "file:///tmp/whatever",
                "bearer",
            ),
            cloud_entry(
                "profile-native",
                "bedrock-invoke",
                "aws://profile/bedrock-prod",
                "aws-sigv4",
            ),
            cloud_entry(
                "chain-native",
                "bedrock-converse",
                "aws://chain",
                "aws-sigv4",
            ),
            cloud_entry("chain-managed", "openai-chat", "aws://chain", "aws-sigv4"),
        ]
        .concat();

        let errors = errors_of(&text);

        assert!(
            errors.is_empty(),
            "every accepted form must pass: {errors:?}"
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
    fn validate_alias_chain_targets_accepts_every_configured_model() {
        let models = vec![
            ("haiku", ModelEntry::new("anthropic", "claude-haiku")),
            ("staged", ModelEntry::new("anthropic", "claude-staged")),
        ];
        let cfg = config_with(
            models.clone(),
            vec![
                ("fast", AliasValue::Single("staged".into())),
                (
                    "both",
                    AliasValue::Chain(vec!["staged".into(), "haiku".into()]),
                ),
            ],
        );
        validate_alias_chain_targets(&cfg).expect("an alias may name any configured model");

        let control = config_with(models, vec![("fast", AliasValue::Single("absent".into()))]);
        assert!(
            validate_alias_chain_targets(&control).is_err(),
            "control: the same fixture refuses a target outside [models]"
        );
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
        // Two unrelated misconfigurations -- aliases reference unknown
        // nicknames, another alias has an empty chain. The
        // validator must surface BOTH in a single error so the
        // operator doesn't fix one and discover the other on the
        // next run.
        let cfg = config_with(
            vec![("haiku", ModelEntry::new("anthropic", "claude-haiku"))],
            vec![
                ("alpha", AliasValue::Single("missing-1".into())),
                ("beta", AliasValue::Chain(vec![])),
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
        assert!(msg.contains("alias `beta`: chain is empty"), "msg: {msg}");
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
    //! always `Ok`; presence rejects a `listen_port` that collides with
    //! `[server] port`.

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
}

#[cfg(test)]
mod forwarded_credential_validation_tests {
    //! Config-BOUNDARY tests (parse via `toml::from_str`, not bare-struct
    //! construction only) for the `credential = "forwarded"` rule the
    //! provider plan enforces and the validation suite reports: the
    //! forwarded seat is accepted only on `anthropic-messages` with
    //! `auth = "bearer"` and a base_url on `api.anthropic.com`; an entry with
    //! its own credential needs a non-empty one.

    use super::collect_config_validation;
    use crate::config::Config;

    fn errors_of(toml_text: &str) -> Vec<String> {
        let cfg: Config = toml::from_str(toml_text).expect("config must parse");
        collect_config_validation(&cfg).errors
    }

    /// The four-line forwarded block parses and validates clean: the
    /// positive control for every refusal below.
    #[test]
    fn forwarded_block_parses_and_validates() {
        let errors = errors_of(
            r#"
[providers.anthropic-forwarded]
format = "anthropic-messages"
base_url = "https://api.anthropic.com"
credential = "forwarded"
auth = "bearer"
"#,
        );
        assert!(errors.is_empty(), "clean forwarded block: {errors:?}");
    }

    /// An entry with its own credential validates as before.
    #[test]
    fn own_credential_block_validates() {
        let errors = errors_of(
            r#"
[providers.anthropic]
format = "anthropic-messages"
base_url = "https://api.anthropic.com"
credential = "env://ANTHROPIC_API_KEY"
auth = "x-api-key"
"#,
        );
        assert!(errors.is_empty(), "own-credential block: {errors:?}");
    }

    /// Each row changes one key of the clean forwarded block; each is refused
    /// naming the provider and the pinned host.
    #[test]
    fn rejects_forwarded_off_its_pin() {
        let rows = [
            (
                "non-anthropic host",
                "anthropic-messages",
                "https://evil.example.com",
                "bearer",
            ),
            (
                "x-api-key auth",
                "anthropic-messages",
                "https://api.anthropic.com",
                "x-api-key",
            ),
            (
                "openai-chat format",
                "openai-chat",
                "https://api.anthropic.com",
                "bearer",
            ),
        ];
        for (label, format, base_url, auth) in rows {
            let errors = errors_of(&format!(
                "[providers.sneaky]\nformat = \"{format}\"\nbase_url = \"{base_url}\"\n\
                 credential = \"forwarded\"\nauth = \"{auth}\"\n"
            ));
            assert_eq!(errors.len(), 1, "{label}: {errors:?}");
            assert!(errors[0].contains("sneaky"), "{label}: {errors:?}");
            assert!(
                errors[0].contains("api.anthropic.com"),
                "{label}: {errors:?}"
            );
        }
    }

    #[test]
    fn rejects_an_empty_credential() {
        let errors = errors_of(
            r#"
[providers.no-key]
format = "anthropic-messages"
base_url = "https://api.anthropic.com"
credential = ""
auth = "x-api-key"
"#,
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("no-key"), "{errors:?}");
        assert!(errors[0].contains("`credential`"), "{errors:?}");
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
        let errors = errors_of(
            r#"
[providers.sneaky]
format = "anthropic-messages"
base_url = "https://user:sk-live-LEAKED@internal.example/v1?token=sk-query-LEAKED"
credential = "forwarded"
auth = "bearer"
"#,
        );
        assert_eq!(
            errors.len(),
            2,
            "forwarded pin and plain-URL rule: {errors:?}"
        );
        for secret in [
            "sk-live-LEAKED",
            "sk-query-LEAKED",
            "internal.example",
            "/v1",
            "user:",
            "https://user",
        ] {
            for msg in &errors {
                assert!(
                    !msg.contains(secret),
                    "operator-supplied `{secret}` must not surface; got: {msg}"
                );
            }
        }
        let msg = errors
            .iter()
            .find(|e| e.contains("forwarded"))
            .expect("the forwarded pin refuses the entry");

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

    /// Structural tripwire over the whole of `validate.rs`, `plan.rs` and
    /// `config/entry_shape.rs`: no
    /// error message may interpolate the operator-supplied `base_url`. That
    /// value can carry a credential in userinfo, a path, or a query, and every
    /// validator error string reaches `/status/doctor` as a serialized
    /// `Finding.detail`.
    ///
    /// Each file keeps its tests in a sibling `#[path]` file, so the scan
    /// needs no production/test slicing. The needle is assembled from
    /// fragments so this test's own source line cannot satisfy the scan it
    /// performs; the premise rows prove each scanned text is the real file.
    #[test]
    fn no_validator_message_interpolates_the_raw_base_url() {
        for (file, src, premise) in [
            (
                "validate.rs",
                include_str!("validate.rs"),
                "pub fn collect_config_validation",
            ),
            ("plan.rs", include_str!("plan.rs"), "pub fn plan_provider"),
            (
                "entry_shape.rs",
                include_str!("../config/entry_shape.rs"),
                "pub fn shape_refusals",
            ),
        ] {
            assert!(src.contains(premise), "premise: {file} is the real source");
            assert!(
                !src.contains(concat!("{", "base_url")),
                "{file}: a validator message interpolates the raw base_url; that string may \
                 carry a credential and every validator error reaches /status/doctor as \
                 Finding.detail. Name the provider and the violated invariant instead, and \
                 withhold the value"
            );
        }
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

    /// The reserved `[retry.classes] feature-unsupported` count -- trips
    /// `validate_class_policy`.
    fn reserved_class_override_config() -> Config {
        toml::from_str("[retry.classes]\nfeature-unsupported = 1\n").expect("must parse")
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

    /// A capability override cell carrying contradictory verdicts (an
    /// `unsupported` entry routes a capability away while a
    /// `force_supported` entry marks it supported) -- trips
    /// `validate_capability_overrides`.
    fn contradictory_capability_override_config() -> Config {
        toml::from_str(
            "[providers.p]\n\
             format = \"openai-chat\"\n\
             base_url = \"https://x\"\n\
             credential = \"env://K\"\n\
             auth = \"bearer\"\n\
             [capability.overrides.p]\n\
             unsupported = [\"web_search\"]\n\
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

    /// Config surface for a `[capability] essential` list. Parsed through
    /// the real serde path so the operator-written TOML shape is what the
    /// validator sees.
    fn essential_config(entries: &str) -> Config {
        toml::from_str(&format!("[capability]\nessential = [{entries}]\n"))
            .expect("fixture must parse")
    }

    #[test]
    fn collects_the_unknown_essential_capability_error() {
        let validation = collect_config_validation(&essential_config("\"advisorr\""));

        assert_eq!(
            validation.errors.len(),
            1,
            "exactly one validator should fire: {:?}",
            validation.errors
        );
        assert!(
            validation.errors[0].contains("advisorr") && validation.errors[0].contains("unknown"),
            "error should name the unknown capability: {}",
            validation.errors[0]
        );
    }

    /// A trailing space is invisible in prose, so the rejection has to show
    /// the key's bounds AND say which accepted key it is a variant of --
    /// otherwise the operator reads an error naming what looks like the key
    /// they wrote.
    #[test]
    fn trailing_space_essential_entry_is_quoted_and_suggests_the_trimmed_key() {
        let validation = collect_config_validation(&essential_config("\"advisor \""));

        assert_eq!(
            validation.errors.len(),
            1,
            "exactly one validator should fire: {:?}",
            validation.errors
        );
        assert!(
            validation.errors[0].contains("`advisor `"),
            "error should quote the raw key so the trailing space is visible: {}",
            validation.errors[0]
        );
        assert!(
            validation.errors[0].contains("did you mean `advisor`?")
                && validation.errors[0].contains("whitespace"),
            "error should suggest the trimmed key and name the difference: {}",
            validation.errors[0]
        );
    }

    #[test]
    fn case_folded_essential_entry_suggests_the_canonical_key() {
        let validation = collect_config_validation(&essential_config("\"Advisor\""));

        assert_eq!(
            validation.errors.len(),
            1,
            "exactly one validator should fire: {:?}",
            validation.errors
        );
        assert!(
            validation.errors[0].contains("did you mean `advisor`?")
                && validation.errors[0].contains("letter case"),
            "error should suggest the canonical key and name the difference: {}",
            validation.errors[0]
        );
    }

    /// The suggestion covers well-known keys too, not just the droppables --
    /// and both defects at once are named as one difference.
    #[test]
    fn whitespace_and_case_essential_entry_suggests_a_well_known_key() {
        let validation = collect_config_validation(&essential_config("\" Web_Search \""));

        assert_eq!(
            validation.errors.len(),
            1,
            "exactly one validator should fire: {:?}",
            validation.errors
        );
        assert!(
            validation.errors[0].contains("` Web_Search `")
                && validation.errors[0].contains("did you mean `web_search`?"),
            "error should quote the raw key and suggest the canonical one: {}",
            validation.errors[0]
        );
    }

    /// A misspelling is already visible in the quoted key, so it earns no
    /// guess -- the hint is reserved for defects the operator cannot see.
    #[test]
    fn misspelled_essential_entry_gets_no_suggestion() {
        let validation = collect_config_validation(&essential_config("\"advisorr\""));

        assert_eq!(
            validation.errors.len(),
            1,
            "exactly one validator should fire: {:?}",
            validation.errors
        );
        assert!(
            !validation.errors[0].contains("did you mean"),
            "a visible misspelling must not gain a suggestion: {}",
            validation.errors[0]
        );
    }

    /// `reasoning_replay` is deliberately outside the accept set, so no
    /// near-miss of it may ever be offered as a fix.
    #[test]
    fn reasoning_replay_variant_is_never_suggested() {
        let validation = collect_config_validation(&essential_config("\"Reasoning_Replay\""));

        assert_eq!(
            validation.errors.len(),
            1,
            "exactly one validator should fire: {:?}",
            validation.errors
        );
        assert!(
            !validation.errors[0].contains("did you mean"),
            "a rejected-by-design key must never be suggested: {}",
            validation.errors[0]
        );
    }

    #[test]
    fn collects_the_reasoning_replay_essential_carve_out_error() {
        let validation = collect_config_validation(&essential_config("\"reasoning_replay\""));

        assert_eq!(
            validation.errors.len(),
            1,
            "exactly one validator should fire: {:?}",
            validation.errors
        );
        assert!(
            validation.errors[0].contains("reasoning_replay")
                && validation.errors[0].contains("replay path"),
            "error should name the lane-managed replay carve-out: {}",
            validation.errors[0]
        );
    }

    #[test]
    fn accepts_known_essential_capabilities_including_already_essential_ones() {
        // web_search already routes away by the baked table, so listing it
        // is an idempotent declaration rather than an error; advisor and
        // context_management are the droppables the list actually tightens.
        let validation = collect_config_validation(&essential_config(
            "\"web_search\", \"advisor\", \"context_management\"",
        ));

        assert!(
            validation.errors.is_empty(),
            "known capability keys must be accepted: {:?}",
            validation.errors
        );
    }

    /// `forced_tool_choice` is neither strippable nor a typed tool, so only its
    /// well-known membership admits it to the list.
    #[test]
    fn accepts_forced_tool_choice_as_an_essential_capability() {
        let validation = collect_config_validation(&essential_config("\"forced_tool_choice\""));

        assert!(
            validation.errors.is_empty(),
            "a well-known capability key must be accepted: {:?}",
            validation.errors
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
            .any(|e| e.contains("base_url is set but empty"))
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
    fn rejects_empty_base_url_openai_compat() {
        let validation = collect_config_validation(&parse(
            "[providers.p]\nformat = \"openai-chat\"\nbase_url = \"\"\ncredential = \"env://K\"\nauth = \"bearer\"\n",
        ));
        assert!(
            has_base_url_error(&validation),
            "empty openai-compat base_url must be rejected: {:?}",
            validation.errors
        );
    }

    #[test]
    fn rejects_empty_base_url_anthropic_api() {
        let validation = collect_config_validation(&parse(
            "[providers.p]\nformat = \"anthropic-messages\"\nbase_url = \"\"\ncredential = \"env://K\"\nauth = \"x-api-key\"\n",
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
            "[providers.p]\nformat = \"gemini\"\nbase_url = \"\"\ncredential = \"env://K\"\nauth = \"x-goog-api-key\"\n",
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
            "[providers.p]\nformat = \"openai-responses\"\nbase_url = \"\"\ncredential = \"env://K\"\nauth = \"bearer\"\n",
        ));
        assert!(
            has_base_url_error(&validation),
            "present-but-empty openai-responses base_url must be rejected: {:?}",
            validation.errors
        );
    }

    #[cfg(feature = "openai-responses")]
    #[test]
    fn a_set_base_url_openai_responses_stays_valid() {
        let validation = collect_config_validation(&parse(
            "[providers.p]\nformat = \"openai-responses\"\nbase_url = \"https://api.openai.com/v1\"\ncredential = \"env://K\"\nauth = \"bearer\"\n",
        ));
        assert!(
            validation.errors.is_empty(),
            "a set openai-responses base_url must stay valid: {:?}",
            validation.errors
        );
    }

    /// Drift tripwire: every schema leaf whose `type` includes `number`
    /// (float leaves; integers are out of scope) must be registered in the
    /// covered set of `validate_float_fields` or, for `PricingConfig`,
    /// `validate_model_pricing`. A future f64/f32 leaf added
    /// to the config without registering it here fails this test.
    const COVERED_FLOAT_LEAVES: [&str; 6] = [
        "RetryPolicy.backoff_multiplier",
        "PricingConfig.input_per_mtok",
        "PricingConfig.output_per_mtok",
        "PricingConfig.cache_read_per_mtok",
        "PricingConfig.cache_write_5m_per_mtok",
        "PricingConfig.cache_write_1h_per_mtok",
    ];

    fn committed_schema() -> serde_json::Value {
        let committed = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../routectl.schema.json"
        ));
        serde_json::from_str(committed).expect("committed schema parses")
    }

    /// Collects every `anyOf`/`oneOf` branch in the schema that declares a
    /// `number` type, as `<json-pointer-ish path>` strings. The float-leaf
    /// coverage walker reads `type` off the property node directly, so any
    /// such wrapper would drop a float leaf out of its covered-set check
    /// silently. Empty result = the walker's flat-`type` assumption holds.
    fn wrapped_number_leaf_paths(schema: &serde_json::Value) -> Vec<String> {
        fn declares_number(node: &serde_json::Value) -> bool {
            match node.get("type") {
                Some(serde_json::Value::String(s)) => s == "number",
                Some(serde_json::Value::Array(items)) => {
                    items.iter().any(|v| v.as_str() == Some("number"))
                }
                _ => false,
            }
        }

        fn walk(node: &serde_json::Value, path: &str, found: &mut Vec<String>) {
            match node {
                serde_json::Value::Object(map) => {
                    for keyword in ["anyOf", "oneOf"] {
                        let Some(branches) = map.get(keyword).and_then(serde_json::Value::as_array)
                        else {
                            continue;
                        };
                        for (index, branch) in branches.iter().enumerate() {
                            if declares_number(branch) {
                                found.push(format!("{path}/{keyword}/{index}"));
                            }
                        }
                    }
                    for (key, value) in map {
                        walk(value, &format!("{path}/{key}"), found);
                    }
                }
                serde_json::Value::Array(items) => {
                    for (index, item) in items.iter().enumerate() {
                        walk(item, &format!("{path}/{index}"), found);
                    }
                }
                _ => {}
            }
        }

        let mut found = Vec::new();
        walk(schema, "", &mut found);
        found
    }

    /// Drift tripwire for the tripwire: `float_leaf_coverage_matches_schema`
    /// reads `type` straight off each property node. Today schemars emits
    /// every float leaf that way (`"number"` or `["number","null"]`), and
    /// its `anyOf` wrappers only ever hold a `$ref` plus `null`. If a
    /// schemars upgrade starts wrapping number leaves, the coverage walker
    /// would go quiet instead of red -- this fails loudly instead, and the
    /// fix is to teach that walker to recurse.
    #[test]
    fn schema_has_no_anyof_wrapped_number_leaves() {
        let wrapped = wrapped_number_leaf_paths(&committed_schema());

        assert!(
            wrapped.is_empty(),
            "schema now wraps number leaves in anyOf/oneOf, which the \
             float-leaf coverage walker cannot see: {wrapped:?}"
        );
    }

    /// Negative control for the guard above: an assertion that only ever
    /// sees a clean schema proves nothing, so inject the exact shape it
    /// exists to catch and confirm it is reported.
    #[test]
    fn wrapped_number_leaf_detector_reports_an_injected_wrapper() {
        let mut schema = committed_schema();
        schema["$defs"]["PricingConfig"]["properties"]["synthetic_rate"] = serde_json::json!({
            "anyOf": [{ "type": "number", "format": "double" }, { "type": "null" }],
        });

        let wrapped = wrapped_number_leaf_paths(&schema);

        assert_eq!(
            wrapped,
            vec!["/$defs/PricingConfig/properties/synthetic_rate/anyOf/0".to_string()],
            "an anyOf-wrapped number leaf must be reported"
        );
    }

    #[test]
    fn float_leaf_coverage_matches_schema() {
        use std::collections::BTreeSet;

        let schema = committed_schema();

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
mod cloud_carrier_validation_tests {
    //! Config-BOUNDARY tests (parse via `toml::from_str`) for entries over
    //! the AWS cloud carrier on the managed formats (`anthropic-messages`,
    //! `openai-chat`, `openai-responses`). The `cloud` table selects the
    //! carrier; the endpoint derives from its region and the credential is an
    //! AWS form or a bearer key reference. Every row runs the whole
    //! validation suite the daemon runs, and each refusal row is the clean
    //! row with one key changed.

    use super::collect_config_validation;
    use crate::config::Config;

    fn errors_of(toml_text: &str) -> Vec<String> {
        let cfg: Config = toml::from_str(toml_text).expect("config must parse");
        collect_config_validation(&cfg).errors
    }

    fn managed_formats() -> Vec<&'static str> {
        #[cfg_attr(not(feature = "openai-responses"), allow(unused_mut))]
        let mut formats = vec!["anthropic-messages", "openai-chat"];
        #[cfg(feature = "openai-responses")]
        formats.push("openai-responses");
        formats
    }

    fn cloud_entry(format: &str, region: &str, credential: &str, auth: &str) -> String {
        format!(
            "[providers.mantle]\nformat = \"{format}\"\n\
             cloud = {{ name = \"aws\", region = \"{region}\" }}\n\
             credential = \"{credential}\"\nauth = \"{auth}\"\n"
        )
    }

    /// The positive control for every refusal below: each managed format
    /// over the cloud, on the default chain and on a bearer key, validates
    /// clean.
    #[test]
    fn each_managed_format_over_the_cloud_validates() {
        for format in managed_formats() {
            for (credential, auth) in [
                ("aws://chain", "aws-sigv4"),
                ("env://AWS_BEARER_TOKEN_BEDROCK", "bearer"),
            ] {
                let errors = errors_of(&cloud_entry(format, "eu-west-1", credential, auth));
                assert!(errors.is_empty(), "{format} {auth}: {errors:?}");
            }
        }
    }

    /// `auth` must match the credential kind: `aws-sigv4` signs with an
    /// `aws://` credential, and an `aws://` credential needs `aws-sigv4`.
    #[test]
    fn rejects_an_auth_that_does_not_match_the_credential() {
        for format in managed_formats() {
            for (credential, auth) in [
                ("aws://chain", "bearer"),
                ("env://AWS_BEARER_TOKEN_BEDROCK", "aws-sigv4"),
            ] {
                let errors = errors_of(&cloud_entry(format, "us-east-1", credential, auth));
                assert_eq!(errors.len(), 1, "{format} {credential} {auth}: {errors:?}");
                assert!(errors[0].contains("mantle"), "{errors:?}");
                assert!(errors[0].contains("aws-sigv4"), "{errors:?}");
            }
        }
    }

    /// An auth no AWS route accepts is refused by the descriptor.
    #[test]
    fn rejects_an_auth_the_cloud_route_does_not_accept() {
        let errors = errors_of(&cloud_entry(
            "anthropic-messages",
            "us-east-1",
            "env://AWS_BEARER_TOKEN_BEDROCK",
            "x-api-key",
        ));
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("auth = \"x-api-key\""), "{errors:?}");
        assert!(errors[0].contains("`cloud` aws"), "{errors:?}");
    }

    /// The forwarded seat never rides the cloud.
    #[test]
    fn rejects_a_forwarded_credential_over_the_cloud() {
        let errors = errors_of(&cloud_entry(
            "anthropic-messages",
            "us-east-1",
            "forwarded",
            "bearer",
        ));
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("mantle"), "{errors:?}");
        assert!(errors[0].contains("forwarded"), "{errors:?}");
    }

    /// `base_url` and `cloud` are two carriers; an entry names one.
    #[test]
    fn rejects_a_cloud_entry_that_also_sets_base_url() {
        for format in managed_formats() {
            let text = format!(
                "{}base_url = \"https://example.invalid/v1\"\n",
                cloud_entry(format, "us-east-1", "aws://chain", "aws-sigv4")
            );
            let errors = errors_of(&text);
            assert_eq!(errors.len(), 1, "{format}: {errors:?}");
            assert!(
                errors[0].contains("exactly one of `base_url` or `cloud`"),
                "{format}: {errors:?}"
            );
        }
    }

    /// A region that fails admission is refused naming `cloud.region`.
    #[test]
    fn rejects_an_empty_region() {
        for format in managed_formats() {
            let errors = errors_of(&cloud_entry(format, "   ", "aws://chain", "aws-sigv4"));
            assert!(
                errors
                    .iter()
                    .any(|e| e.contains("mantle") && e.contains("cloud.region")),
                "{format}: {errors:?}"
            );
        }
    }

    /// `account_id_ref` belongs to the Codex host; a cloud Responses entry
    /// refuses it.
    #[cfg(feature = "openai-responses")]
    #[test]
    fn responses_rejects_a_cloud_entry_with_account_id_ref() {
        let text = format!(
            "{}account_id_ref = \"env://ACCOUNT\"\n",
            cloud_entry("openai-responses", "us-east-1", "aws://chain", "aws-sigv4")
        );
        let errors = errors_of(&text);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("account_id_ref"), "{errors:?}");
    }

    /// The Responses `store` flag is forced off over the cloud; a `store`
    /// key in payload_extras is refused at config load. The same key on a
    /// base-URL Responses entry is the positive control.
    #[cfg(feature = "openai-responses")]
    #[test]
    fn responses_rejects_a_cloud_entry_with_a_store_payload_extra() {
        let cloud = format!(
            "{}payload_extras = {{ store = true }}\n",
            cloud_entry("openai-responses", "us-east-1", "aws://chain", "aws-sigv4")
        );
        let base_url = "[providers.mantle]\nformat = \"openai-responses\"\n\
                        base_url = \"https://api.openai.com/v1\"\n\
                        credential = \"env://OPENAI_API_KEY\"\nauth = \"bearer\"\n\
                        payload_extras = { store = true }\n";

        let refused = errors_of(&cloud);
        let accepted = errors_of(base_url);

        assert!(accepted.is_empty(), "base-URL control: {accepted:?}");
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert!(refused[0].contains("store"), "{refused:?}");
    }

    /// An entry that names neither carrier is refused, whatever its format.
    #[test]
    fn rejects_an_entry_with_neither_base_url_nor_cloud() {
        let errors = errors_of(
            "[providers.plain]\nformat = \"openai-chat\"\n\
             credential = \"env://KEY\"\nauth = \"bearer\"\n",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("exactly one of `base_url` or `cloud`"),
            "{errors:?}"
        );
    }
}

#[cfg(all(test, not(feature = "bedrock")))]
mod cloud_feature_off_tests {
    //! With the `bedrock` feature off, the cloud carrier still parses (the
    //! entry shape is the same in every build), and the plan refuses it as
    //! a route this build does not have. A base-URL entry of the same format
    //! is the positive control.

    use crate::config::Config;
    use crate::factory::collect_config_validation;

    #[test]
    fn a_cloud_entry_is_refused_as_unavailable_when_feature_off() {
        let mut formats = vec![
            (
                "anthropic-messages",
                "https://api.anthropic.com",
                "x-api-key",
            ),
            ("openai-chat", "https://example.invalid/v1", "bearer"),
        ];
        if cfg!(feature = "openai-responses") {
            formats.push(("openai-responses", "https://api.openai.com/v1", "bearer"));
        }
        for (format, base_url, auth) in formats {
            let cloud: Config = toml::from_str(&format!(
                "[providers.mantle]\nformat = \"{format}\"\n\
                 cloud = {{ name = \"aws\", region = \"us-east-1\" }}\n\
                 credential = \"env://KEY\"\nauth = \"bearer\"\n"
            ))
            .expect("the cloud carrier parses in every build");
            let plain: Config = toml::from_str(&format!(
                "[providers.mantle]\nformat = \"{format}\"\nbase_url = \"{base_url}\"\n\
                 credential = \"env://KEY\"\nauth = \"{auth}\"\n"
            ))
            .expect("base-URL control parses");

            let refused = collect_config_validation(&cloud).errors;
            let accepted = collect_config_validation(&plain).errors;

            assert!(accepted.is_empty(), "{format} control: {accepted:?}");
            assert_eq!(refused.len(), 1, "{format}: {refused:?}");
            assert!(
                refused[0].contains("is not available over `cloud` aws in this build"),
                "{format}: {refused:?}"
            );
        }
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
            "[providers.a]\nformat = \"openai-responses\"\nbase_url = \"https://chatgpt.com/backend-api/codex\"\ncredential = \"oauth://codex\"\nauth = \"bearer\"\naccount_id_ref = \"env://ACCT\"\n",
        );
        assert_eq!(resolved_codex_version(&config), None);
        assert!(validate_codex_version(&config).is_ok());
    }

    #[test]
    fn single_configured_value_resolves_and_passes() {
        let config = parse(
            "[providers.a]\nformat = \"openai-responses\"\nbase_url = \"https://chatgpt.com/backend-api/codex\"\ncredential = \"oauth://codex\"\nauth = \"bearer\"\naccount_id_ref = \"env://ACCT\"\n\
             codex_version = \"0.200.0\"\n",
        );
        assert_eq!(resolved_codex_version(&config).as_deref(), Some("0.200.0"));
        assert!(validate_codex_version(&config).is_ok());
    }

    #[test]
    fn matching_values_across_providers_pass() {
        let config = parse(
            "[providers.a]\nformat = \"openai-responses\"\nbase_url = \"https://chatgpt.com/backend-api/codex\"\ncredential = \"oauth://codex\"\nauth = \"bearer\"\naccount_id_ref = \"env://A\"\ncodex_version = \"0.200.0\"\n\
             [providers.b]\nformat = \"openai-responses\"\nbase_url = \"https://chatgpt.com/backend-api/codex\"\ncredential = \"oauth://codex2\"\nauth = \"bearer\"\naccount_id_ref = \"env://B\"\ncodex_version = \"0.200.0\"\n",
        );
        assert!(validate_codex_version(&config).is_ok());
    }

    #[test]
    fn divergent_values_error_naming_both_providers() {
        let config = parse(
            "[providers.alpha]\nformat = \"openai-responses\"\nbase_url = \"https://chatgpt.com/backend-api/codex\"\ncredential = \"oauth://codex\"\nauth = \"bearer\"\naccount_id_ref = \"env://A\"\ncodex_version = \"0.200.0\"\n\
             [providers.beta]\nformat = \"openai-responses\"\nbase_url = \"https://chatgpt.com/backend-api/codex\"\ncredential = \"oauth://codex2\"\nauth = \"bearer\"\naccount_id_ref = \"env://B\"\ncodex_version = \"0.201.0\"\n",
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
            "[providers.a]\nformat = \"openai-responses\"\nbase_url = \"https://chatgpt.com/backend-api/codex\"\ncredential = \"oauth://codex\"\nauth = \"bearer\"\naccount_id_ref = \"env://A\"\ncodex_version = \"{version}\"\n"
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
            "[providers.a]\nformat = \"openai-responses\"\nbase_url = \"https://chatgpt.com/backend-api/codex\"\ncredential = \"oauth://codex\"\nauth = \"bearer\"\naccount_id_ref = \"env://A\"\ncodex_version = \"0.200.0\"\n\
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
            "[providers.a]\nformat = \"openai-responses\"\nbase_url = \"https://chatgpt.com/backend-api/codex\"\ncredential = \"oauth://codex\"\nauth = \"bearer\"\naccount_id_ref = \"env://A\"\ncodex_version = \"0.200.0\"\n\
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
            "[providers.a]\nformat = \"openai-responses\"\nbase_url = \"https://api.openai.com/v1\"\ncredential = \"env://KEY\"\nauth = \"bearer\"\n[providers.a.header_extras]\nversion = \"0.999.0\"\n",
        );
        assert!(
            codex_identity_warnings(&config).is_empty(),
            "api-key surface must not warn on a version header"
        );
    }

    #[test]
    fn absent_override_does_not_warn() {
        let config = parse(
            "[providers.a]\nformat = \"openai-responses\"\nbase_url = \"https://chatgpt.com/backend-api/codex\"\ncredential = \"oauth://codex\"\nauth = \"bearer\"\naccount_id_ref = \"env://A\"\ncodex_version = \"0.200.0\"\n",
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

    /// A Bedrock provider of `format` plus one model on `upstream`.
    fn config_with(format: &str, upstream: &str) -> Config {
        let toml_text = format!(
            "[providers.aws]\n\
             format = \"{format}\"\n\
             cloud = {{ name = \"aws\", region = \"us-west-2\" }}\n\
             credential = \"aws://chain\"\n\
             auth = \"aws-sigv4\"\n\
             [models.seat]\n\
             provider = \"aws\"\n\
             upstream = \"{upstream}\"\n"
        );
        toml::from_str(&toml_text).expect("config must parse")
    }

    #[test]
    fn rejects_a_non_anthropic_model_on_the_invoke_format() {
        // Arrange
        let config = config_with("bedrock-invoke", "meta.llama3-70b-instruct-v1:0");

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
    fn rejects_a_second_non_anthropic_vendor_on_the_invoke_format() {
        let config = config_with("bedrock-invoke", "mistral.mistral-large-2402-v1:0");

        let result = validate_bedrock_invoke_model_family(&config);

        let message = result
            .expect_err("every non-Anthropic vendor is gated on invoke")
            .to_string();
        assert!(
            message.contains("mistral.mistral-large-2402-v1:0"),
            "message must name the model id: {message}"
        );
    }

    #[test]
    fn accepts_a_non_anthropic_model_on_the_converse_format() {
        let config = config_with("bedrock-converse", "meta.llama3-70b-instruct-v1:0");

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
            let config = config_with("bedrock-invoke", upstream);

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
            "bedrock-invoke",
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
            "bedrock-invoke",
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
            "bedrock-invoke",
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
             format = \"openai-chat\"\n\
             base_url = \"https://x\"\n\
             credential = \"env://K\"\n\
             auth = \"bearer\"\n\
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
        let config = config_with("bedrock-invoke", "meta.llama3-70b-instruct-v1:0");

        let errors = collect_config_validation(&config).errors;

        assert!(
            errors
                .iter()
                .any(|e| e.contains("meta.llama3-70b-instruct-v1:0")),
            "the collected suite must surface the error: {errors:?}"
        );
    }
}

#[cfg(test)]
mod pool_validation_tests {
    //! Every `[pools.<name>]` rejection class, table-driven, plus proof that
    //! each fires through the collected suite every config surface routes
    //! through -- `config check`, the serve gate, the hot-reload gate, and
    //! doctor's config section all read that one collection.

    use super::{MAX_POOL_MEMBERS, collect_config_validation, validate_pools};
    use crate::config::Config;

    fn parse(text: &str) -> Config {
        toml::from_str(text).expect("fixture config parses")
    }

    /// Two same-kind OAuth accounts, the shape a valid pool groups.
    const OAUTH_ACCOUNTS: &str = "[providers.anthropic-default]\n\
         format = \"anthropic-messages\"\n\
         base_url = \"https://api.anthropic.com\"\n\
         credential = \"oauth://anthropic\"\n\
         auth = \"bearer\"\n\
         [providers.anthropic-work]\n\
         format = \"anthropic-messages\"\n\
         base_url = \"https://api.anthropic.com\"\n\
         credential = \"oauth://anthropic#work\"\n\
         auth = \"bearer\"\n";

    /// One rejection class: a config that trips it and the substrings its
    /// error must carry.
    struct Case {
        name: &'static str,
        config: String,
        expect: &'static [&'static str],
    }

    fn cases() -> Vec<Case> {
        #[cfg_attr(not(feature = "openai-responses"), allow(unused_mut))]
        let mut cases = vec![
            Case {
                name: "unknown-member",
                config: format!(
                    "{OAUTH_ACCOUNTS}\
                     [pools.anthropic]\n\
                     members = [\"anthropic-default\", \"ghost\"]\n"
                ),
                expect: &["ghost", "not a known provider entry"],
            },
            Case {
                name: "member-in-two-pools",
                config: format!(
                    "{OAUTH_ACCOUNTS}\
                     [pools.first]\n\
                     members = [\"anthropic-default\"]\n\
                     [pools.second]\n\
                     members = [\"anthropic-default\"]\n"
                ),
                expect: &["anthropic-default", "at most one pool"],
            },
            Case {
                name: "api-key-member",
                config: "[providers.keyed]\n\
                     format = \"openai-chat\"\n\
                     base_url = \"https://api.example.invalid\"\n\
                     credential = \"env://SOME_KEY\"\n\
                     auth = \"bearer\"\n\
                     [pools.keyed-pool]\n\
                     members = [\"keyed\"]\n"
                    .to_string(),
                expect: &["keyed", "oauth://", "this release"],
            },
            Case {
                name: "pool-provider-name-collision",
                config: format!(
                    "{OAUTH_ACCOUNTS}\
                     [providers.anthropic]\n\
                     format = \"anthropic-messages\"\n\
                     base_url = \"https://api.anthropic.com\"\n\
                     credential = \"oauth://anthropic\"\n\
                     auth = \"bearer\"\n\
                     [pools.anthropic]\n\
                     members = [\"anthropic-default\"]\n"
                ),
                expect: &["anthropic", "ONE namespace"],
            },
            Case {
                name: "empty-members",
                config: "[pools.empty]\nmembers = []\n".to_string(),
                expect: &["empty", "serves no traffic"],
            },
            Case {
                name: "pool-nickname-collision",
                config: format!(
                    "{OAUTH_ACCOUNTS}\
                     [models.anthropic]\n\
                     provider = \"anthropic-default\"\n\
                     upstream = \"claude-opus-4-7\"\n\
                     [pools.anthropic]\n\
                     members = [\"anthropic-default\"]\n"
                ),
                expect: &["anthropic", "model nickname"],
            },
            Case {
                name: "pool-name-collides-with-a-generated-account-name",
                config: format!(
                    "{OAUTH_ACCOUNTS}\
                     [pools.anthropic-work]\n\
                     members = [\"anthropic-default\"]\n"
                ),
                expect: &["anthropic-work", "ONE namespace"],
            },
            Case {
                name: "member-cap",
                config: {
                    let mut text = String::new();
                    let mut members: Vec<String> = Vec::new();
                    for i in 0..=MAX_POOL_MEMBERS {
                        let name = format!("anthropic-s{i:03}");
                        text.push_str(&format!(
                            "[providers.{name}]\n\
                             format = \"anthropic-messages\"\n\
                             base_url = \"https://api.anthropic.com\"\n\
                             credential = \"oauth://anthropic#s{i:03}\"\n\
                             auth = \"bearer\"\n"
                        ));
                        members.push(format!("\"{name}\""));
                    }
                    text.push_str(&format!(
                        "[pools.anthropic]\nmembers = [{}]\n",
                        members.join(", ")
                    ));
                    text
                },
                expect: &["exceeds", "32-member cap"],
            },
        ];
        #[cfg(feature = "openai-responses")]
        cases.push(Case {
            name: "mixed-kind",
            config: format!(
                "{OAUTH_ACCOUNTS}\
                 [providers.codex-default]\n\
                 format = \"openai-responses\"\n\
                 base_url = \"https://chatgpt.com/backend-api/codex\"\n\
                 credential = \"oauth://codex\"\n\
                 auth = \"bearer\"\n\
                 [pools.mixed]\n\
                 members = [\"anthropic-default\", \"codex-default\"]\n"
            ),
            expect: &[
                "codex-default",
                "anthropic-default",
                "`format`",
                "must share",
            ],
        });
        cases
    }

    #[test]
    fn every_rejection_class_fires_with_its_own_named_error() {
        // Arrange / Act / Assert
        for case in cases() {
            let config = parse(&case.config);
            let err = match validate_pools(&config) {
                Ok(()) => panic!("`{}` must be rejected", case.name),
                Err(e) => e.to_string(),
            };
            for needle in case.expect {
                assert!(
                    err.contains(needle),
                    "`{}` error must mention `{needle}`: {err}",
                    case.name
                );
            }
        }
    }

    /// The rejection classes must not collapse into one generic message: a
    /// distinct error per class is what makes an operator able to act.
    #[test]
    fn each_rejection_class_renders_a_distinct_message() {
        // Arrange / Act
        let mut messages: Vec<String> = Vec::new();
        for case in cases() {
            let config = parse(&case.config);
            let err = validate_pools(&config)
                .expect_err("every case rejects")
                .to_string();
            messages.push(err);
        }

        // Assert
        let mut deduped = messages.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(
            deduped.len(),
            messages.len(),
            "each class needs its own wording: {messages:?}"
        );
    }

    /// Wiring proof: every class reaches the ONE collected suite, so the
    /// serve gate, `config check`, the hot-reload gate and doctor's config
    /// section all reject it without per-surface plumbing.
    #[test]
    fn every_rejection_class_surfaces_through_the_collected_suite() {
        // Arrange / Act / Assert
        for case in cases() {
            let config = parse(&case.config);
            let errors = collect_config_validation(&config).errors;
            assert!(
                errors
                    .iter()
                    .any(|e| case.expect.iter().all(|n| e.contains(n))),
                "`{}` must surface through the collected suite: {errors:?}",
                case.name
            );
        }
    }

    /// A well-formed pool passes: same-kind OAuth members, membership
    /// claimed once, a name held by neither a provider nor a nickname.
    #[test]
    fn a_same_kind_oauth_pool_passes() {
        // Arrange
        let config = parse(&format!(
            "{OAUTH_ACCOUNTS}\
             [pools.anthropic]\n\
             members = [\"anthropic-default\", \"anthropic-work\"]\n\
             seat_selection = \"fill-first\"\n\
             accepts_new_logins = true\n"
        ));

        // Act / Assert
        assert!(
            validate_pools(&config).is_ok(),
            "a same-kind oauth pool must pass: {:?}",
            validate_pools(&config).err().map(|e| e.to_string())
        );
        assert!(collect_config_validation(&config).errors.is_empty());
    }

    /// Exactly at the cap is accepted -- the rejection is strictly above it.
    #[test]
    fn a_pool_at_the_member_cap_passes() {
        // Arrange
        let mut text = String::new();
        let mut members: Vec<String> = Vec::new();
        for i in 0..MAX_POOL_MEMBERS {
            let name = format!("anthropic-s{i:03}");
            text.push_str(&format!(
                "[providers.{name}]\n\
                 format = \"anthropic-messages\"\n\
                 base_url = \"https://api.anthropic.com\"\n\
                 credential = \"oauth://anthropic#s{i:03}\"\n\
                 auth = \"bearer\"\n"
            ));
            members.push(format!("\"{name}\""));
        }
        text.push_str(&format!(
            "[pools.anthropic]\nmembers = [{}]\n",
            members.join(", ")
        ));

        // Act / Assert
        assert!(validate_pools(&parse(&text)).is_ok());
    }

    /// A config with no pool table at all is unaffected -- the validator
    /// adds no error to configs written before pools existed.
    #[test]
    fn a_config_without_pools_is_unaffected() {
        assert!(validate_pools(&Config::default()).is_ok());
        assert!(validate_pools(&parse(OAUTH_ACCOUNTS)).is_ok());
    }

    /// One pass names EVERY problem: an operator fixing a pool table should
    /// not have to re-run the check once per mistake.
    #[test]
    fn one_pass_collects_every_problem_in_the_table() {
        // Arrange: an empty pool AND an unknown member in a second pool.
        let config = parse(&format!(
            "{OAUTH_ACCOUNTS}\
             [pools.empty]\n\
             members = []\n\
             [pools.anthropic]\n\
             members = [\"ghost\"]\n"
        ));

        // Act
        let err = validate_pools(&config)
            .expect_err("both problems reject")
            .to_string();

        // Assert
        assert!(err.contains("serves no traffic"), "{err}");
        assert!(err.contains("ghost"), "{err}");
    }

    /// A member entry whose credential is not an `oauth://` seat at all (a
    /// Bedrock lane authenticates through `creds`) is rejected by the same
    /// milestone-scoped rule rather than slipping through.
    #[cfg(feature = "bedrock")]
    #[test]
    fn a_non_oauth_credential_member_is_rejected() {
        // Arrange
        let config = parse(
            "[providers.br]\n\
             format = \"bedrock-invoke\"\n\
             cloud = { name = \"aws\", region = \"us-east-1\" }\n\
             credential = \"aws://chain\"\n\
             auth = \"aws-sigv4\"\n\
             [pools.br-pool]\n\
             members = [\"br\"]\n",
        );

        // Act
        let err = validate_pools(&config)
            .expect_err("a non-oauth member rejects")
            .to_string();

        // Assert
        assert!(err.contains("oauth://"), "{err}");
    }
}

#[cfg(test)]
mod managed_anthropic_credential_tests {
    //! Containment of the managed Anthropic subscription token: an
    //! `oauth://anthropic[#label]` reference is accepted only on an
    //! `anthropic-messages` entry with `auth = "bearer"`, no `cloud`, and a
    //! `base_url` whose host is exactly `api.anthropic.com` over https.

    use super::{collect_config_validation, validate_managed_anthropic_credential};
    use crate::config::{Auth, Config, ProviderEntry};
    use routectl_providers::anthropic_api::AuthKind;

    const REFS: [&str; 2] = ["oauth://anthropic", "oauth://anthropic#seat-secret-label"];
    /// The ceiling the CLI's config and doctor reports truncate a validator line
    /// at (`MAX_REPORTED_LINE_CHARS` in routectl-cli `commands/config.rs`); a
    /// containment refusal must arrive whole.
    const REPORTED_LINE_CAP: usize = 512;
    const SENTINEL_HOST: &str = "https://gateway.sentinel-host.example/sentinel-path?k=1";

    fn anthropic(api_key_ref: &str, base_url: &str) -> ProviderEntry {
        ProviderEntry::anthropic_api(api_key_ref)
            .with_auth_kind(AuthKind::OauthBearer)
            .with_base_url(base_url)
    }

    fn parse(text: &str) -> Config {
        toml::from_str(text).expect("fixture config parses")
    }

    fn rejects(entry: &ProviderEntry) -> bool {
        validate_managed_anthropic_credential("p", entry).is_err()
    }

    #[test]
    fn accepts_only_the_exact_https_anthropic_host_for_bare_and_labeled_refs() {
        let accepted = [
            "https://api.anthropic.com",
            "https://api.anthropic.com/",
            "https://api.anthropic.com/v1",
            "https://API.Anthropic.com:443",
        ];
        let rejected = [
            "https://api.anthropic.com.evil.example",
            "https://evil-api.anthropic.com",
            "https://eu.api.anthropic.com",
            "https://anthropic.com",
            "https://api.anthropic.co",
            "https://api-anthropic.com",
            "https://apianthropic.com",
            "https://evil.example/api.anthropic.com",
            "https://evil.example?h=api.anthropic.com",
            "https://evil.example#api.anthropic.com",
            "https://api.anthropic.com@evil.example",
            "https://evil.example\\@api.anthropic.com",
            "https://api.anthropic.com.",
            "http://api.anthropic.com",
            "http://127.0.0.1:18080",
            "http://localhost:18080",
            "https://127.0.0.1",
            "http://[::1]:18080",
            "",
            "not a url",
        ];
        for r in REFS {
            for url in accepted {
                assert!(!rejects(&anthropic(r, url)), "must accept {url} with {r}");
            }
            for url in rejected {
                assert!(rejects(&anthropic(r, url)), "must reject {url:?} with {r}");
            }
        }
    }

    #[test]
    fn accepts_the_builder_base_url_for_bare_and_labeled_refs() {
        for r in REFS {
            let entry = ProviderEntry::anthropic_api(r).with_auth(Auth::Bearer);
            assert!(
                !rejects(&entry),
                "the builder's base_url must be accepted: {r}"
            );
        }
    }

    /// The managed ref rides only the own lane's `credential`: in the
    /// second reference slot of an entry that is not the own lane it is
    /// refused, whichever key moved the entry off the lane. The own lane
    /// carrying the same ref in that slot is the positive control.
    #[test]
    fn rejects_a_managed_ref_in_the_second_slot_off_the_own_lane() {
        let with_second_slot = |entry: ProviderEntry, r: &str| {
            let mut entry = entry;
            entry.account_id_ref = Some(r.to_string());
            entry
        };
        for r in REFS {
            let own_lane =
                with_second_slot(anthropic("env://SOME_KEY", "https://api.anthropic.com"), r);
            assert!(!rejects(&own_lane), "own lane control must pass: {r}");

            for (label, entry) in [
                ("gateway host", anthropic("env://SOME_KEY", SENTINEL_HOST)),
                (
                    "x-api-key on the anthropic host",
                    ProviderEntry::anthropic_api("env://SOME_KEY"),
                ),
                (
                    "forwarded seat on a gateway host",
                    ProviderEntry::anthropic_api("forwarded")
                        .with_auth(Auth::Bearer)
                        .with_base_url(SENTINEL_HOST),
                ),
            ] {
                assert!(
                    rejects(&with_second_slot(entry, r)),
                    "{label} must reject {r}"
                );
            }
        }
    }

    #[test]
    fn rejects_every_foreign_provider_kind_for_bare_and_labeled_refs() {
        for r in REFS {
            assert!(rejects(&ProviderEntry::openai_compat(SENTINEL_HOST, r)));
            assert!(rejects(&ProviderEntry::openai_compat(
                "https://api.anthropic.com",
                r
            )));
            #[cfg(feature = "openai-responses")]
            {
                assert!(rejects(&ProviderEntry::openai_responses(r)));
                assert!(rejects(
                    &ProviderEntry::openai_responses(r)
                        .with_openai_responses_base_url("https://api.anthropic.com")
                ));
            }
            #[cfg(feature = "gemini")]
            {
                assert!(rejects(&ProviderEntry::gemini(r)));
            }
        }
    }

    #[cfg(feature = "bedrock")]
    #[test]
    fn rejects_a_managed_ref_on_the_aws_cloud() {
        let bedrock = parse(
            "[providers.br]\n\
             format = \"bedrock-invoke\"\n\
             cloud = { name = \"aws\", region = \"us-east-1\" }\n\
             credential = \"oauth://anthropic\"\n\
             auth = \"bearer\"\n",
        );
        assert!(rejects(&bedrock.providers["br"]));

        let managed_surface = parse(
            "[providers.m]\n\
             format = \"anthropic-messages\"\n\
             cloud = { name = \"aws\", region = \"us-east-1\" }\n\
             credential = \"oauth://anthropic#x\"\n\
             auth = \"bearer\"\n",
        );
        assert!(rejects(&managed_surface.providers["m"]));
    }

    #[test]
    fn leaves_static_credentials_and_other_oauth_families_alone() {
        let cases = [
            anthropic("env://SOME_KEY", "https://gateway.example/api"),
            anthropic("file:///tmp/some-key", "http://127.0.0.1:18080"),
            ProviderEntry::openai_compat(SENTINEL_HOST, "env://SOME_KEY"),
            ProviderEntry::anthropic_api("oauth://anthropic-a").with_base_url(SENTINEL_HOST),
            ProviderEntry::anthropic_api("oauth://codex").with_base_url(SENTINEL_HOST),
        ];
        for entry in &cases {
            assert!(!rejects(entry), "must not touch {entry:?}");
        }
    }

    #[test]
    fn config_validation_rejects_a_gateway_and_a_loopback_naming_the_provider_only() {
        for r in REFS {
            let config = parse(&format!(
                "[providers.gw]\n\
                 format = \"anthropic-messages\"\n\
                 base_url = \"{SENTINEL_HOST}\"\n\
                 credential = \"{r}\"\n\
                 auth = \"bearer\"\n\
                 [providers.lo]\n\
                 format = \"anthropic-messages\"\n\
                 base_url = \"http://127.0.0.1:18080\"\n\
                 credential = \"{r}\"\n\
                 auth = \"bearer\"\n\
                 [providers.oc]\n\
                 format = \"openai-chat\"\n\
                 base_url = \"{SENTINEL_HOST}\"\n\
                 credential = \"{r}\"\n\
                 auth = \"bearer\"\n"
            ));

            let errors = collect_config_validation(&config).errors;

            for name in ["gw", "lo", "oc"] {
                let hit: Vec<&String> = errors
                    .iter()
                    .filter(|e| e.contains(&format!("provider `{name}`")) && e.contains("managed"))
                    .collect();
                assert_eq!(hit.len(), 1, "one containment error for {name}: {errors:?}");
                let msg = hit[0];
                for leaked in [
                    "sentinel-host",
                    "sentinel-path",
                    "127.0.0.1",
                    "seat-secret-label",
                    "#",
                ] {
                    assert!(!msg.contains(leaked), "message leaks {leaked}: {msg}");
                }
                assert!(
                    msg.chars().count() <= REPORTED_LINE_CAP,
                    "containment error must fit the reported-line cap: {msg}"
                );
            }
        }
    }

    #[test]
    fn config_validation_keeps_the_supported_shapes_valid() {
        #[cfg_attr(not(feature = "openai-responses"), allow(unused_mut))]
        let mut text = String::from(
            "[providers.managed-default]\n\
             format = \"anthropic-messages\"\n\
             base_url = \"https://api.anthropic.com\"\n\
             credential = \"oauth://anthropic\"\n\
             auth = \"bearer\"\n\
             [providers.managed-seat]\n\
             format = \"anthropic-messages\"\n\
             base_url = \"https://api.anthropic.com\"\n\
             credential = \"oauth://anthropic#seat-b\"\n\
             auth = \"bearer\"\n\
             [providers.passthrough]\n\
             format = \"anthropic-messages\"\n\
             base_url = \"http://127.0.0.1:18080\"\n\
             credential = \"file:///tmp/passthrough-key\"\n\
             auth = \"x-api-key\"\n\
             [providers.third-party-gateway]\n\
             format = \"anthropic-messages\"\n\
             base_url = \"https://gateway.example/api\"\n\
             credential = \"env://GATEWAY_KEY\"\n\
             auth = \"bearer\"\n",
        );
        #[cfg(feature = "openai-responses")]
        text.push_str(
            "[providers.responses-oauth]\n\
             format = \"openai-responses\"\n\
             base_url = \"https://chatgpt.com/backend-api/codex\"\n\
             credential = \"oauth://codex\"\n\
             auth = \"bearer\"\n",
        );
        let config = parse(&text);

        let errors = collect_config_validation(&config).errors;

        assert!(
            errors.is_empty(),
            "supported shapes must validate: {errors:?}"
        );
    }
}

#[cfg(test)]
mod model_provider_resolution_tests {
    //! `resolve_model_provider` over the shared pools-and-providers
    //! namespace a `[models.X] provider` value names.

    use super::{ModelProviderTarget, resolve_model_provider};
    use crate::config::{Config, PoolEntry, ProviderEntry};

    #[derive(Debug, PartialEq)]
    enum Expected {
        Pool,
        Provider,
        Unresolved,
    }

    fn config_with_one_pool_and_one_provider() -> Config {
        let mut cfg = Config::default();
        cfg.providers.insert(
            "seat-a".into(),
            ProviderEntry::anthropic_api("literal:sk-ant-test"),
        );
        cfg.pools.insert(
            "alpha".into(),
            PoolEntry {
                members: vec!["seat-a".into()],
                ..Default::default()
            },
        );
        cfg
    }

    #[test]
    fn resolves_a_model_provider_name_to_its_pool_or_provider_entry() {
        // Arrange
        let cfg = config_with_one_pool_and_one_provider();
        let rows = [
            ("provider name", "seat-a", Expected::Provider),
            ("pool name", "alpha", Expected::Pool),
            ("unknown name", "ghost-provider", Expected::Unresolved),
        ];

        for (label, name, expected) in rows {
            // Act
            let target = resolve_model_provider(&cfg, name);

            // Assert: the variant AND the borrowed entry it carries.
            match (&expected, target) {
                (Expected::Pool, Some(ModelProviderTarget::Pool(pool))) => {
                    assert!(
                        std::ptr::eq(pool, cfg.pools.get(name).unwrap()),
                        "row `{label}`"
                    );
                }
                (Expected::Provider, Some(ModelProviderTarget::Provider(entry))) => {
                    assert!(
                        std::ptr::eq(entry, cfg.providers.get(name).unwrap()),
                        "row `{label}`"
                    );
                }
                (Expected::Unresolved, None) => {}
                (_, got) => panic!("row `{label}`: expected {expected:?}, got {got:?}"),
            }
        }
    }
}
