//! Live Bedrock mantle matrix: OpenAI Responses vocabulary over the
//! managed mantle endpoint, signed with a Bedrock bearer key.

use super::*;

// -- Bedrock mantle OpenAI-Responses live matrix ----------------------------
//
// The mantle lane rides the openai-responses provider: the factory derives
// the endpoint host from `region`
// (`https://bedrock-mantle.<region>.api.aws/openai/v1`) and signs every
// request under the `bedrock-mantle` SigV4 scope. Here we build the provider
// directly with a resolved bearer credential and point it at the
// region-derived base, mirroring the per-model Arc fan-out the factory
// produces.
//
// Reads `AWS_BEARER_TOKEN_BEDROCK` (a Bedrock API key) and `AWS_REGION`
// (defaults to `us-east-1`). Skips cleanly when the key is unset.
//
// Run:
//   cargo test -p routectl-cli --features live-integration,bedrock --release \
//     --test live_matrix mantle_responses -- --nocapture --test-threads=1
//
// The model id is BARE (no inference-profile prefix): the mantle Responses
// lane carries the model verbatim in the request body. This id needs live
// confirmation against the mantle roster.

const MANTLE_MODEL: &str = "openai.gpt-oss-120b";

async fn build_mantle_test_router(model_id: &str) -> Option<Arc<Router>> {
    use routectl_providers::bedrock::{BedrockCreds, auth as bedrock_auth};
    use routectl_providers::mantle::{MantleAuth, mantle_openai_base};
    use routectl_providers::openai_responses::{
        AuthKind, OpenAiResponsesConfig, OpenAiResponsesProvider,
    };

    let key = std::env::var("AWS_BEARER_TOKEN_BEDROCK").ok()?;
    if key.trim().is_empty() {
        return None;
    }
    // Test-vehicle default only, mirroring the sibling bedrock live
    // tests; the production factory derives the endpoint solely from
    // the configured region and has no fallback.
    let region = std::env::var("AWS_REGION")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "us-east-1".into());

    use routectl_router::ResolvedModel;
    use std::sync::Arc as ArcAlias;

    let creds = BedrockCreds::BearerKey { key };
    let resolved = bedrock_auth::resolve(&creds, &region)
        .await
        .expect("resolve mantle bearer creds");

    let provider_name = format!("mantle-{}", sanitize_provider_name(model_id));
    let cfg = OpenAiResponsesConfig {
        id: format!("mantle:{provider_name}"),
        auth: Arc::new(routectl_core::StaticToken::new("")),
        account_id: None,
        base_url: mantle_openai_base(&region),
        auth_kind: AuthKind::BedrockMantle,
        header_extras: Vec::new(),
        user_agent: Some("routectl-live-test/0.4".into()),
        session_id: None,
        installation_id: None,
        mantle: Some(MantleAuth {
            region: region.clone(),
            creds: resolved,
        }),
    };
    let provider: ArcAlias<dyn routectl_core::Provider> =
        ArcAlias::new(OpenAiResponsesProvider::new(cfg));

    let nickname = alias_nickname(model_id);
    let mut providers = BTreeMap::new();
    let mut aliases = BTreeMap::new();
    let mut models = BTreeMap::new();
    let mut resolved_models: BTreeMap<String, ArcAlias<ResolvedModel>> = BTreeMap::new();

    // Placeholder provider entry so Router::new sees the provider name in
    // its config; the actual provider Arc is installed via the
    // resolved-models table below.
    providers.insert(
        provider_name.clone(),
        ProviderEntry::openai_compat("https://placeholder.invalid/v1", "literal:placeholder"),
    );
    models.insert(
        nickname.clone(),
        ModelEntry::new(provider_name.clone(), model_id.to_string()),
    );
    aliases.insert(model_id.to_string(), AliasValue::Single(nickname.clone()));
    resolved_models.insert(
        nickname.clone(),
        ArcAlias::new(ResolvedModel::new(
            nickname,
            provider_name,
            provider,
            model_id.to_string(),
        )),
    );

    let cfg = Arc::new(Config {
        server: Default::default(),
        providers,
        aliases,
        models,
        retry: Default::default(),
        ..Default::default()
    });

    let mut router = Router::new(cfg);
    router.install_resolved_models(resolved_models);
    Some(Arc::new(router))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mantle_responses_complete() {
    let Some(router) = build_mantle_test_router(MANTLE_MODEL).await else {
        eprintln!("skip: AWS_BEARER_TOKEN_BEDROCK not set");
        return;
    };

    let rows = run_matrix(vec![MANTLE_MODEL.to_string()], move |t| {
        let r = router.clone();
        async move { run_complete(r, t).await }
    })
    .await;
    print_summary("Bedrock mantle (responses)", "complete", &rows);

    let pass = rows.iter().filter(|r| r.ok).count();
    assert!(
        pass > 0,
        "Bedrock mantle responses: complete failed -- routectl or mantle lane broken"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mantle_responses_stream() {
    let Some(router) = build_mantle_test_router(MANTLE_MODEL).await else {
        eprintln!("skip: AWS_BEARER_TOKEN_BEDROCK not set");
        return;
    };

    let rows = run_matrix(vec![MANTLE_MODEL.to_string()], move |t| {
        let r = router.clone();
        async move { run_stream(r, t).await }
    })
    .await;
    print_summary("Bedrock mantle (responses)", "stream", &rows);
}
