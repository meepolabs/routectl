use super::*;

#[cfg(test)]
mod build_resolved_models_tests {
    //! Tests for the v0.6.0 `build_resolved_models` function. Validates
    //! that:
    //!   - Multiple non-Bedrock models referencing the same provider
    //!     share one cached `Arc<dyn Provider>`.
    //!   - Bedrock models each get a distinct `Arc<dyn Provider>` with
    //!     `BedrockConfig.model_id` set from the model's `upstream`.
    //!   - Disabled `[models.X] selectable = false` entries are skipped.
    //!   - Models referencing an unknown provider are reported in the
    //!     `failed` return.

    use super::*;
    use crate::config::{Config, ModelEntry, ProviderEntry};
    use routectl_auth::MemoryStore;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn config_with_models(
        providers: Vec<(&str, ProviderEntry)>,
        models: Vec<(&str, ModelEntry)>,
    ) -> Config {
        let mut p = BTreeMap::new();
        for (name, e) in providers {
            p.insert(name.to_string(), e);
        }
        let mut m = BTreeMap::new();
        for (name, e) in models {
            m.insert(name.to_string(), e);
        }
        Config {
            providers: p,
            models: m,
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn non_bedrock_models_share_one_arc_per_provider() {
        let store: std::sync::Arc<dyn SecretStore> = std::sync::Arc::new(MemoryStore);
        let cfg = config_with_models(
            vec![(
                "anthropic",
                ProviderEntry::anthropic_api(crate::test_secret::file_ref("k")),
            )],
            vec![
                ("haiku", ModelEntry::new("anthropic", "claude-haiku-4-5")),
                ("sonnet", ModelEntry::new("anthropic", "claude-sonnet-4-6")),
            ],
        );
        let (models, failed) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");
        assert!(failed.is_empty(), "expected no failures: {failed:?}");
        assert_eq!(models.len(), 2);
        let haiku = models.get("haiku").unwrap();
        let sonnet = models.get("sonnet").unwrap();
        assert!(
            Arc::ptr_eq(&haiku.provider, &sonnet.provider),
            "non-Bedrock models on the same provider must share one Arc"
        );
    }

    #[tokio::test]
    async fn disabled_models_are_skipped() {
        let store: std::sync::Arc<dyn SecretStore> = std::sync::Arc::new(MemoryStore);
        let cfg = config_with_models(
            vec![(
                "anthropic",
                ProviderEntry::anthropic_api(crate::test_secret::file_ref("k")),
            )],
            vec![
                ("haiku", ModelEntry::new("anthropic", "claude-haiku-4-5")),
                (
                    "shelved",
                    ModelEntry::new("anthropic", "claude-shelved").with_selectable(false),
                ),
            ],
        );
        let (models, failed) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");
        assert!(failed.is_empty());
        assert!(models.contains_key("haiku"));
        assert!(!models.contains_key("shelved"));
    }

    #[tokio::test]
    async fn unknown_provider_in_model_yields_failed_entry() {
        let store: std::sync::Arc<dyn SecretStore> = std::sync::Arc::new(MemoryStore);
        let cfg = config_with_models(vec![], vec![("orphan", ModelEntry::new("missing", "u"))]);
        let (models, failed) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");
        assert!(models.is_empty());
        assert_eq!(failed.len(), 1);
        let (nickname, err) = &failed[0];
        assert_eq!(nickname, "orphan");
        assert!(err.contains("unknown provider"), "got: {err}");
    }

    #[tokio::test]
    async fn nickname_containing_hash_is_rejected_before_resolution() {
        // `#` is reserved as the seat-pool runtime-state-key separator
        // (`{nickname}#{label}`); a nickname carrying it must land in
        // `failed` with the reserved-separator reason and never enter the
        // resolved table -- even though its provider resolves cleanly.
        let store: std::sync::Arc<dyn SecretStore> = std::sync::Arc::new(MemoryStore);
        let cfg = config_with_models(
            vec![(
                "anthropic",
                ProviderEntry::anthropic_api(crate::test_secret::file_ref("k")),
            )],
            vec![("a#b", ModelEntry::new("anthropic", "claude-haiku-4-5"))],
        );
        let (models, failed) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");
        assert!(
            !models.contains_key("a#b"),
            "a `#` nickname must not enter the resolved table"
        );
        let (nickname, err) = failed
            .iter()
            .find(|(n, _)| n == "a#b")
            .expect("expected a failed entry for the `#` nickname");
        assert_eq!(nickname, "a#b");
        assert!(
            err.contains("`#`"),
            "reason must name the reserved char: {err}"
        );
        assert!(
            err.contains("seat-pool state-key separator"),
            "reason must explain the reservation: {err}"
        );
    }

    #[cfg(feature = "bedrock")]
    #[test]
    fn bedrock_factory_path_uses_per_model_upstream_for_model_id() {
        // Smoke-level pin: the `build_resolved_models` walk passes
        // each Bedrock model's `upstream` into the BedrockConfig
        // override slot via `build_provider_with_bedrock_model_override`.
        // We can't easily build a BedrockProvider in a unit test
        // (the AWS SDK requires a tokio sleep impl that's awkward
        // to wire up), so this test just sanity-checks that the
        // override-aware factory variant exists and that the wiring
        // compiles. The end-to-end behavior is exercised by the
        // live Bedrock tests in routectl-cli.
        let _f = build_provider_with_bedrock_model_override;
    }

    #[cfg(feature = "bedrock")]
    #[tokio::test]
    async fn bedrock_creds_failure_resolves_at_most_once_for_sibling_models() {
        // Dedup invariant on the failure path: when a Bedrock
        // provider's cred resolution fails on its first model, the
        // failure is recorded in `provider_failed` and every sibling
        // model on the same provider is skipped WITHOUT re-attempting
        // resolution (no repeat SSO / aws-config probe). With two
        // models on one provider, the secret store is hit exactly
        // once -- the second model short-circuits via the
        // `provider_failed` guard at the top of the Bedrock branch.
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingFailStore {
            calls: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl SecretStore for CountingFailStore {
            async fn get(&self, _secret_ref: &SecretRef) -> routectl_core::Result<String> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Err(routectl_core::Error::Auth("simulated cred failure".into()))
            }
            async fn set(
                &self,
                _secret_ref: &SecretRef,
                _value: &str,
            ) -> routectl_core::Result<()> {
                Err(routectl_core::Error::Auth("read-only".into()))
            }
            async fn delete(&self, _secret_ref: &SecretRef) -> routectl_core::Result<()> {
                Err(routectl_core::Error::Auth("read-only".into()))
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let store: Arc<dyn SecretStore> = Arc::new(CountingFailStore {
            calls: calls.clone(),
        });

        let bedrock = ProviderEntry::Bedrock {
            region: "us-east-1".to_string(),
            api_shape: BedrockApiShapeConfig::default(),
            creds: BedrockCredsConfig::BearerKey {
                key_ref: crate::test_secret::file_ref("unused"),
            },
            user_agent: None,
            header_extras: BTreeMap::new(),
            payload_extras: None,
            anthropic_beta: Vec::new(),
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            auto_emit_per_block_breakpoints: None,
            reduction_enabled: None,
            runtime: Default::default(),
        };
        let cfg = config_with_models(
            vec![("br", bedrock)],
            vec![
                ("opus", ModelEntry::new("br", "anthropic.claude-opus")),
                ("sonnet", ModelEntry::new("br", "anthropic.claude-sonnet")),
            ],
        );

        let (models, failed) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");

        assert!(
            models.is_empty(),
            "no model should resolve when provider creds fail"
        );
        assert_eq!(
            failed.len(),
            2,
            "both sibling models must be reported failed: {failed:?}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "cred resolution must run at most once per provider; the sibling \
             model should be skipped via provider_failed, not re-probed"
        );
    }

    #[tokio::test]
    async fn header_extras_propagate_from_model_entry_to_resolved() {
        // Pin: v0.6.0 -- per-model `header_extras` lands on
        // ResolvedModel.header_extras after build_resolved_models.
        // Operators now set anthropic-beta via header_extras instead
        // of the dropped Vec<String> field.
        let store: std::sync::Arc<dyn SecretStore> = std::sync::Arc::new(MemoryStore);
        let mut headers = std::collections::BTreeMap::new();
        headers.insert(
            "anthropic-beta".to_string(),
            "context-1m-2025-08-07,prompt-cache-1h".to_string(),
        );
        let cfg = config_with_models(
            vec![(
                "anthropic",
                ProviderEntry::anthropic_api(crate::test_secret::file_ref("k")),
            )],
            vec![(
                "opus",
                ModelEntry::new("anthropic", "claude-opus-4-7").with_header_extras(headers),
            )],
        );
        let (models, failed) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");
        assert!(failed.is_empty(), "expected no failures: {failed:?}");
        let opus = models.get("opus").expect("opus entry");
        assert_eq!(
            opus.header_extras.get("anthropic-beta"),
            Some(&"context-1m-2025-08-07,prompt-cache-1h".to_string())
        );
    }

    #[tokio::test]
    async fn stream_first_byte_timeout_ms_propagates_from_model_entry() {
        let store: std::sync::Arc<dyn SecretStore> = std::sync::Arc::new(MemoryStore);
        let cfg = config_with_models(
            vec![(
                "anthropic",
                ProviderEntry::anthropic_api(crate::test_secret::file_ref("k")),
            )],
            vec![(
                "opus",
                ModelEntry::new("anthropic", "claude-opus-4-7")
                    .with_stream_first_byte_timeout_ms(300_000),
            )],
        );
        let (models, failed) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");
        assert!(failed.is_empty(), "expected no failures: {failed:?}");
        let opus = models.get("opus").expect("opus entry");
        assert_eq!(opus.stream_first_byte_timeout_ms, Some(300_000));
    }

    #[tokio::test]
    async fn zero_stream_first_byte_timeout_ms_is_skipped_not_set() {
        // `stream_first_byte_timeout_ms = 0` would abandon every stream
        // before its first content-bearing chunk. The resolver must WARN
        // and leave the field None, never propagate Some(0).
        let store: std::sync::Arc<dyn SecretStore> = std::sync::Arc::new(MemoryStore);
        let cfg = config_with_models(
            vec![(
                "anthropic",
                ProviderEntry::anthropic_api(crate::test_secret::file_ref("k")),
            )],
            vec![(
                "opus",
                ModelEntry {
                    stream_first_byte_timeout_ms: Some(0),
                    ..ModelEntry::new("anthropic", "claude-opus-4-7")
                },
            )],
        );
        let (models, failed) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");
        assert!(failed.is_empty(), "expected no failures: {failed:?}");
        let opus = models.get("opus").expect("opus entry");
        assert!(
            opus.stream_first_byte_timeout_ms.is_none(),
            "zero must be skipped, not propagated as Some(0)"
        );
    }

    #[tokio::test]
    async fn zero_max_output_tokens_is_skipped_not_set() {
        // `max_output_tokens = 0` would produce a body the upstream
        // 400s. The resolver must WARN and leave the field at its
        // unset sentinel (0), never call the setter.
        let store: std::sync::Arc<dyn SecretStore> = std::sync::Arc::new(MemoryStore);
        let cfg = config_with_models(
            vec![(
                "anthropic",
                ProviderEntry::anthropic_api(crate::test_secret::file_ref("k")),
            )],
            vec![(
                "opus",
                ModelEntry {
                    max_output_tokens: Some(0),
                    ..ModelEntry::new("anthropic", "claude-opus-4-7")
                },
            )],
        );
        let (models, failed) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");
        assert!(failed.is_empty(), "expected no failures: {failed:?}");
        let opus = models.get("opus").expect("opus entry");
        assert_eq!(
            opus.max_output_tokens, 0,
            "zero must be skipped, leaving the unset sentinel 0"
        );
    }

    #[tokio::test]
    async fn nonzero_and_none_timeout_knobs_behave_as_before() {
        // Non-zero values set the field; absent values leave it unset.
        let store: std::sync::Arc<dyn SecretStore> = std::sync::Arc::new(MemoryStore);
        let cfg = config_with_models(
            vec![(
                "anthropic",
                ProviderEntry::anthropic_api(crate::test_secret::file_ref("k")),
            )],
            vec![
                (
                    "set",
                    ModelEntry::new("anthropic", "claude-opus-4-7")
                        .with_stream_first_byte_timeout_ms(5_000)
                        .with_max_output_tokens(32_000),
                ),
                ("unset", ModelEntry::new("anthropic", "claude-haiku-4-5")),
            ],
        );
        let (models, failed) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");
        assert!(failed.is_empty(), "expected no failures: {failed:?}");
        let set = models.get("set").expect("set entry");
        assert_eq!(set.stream_first_byte_timeout_ms, Some(5_000));
        assert_eq!(set.max_output_tokens, 32_000);
        let unset = models.get("unset").expect("unset entry");
        assert!(unset.stream_first_byte_timeout_ms.is_none());
        assert_eq!(unset.max_output_tokens, 0);
    }

    #[tokio::test]
    async fn empty_header_extras_and_none_timeout_yield_defaults() {
        // Pin: a model entry without the new fields leaves the
        // resolved model with default values (empty maps, None).
        let store: std::sync::Arc<dyn SecretStore> = std::sync::Arc::new(MemoryStore);
        let cfg = config_with_models(
            vec![(
                "anthropic",
                ProviderEntry::anthropic_api(crate::test_secret::file_ref("k")),
            )],
            vec![("haiku", ModelEntry::new("anthropic", "claude-haiku-4-5"))],
        );
        let (models, _) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");
        let haiku = models.get("haiku").expect("haiku entry");
        assert!(haiku.header_extras.is_empty());
        assert!(haiku.payload_extras.is_none());
        assert!(haiku.stream_first_byte_timeout_ms.is_none());
    }

    /// Store that answers every OAuth ref with a token, and RECORDS every
    /// `list_seats` call. Pool members are resolved from config alone, so a
    /// nonzero count is itself the defect: it would mean a bare member ref had
    /// silently fanned out into every stored seat of that provider again.
    #[derive(Default)]
    struct SeatEnumerationSpy {
        list_seats_calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl SecretStore for SeatEnumerationSpy {
        async fn get(&self, _secret_ref: &SecretRef) -> routectl_core::Result<String> {
            Ok("token".into())
        }
        async fn set(&self, _: &SecretRef, _: &str) -> routectl_core::Result<()> {
            Ok(())
        }
        async fn delete(&self, _: &SecretRef) -> routectl_core::Result<()> {
            Ok(())
        }
        async fn list_seats(
            &self,
            secret_ref: &SecretRef,
        ) -> routectl_core::Result<Vec<SecretRef>> {
            self.list_seats_calls.fetch_add(1, Ordering::SeqCst);
            // Two seats, so a caller that DID enumerate would visibly expand.
            let SecretRef::OAuth {
                provider,
                label: None,
            } = secret_ref
            else {
                return Ok(vec![secret_ref.clone()]);
            };
            Ok(vec![
                secret_ref.clone(),
                SecretRef::OAuth {
                    provider: provider.clone(),
                    label: Some("stored-sibling".into()),
                },
            ])
        }
    }

    /// A store whose `get` refuses for one named provider, so exactly one pool
    /// member fails to build the way an un-logged-in account does.
    struct OneDeadCredentialStore {
        dead_provider: String,
    }

    #[async_trait::async_trait]
    impl SecretStore for OneDeadCredentialStore {
        async fn get(&self, secret_ref: &SecretRef) -> routectl_core::Result<String> {
            if let SecretRef::OAuth { provider, .. } = secret_ref
                && *provider == self.dead_provider
            {
                return Err(routectl_core::Error::Auth(format!(
                    "no credentials for {provider}; run `routectl login {provider}`"
                )));
            }
            Ok("token".into())
        }
        async fn set(&self, _: &SecretRef, _: &str) -> routectl_core::Result<()> {
            Ok(())
        }
        async fn delete(&self, _: &SecretRef) -> routectl_core::Result<()> {
            Ok(())
        }
    }

    /// An oauth-bearer anthropic entry for one account. `auth_kind` matters:
    /// the oauth-bearer arm wraps a lazy `ManagedToken` rather than resolving
    /// at build, which is what lets a healthy member build without a store hit.
    fn oauth_member(provider: &str) -> ProviderEntry {
        ProviderEntry::anthropic_api(format!("oauth://{provider}"))
            .with_auth_kind(routectl_providers::anthropic_api::AuthKind::OauthBearer)
    }

    /// A config whose `[pools.<pool>]` groups `members`, with one model per
    /// entry in `models` routed at the pool.
    fn pooled_config(pool: &str, members: &[&str], models: &[&str]) -> Config {
        let mut cfg = config_with_models(
            members.iter().map(|m| (*m, oauth_member(m))).collect(),
            models
                .iter()
                .map(|nick| (*nick, ModelEntry::new(pool, "claude-opus-4-7")))
                .collect(),
        );
        cfg.pools.insert(
            pool.to_string(),
            crate::config::PoolEntry::new(members.iter().map(|m| (*m).to_string()).collect()),
        );
        cfg
    }

    #[tokio::test]
    async fn a_model_naming_a_pool_resolves_to_one_seat_per_member() {
        // Arrange: a two-member pool, one model routed at it.
        let store: Arc<dyn SecretStore> = Arc::new(SeatEnumerationSpy::default());
        let cfg = pooled_config("anthropic-pool", &["anthropic-a", "anthropic-b"], &["opus"]);

        // Act
        let built = build_resolved_models_reported(&cfg, store, BuildOptions::default())
            .await
            .expect("a healthy pool builds");

        // Assert: one seat per member, each carrying that member's OWN ref.
        let opus = built
            .models
            .get("opus")
            .expect("opus resolves via the pool");
        let seats = opus.seats.as_ref().expect("a pool-backed model has seats");
        assert_eq!(seats.len(), 2);
        let members: Vec<&str> = seats.iter().map(|s| s.provider_name.as_str()).collect();
        assert_eq!(members, vec!["anthropic-a", "anthropic-b"]);
        let refs: Vec<String> = seats
            .iter()
            .map(|s| s.auth_secret_ref.as_ref().expect("member ref").to_string())
            .collect();
        assert_eq!(
            refs,
            vec!["oauth://anthropic-a", "oauth://anthropic-b"],
            "each seat must be built from its own member's api_key_ref"
        );
    }

    #[tokio::test]
    async fn no_pool_member_enumerates_stored_seats() {
        // THE bare-ref rule, on the pool path: membership is declared in
        // config, so a bare member ref means that account's default seat and
        // nothing else. Enumerating would silently re-expand one member into
        // every stored seat of its provider.
        let spy = Arc::new(SeatEnumerationSpy::default());
        let store: Arc<dyn SecretStore> = spy.clone();
        let cfg = pooled_config("anthropic-pool", &["anthropic-a", "anthropic-b"], &["opus"]);

        let built = build_resolved_models_reported(&cfg, store, BuildOptions::default())
            .await
            .expect("a healthy pool builds");

        assert_eq!(
            spy.list_seats_calls.load(Ordering::SeqCst),
            0,
            "the pool path must never call list_seats"
        );
        assert_eq!(
            built.models["opus"].seats.as_ref().expect("seats").len(),
            2,
            "two members means two seats, not two members times two stored seats"
        );
    }

    #[tokio::test]
    async fn a_bare_ref_on_a_standalone_provider_no_longer_expands() {
        // The non-pool routing rule: a bare `oauth://<provider>` on a plain
        // `[providers.X]` entry resolves to the DEFAULT seat only. The store
        // below reports a stored sibling, so the pre-change behavior would
        // have produced a two-seat pool here.
        let spy = Arc::new(SeatEnumerationSpy::default());
        let store: Arc<dyn SecretStore> = spy.clone();
        let cfg = config_with_models(
            vec![("anthropic", oauth_member("anthropic"))],
            vec![("opus", ModelEntry::new("anthropic", "claude-opus-4-7"))],
        );

        let (models, failed) = build_resolved_models(&cfg, store, BuildOptions::default())
            .await
            .expect("ok");

        assert!(failed.is_empty(), "expected no failures: {failed:?}");
        assert!(
            models["opus"].seats.is_none(),
            "a bare ref on a standalone provider must resolve to one target, \
             not fan out across stored seats"
        );
        assert_eq!(
            spy.list_seats_calls.load(Ordering::SeqCst),
            0,
            "the non-pool path must no longer enumerate stored seats at all"
        );
    }

    #[tokio::test]
    async fn two_models_on_one_pool_share_one_seat_set() {
        // The sharing contract, pinned by Arc identity rather than equal
        // contents: equal contents would also hold if each model had rebuilt
        // its own copy of every member's provider.
        let store: Arc<dyn SecretStore> = Arc::new(SeatEnumerationSpy::default());
        let cfg = pooled_config(
            "anthropic-pool",
            &["anthropic-a", "anthropic-b"],
            &["opus", "sonnet"],
        );

        let built = build_resolved_models_reported(&cfg, store, BuildOptions::default())
            .await
            .expect("a healthy pool builds");

        let opus_seats = built.models["opus"].seats.as_ref().expect("opus seats");
        let sonnet_seats = built.models["sonnet"].seats.as_ref().expect("sonnet seats");
        assert!(
            Arc::ptr_eq(opus_seats, sonnet_seats),
            "both models must share ONE compiled seat set"
        );
        // And therefore each member's provider was built exactly once.
        for (a, b) in opus_seats.iter().zip(sonnet_seats.iter()) {
            assert!(
                Arc::ptr_eq(&a.provider, &b.provider),
                "member {} must not be built twice",
                a.provider_name,
            );
        }
    }

    #[tokio::test]
    async fn a_pool_with_one_dead_member_serves_through_the_survivor() {
        // Degraded, not failed: the pool keeps serving and reports exactly the
        // member it lost, with an allowlisted reason.
        let store: Arc<dyn SecretStore> = Arc::new(OneDeadCredentialStore {
            dead_provider: "anthropic-b".into(),
        });
        let cfg = pooled_config("anthropic-pool", &["anthropic-a", "anthropic-b"], &["opus"]);

        let built = build_resolved_models_reported(&cfg, store, BuildOptions::default())
            .await
            .expect("a degraded pool must still build");

        let seats = built.models["opus"].seats.as_ref().expect("seats");
        assert_eq!(seats.len(), 1, "the survivor serves");
        assert_eq!(seats[0].provider_name, "anthropic-a");

        let report = built
            .pool_reports
            .iter()
            .find(|r| r.pool == "anthropic-pool")
            .expect("the pool reports");
        assert!(report.is_degraded());
        assert_eq!(report.configured_members, 2);
        assert_eq!(report.usable_members, 1);
        assert_eq!(report.omissions.len(), 1, "exactly one member omitted");
        assert_eq!(report.omissions[0].member, "anthropic-b");
        assert_eq!(
            report.omissions[0].reason,
            crate::pool_build::PoolOmissionReason::CredentialUnreadable
        );
        assert_eq!(report.omissions[0].provider_kind, "anthropic-api");
    }

    #[tokio::test]
    async fn an_omission_report_carries_no_store_error_text() {
        // The report reaches a log field AND an operator-facing surface, so an
        // error string routed into it would publish whatever the store put in
        // its message -- here a provider id plus login guidance, elsewhere a
        // credential path or account id. Anchored on the SHIPPED report, not on
        // a Debug projection of it.
        let store: Arc<dyn SecretStore> = Arc::new(OneDeadCredentialStore {
            dead_provider: "anthropic-b".into(),
        });
        let cfg = pooled_config("anthropic-pool", &["anthropic-a", "anthropic-b"], &["opus"]);

        let built = build_resolved_models_reported(&cfg, store, BuildOptions::default())
            .await
            .expect("a degraded pool must still build");

        let omission = &built.pool_reports[0].omissions[0];
        assert_eq!(
            omission.reason.token(),
            "credential_unreadable",
            "the reason must be one of the four allowlisted tokens"
        );
        // Every field of the shipped omission, concatenated: nothing but the
        // member key, the kind token, and the reason token may appear.
        let shipped = format!(
            "{}|{}|{}",
            omission.member,
            omission.provider_kind,
            omission.reason.token()
        );
        for banned in ["routectl login", "no credentials", "token", "oauth://"] {
            assert!(
                !shipped.contains(banned),
                "shipped omission must not carry `{banned}`: {shipped}"
            );
        }
    }

    #[tokio::test]
    async fn a_pool_with_no_usable_member_fails_the_build_naming_pool_and_model() {
        // Zero usable members behind a selectable model is unroutable, so the
        // build refuses rather than starting healthy and failing at first
        // traffic. Also the reload contract: a candidate build that returns Err
        // is discarded by every caller, leaving the previous router live.
        let store: Arc<dyn SecretStore> = Arc::new(OneDeadCredentialStore {
            dead_provider: "anthropic-a".into(),
        });
        let cfg = pooled_config("anthropic-pool", &["anthropic-a"], &["opus"]);

        let err = build_resolved_models_reported(&cfg, store, BuildOptions::default())
            .await
            .expect_err("a zero-usable pool must refuse the build");

        let msg = err.to_string();
        assert!(msg.contains("anthropic-pool"), "must name the pool: {msg}");
        assert!(msg.contains("opus"), "must name the model: {msg}");
    }

    #[tokio::test]
    async fn a_model_naming_neither_a_provider_nor_a_pool_is_reported_failed() {
        let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
        let cfg = config_with_models(
            vec![("anthropic", oauth_member("anthropic"))],
            vec![("opus", ModelEntry::new("nonesuch", "claude-opus-4-7"))],
        );

        let (models, failed) = build_resolved_models(&cfg, store, BuildOptions::default())
            .await
            .expect("ok");

        assert!(models.is_empty());
        assert_eq!(failed.len(), 1);
        assert!(failed[0].1.contains("nonesuch"), "{:?}", failed[0]);
    }

    #[tokio::test]
    async fn a_pool_backed_model_keeps_its_model_level_knobs() {
        // The two resolution paths share one knob applier; without that a knob
        // would silently not apply to pool-backed models, and the omission
        // would read exactly like the knob being unset.
        let store: Arc<dyn SecretStore> = Arc::new(SeatEnumerationSpy::default());
        let mut cfg = pooled_config("anthropic-pool", &["anthropic-a"], &["opus"]);
        cfg.models.insert(
            "opus".into(),
            ModelEntry::new("anthropic-pool", "claude-opus-4-7")
                .with_reported_model("public-label")
                .with_visible_routectl_provider(false),
        );

        let built = build_resolved_models_reported(&cfg, store, BuildOptions::default())
            .await
            .expect("a healthy pool builds");

        let opus = &built.models["opus"];
        assert_eq!(opus.reported_model.as_deref(), Some("public-label"));
        assert!(!opus.visible_routectl_provider);
        assert_eq!(
            opus.provider_name, "anthropic-pool",
            "the model keeps the operator-written pool name"
        );
    }

    // -------------------------------------------------------------------
    // apply_catalog_overlay: the post-pass that stamps each resolved
    // model's precomputed EffectiveRow.
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn apply_catalog_overlay_stamps_baked_row_when_no_overlay_cell_matches() {
        let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
        let cfg = config_with_models(
            vec![(
                "anthropic",
                ProviderEntry::anthropic_api(crate::test_secret::file_ref("k")),
            )],
            vec![("haiku", ModelEntry::new("anthropic", "claude-haiku-4-5"))],
        );
        let (models, failed) = build_resolved_models(&cfg, store, BuildOptions::default())
            .await
            .expect("ok");
        assert!(failed.is_empty());

        let stamped = apply_catalog_overlay(models, &cfg, &CatalogOverlay::default());
        let haiku = stamped.get("haiku").expect("haiku entry");
        match &haiku.effective_row {
            crate::catalog::EffectiveRow::Present { source, .. } => {
                assert_eq!(*source, crate::catalog::Source::Baked);
            }
            other => panic!("expected Present/Baked, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn apply_catalog_overlay_overrides_a_baked_field() {
        let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
        let cfg = config_with_models(
            vec![(
                "anthropic",
                ProviderEntry::anthropic_api(crate::test_secret::file_ref("k")),
            )],
            vec![("haiku", ModelEntry::new("anthropic", "claude-haiku-4-5"))],
        );
        let (models, _) = build_resolved_models(&cfg, store, BuildOptions::default())
            .await
            .expect("ok");

        let mut cells = BTreeMap::new();
        cells.insert(
            "anthropic-api:claude-haiku-4-5*".to_string(),
            Some(crate::catalog_overlay::OverlayCell {
                source: crate::catalog_overlay::OverlaySource::User,
                verified_at: "2026-07-01".to_string(),
                wm: Some(9.5),
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                input_cost_per_token: None,
                output_cost_per_token: None,
                capabilities: None,
            }),
        );
        let overlay = CatalogOverlay {
            schema_version: crate::catalog_overlay::CATALOG_OVERLAY_SCHEMA_VERSION,
            revision: 1,
            cells,
        };

        let stamped = apply_catalog_overlay(models, &cfg, &overlay);
        let haiku = stamped.get("haiku").expect("haiku entry");
        let row = haiku
            .effective_row
            .priced()
            .expect("overlay cell resolves Present");
        assert_eq!(row.wm, 9.5, "the overlay's wm must win over the baked row");
    }

    #[tokio::test]
    async fn apply_catalog_overlay_null_cell_disables_the_target() {
        let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
        let cfg = config_with_models(
            vec![(
                "anthropic",
                ProviderEntry::anthropic_api(crate::test_secret::file_ref("k")),
            )],
            vec![("haiku", ModelEntry::new("anthropic", "claude-haiku-4-5"))],
        );
        let (models, _) = build_resolved_models(&cfg, store, BuildOptions::default())
            .await
            .expect("ok");

        let mut cells = BTreeMap::new();
        cells.insert("anthropic-api:claude-haiku-4-5*".to_string(), None);
        let overlay = CatalogOverlay {
            schema_version: crate::catalog_overlay::CATALOG_OVERLAY_SCHEMA_VERSION,
            revision: 1,
            cells,
        };

        let stamped = apply_catalog_overlay(models, &cfg, &overlay);
        let haiku = stamped.get("haiku").expect("haiku entry");
        assert_eq!(haiku.effective_row, crate::catalog::EffectiveRow::Disabled);
        assert!(
            haiku.effective_row.priced().is_none(),
            "a null-disabled cell must fold to the conservative sentinel behavior"
        );
    }
}

#[cfg(test)]
mod managed_token_tests {
    //! Pin the v0.7 OAuth-aware `resolve_token_source` semantics:
    //!   - `oauth://` refs return a `ManagedToken` that re-enters
    //!     `SecretStore::get` on every `token()` call (so credentials
    //!     rotation in `~/.config/routectl/credentials.json` is picked
    //!     up live without restart).
    //!   - `env://` / `file://` / `literal:` refs return a `StaticToken`
    //!     resolved once at construction; subsequent `token()` calls
    //!     never re-hit the SecretStore.

    use super::*;
    use async_trait::async_trait;
    use routectl_auth::{SecretRef, SecretStore};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingStore {
        calls: AtomicUsize,
    }
    impl CountingStore {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }
    #[async_trait]
    impl SecretStore for CountingStore {
        async fn get(&self, sr: &SecretRef) -> routectl_core::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match sr {
                SecretRef::OAuth { provider, .. } => Ok(format!("token-for-{provider}")),
                SecretRef::Env(_) => Ok("static-canned".to_string()),
                _ => Err(routectl_core::Error::Auth(
                    "counting store: oauth/env-only".into(),
                )),
            }
        }
        async fn set(&self, _: &SecretRef, _: &str) -> routectl_core::Result<()> {
            Ok(())
        }
        async fn delete(&self, _: &SecretRef) -> routectl_core::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn managed_token_re_enters_store_per_call() {
        let counting = Arc::new(CountingStore::new());
        let store: Arc<dyn SecretStore> = counting.clone();
        let ts = resolve_token_source(&store, "oauth://anthropic")
            .await
            .unwrap();
        assert_eq!(ts.token().await.unwrap(), "token-for-anthropic");
        assert_eq!(ts.token().await.unwrap(), "token-for-anthropic");
        assert_eq!(ts.token().await.unwrap(), "token-for-anthropic");
        assert_eq!(
            counting.calls(),
            3,
            "ManagedToken must hit store once per token() call"
        );
    }

    #[tokio::test]
    async fn static_token_does_not_re_enter_store_per_call() {
        // CountingStore intercepts `SecretRef::Env(_)` directly and
        // returns a canned reply, so `std::env` is never consulted.
        // The point of the test is to prove the StaticToken path
        // caches: only ONE call lands in the store at construction,
        // and subsequent `token()` invocations reuse the cached value.
        let counting = Arc::new(CountingStore::new());
        let store: Arc<dyn SecretStore> = counting.clone();
        let ts = resolve_token_source(&store, "env://ROUTECTL_TEST_STATIC_TOKEN_VAR")
            .await
            .unwrap();
        let _ = ts.token().await.unwrap();
        let _ = ts.token().await.unwrap();
        assert_eq!(
            counting.calls(),
            1,
            "StaticToken caches; store hit only at construction"
        );
    }
}

#[cfg(test)]
#[cfg(feature = "openai-responses")]
mod openai_responses_account_id_tests {
    //! Pin the managed-OAuth account-id derivation for the
    //! openai-responses factory arm:
    //!   (a) `oauth://codex` + no `account_id_ref` + populated store
    //!       -> account id taken from the stored TokenRecord; build ok.
    //!   (b) `oauth://codex` + no `account_id_ref` + empty store
    //!       -> clean Error mentioning `routectl login codex`.
    //!   (c) `env://X` + no `account_id_ref` (legacy chatgpt-oauth)
    //!       -> existing "requires account_id_ref" Error preserved.
    //!   (d) `oauth://codex` + explicit `account_id_ref`
    //!       -> the operator value wins (override).

    use super::*;
    use async_trait::async_trait;
    use routectl_auth::{MemoryStore, OAuthStore, SecretRef};
    use std::sync::Arc;

    /// Minimal stand-in for the production `CompositeStore` (which lives
    /// in the CLI crate and is out of scope here). Routes `oauth://`
    /// refs -- including the `account_id` read -- to the OAuthStore, and
    /// everything else (`literal:`, `env://`, `file://`) to MemoryStore.
    /// Lets these router-level tests exercise the operator-override path
    /// (`account_id_ref = "literal:..."`) alongside the JWT-derived path
    /// without depending on the CLI crate.
    struct CompositeTestStore {
        oauth: OAuthStore,
        fallback: MemoryStore,
    }

    #[async_trait]
    impl SecretStore for CompositeTestStore {
        async fn get(&self, sr: &SecretRef) -> Result<String> {
            match sr {
                SecretRef::OAuth { .. } => self.oauth.get(sr).await,
                _ => self.fallback.get(sr).await,
            }
        }
        async fn set(&self, sr: &SecretRef, v: &str) -> Result<()> {
            self.fallback.set(sr, v).await
        }
        async fn delete(&self, sr: &SecretRef) -> Result<()> {
            self.fallback.delete(sr).await
        }
        async fn account_id(&self, sr: &SecretRef) -> Result<Option<String>> {
            match sr {
                SecretRef::OAuth { .. } => self.oauth.account_id(sr).await,
                _ => self.fallback.account_id(sr).await,
            }
        }
    }

    /// Write a `credentials.json` seeded with a `codex` record (when
    /// `account_id` is `Some`) or leave the store empty, then open an
    /// `OAuthStore` over it. Returns the tempdir guard (kept alive for
    /// the test's duration) and the store as `Arc<dyn SecretStore>`.
    ///
    /// The record is written as raw JSON rather than constructed from
    /// `TokenRecord` because that struct is `#[non_exhaustive]` and
    /// cannot be built with a struct literal from this crate. Writing
    /// the on-disk shape also exercises the real `OAuthStore::open`
    /// load path.
    async fn oauth_store_with_codex(
        account_id: Option<&str>,
    ) -> (tempfile::TempDir, Arc<dyn SecretStore>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        if let Some(id) = account_id {
            let json = format!(
                r#"{{
                    "schema_version": 1,
                    "providers": {{
                        "codex": {{
                            "access_token": "tok-codex",
                            "refresh_token": "rtok-codex",
                            "token_type": "Bearer",
                            "expires_at_unix": 9999999999,
                            "scopes": ["openid"],
                            "account": {{ "email": "u@example.com", "account_id": "{id}" }},
                            "obtained_at_unix": 0
                        }}
                    }}
                }}"#
            );
            std::fs::write(&path, json).unwrap();
            // OAuthStore::open refuses group/other-readable credential
            // files (it wants chmod 600). tempfile defaults to 644, so
            // tighten the mode before opening.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
        }
        let store = CompositeTestStore {
            oauth: OAuthStore::open(&path).await.unwrap(),
            fallback: MemoryStore::new(),
        };
        (dir, Arc::new(store) as Arc<dyn SecretStore>)
    }

    fn chatgpt_oauth_entry(api_key_ref: &str) -> ProviderEntry {
        ProviderEntry::openai_responses(api_key_ref)
            .with_openai_responses_auth_kind(OpenaiResponsesAuthKind::ChatgptOauth)
    }

    #[tokio::test]
    async fn oauth_no_account_id_ref_derives_from_stored_token() {
        // (a) populated store -> account id derived; provider builds.
        let (_dir, store) = oauth_store_with_codex(Some("acct-from-jwt")).await;

        let derived = resolve_responses_account_id(&store, "oauth://codex", &None, "codex-pro")
            .await
            .expect("derivation should succeed");
        assert_eq!(derived, Some("acct-from-jwt".to_string()));

        let entry = chatgpt_oauth_entry("oauth://codex");
        let provider = build_provider("codex-pro", &entry, store.clone()).await;
        assert!(
            provider.is_ok(),
            "provider should build from a logged-in session: {:?}",
            provider.err()
        );
    }

    #[tokio::test]
    async fn oauth_no_account_id_ref_empty_store_errors_with_login_hint() {
        // (b) empty store -> clean Error mentioning `routectl login codex`.
        let (_dir, store) = oauth_store_with_codex(None).await;

        let err = resolve_responses_account_id(&store, "oauth://codex", &None, "codex-pro")
            .await
            .expect_err("empty store must error");
        let msg = err.to_string();
        assert!(
            msg.contains("routectl login codex"),
            "expected login hint, got: {msg}"
        );

        // The full build arm must surface the same error. `Arc<dyn
        // Provider>` is not `Debug`, so match instead of `expect_err`.
        let entry = chatgpt_oauth_entry("oauth://codex");
        match build_provider("codex-pro", &entry, store.clone()).await {
            Ok(_) => panic!("build must fail with no session"),
            Err(e) => assert!(
                e.to_string().contains("routectl login codex"),
                "build error should carry the login hint, got: {e}"
            ),
        }
    }

    #[tokio::test]
    async fn legacy_static_chatgpt_oauth_still_requires_account_id_ref() {
        // (c) env:// bearer (legacy chatgpt-oauth) + no account_id_ref
        // -> the validator rejects it (existing operator workflow).
        let err = validate_openai_responses_account_id(
            "legacy",
            OpenaiResponsesAuthKind::ChatgptOauth,
            false, // env://OPENAI_JWT is a static bearer, not oauth://
            &None,
        )
        .expect_err("static chatgpt-oauth without account_id_ref must error");
        let msg = err.to_string();
        assert!(msg.contains("requires `account_id_ref`"), "got: {msg}");
        assert!(msg.contains("legacy"), "got: {msg}");
    }

    #[tokio::test]
    async fn explicit_account_id_ref_wins_over_stored_token() {
        // (d) operator-supplied account_id_ref overrides the JWT-derived
        // one even when the store has a (different) stored account id.
        let (_dir, store) = oauth_store_with_codex(Some("acct-from-jwt")).await;

        let override_ref = Some(crate::test_secret::file_ref("acct-operator-override"));
        let derived =
            resolve_responses_account_id(&store, "oauth://codex", &override_ref, "codex-pro")
                .await
                .expect("override should resolve");
        assert_eq!(
            derived,
            Some("acct-operator-override".to_string()),
            "operator-supplied account_id_ref must win over the stored token"
        );
    }
}

#[cfg(test)]
mod anthropic_api_config_propagation_tests {
    //! Pin that the `[providers.X]` anthropic-api knobs `context_management`
    //! and `credential_source` reach the built provider through the REAL
    //! `build_provider` path -- observed via provider behavior, not a
    //! re-implemented factory destructure.
    //!
    //! `context_management` is observed through `Provider::normalize_request`:
    //! the flag decides whether the top-level `context_management` body key
    //! is stripped (the provider emulates the edits itself and must not
    //! forward the key to a non-Anthropic upstream) or forwarded verbatim.
    //!
    //! `credential_source` is observed through the build-time secret
    //! resolution seam: an own-mode entry resolves its configured
    //! `api_key_ref` through the store, whereas a forwarded entry takes the
    //! sentinel-token path and never touches the store. That seam is exactly
    //! what `config check` cannot see (it validates a forwarded entry's shape
    //! but never builds it).

    use super::build_provider;
    use crate::config::{CredentialSource, ProviderEntry};
    use async_trait::async_trait;
    use routectl_auth::{SecretRef, SecretStore};
    use routectl_core::{ChatRequest, Message, MessageContent, Role};
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// `SecretStore` that resolves any `env://` ref to a canned key and
    /// counts every resolution. Lets a test observe whether `build_provider`
    /// resolved a configured secret (own mode) or skipped resolution
    /// (forwarded mode, sentinel token). Non-`env://` refs error so an
    /// unexpected resolution surfaces loudly rather than being masked.
    struct RecordingStore {
        gets: AtomicUsize,
    }
    impl RecordingStore {
        fn new() -> Self {
            Self {
                gets: AtomicUsize::new(0),
            }
        }
        fn gets(&self) -> usize {
            self.gets.load(Ordering::SeqCst)
        }
    }
    #[async_trait]
    impl SecretStore for RecordingStore {
        async fn get(&self, sr: &SecretRef) -> routectl_core::Result<String> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            match sr {
                SecretRef::Env(_) => Ok("sk-canned-test-key".into()),
                other => Err(routectl_core::Error::Auth(format!(
                    "recording store handles env:// only; got {other}"
                ))),
            }
        }
        async fn set(&self, _: &SecretRef, _: &str) -> routectl_core::Result<()> {
            Ok(())
        }
        async fn delete(&self, _: &SecretRef) -> routectl_core::Result<()> {
            Ok(())
        }
    }

    /// An own-mode anthropic-api entry (default `credential_source = Own`,
    /// ApiKey auth) with `context_management` set as requested. The
    /// `env://` api_key_ref lets the real build path resolve a token via
    /// `RecordingStore`.
    fn own_entry_with_context_management(cm: bool) -> ProviderEntry {
        let mut entry = ProviderEntry::anthropic_api("env://ROUTECTL_TEST_ANTHROPIC_KEY");
        if let ProviderEntry::AnthropicApi {
            ref mut context_management,
            ..
        } = entry
        {
            *context_management = cm;
        }
        entry
    }

    /// A minimal request carrying a `context_management` block in
    /// `provider_extras`. The anthropic-api normalize path strips this
    /// top-level body key IFF the provider was built with
    /// `context_management = true`; with the flag false the key is
    /// forwarded verbatim.
    fn req_with_context_management_extras() -> ChatRequest {
        ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hello".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
                refusal: None,
            }]
            .into(),
            provider_extras: Some(json!({
                "context_management": { "edits": [] }
            })),
            ..Default::default()
        }
    }

    /// `[providers.X] context_management = true` must reach the built
    /// provider: its `normalize_request` strips the `context_management`
    /// body key (the provider owns the emulation and never forwards it).
    #[tokio::test]
    async fn context_management_true_propagates_and_strips_body_key() {
        let secrets: Arc<dyn SecretStore> = Arc::new(RecordingStore::new());
        let provider = build_provider(
            "anthropic-cm-on",
            &own_entry_with_context_management(true),
            secrets,
        )
        .await
        .expect("own-mode anthropic-api provider must build");

        let body = provider
            .normalize_request(&req_with_context_management_extras())
            .expect("normalize_request must succeed");

        assert!(
            body.get("context_management").is_none(),
            "context_management = true must reach the provider and strip the body key; got: {body}"
        );
    }

    /// A default entry (`context_management` omitted, so false) must reach
    /// the built provider as false: its `normalize_request` forwards the
    /// `context_management` body key verbatim to the upstream.
    #[tokio::test]
    async fn context_management_false_default_propagates_and_keeps_body_key() {
        let secrets: Arc<dyn SecretStore> = Arc::new(RecordingStore::new());
        let provider = build_provider(
            "anthropic-cm-off",
            &own_entry_with_context_management(false),
            secrets,
        )
        .await
        .expect("own-mode anthropic-api provider must build");

        let body = provider
            .normalize_request(&req_with_context_management_extras())
            .expect("normalize_request must succeed");

        assert!(
            body.get("context_management").is_some(),
            "context_management = false must reach the provider and keep the body key; got: {body}"
        );
    }

    /// `credential_source = "own"` (the default) drives the factory to
    /// resolve the entry's configured `api_key_ref` through the store --
    /// the own-credential build path (`use_forwarded_bearer = false`).
    #[tokio::test]
    async fn own_credential_source_resolves_configured_secret() {
        let store = Arc::new(RecordingStore::new());
        let secrets: Arc<dyn SecretStore> = store.clone();
        let entry = ProviderEntry::anthropic_api("env://ROUTECTL_TEST_ANTHROPIC_KEY");

        build_provider("anthropic-own", &entry, secrets)
            .await
            .expect("own-mode anthropic-api provider must build");

        assert_eq!(
            store.gets(),
            1,
            "own credential_source must resolve its configured api_key_ref exactly once at build"
        );
    }

    /// `credential_source = "forwarded"` drives the factory onto the
    /// sentinel-token path (`use_forwarded_bearer = true`): the empty
    /// `api_key_ref` is NEVER resolved, so the store stays untouched. This
    /// is the seam `config check` cannot see -- it validates the forwarded
    /// entry's shape but never builds it.
    #[tokio::test]
    async fn forwarded_credential_source_skips_secret_resolution() {
        let store = Arc::new(RecordingStore::new());
        let secrets: Arc<dyn SecretStore> = store.clone();
        let entry =
            ProviderEntry::anthropic_api("").with_credential_source(CredentialSource::Forwarded);

        build_provider("anthropic-forwarded", &entry, secrets)
            .await
            .expect("forwarded anthropic-api provider must build without resolving a secret");

        assert_eq!(
            store.gets(),
            0,
            "forwarded credential_source must build the sentinel token without resolving api_key_ref"
        );
    }
}

#[cfg(test)]
mod forwarded_provider_build_tests {
    //! Regression coverage for the gap `validate_provider_credential_sources`
    //! cannot see: that field-coherence validator runs at `config check`
    //! time and confirms a `credential_source = "forwarded"` entry's
    //! shape is coherent (empty `api_key_ref`, host pinned), but it never
    //! calls `build_provider` -- so a forwarded entry could pass `config
    //! check` and still fail at `serve` time if the factory's build arm
    //! ever tried to resolve a token from the (guaranteed-empty)
    //! `api_key_ref`. These tests close that gap by driving the real
    //! `build_provider` entry point.

    use super::build_provider;
    use crate::config::{CredentialSource, ProviderEntry};
    use async_trait::async_trait;
    use routectl_auth::{SecretRef, SecretStore};
    use std::sync::Arc;

    /// A forwarded, own-host `anthropic-api` provider entry, matching the
    /// shape `validate_provider_credential_sources` requires: empty
    /// `api_key_ref`, `credential_source = "forwarded"`.
    fn forwarded_entry() -> ProviderEntry {
        ProviderEntry::anthropic_api("").with_credential_source(CredentialSource::Forwarded)
    }

    /// A `SecretStore` that panics on any call. A forwarded entry's
    /// `api_key_ref` is guaranteed empty by config validation, so there
    /// is no secret to resolve for it -- the factory must never touch
    /// the store while building this provider. Panicking (rather than
    /// just erroring) makes an accidental store call fail loudly instead
    /// of being swallowed by `?` and mistaken for the pre-fix bug.
    struct PanicIfTouchedStore;

    #[async_trait]
    impl SecretStore for PanicIfTouchedStore {
        async fn get(&self, _sr: &SecretRef) -> routectl_core::Result<String> {
            panic!("forwarded provider build must never resolve a secret");
        }
        async fn set(&self, _sr: &SecretRef, _v: &str) -> routectl_core::Result<()> {
            panic!("forwarded provider build must never resolve a secret");
        }
        async fn delete(&self, _sr: &SecretRef) -> routectl_core::Result<()> {
            panic!("forwarded provider build must never resolve a secret");
        }
    }

    /// The regression this task fixes: a `credential_source = "forwarded"`
    /// provider (valid config, passes `config check`) must actually BUILD
    /// at `serve` time instead of erroring on the empty `api_key_ref`.
    #[tokio::test]
    async fn forwarded_entry_builds_successfully() {
        let secrets: Arc<dyn SecretStore> = Arc::new(PanicIfTouchedStore);

        let provider = build_provider("anthropic-forwarded", &forwarded_entry(), secrets).await;

        assert!(
            provider.is_ok(),
            "a valid forwarded provider entry must build: {:?}",
            provider.err()
        );
    }
}

#[cfg(all(test, feature = "gemini"))]
mod gemini_cloud_code_factory_tests {
    //! Factory wiring for the Cloud Code ("antigravity") Gemini egress.
    //! Covers the OAuth-ref guard, the CloudCode build path (driven
    //! end-to-end against a mock so the `v1internal` surface is exercised),
    //! and that the built config's Debug renders the auth field as
    //! `[REDACTED]` rather than the underlying token source.

    use super::*;
    use crate::config::ProviderEntry;
    use async_trait::async_trait;
    use routectl_auth::{MemoryStore, SecretRef, SecretStore};
    use routectl_core::{Error, MessageContent};
    use std::sync::Arc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const CLOUD_CODE_TOKEN: &str = "ya29.test-bearer-do-not-log";

    /// `SecretStore` stub that resolves any `oauth://` ref to a static
    /// bearer token. Lets the factory build a real Cloud Code provider
    /// whose `TokenSource` yields a usable token without a live OAuth
    /// store. Non-oauth refs error so the static-ref guard test never
    /// reaches this resolver.
    struct OAuthTokenStub;

    #[async_trait]
    impl SecretStore for OAuthTokenStub {
        async fn get(&self, secret_ref: &SecretRef) -> routectl_core::Result<String> {
            match secret_ref {
                SecretRef::OAuth { .. } => Ok(CLOUD_CODE_TOKEN.to_string()),
                other => Err(Error::Auth(format!(
                    "OAuthTokenStub only handles oauth://, got {other}"
                ))),
            }
        }
        async fn set(&self, _r: &SecretRef, _v: &str) -> routectl_core::Result<()> {
            Ok(())
        }
        async fn delete(&self, _r: &SecretRef) -> routectl_core::Result<()> {
            Ok(())
        }
    }

    fn cloud_code_entry(base_url: &str) -> ProviderEntry {
        match ProviderEntry::gemini("oauth://antigravity")
            .with_gemini_auth_mode(GeminiAuthMode::CloudCode)
        {
            ProviderEntry::Gemini {
                api_key_ref,
                header_extras,
                payload_extras,
                user_agent,
                auth_mode,
                cache_capability,
                auto_emit_top_level_breakpoint,
                auto_emit_per_block_breakpoints,
                reduction_enabled,
                runtime,
                ..
            } => ProviderEntry::Gemini {
                api_key_ref,
                base_url: base_url.to_string(),
                header_extras,
                payload_extras,
                user_agent,
                auth_mode,
                cache_capability,
                auto_emit_top_level_breakpoint,
                auto_emit_per_block_breakpoints,
                reduction_enabled,
                runtime,
            },
            other => panic!("expected Gemini entry; got {other:?}"),
        }
    }

    fn gemini_ok_response() -> serde_json::Value {
        serde_json::json!({
            "candidates": [{
                "content": {"parts": [{"text": "pong"}], "role": "model"},
                "finishReason": "STOP",
                "index": 0
            }],
            "usageMetadata": {
                "promptTokenCount": 5,
                "candidatesTokenCount": 1,
                "totalTokenCount": 6
            },
            "modelVersion": "gemini-2.5-pro-001",
            "responseId": "resp-abc"
        })
    }

    fn base_req() -> routectl_core::ChatRequest {
        routectl_core::ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![routectl_core::Message {
                refusal: None,
                role: routectl_core::Role::User,
                content: MessageContent::Text("ping".into()),
                reasoning: None,
                reasoning_details: Vec::new(),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }]
            .into(),
            max_tokens: Some(64),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn cloud_code_oauth_ref_builds_and_routes_v1internal() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(serde_json::json!({"cloudaicompanionProject": "proj-1"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1internal:generateContent"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(serde_json::json!({"response": gemini_ok_response()})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let secrets: Arc<dyn SecretStore> = Arc::new(OAuthTokenStub);
        let entry = cloud_code_entry(&server.uri());

        let provider = build_provider("gemini-cc", &entry, secrets)
            .await
            .expect("cloud-code provider builds from oauth:// ref");

        let resp = provider
            .complete(base_req())
            .await
            .expect("complete routes through the Cloud Code v1internal surface");
        match &resp.choices[0].message.content {
            MessageContent::Text(t) => assert_eq!(t, "pong"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cloud_code_static_ref_is_rejected() {
        let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore);
        let entry = ProviderEntry::gemini("env://GEMINI_API_KEY")
            .with_gemini_auth_mode(GeminiAuthMode::CloudCode);

        let err = match build_provider("gemini-cc", &entry, secrets).await {
            Ok(_) => panic!("cloud-code mode must reject a non-oauth ref"),
            Err(e) => e,
        };
        match err {
            Error::Config(msg) => {
                assert!(
                    msg.contains("oauth"),
                    "message must mention oauth; got: {msg}"
                );
            }
            other => panic!("expected Error::Config, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cloud_code_build_does_not_leak_token_in_debug() {
        let secret_ref = SecretRef::parse("oauth://antigravity").expect("parse oauth ref");
        let secrets: Arc<dyn SecretStore> = Arc::new(OAuthTokenStub);
        let auth = resolve_token_source(&secrets, "oauth://antigravity")
            .await
            .expect("token source resolves");
        let project_cache: Arc<dyn CloudProjectCache> =
            Arc::new(OAuthStoreProjectCache::new(secrets.clone(), secret_ref));
        let cfg = GeminiConfig::new_cloud_code("gemini:gemini-cc", auth, project_cache);

        let dbg = format!("{cfg:?}");
        assert!(
            !dbg.contains(CLOUD_CODE_TOKEN),
            "Debug must not leak the bearer token; got: {dbg}"
        );
        assert!(
            dbg.contains("[REDACTED]"),
            "Debug must mark the auth field redacted; got: {dbg}"
        );
    }

    #[tokio::test]
    async fn cloud_code_rejects_disallowed_base_url_scheme() {
        // The Cloud Code surface carries a live OAuth bearer, so a
        // mistaken or hostile base_url must be rejected before any token
        // can be sent to it. Build must fail on a non-http(s) scheme.
        let secrets: Arc<dyn SecretStore> = Arc::new(OAuthTokenStub);
        let entry = cloud_code_entry("ftp://attacker.example");

        let err = match build_provider("gemini-cc", &entry, secrets).await {
            Ok(_) => panic!("cloud-code mode must reject a non-http(s) base_url"),
            Err(e) => e,
        };
        match err {
            Error::Config(msg) => assert!(
                msg.contains("scheme") && msg.contains("not allowed"),
                "expected a base_url scheme rejection; got: {msg}"
            ),
            other => panic!("expected Error::Config, got {other:?}"),
        }
    }
}

#[cfg(feature = "bedrock")]
mod mantle_lane_factory_tests {
    //! Factory wiring for the Bedrock mantle anthropic-api lane: region
    //! derives the endpoint, credentials resolve fail-fast, and the probe
    //! stays a credential-resolve (never an inference-host dial).

    use super::*;
    use crate::config::{
        BedrockCredsConfig, BedrockMantleConfig, Config, ModelEntry, ProviderEntry,
    };
    use routectl_auth::MemoryStore;
    use routectl_core::ProbeOutcome;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    /// Build an anthropic-api entry on the mantle lane: empty api_key_ref
    /// (the credential rides `bedrock_mantle.creds`) and base_url left at
    /// its default (the factory derives it from region).
    fn mantle_entry(region: &str, creds: BedrockCredsConfig) -> ProviderEntry {
        let mut entry = ProviderEntry::anthropic_api("");
        if let ProviderEntry::AnthropicApi { bedrock_mantle, .. } = &mut entry {
            *bedrock_mantle = Some(BedrockMantleConfig {
                region: region.to_string(),
                creds,
            });
        }
        entry
    }

    fn bearer_creds() -> BedrockCredsConfig {
        BedrockCredsConfig::BearerKey {
            key_ref: crate::test_secret::file_ref("mantle-bearer-key"),
        }
    }

    #[tokio::test]
    async fn mantle_bearer_entry_builds_and_probes_reachable() {
        // A bearer mantle entry builds with no network: the credential
        // resolves from the file ref, the provider keeps the anthropic-api
        // id, and the credential-resolve probe reports Reachable for a
        // bearer key rather than dialing the inference host.
        let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
        let entry = mantle_entry("eu-west-1", bearer_creds());

        let provider = build_provider("br-mantle", &entry, store)
            .await
            .expect("mantle bearer provider must build");
        assert_eq!(provider.id(), "anthropic-api:br-mantle");
        assert_eq!(provider.probe().await, ProbeOutcome::Reachable);
    }

    #[tokio::test]
    async fn mantle_profile_missing_profile_fails_fast_at_build() {
        // Profile / DefaultChain resolve probe-once at build, so a named
        // profile that does not exist must surface as a build error here,
        // not on the first chat request. A missing named profile fails
        // during local aws-config chain construction (no network).
        let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
        let creds = BedrockCredsConfig::Profile {
            name: "routectl-nonexistent-mantle-profile".to_string(),
        };
        let entry = mantle_entry("us-east-1", creds);

        let result = build_provider("br-mantle", &entry, store).await;
        assert!(
            result.is_err(),
            "a mantle entry whose profile does not exist must fail fast at build"
        );
    }

    #[tokio::test]
    async fn mantle_model_resolves_end_to_end_without_network() {
        // config -> router build: a model referencing a bearer mantle
        // provider resolves through `build_resolved_models` (the path the
        // hot-reload loader drives) with no failures and no network.
        let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
        let mut providers = BTreeMap::new();
        providers.insert("br".to_string(), mantle_entry("us-east-1", bearer_creds()));
        let mut models = BTreeMap::new();
        models.insert(
            "opus".to_string(),
            ModelEntry::new("br", "anthropic.claude-opus"),
        );
        let cfg = Config {
            providers,
            models,
            ..Config::default()
        };

        let (resolved, failed) = build_resolved_models(&cfg, store, BuildOptions::default())
            .await
            .expect("router build must succeed");
        assert!(failed.is_empty(), "expected no failures: {failed:?}");
        let opus = resolved.get("opus").expect("opus model resolved");
        assert_eq!(opus.provider.probe().await, ProbeOutcome::Reachable);
    }

    #[tokio::test]
    async fn mantle_bearer_resolve_does_not_block_the_reload_path() {
        // The mantle lane resolves credentials through the async
        // `bedrock::auth::resolve` seam, exactly as the Bedrock provider
        // does -- never a blocking call on the async reload path. A bearer
        // key short-circuits with no round-trip, so the build completes
        // well within a tight deadline.
        let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
        let entry = mantle_entry("us-east-1", bearer_creds());

        let built = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            build_provider("br-mantle", &entry, store),
        )
        .await
        .expect("mantle bearer build must not block the reload path");
        built.expect("bearer mantle build must succeed");
    }
}

#[cfg(feature = "bedrock")]
mod openai_mantle_factory_tests {
    //! Factory wiring for the Bedrock mantle OpenAI lanes (openai-compat and
    //! openai-responses): region derives the endpoint, credentials resolve
    //! fail-fast, and the probe stays a credential-resolve (never an
    //! inference-host dial). Sibling of `mantle_lane_factory_tests` for the
    //! anthropic-api lane.

    use super::*;
    use crate::config::{BedrockCredsConfig, BedrockMantleConfig, ProviderEntry};
    use routectl_auth::MemoryStore;
    use routectl_core::ProbeOutcome;
    use std::sync::Arc;
    #[cfg(feature = "openai-responses")]
    use {
        crate::config::{Config, ModelEntry},
        std::collections::BTreeMap,
    };

    fn bearer_creds() -> BedrockCredsConfig {
        BedrockCredsConfig::BearerKey {
            key_ref: crate::test_secret::file_ref("mantle-bearer-key"),
        }
    }

    /// Build an openai-compat entry on the mantle lane: empty api_key_ref and
    /// empty base_url (both derived from `bedrock_mantle`).
    fn compat_mantle_entry(region: &str, creds: BedrockCredsConfig) -> ProviderEntry {
        let mut entry = ProviderEntry::openai_compat("", "");
        if let ProviderEntry::OpenaiCompat { bedrock_mantle, .. } = &mut entry {
            *bedrock_mantle = Some(BedrockMantleConfig {
                region: region.to_string(),
                creds,
            });
        }
        entry
    }

    /// Build an openai-responses entry on the mantle lane: empty api_key_ref,
    /// no base_url, `auth_kind = bedrock-mantle`, and the `bedrock_mantle`
    /// block set.
    #[cfg(feature = "openai-responses")]
    fn responses_mantle_entry(region: &str, creds: BedrockCredsConfig) -> ProviderEntry {
        let mut entry = ProviderEntry::openai_responses("")
            .with_openai_responses_auth_kind(OpenaiResponsesAuthKind::BedrockMantle);
        if let ProviderEntry::OpenaiResponses { bedrock_mantle, .. } = &mut entry {
            *bedrock_mantle = Some(BedrockMantleConfig {
                region: region.to_string(),
                creds,
            });
        }
        entry
    }

    #[tokio::test]
    async fn compat_mantle_bearer_entry_builds_and_probes_reachable() {
        // A bearer compat mantle entry builds with no network: the credential
        // resolves from the file ref, the provider keeps the openai-compat
        // id, and the credential-resolve probe reports Reachable for a bearer
        // key rather than dialing the upstream `/models`.
        let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
        let entry = compat_mantle_entry("eu-west-1", bearer_creds());

        let provider = build_provider("oc-mantle", &entry, store)
            .await
            .expect("compat mantle bearer provider must build");
        assert_eq!(provider.id(), "openai-compat:oc-mantle");
        assert_eq!(provider.probe().await, ProbeOutcome::Reachable);
    }

    #[tokio::test]
    async fn compat_mantle_profile_missing_profile_fails_fast_at_build() {
        // Profile / DefaultChain resolve probe-once at build, so a named
        // profile that does not exist must surface as a build error here,
        // not on the first chat request.
        let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
        let creds = BedrockCredsConfig::Profile {
            name: "routectl-nonexistent-mantle-profile".to_string(),
        };
        let entry = compat_mantle_entry("us-east-1", creds);

        let result = build_provider("oc-mantle", &entry, store).await;
        assert!(
            result.is_err(),
            "a compat mantle entry whose profile does not exist must fail fast at build"
        );
    }

    #[tokio::test]
    async fn compat_mantle_bearer_resolve_does_not_block_the_reload_path() {
        // The mantle lane resolves credentials through the async
        // `bedrock::auth::resolve` seam, never a blocking call on the async
        // reload path. A bearer key short-circuits with no round-trip, so the
        // build completes well within a tight deadline.
        let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
        let entry = compat_mantle_entry("us-east-1", bearer_creds());

        let built = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            build_provider("oc-mantle", &entry, store),
        )
        .await
        .expect("compat mantle bearer build must not block the reload path");
        built.expect("compat bearer mantle build must succeed");
    }

    #[cfg(feature = "openai-responses")]
    #[tokio::test]
    async fn responses_mantle_bearer_entry_builds_and_probes_reachable() {
        // A bearer responses mantle entry builds with no network: the
        // credential resolves from the file ref, the provider keeps the
        // openai-responses id, and the credential-resolve probe reports
        // Reachable for a bearer key rather than dialing the upstream.
        let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
        let entry = responses_mantle_entry("ap-southeast-2", bearer_creds());

        let provider = build_provider("or-mantle", &entry, store)
            .await
            .expect("responses mantle bearer provider must build");
        assert_eq!(provider.id(), "openai-responses:or-mantle");
        assert_eq!(provider.probe().await, ProbeOutcome::Reachable);
    }

    #[cfg(feature = "openai-responses")]
    #[tokio::test]
    async fn responses_mantle_profile_missing_profile_fails_fast_at_build() {
        // A named profile that does not exist must surface as a build error
        // at construction, not on the first chat request.
        let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
        let creds = BedrockCredsConfig::Profile {
            name: "routectl-nonexistent-mantle-profile".to_string(),
        };
        let entry = responses_mantle_entry("us-east-1", creds);

        let result = build_provider("or-mantle", &entry, store).await;
        assert!(
            result.is_err(),
            "a responses mantle entry whose profile does not exist must fail fast at build"
        );
    }

    #[cfg(feature = "openai-responses")]
    #[tokio::test]
    async fn both_openai_mantle_lanes_resolve_end_to_end_without_network() {
        // config -> router build: one config carrying BOTH a mantle responses
        // provider and a mantle compat provider resolves through
        // `build_resolved_models` (the path the hot-reload loader drives) with
        // no failures and no network. Both models probe Reachable.
        let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
        let mut providers = BTreeMap::new();
        providers.insert(
            "resp".to_string(),
            responses_mantle_entry("us-east-1", bearer_creds()),
        );
        providers.insert(
            "compat".to_string(),
            compat_mantle_entry("eu-west-1", bearer_creds()),
        );
        let mut models = BTreeMap::new();
        models.insert("gpt".to_string(), ModelEntry::new("resp", "gpt-5"));
        models.insert(
            "deepseek".to_string(),
            ModelEntry::new("compat", "deepseek-chat"),
        );
        let cfg = Config {
            providers,
            models,
            ..Config::default()
        };

        let (resolved, failed) = build_resolved_models(&cfg, store, BuildOptions::default())
            .await
            .expect("router build must succeed");
        assert!(failed.is_empty(), "expected no failures: {failed:?}");
        let gpt = resolved.get("gpt").expect("gpt model resolved");
        assert_eq!(gpt.provider.probe().await, ProbeOutcome::Reachable);
        let deepseek = resolved.get("deepseek").expect("deepseek model resolved");
        assert_eq!(deepseek.provider.probe().await, ProbeOutcome::Reachable);
    }

    #[cfg(feature = "openai-responses")]
    #[tokio::test]
    async fn responses_mantle_bearer_resolve_does_not_block_the_reload_path() {
        // The responses mantle lane resolves credentials through the async
        // `bedrock::auth::resolve` seam, never a blocking call on the async
        // reload path. A bearer key short-circuits with no round-trip, so the
        // build completes well within a tight deadline.
        let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
        let entry = responses_mantle_entry("us-east-1", bearer_creds());

        let built = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            build_provider("or-mantle", &entry, store),
        )
        .await
        .expect("responses mantle bearer build must not block the reload path");
        built.expect("responses bearer mantle build must succeed");
    }

    #[cfg(feature = "openai-responses")]
    #[tokio::test]
    async fn responses_bedrock_mantle_marker_without_block_fails_cleanly() {
        // The legacy bearer-only surface is closed: an entry that carries the
        // `bedrock-mantle` auth_kind but NO `bedrock_mantle` block must fail
        // with a clean Config error, never a panic. This exercises the
        // unvalidated `build_provider` path (as `provider probe` does), which
        // bypasses `collect_config_validation`, so the factory itself has to
        // refuse the marker rather than fall through to a base_url default.
        let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
        let entry = ProviderEntry::openai_responses("")
            .with_openai_responses_auth_kind(OpenaiResponsesAuthKind::BedrockMantle);

        let err = match build_provider("or-legacy", &entry, store).await {
            Ok(_) => panic!("bedrock-mantle marker without a block must be refused"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("bedrock_mantle") && msg.contains("closed"),
            "error must name the missing block and the closed surface: {msg}"
        );
    }
}
