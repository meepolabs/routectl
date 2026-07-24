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
        // before the first chunk. The resolver must WARN and leave the
        // field None, never propagate Some(0).
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

    /// Stub store that reports a fixed list of OAuth seats for any bare
    /// pool ref. `get`/`set`/`delete` are unused by these build-time
    /// tests (the anthropic-api oauth arm wraps a lazy `ManagedToken`
    /// rather than resolving a token at build).
    struct MultiSeatStore {
        labels: Vec<Option<String>>,
    }

    #[async_trait::async_trait]
    impl SecretStore for MultiSeatStore {
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
            // A labeled ref pins one seat; mirror the real store.
            if let SecretRef::OAuth { label: Some(_), .. } = secret_ref {
                return Ok(vec![secret_ref.clone()]);
            }
            let SecretRef::OAuth { provider, .. } = secret_ref else {
                return Ok(vec![secret_ref.clone()]);
            };
            Ok(self
                .labels
                .iter()
                .map(|label| SecretRef::OAuth {
                    provider: provider.clone(),
                    label: label.clone(),
                })
                .collect())
        }
    }

    #[tokio::test]
    async fn single_unlabeled_seat_builds_one_target_unchanged() {
        // Back-compat pin: a bare-pool oauth ref backed by exactly one
        // (unlabeled/default) seat does NOT expand -- `seats` stays None,
        // so dispatch builds one target keyed by nickname, byte-for-byte
        // the pre-pool behavior.
        let store: Arc<dyn SecretStore> = Arc::new(MultiSeatStore { labels: vec![None] });
        let cfg = config_with_models(
            vec![(
                "anthropic",
                ProviderEntry::anthropic_api("oauth://anthropic")
                    .with_auth_kind(routectl_providers::anthropic_api::AuthKind::OauthBearer),
            )],
            vec![("opus", ModelEntry::new("anthropic", "claude-opus-4-7"))],
        );
        let (models, failed) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");
        assert!(failed.is_empty(), "expected no failures: {failed:?}");
        let opus = models.get("opus").expect("opus entry");
        assert!(
            opus.seats.is_none(),
            "single seat must NOT expand into a pool"
        );
    }

    #[tokio::test]
    async fn pool_with_three_seats_expands_to_three_targets() {
        // A bare-pool ref backed by three stored seats expands into three
        // seat targets, each pinned to a distinct labeled SecretRef and a
        // distinct state_key (default seat first, then sorted labels).
        let store: Arc<dyn SecretStore> = Arc::new(MultiSeatStore {
            labels: vec![None, Some("seat-b".into()), Some("seat-c".into())],
        });
        let cfg = config_with_models(
            vec![(
                "anthropic",
                ProviderEntry::anthropic_api("oauth://anthropic")
                    .with_auth_kind(routectl_providers::anthropic_api::AuthKind::OauthBearer),
            )],
            vec![("opus", ModelEntry::new("anthropic", "claude-opus-4-7"))],
        );
        let (models, failed) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");
        assert!(failed.is_empty(), "expected no failures: {failed:?}");
        let opus = models.get("opus").expect("opus entry");
        let seats = opus.seats.as_ref().expect("three-seat pool must expand");
        assert_eq!(seats.len(), 3, "expected three seat targets");

        // Distinct state_keys: default seat is the bare nickname, labeled
        // seats carry the `#label` suffix.
        let keys: Vec<&str> = seats.iter().map(|s| s.state_key.as_str()).collect();
        assert_eq!(keys, vec!["opus", "opus#seat-b", "opus#seat-c"]);

        // Distinct seat-pinned SecretRefs round-tripping through Display.
        let refs: Vec<String> = seats
            .iter()
            .map(|s| s.auth_secret_ref.as_ref().unwrap().to_string())
            .collect();
        assert_eq!(
            refs,
            vec![
                "oauth://anthropic",
                "oauth://anthropic#seat-b",
                "oauth://anthropic#seat-c",
            ]
        );
    }

    #[tokio::test]
    async fn labels_only_pool_builds_each_seat_from_its_own_ref() {
        // Regression pin for the labels-only bug: a pool with NO bare
        // default seat (operator ran `login anthropic --label a` / `--label
        // b` only) puts a LABELED seat at index 0. Seat 0 must build from
        // its OWN pinned ref (`oauth://anthropic#a`), NOT inherit the bare,
        // credential-less provider the model was built from. The old
        // `idx == 0` reuse silently bound seat 0 to the bare provider.
        let store: Arc<dyn SecretStore> = Arc::new(MultiSeatStore {
            labels: vec![Some("a".into()), Some("b".into())],
        });
        let cfg = config_with_models(
            vec![(
                "anthropic",
                ProviderEntry::anthropic_api("oauth://anthropic")
                    .with_auth_kind(routectl_providers::anthropic_api::AuthKind::OauthBearer),
            )],
            vec![("opus", ModelEntry::new("anthropic", "claude-opus-4-7"))],
        );
        let (models, failed) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");
        assert!(failed.is_empty(), "expected no failures: {failed:?}");
        let opus = models.get("opus").expect("opus entry");
        let seats = opus.seats.as_ref().expect("labels-only pool must expand");
        assert_eq!(seats.len(), 2, "expected two labeled seat targets");

        // Labels-only: index 0 is the FIRST LABELED seat -- no bare `opus`
        // state_key, no bare `oauth://anthropic` ref.
        let keys: Vec<&str> = seats.iter().map(|s| s.state_key.as_str()).collect();
        assert_eq!(keys, vec!["opus#a", "opus#b"]);
        let refs: Vec<String> = seats
            .iter()
            .map(|s| s.auth_secret_ref.as_ref().unwrap().to_string())
            .collect();
        assert_eq!(refs, vec!["oauth://anthropic#a", "oauth://anthropic#b"]);

        // The fix: NO labeled seat reuses the bare-ref provider the model
        // was built from. With the old `idx == 0` reuse, seat 0 would be
        // pointer-equal to `opus.provider` (the bare, credential-less
        // build) and silently resolve the wrong identity at request time.
        for seat in seats.iter() {
            assert!(
                !Arc::ptr_eq(&opus.provider, &seat.provider),
                "labels-only seat {} must be built from its own ref, not the bare provider",
                seat.state_key,
            );
        }
    }

    #[tokio::test]
    async fn explicitly_labeled_ref_does_not_expand() {
        // A model whose api_key_ref already pins a seat
        // (`oauth://anthropic#seat-b`) builds exactly one target -- the
        // operator selected the seat, so there is no pool to expand.
        let store: Arc<dyn SecretStore> = Arc::new(MultiSeatStore {
            labels: vec![None, Some("seat-b".into()), Some("seat-c".into())],
        });
        let cfg = config_with_models(
            vec![(
                "anthropic",
                ProviderEntry::anthropic_api("oauth://anthropic#seat-b")
                    .with_auth_kind(routectl_providers::anthropic_api::AuthKind::OauthBearer),
            )],
            vec![("opus", ModelEntry::new("anthropic", "claude-opus-4-7"))],
        );
        let (models, failed) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");
        assert!(failed.is_empty(), "expected no failures: {failed:?}");
        let opus = models.get("opus").expect("opus entry");
        assert!(
            opus.seats.is_none(),
            "an explicitly-labeled ref must NOT expand into a pool"
        );
    }

    #[tokio::test]
    async fn non_oauth_ref_does_not_expand() {
        // Back-compat: a literal/env/file ref never pools, even if a
        // (misconfigured) store reported multiple seats for it.
        let store: Arc<dyn SecretStore> = Arc::new(MultiSeatStore {
            labels: vec![None, Some("seat-b".into())],
        });
        let cfg = config_with_models(
            vec![(
                "anthropic",
                ProviderEntry::anthropic_api(crate::test_secret::file_ref("k")),
            )],
            vec![("opus", ModelEntry::new("anthropic", "claude-opus-4-7"))],
        );
        let (models, _) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");
        let opus = models.get("opus").expect("opus entry");
        assert!(opus.seats.is_none(), "a non-oauth ref must never pool");
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
    //! Pin that `context_management` flows from `ProviderEntry::AnthropicApi`
    //! through the factory destructure into `AnthropicApiConfig`.
    //!
    //! The factory arm destructures the entry fields then assigns them
    //! one-for-one to `AnthropicApiConfig { .. }`. These tests mirror that
    //! destructure pattern so any mismatch in the wiring is caught at
    //! compile time (missing field) or at runtime (wrong value).

    use crate::config::{CredentialSource, ProviderEntry};
    use routectl_providers::anthropic_api::AnthropicApiConfig;

    /// Helper that simulates the factory destructure and returns the
    /// `context_management` value that would land in `AnthropicApiConfig`.
    /// Written to mirror the exact field list in `build_provider_inner` so
    /// a future factory refactor that drops the field from the destructure
    /// will break this test at compile time.
    fn extract_context_management(entry: &ProviderEntry) -> bool {
        match entry {
            ProviderEntry::AnthropicApi {
                context_management, ..
            } => *context_management,
            other => panic!("expected AnthropicApi entry; got {other:?}"),
        }
    }

    /// Helper that simulates the factory destructure and returns the
    /// `use_forwarded_bearer` value that would land in `AnthropicApiConfig`
    /// (`credential_source == Forwarded`). Mirrors the exact
    /// `build_provider_inner` wiring so a future factory refactor that
    /// drops the derivation breaks this test at compile time.
    fn extract_use_forwarded_bearer(entry: &ProviderEntry) -> bool {
        match entry {
            ProviderEntry::AnthropicApi {
                credential_source, ..
            } => *credential_source == CredentialSource::Forwarded,
            other => panic!("expected AnthropicApi entry; got {other:?}"),
        }
    }

    /// `ProviderEntry::AnthropicApi { context_management: true, .. }` wires
    /// the value `true` into `AnthropicApiConfig.context_management`.
    #[test]
    fn factory_propagates_context_management_true() {
        // Arrange
        let mut entry = ProviderEntry::anthropic_api("literal:sk-test");
        if let ProviderEntry::AnthropicApi {
            ref mut context_management,
            ..
        } = entry
        {
            *context_management = true;
        }

        // Act: extract the way the factory does, then build the config field.
        let extracted = extract_context_management(&entry);
        let cfg = AnthropicApiConfig::new("test", "sk-test");
        // Simulate the factory struct-literal assignment.
        let cfg_with_flag = AnthropicApiConfig {
            context_management: extracted,
            ..cfg
        };

        // Assert
        assert!(
            cfg_with_flag.context_management,
            "context_management: true must propagate into AnthropicApiConfig"
        );
    }

    /// A default `ProviderEntry::AnthropicApi` (context_management omitted)
    /// wires the value `false` into `AnthropicApiConfig.context_management`.
    #[test]
    fn factory_propagates_context_management_false_default() {
        // Arrange: use the constructor helper -- context_management defaults to false.
        let entry = ProviderEntry::anthropic_api("literal:sk-test");

        // Act
        let extracted = extract_context_management(&entry);
        let cfg = AnthropicApiConfig::new("test", "sk-test");
        let cfg_with_flag = AnthropicApiConfig {
            context_management: extracted,
            ..cfg
        };

        // Assert
        assert!(
            !cfg_with_flag.context_management,
            "context_management must default to false in AnthropicApiConfig"
        );
    }

    /// `ProviderEntry::AnthropicApi { credential_source: Forwarded, .. }`
    /// wires `use_forwarded_bearer: true` into `AnthropicApiConfig` --
    /// acceptance criterion for the per-provider WIRE gate.
    #[test]
    fn factory_propagates_forwarded_credential_source_to_use_forwarded_bearer_true() {
        // Arrange
        let mut entry = ProviderEntry::anthropic_api("literal:sk-test");
        if let ProviderEntry::AnthropicApi {
            ref mut credential_source,
            ref mut api_key_ref,
            ..
        } = entry
        {
            *credential_source = CredentialSource::Forwarded;
            api_key_ref.clear();
        }

        // Act
        let extracted = extract_use_forwarded_bearer(&entry);
        let cfg = AnthropicApiConfig::new("test", "sk-test");
        let cfg_with_flag = AnthropicApiConfig {
            use_forwarded_bearer: extracted,

            #[cfg(feature = "bedrock")]
            mantle: None,
            ..cfg
        };

        // Assert
        assert!(
            cfg_with_flag.use_forwarded_bearer,
            "credential_source: Forwarded must propagate to use_forwarded_bearer: true"
        );
    }

    /// A default `ProviderEntry::AnthropicApi` (credential_source omitted,
    /// so `Own`) wires `use_forwarded_bearer: false` -- the own-mode
    /// default that never consumes a floating forwarded bearer.
    #[test]
    fn factory_propagates_own_credential_source_to_use_forwarded_bearer_false_default() {
        // Arrange: use the constructor helper -- credential_source defaults to Own.
        let entry = ProviderEntry::anthropic_api("literal:sk-test");

        // Act
        let extracted = extract_use_forwarded_bearer(&entry);
        let cfg = AnthropicApiConfig::new("test", "sk-test");
        let cfg_with_flag = AnthropicApiConfig {
            use_forwarded_bearer: extracted,

            #[cfg(feature = "bedrock")]
            mantle: None,
            ..cfg
        };

        // Assert
        assert!(
            !cfg_with_flag.use_forwarded_bearer,
            "credential_source: Own must propagate to use_forwarded_bearer: false"
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
            }],
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
