//! Unit tests for the v0.6.0 `merge_header_extras` helper.
use super::*;

/// Minimal provider stub so the `apply_layered_overlays` fixture can
/// build a real `Arc<ResolvedModel>` (which requires an
/// `Arc<dyn Provider>`). None of its methods are called by
/// `apply_layered_overlays`, which reads only the model's config
/// overlays.
struct StubProvider;

#[async_trait::async_trait]
impl Provider for StubProvider {
    fn id(&self) -> &'static str {
        "stub"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("stub", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        unreachable!()
    }
    async fn stream(
        &self,
        _: ChatRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<ChatChunk>>> {
        unreachable!()
    }
}

fn req_with_betas(betas: Vec<&str>) -> ChatRequest {
    ChatRequest {
        model: "any".into(),
        messages: vec![],
        anthropic_beta: betas.into_iter().map(String::from).collect(),
        ..Default::default()
    }
}

fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

#[test]
fn empty_both_is_noop() {
    let mut req = req_with_betas(vec![]);
    merge_header_extras("p", None, &BTreeMap::new(), &mut req);
    assert!(req.anthropic_beta.is_empty());
}

#[test]
fn anthropic_beta_unions_three_sources_in_visit_order() {
    let mut req = req_with_betas(vec!["foo"]);
    let provider = map(&[("anthropic-beta", "claude-code-20250219,oauth-2025-04-20")]);
    let model = map(&[("anthropic-beta", "context-1m-2025-08-07")]);
    merge_header_extras("p", Some(&provider), &model, &mut req);
    assert_eq!(
        req.anthropic_beta,
        vec![
            "foo".to_string(),
            "claude-code-20250219".to_string(),
            "oauth-2025-04-20".to_string(),
            "context-1m-2025-08-07".to_string(),
        ]
    );
}

#[test]
fn anthropic_beta_dedups_across_sources() {
    let mut req = req_with_betas(vec!["foo", "bar"]);
    let provider = map(&[("anthropic-beta", "foo,baz")]);
    let model = map(&[("anthropic-beta", "bar,qux")]);
    merge_header_extras("p", Some(&provider), &model, &mut req);
    assert_eq!(
        req.anthropic_beta,
        vec![
            "foo".to_string(),
            "bar".to_string(),
            "baz".to_string(),
            "qux".to_string()
        ]
    );
}

#[test]
fn model_only_anthropic_beta_lifts_onto_req() {
    let mut req = req_with_betas(vec![]);
    let model = map(&[("anthropic-beta", "context-1m-2025-08-07")]);
    merge_header_extras("p", None, &model, &mut req);
    assert_eq!(
        req.anthropic_beta,
        vec!["context-1m-2025-08-07".to_string()]
    );
}

#[test]
fn auth_reserved_keys_drop_but_other_keys_survive() {
    // Pairing the reserved key with a non-reserved key gives a real
    // observable: the reserved key MUST NOT land on the merged map
    // published via `req.routectl_internal.header_extras`, while
    // the non-reserved sibling MUST land.
    let mut req = req_with_betas(vec![]);
    let model = map(&[("authorization", "Bearer evil"), ("x-app", "ok")]);
    merge_header_extras("p", None, &model, &mut req);
    let published = req
        .routectl_internal
        .header_extras
        .expect("merger publishes a map");
    assert!(
        !published.contains_key("authorization"),
        "auth-reserved key must drop, not propagate",
    );
    assert_eq!(
        published.get("x-app").map(String::as_str),
        Some("ok"),
        "non-reserved sibling on the same model entry must reach the published map",
    );
}

#[test]
fn managed_reserved_keys_drop_but_other_keys_survive() {
    let mut req = req_with_betas(vec![]);
    let model = map(&[
        ("host", "evil.example.com"),
        ("content-type", "text/plain"),
        ("x-app", "ok"),
    ]);
    merge_header_extras("p", None, &model, &mut req);
    let published = req
        .routectl_internal
        .header_extras
        .expect("merger publishes a map");
    assert!(!published.contains_key("host"));
    assert!(!published.contains_key("content-type"));
    assert_eq!(published.get("x-app").map(String::as_str), Some("ok"));
}

#[test]
fn non_list_header_model_wins_on_key_collision() {
    // Pin the model > provider precedence for plain key-value
    // headers. Without this contract, per-model header_extras
    // would only matter for `anthropic-beta`.
    let mut req = req_with_betas(vec![]);
    let provider = map(&[("x-foo", "provider-value")]);
    let model = map(&[("x-foo", "model-value")]);
    merge_header_extras("p", Some(&provider), &model, &mut req);
    let published = req
        .routectl_internal
        .header_extras
        .expect("merger publishes a map");
    assert_eq!(
        published.get("x-foo").map(String::as_str),
        Some("model-value"),
        "model header_extras must win on key collision (last-writer-wins)",
    );
}

#[test]
fn non_list_provider_only_header_propagates_to_published_map() {
    // Pure provider header (no per-model override) must still
    // reach the egress via the published map.
    let mut req = req_with_betas(vec![]);
    let provider = map(&[("x-stainless-arch", "x64")]);
    merge_header_extras("p", Some(&provider), &BTreeMap::new(), &mut req);
    let published = req
        .routectl_internal
        .header_extras
        .expect("merger publishes a map");
    assert_eq!(
        published.get("x-stainless-arch").map(String::as_str),
        Some("x64"),
    );
}

#[test]
fn anthropic_beta_stripped_from_published_map() {
    // After the list-valued union writes to `req.anthropic_beta`,
    // the merger MUST remove `anthropic-beta` from the published
    // header_extras map. Leaving it would cause the Anthropic-API
    // egress (which also unions with req.anthropic_beta) to emit
    // duplicate values on the wire.
    let mut req = req_with_betas(vec![]);
    let provider = map(&[("anthropic-beta", "claude-code-20250219")]);
    merge_header_extras("p", Some(&provider), &BTreeMap::new(), &mut req);
    let published = req
        .routectl_internal
        .header_extras
        .expect("merger publishes a map");
    assert!(
        !published.contains_key("anthropic-beta"),
        "anthropic-beta must be stripped from the published map (it rides on req.anthropic_beta instead)",
    );
    assert_eq!(req.anthropic_beta, vec!["claude-code-20250219".to_string()]);
}

#[test]
fn router_side_auth_and_managed_constants_are_disjoint() {
    // The router defines its own `AUTH_HEADERS` / `MANAGED_HEADERS`
    // constants for the merge_header_extras dispatch. http_client
    // has its own copies (the egress-side gate). Both copies must
    // be disjoint independently.
    for h in AUTH_HEADERS {
        assert!(
            !MANAGED_HEADERS.contains(h),
            "router-side: {h:?} appears in both AUTH and MANAGED",
        );
    }
    for h in MANAGED_HEADERS {
        assert!(
            !AUTH_HEADERS.contains(h),
            "router-side: {h:?} appears in both MANAGED and AUTH",
        );
    }
}

#[test]
fn apply_layered_overlays_records_operator_betas_excluding_client() {
    // Invariant: operator-configured betas (provider + model
    // header_extras) are recorded on `routectl_internal.operator_betas`
    // so the Anthropic-API egress can re-add them as a floor that
    // bypasses the per-provider `allowed_betas` allowlist. The
    // client/ingress betas (on `req.anthropic_beta`) MUST NOT leak
    // into that floor -- the allowlist still gates them.
    let mut config = Config::default();
    config.providers.insert(
        "test-prov".into(),
        crate::config::ProviderEntry::anthropic_api("literal:k")
            .with_header_extras(map(&[("anthropic-beta", "prov-beta")])),
    );

    let model: Arc<ResolvedModel> = Arc::new(
        ResolvedModel::new("nick", "test-prov", Arc::new(StubProvider), "claude-x")
            .with_header_extras(map(&[("anthropic-beta", "model-beta")])),
    );
    let target = into_one_dispatch_target(model);

    let mut req = req_with_betas(vec!["client-beta"]);
    apply_layered_overlays(&config, &target, &mut req);

    assert_eq!(
        req.routectl_internal.operator_betas,
        vec!["prov-beta".to_string(), "model-beta".to_string()],
        "operator_betas must hold the provider + model floor only",
    );
    assert!(
        !req.routectl_internal
            .operator_betas
            .iter()
            .any(|b| b == "client-beta"),
        "client/ingress betas must never enter the operator floor",
    );

    // `req.anthropic_beta` still carries the full union (client +
    // provider + model) so Bedrock's `filter_bedrock_betas` and the
    // log-safe summary see the complete set.
    for expected in ["client-beta", "prov-beta", "model-beta"] {
        assert!(
            req.anthropic_beta.iter().any(|b| b == expected),
            "req.anthropic_beta must carry the full union; missing {expected}",
        );
    }
}

/// Regression guard for the per-attempt overlay rebuild hazard:
/// `apply_layered_overlays` reconstructs `routectl_internal` from
/// `Default::default()` every dispatch attempt. Ingress-set provenance
/// must survive that rebuild rather than reset to `Library`.
#[test]
fn apply_layered_overlays_preserves_ingress_provenance() {
    let config = Config::default();
    let model: Arc<ResolvedModel> = Arc::new(ResolvedModel::new(
        "nick",
        "test-prov",
        Arc::new(StubProvider),
        "claude-x",
    ));
    let target = into_one_dispatch_target(model);

    let mut req = req_with_betas(vec![]);
    req.routectl_internal.provenance = routectl_core::RequestProvenance::AnthropicIngress;
    apply_layered_overlays(&config, &target, &mut req);

    assert_eq!(
        req.routectl_internal.provenance,
        routectl_core::RequestProvenance::AnthropicIngress,
        "ingress provenance must survive the per-attempt overlay rebuild",
    );
}

/// Regression guard for the same per-attempt overlay rebuild hazard:
/// the ingress-captured inbound per-conversation session key must
/// survive the rebuild rather than reset to `None` on a later attempt.
#[test]
fn apply_layered_overlays_preserves_inbound_session_key() {
    let config = Config::default();
    let model: Arc<ResolvedModel> = Arc::new(ResolvedModel::new(
        "nick",
        "test-prov",
        Arc::new(StubProvider),
        "claude-x",
    ));
    let target = into_one_dispatch_target(model);

    let mut req = req_with_betas(vec![]);
    req.routectl_internal.inbound_session_key = Some("sid-abc".into());
    apply_layered_overlays(&config, &target, &mut req);

    assert_eq!(
        req.routectl_internal.inbound_session_key.as_deref(),
        Some("sid-abc"),
        "inbound session key must survive the per-attempt overlay rebuild",
    );
}

/// Regression guard for the same per-attempt overlay rebuild hazard:
/// the ingress-forwarded bearer token must survive the rebuild rather
/// than reset to `None`, on the first attempt AND every subsequent
/// chain attempt (the rebuild runs once per dispatch attempt).
#[test]
fn apply_layered_overlays_preserves_forwarded_bearer() {
    let config = Config::default();
    let model: Arc<ResolvedModel> = Arc::new(ResolvedModel::new(
        "nick",
        "test-prov",
        Arc::new(StubProvider),
        "claude-x",
    ));
    let target = into_one_dispatch_target(model);

    let mut req = req_with_betas(vec![]);
    req.routectl_internal.forwarded_bearer = Some(routectl_core::schema::ForwardedBearer::new(
        "sk-forwarded".into(),
    ));

    for attempt in 1..=2 {
        apply_layered_overlays(&config, &target, &mut req);
        assert_eq!(
            req.routectl_internal
                .forwarded_bearer
                .as_ref()
                .map(routectl_core::schema::ForwardedBearer::expose),
            Some("sk-forwarded"),
            "forwarded bearer must survive the per-attempt overlay rebuild (attempt {attempt})",
        );
    }
}

/// Regression guard for the same per-attempt overlay rebuild hazard:
/// the ingress-captured forwarded `x-stainless-*` headers must survive
/// the rebuild rather than reset to empty, so the egress can present
/// the client's real fingerprint on every chain attempt, not just the
/// first.
#[test]
fn apply_layered_overlays_preserves_stainless_headers() {
    let config = Config::default();
    let model: Arc<ResolvedModel> = Arc::new(ResolvedModel::new(
        "nick",
        "test-prov",
        Arc::new(StubProvider),
        "claude-x",
    ));
    let target = into_one_dispatch_target(model);

    let mut req = req_with_betas(vec![]);
    req.routectl_internal.stainless_headers = vec![
        ("x-stainless-lang".to_string(), "js".to_string()),
        (
            "x-stainless-package-version".to_string(),
            "0.94.0-client".to_string(),
        ),
    ];

    for attempt in 1..=2 {
        apply_layered_overlays(&config, &target, &mut req);
        assert_eq!(
            req.routectl_internal.stainless_headers,
            vec![
                ("x-stainless-lang".to_string(), "js".to_string()),
                (
                    "x-stainless-package-version".to_string(),
                    "0.94.0-client".to_string()
                ),
            ],
            "stainless headers must survive the per-attempt overlay rebuild (attempt {attempt})",
        );
    }
}
