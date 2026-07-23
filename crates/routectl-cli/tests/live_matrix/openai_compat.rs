//! Live openai-compat provider matrices: OpenRouter, OpenCode-Go, and NIM.

use super::*;

const OPENROUTER_BASE: &str = "https://openrouter.ai/api/v1";
const OPENCODE_GO_BASE: &str = "https://opencode.ai/zen/go/v1";
const NIM_BASE: &str = "https://integrate.api.nvidia.com/v1";

/// One representative model per major OpenRouter provider org.
/// Selected by hand to favor cheap / free / small models.
const OPENROUTER_MODELS: &[&str] = &[
    "openai/gpt-4o-mini",
    "openai/gpt-oss-120b:free",
    "anthropic/claude-haiku-4-5",
    "google/gemma-3n-e4b-it",
    "meta-llama/llama-3.2-3b-instruct:free",
    "mistralai/mistral-nemo",
    "deepseek/deepseek-v4-flash",
    "deepseek/deepseek-r1",
    "qwen/qwen3-coder:free",
    "x-ai/grok-3-mini",
    "nvidia/nemotron-nano-9b-v2:free",
    "microsoft/phi-4",
    "cohere/command-r7b-12-2024",
    "minimax/minimax-m2.5:free",
    "moonshotai/kimi-k2-0905",
    "z-ai/glm-4.5-air:free",
    "amazon/nova-micro-v1",
    "perplexity/sonar",
    "arcee-ai/trinity-mini",
    "nousresearch/hermes-3-llama-3.1-405b:free",
];

/// All models exposed under the OpenCode-Go subscription. Discovered by
/// hitting `<base>/models` if `OPENCODE_GO_FETCH_MODELS=1` is set; otherwise
/// uses this static list. Static is the default to keep tests deterministic
/// in CI.
const OPENCODE_GO_MODELS: &[&str] = &[
    "minimax-m2.7",
    "minimax-m2.5",
    "kimi-k2.6",
    "kimi-k2.5",
    "glm-5.1",
    "glm-5",
    "deepseek-v4-pro",
    "deepseek-v4-flash",
    "qwen3.6-plus",
    "qwen3.5-plus",
    "mimo-v2-pro",
    "mimo-v2-omni",
    "mimo-v2.5-pro",
    "mimo-v2.5",
];

const NIM_MODELS: &[&str] = &[
    "meta/llama-3.1-8b-instruct",
    "meta/llama-3.3-70b-instruct",
    "meta/llama-4-maverick-17b-128e-instruct",
    "google/gemma-3-12b-it",
    "qwen/qwen3-coder-480b-a35b-instruct",
];

/// Build a Router with one provider entry plus aliases, all targets routed
/// through `provider_name`. The MemoryStore feeds the API key directly.
async fn build_test_router(
    provider_name: &str,
    base_url: &str,
    api_key_env: &str,
    dialect: ReasoningDialect,
    targets: &[&str],
    extra_headers: BTreeMap<String, String>,
) -> Option<Arc<Router>> {
    let key = std::env::var(api_key_env).ok()?;
    if key.trim().is_empty() {
        return None;
    }

    // MemoryStore resolves env:// refs by reading the process env directly,
    // so we don't need to write the key into the store.
    let store: std::sync::Arc<dyn routectl_auth::SecretStore> =
        std::sync::Arc::new(MemoryStore::new());
    let secret_uri = format!("env://{api_key_env}");

    let mut providers = BTreeMap::new();
    providers.insert(
        provider_name.to_string(),
        ProviderEntry::openai_compat(base_url, secret_uri.clone())
            .with_header_extras(extra_headers),
    );

    // v0.6.0: each target becomes one [models.X] entry (nickname == wire
    // model id) and one [aliases] entry pointing the wire model at its
    // own nickname. This mirrors how the matrix tests dispatch against
    // exactly the wire id the client sent.
    let mut models = BTreeMap::new();
    let mut aliases = BTreeMap::new();
    for t in targets {
        let nickname = sanitize_provider_name(t);
        models.insert(
            nickname.clone(),
            ModelEntry::new(provider_name.to_string(), (*t).to_string())
                .with_reasoning_dialect(dialect),
        );
        aliases.insert((*t).to_string(), AliasValue::Single(nickname));
    }

    let cfg = Arc::new(Config {
        server: Default::default(),
        providers,
        aliases,
        models,
        retry: Default::default(),
        ..Default::default()
    });

    let mut router = Router::new(cfg.clone());
    let (resolved_models, failed) =
        build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("build resolved models");
    assert!(failed.is_empty(), "unexpected build failures: {failed:?}");
    router.install_resolved_models(resolved_models);
    Some(Arc::new(router))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn opencode_go_complete_matrix() {
    let mut headers = BTreeMap::new();
    headers.insert("X-Title".into(), "routectl-live-test".into());
    let Some(router) = build_test_router(
        "example-deepseek-host",
        OPENCODE_GO_BASE,
        "OPENCODE_GO_API_KEY",
        ReasoningDialect::Deepseek,
        OPENCODE_GO_MODELS,
        headers,
    )
    .await
    else {
        eprintln!("skip: OPENCODE_GO_API_KEY not set");
        return;
    };

    let targets: Vec<String> = OPENCODE_GO_MODELS
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
    print_summary("OpenCode-Go", "complete", &rows);

    let pass = rows.iter().filter(|r| r.ok).count();
    assert!(
        pass > 0,
        "OpenCode-Go: 0/{total} models passed -- routectl or provider broken"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn openrouter_complete_matrix() {
    let mut headers = BTreeMap::new();
    headers.insert(
        "HTTP-Referer".into(),
        "https://github.com/meepolabs/routectl".into(),
    );
    headers.insert("X-Title".into(), "routectl-live-test".into());
    let Some(router) = build_test_router(
        "openrouter",
        OPENROUTER_BASE,
        "OPENROUTER_API_KEY",
        ReasoningDialect::Openrouter,
        OPENROUTER_MODELS,
        headers,
    )
    .await
    else {
        eprintln!("skip: OPENROUTER_API_KEY not set");
        return;
    };

    let targets: Vec<String> = OPENROUTER_MODELS.iter().map(|s| (*s).to_string()).collect();
    let total = targets.len();
    let r = router.clone();
    let rows = run_matrix(targets, move |t| {
        let r = r.clone();
        async move { run_complete(r, t).await }
    })
    .await;
    print_summary("OpenRouter", "complete", &rows);

    let pass = rows.iter().filter(|r| r.ok).count();
    assert!(
        pass > 0,
        "OpenRouter: 0/{total} models passed -- routectl or provider broken"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nim_complete_matrix() {
    let Some(router) = build_test_router(
        "nim",
        NIM_BASE,
        "NIM_API_KEY",
        ReasoningDialect::Openai,
        NIM_MODELS,
        BTreeMap::new(),
    )
    .await
    else {
        eprintln!("skip: NIM_API_KEY not set");
        return;
    };

    let targets: Vec<String> = NIM_MODELS.iter().map(|s| (*s).to_string()).collect();
    let total = targets.len();
    let r = router.clone();
    let rows = run_matrix(targets, move |t| {
        let r = r.clone();
        async move { run_complete(r, t).await }
    })
    .await;
    print_summary("NIM", "complete", &rows);

    let pass = rows.iter().filter(|r| r.ok).count();
    assert!(
        pass > 0,
        "NIM: 0/{total} models passed -- routectl or provider broken"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn opencode_go_stream_subset() {
    let Some(router) = build_test_router(
        "example-deepseek-host",
        OPENCODE_GO_BASE,
        "OPENCODE_GO_API_KEY",
        ReasoningDialect::Deepseek,
        OPENCODE_GO_MODELS,
        BTreeMap::new(),
    )
    .await
    else {
        eprintln!("skip: OPENCODE_GO_API_KEY not set");
        return;
    };

    // One per provider family.
    let subset = vec![
        "qwen3.6-plus".to_string(),
        "kimi-k2.6".to_string(),
        "deepseek-v4-flash".to_string(),
        "glm-5.1".to_string(),
    ];
    let r = router.clone();
    let rows = run_matrix(subset, move |t| {
        let r = r.clone();
        async move { run_stream(r, t).await }
    })
    .await;
    print_summary("OpenCode-Go", "stream", &rows);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn openrouter_stream_subset() {
    let mut headers = BTreeMap::new();
    headers.insert(
        "HTTP-Referer".into(),
        "https://github.com/meepolabs/routectl".into(),
    );
    headers.insert("X-Title".into(), "routectl-live-test".into());
    let Some(router) = build_test_router(
        "openrouter",
        OPENROUTER_BASE,
        "OPENROUTER_API_KEY",
        ReasoningDialect::Openrouter,
        OPENROUTER_MODELS,
        headers,
    )
    .await
    else {
        eprintln!("skip: OPENROUTER_API_KEY not set");
        return;
    };

    // Mix of standard and reasoning models.
    let subset = vec![
        "openai/gpt-4o-mini".to_string(),
        "deepseek/deepseek-r1".to_string(),
        "anthropic/claude-haiku-4-5".to_string(),
        "google/gemma-3n-e4b-it".to_string(),
    ];
    let r = router.clone();
    let rows = run_matrix(subset, move |t| {
        let r = r.clone();
        async move { run_stream(r, t).await }
    })
    .await;
    print_summary("OpenRouter", "stream", &rows);
}
