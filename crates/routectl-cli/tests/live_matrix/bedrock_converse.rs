//! Live Bedrock Converse matrix: same bearer-key auth, api_shape = Converse.

use super::*;

// -- Bedrock Converse live matrix -------------------------------------------
//
// Same bearer-key auth as the Invoke matrix above. Models are the same
// cross-region inference profiles; the only difference is api_shape =
// Converse. The goal is to verify that the Converse adapter produces
// equivalent canonical output to the Invoke adapter for the same model.
//
// Run:
//   cargo test -p routectl-cli --features live-integration,bedrock --release \
//     --test live_matrix bedrock_converse -- --nocapture --test-threads=1
//
// Requires AWS_BEARER_TOKEN_BEDROCK and (optionally) AWS_REGION in env.
// Skips cleanly when the key is absent.

const BEDROCK_CONVERSE_MODELS: &[&str] = &[
    "us.anthropic.claude-haiku-4-5-20251001-v1:0",
    "us.anthropic.claude-3-5-haiku-20241022-v1:0",
];

async fn build_bedrock_converse_test_router(targets: &[&str]) -> Option<Arc<Router>> {
    use routectl_providers::bedrock::{
        BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth as bedrock_auth,
    };
    use routectl_router::ResolvedModel;
    use std::sync::Arc as ArcAlias;

    let key = std::env::var("AWS_BEARER_TOKEN_BEDROCK").ok()?;
    if key.trim().is_empty() {
        return None;
    }
    let region = std::env::var("AWS_REGION")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "us-east-1".into());

    let mut providers = BTreeMap::new();
    let mut aliases = BTreeMap::new();
    let mut models = BTreeMap::new();
    let mut resolved_models: BTreeMap<String, ArcAlias<ResolvedModel>> = BTreeMap::new();

    for model_id in targets {
        let provider_name = format!("bedrock-converse-{}", sanitize_provider_name(model_id));
        let creds = BedrockCreds::BearerKey { key: key.clone() };
        let resolved = bedrock_auth::resolve(&creds, &region)
            .await
            .expect("resolve bedrock bearer creds");
        let cfg = BedrockConfig {
            id: format!("bedrock:{provider_name}"),
            region: region.clone(),
            model_id: (*model_id).to_string(),
            api_shape: BedrockApiShape::Converse,
            creds,
            user_agent: Some("routectl-live-test/0.4".into()),
            header_extras: Vec::new(),
            anthropic_beta: Vec::new(),
            allowed_betas: vec![
                "context-1m-2025-08-07".into(),
                "claude-code-20250219".into(),
                "interleaved-thinking-2025-05-14".into(),
                "context-management-2025-06-27".into(),
                "effort-2025-11-24".into(),
                "fine-grained-tool-streaming-2025-05-14".into(),
                "computer-use-2025-01-24".into(),
                "computer-use-2024-10-22".into(),
                "mcp-client-2025-04-04".into(),
                "search-results-2025-06-09".into(),
            ],
            allowed_body_fields: vec![
                "anthropic_version".into(),
                "anthropic_beta".into(),
                "max_tokens".into(),
                "messages".into(),
                "system".into(),
                "temperature".into(),
                "top_p".into(),
                "top_k".into(),
                "tools".into(),
                "tool_choice".into(),
                "stop_sequences".into(),
                "thinking".into(),
                "output_config".into(),
                "cache_control".into(),
                "metadata".into(),
                "context_management".into(),
            ],
            additional_model_request_fields: None,
            adaptive_thinking: None,
        };
        let provider: ArcAlias<dyn routectl_core::Provider> =
            ArcAlias::new(BedrockProvider::new(cfg, resolved));

        providers.insert(
            provider_name.clone(),
            ProviderEntry::openai_compat("https://placeholder.invalid/v1", "literal:placeholder"),
        );
        let nickname = sanitize_provider_name(model_id);
        models.insert(
            nickname.clone(),
            ModelEntry::new(provider_name.clone(), (*model_id).to_string()),
        );
        aliases.insert(
            (*model_id).to_string(),
            AliasValue::Single(nickname.clone()),
        );
        resolved_models.insert(
            nickname.clone(),
            ArcAlias::new(ResolvedModel::new(
                nickname,
                provider_name,
                provider,
                (*model_id).to_string(),
            )),
        );
    }

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
async fn bedrock_converse_complete_matrix() {
    let Some(router) = build_bedrock_converse_test_router(BEDROCK_CONVERSE_MODELS).await else {
        eprintln!("skip: AWS_BEARER_TOKEN_BEDROCK not set");
        return;
    };

    let targets: Vec<String> = BEDROCK_CONVERSE_MODELS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    let total = targets.len();
    let r = router.clone();
    let rows = run_matrix(targets, move |t| {
        let r = r.clone();
        async move { run_complete(r, t).await }
    })
    .await;
    print_summary("Bedrock-Converse", "complete", &rows);

    let pass = rows.iter().filter(|r| r.ok).count();
    assert!(
        pass > 0,
        "Bedrock-Converse: 0/{total} models passed -- routectl or provider broken"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bedrock_converse_stream_matrix() {
    let Some(router) = build_bedrock_converse_test_router(BEDROCK_CONVERSE_MODELS).await else {
        eprintln!("skip: AWS_BEARER_TOKEN_BEDROCK not set");
        return;
    };

    let targets: Vec<String> = BEDROCK_CONVERSE_MODELS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    let r = router.clone();
    let rows = run_matrix(targets, move |t| {
        let r = r.clone();
        async move { run_stream(r, t).await }
    })
    .await;
    print_summary("Bedrock-Converse", "stream", &rows);
}
