// Pin of the engine's beta repair across a fallback chain: a named beta
// rejection draws one repair, the repaired attempt is re-sent once, and when
// that attempt fails too the request learns nothing and the next target is
// sent the client's own beta set. The ceiling binds with a chain longer than
// it, so a repair that charged nothing would show as one more upstream call.
// An `include!`d FRAGMENT of `field_repair_tests.rs`.

mod beta_repair_chain {
    use super::*;

    use routectl_core::BetaAssembly;

    /// The flag the upstream rejects by name.
    const NAMED: &str = "zz-named-2099-01-01";
    /// A client flag the upstream accepts.
    const KEPT: &str = "kept-2099-01-01";
    const BETA_ALIAS: &str = "beta-repair-alias";
    /// One more target than the ceiling, so a free repair is visible.
    const TARGETS: usize = REPAIRS_PER_REQUEST as usize + 1;

    /// A target that rejects the named flag while it is sent, and refuses a
    /// request without it with an unlocalized 400 the walk falls back past.
    #[derive(Default)]
    struct BetaRejectingSeat {
        withheld_seen: Mutex<Vec<Vec<String>>>,
        client_seen: Mutex<Vec<Vec<String>>>,
    }

    impl BetaRejectingSeat {
        fn answer(&self, req: &ChatRequest) -> Error {
            let withheld = req.routectl_internal.withheld_betas.to_vec();
            self.client_seen.lock().push(req.anthropic_beta.clone());
            self.withheld_seen.lock().push(withheld.clone());
            if let Some(slot) = req.routectl_internal.beta_assembly.as_ref() {
                slot.record(BetaAssembly::new(Vec::new(), Vec::new()));
            }
            let message = if withheld.iter().any(|w| w == NAMED) {
                "the request is malformed".to_string()
            } else {
                format!(
                    "Unexpected value(s) `{NAMED}` for the `anthropic-beta` header. Please \
                     consult our documentation at platform.claude.com/docs or try again \
                     without the header."
                )
            };
            Error::upstream_full(
                "beta-seat",
                400,
                json!({"type": "error", "error": {"type": "invalid_request_error", "message": message}})
                    .to_string(),
                None,
                Some("invalid_request_error".into()),
                None,
            )
        }

        fn calls(&self) -> usize {
            self.withheld_seen.lock().len()
        }
    }

    #[async_trait::async_trait]
    impl Provider for BetaRejectingSeat {
        fn id(&self) -> &'static str {
            "beta-seat"
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response("beta-seat", "unused"))
        }
        async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
            Err(self.answer(&req))
        }
        async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            Err(self.answer(&req))
        }
    }

    fn beta_chain() -> (Router, Vec<Arc<BetaRejectingSeat>>) {
        let mut router = Router::new(Arc::new(chain_config(BETA_ALIAS, TARGETS)));
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        let mut seats = Vec::with_capacity(TARGETS);
        for idx in 0..TARGETS {
            let seat = Arc::new(BetaRejectingSeat::default());
            seats.push(seat.clone());
            models.insert(
                format!("m{idx}"),
                Arc::new(ResolvedModel::new(
                    format!("m{idx}"),
                    format!("p{idx}"),
                    seat as Arc<dyn Provider>,
                    format!("wire-{idx}"),
                )),
            );
        }
        router.install_resolved_models(models);
        (router, seats)
    }

    fn client_request() -> ChatRequest {
        ChatRequest {
            model: BETA_ALIAS.into(),
            messages: vec![].into(),
            anthropic_beta: vec![KEPT.to_string(), NAMED.to_string()],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_failed_beta_repair_draws_once_learns_nothing_and_falls_back_with_the_client_set() {
        // Arrange
        let (router, seats) = beta_chain();

        // Act
        let dispatched = router
            .complete_with_options(client_request(), RouterOptions::default())
            .await;

        // Assert: the first target saw exactly two upstream calls, the
        // rejected one and the repaired one.
        assert!(dispatched.result.is_err(), "every target refuses");
        assert_eq!(
            *seats[0].withheld_seen.lock(),
            vec![Vec::<String>::new(), vec![NAMED.to_string()]],
            "two upstream calls: the client set, then the repaired attempt",
        );
        // One draw per repair: the ceiling admits one repair per target until
        // it is spent, and the last target is refused one.
        let calls: Vec<usize> = seats.iter().map(|seat| seat.calls()).collect();
        let mut expected = vec![2; REPAIRS_PER_REQUEST as usize];
        expected.push(1);
        assert_eq!(calls, expected, "one draw per repaired target");
        // No verdict: the repaired attempt never succeeded.
        assert!(dispatched.meta.learned_capabilities.is_empty());
        assert!(router.learned_capabilities.is_empty());
        // The fallback target's first call carries the client's own set.
        assert_eq!(
            seats[1].client_seen.lock()[0],
            vec![KEPT.to_string(), NAMED.to_string()]
        );
        assert!(
            seats[1].withheld_seen.lock()[0].is_empty(),
            "the fallback target withholds nothing the first target repaired",
        );
    }
}
