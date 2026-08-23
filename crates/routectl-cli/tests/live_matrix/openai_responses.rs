//! Live OpenAI Responses matrix: chatgpt-oauth Codex endpoint via bearer JWT.

use super::*;

// -- OpenAI Responses (ChatGPT-OAuth) live matrix ---------------------------
//
// Exercises the `openai-responses` provider type against the real
// ChatGPT Codex endpoint using a ChatGPT subscription bearer JWT.
//
// Required env vars:
//   OPENAI_BEARER_KEY   -- ChatGPT subscription JWT (from <your-codex-CLI-auth-store>)
//   OPENAI_ACCOUNT_ID   -- ChatGPT account UUID (same file, $.openai.accountId)
//
// Run:
//   OPENAI_BEARER_KEY="$(jq -r '.openai.access' <your-codex-CLI-auth-store>)" \
//   OPENAI_ACCOUNT_ID="$(jq -r '.openai.accountId' <your-codex-CLI-auth-store>)" \
//   cargo test -p routectl-cli --features live-integration --release \
//     --test live_matrix openai_responses -- --nocapture --test-threads=1
//
// Skips cleanly when either env var is absent.
// The chatgpt-oauth Responses endpoint is stream-only: complete() internally
// forces stream=true and collects to a single ChatResponse.

// Models available on the ChatGPT Codex endpoint as of 2026-05-12.
// gpt-5.3-codex is the default for the codex CLI; others are available
// to ChatGPT Plus subscribers.

const OPENAI_RESPONSES_MODELS: &[&str] = &["gpt-5.3-codex", "gpt-5.4", "gpt-5.4-mini"];

async fn build_openai_responses_test_router(targets: &[&str]) -> Option<Arc<Router>> {
    use routectl_providers::openai_responses::AuthKind as OpenaiResponsesAuthKind;

    let bearer_key = std::env::var("OPENAI_BEARER_KEY").ok()?;
    if bearer_key.trim().is_empty() {
        return None;
    }
    let account_id = std::env::var("OPENAI_ACCOUNT_ID").ok()?;
    if account_id.trim().is_empty() {
        return None;
    }

    // The MemoryStore resolves env:// refs by reading the process env,
    // so we just need the env:// URI pointing to our env var.
    let store: std::sync::Arc<dyn routectl_auth::SecretStore> =
        std::sync::Arc::new(MemoryStore::new());

    let mut providers = BTreeMap::new();
    let mut aliases = BTreeMap::new();
    let mut models = BTreeMap::new();

    for model_id in targets {
        let provider_name = format!("gpt-{}", sanitize_provider_name(model_id));
        providers.insert(
            provider_name.clone(),
            ProviderEntry::openai_responses("env://OPENAI_BEARER_KEY")
                .with_account_id_ref("env://OPENAI_ACCOUNT_ID")
                .with_openai_responses_base_url(OPENAI_RESPONSES_BASE)
                .with_openai_responses_auth_kind(OpenaiResponsesAuthKind::ChatgptOauth),
        );
        let nickname = alias_nickname(model_id);
        models.insert(
            nickname.clone(),
            ModelEntry::new(provider_name, (*model_id).to_string()),
        );
        aliases.insert((*model_id).to_string(), AliasValue::Single(nickname));
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
            .expect("build resolved openai-responses models");
    assert!(failed.is_empty(), "unexpected build failures: {failed:?}");
    router.install_resolved_models(resolved_models);
    Some(Arc::new(router))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn openai_responses_complete_matrix() {
    let Some(router) = build_openai_responses_test_router(OPENAI_RESPONSES_MODELS).await else {
        eprintln!("skip: OPENAI_BEARER_KEY or OPENAI_ACCOUNT_ID not set");
        return;
    };

    let targets: Vec<String> = OPENAI_RESPONSES_MODELS
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
    print_summary("OpenAI-Responses", "complete", &rows);

    let pass = rows.iter().filter(|r| r.ok).count();
    assert!(
        pass > 0,
        "OpenAI-Responses: 0/{total} models passed -- routectl or provider broken"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn openai_responses_stream_matrix() {
    let Some(router) = build_openai_responses_test_router(OPENAI_RESPONSES_MODELS).await else {
        eprintln!("skip: OPENAI_BEARER_KEY or OPENAI_ACCOUNT_ID not set");
        return;
    };

    let targets: Vec<String> = OPENAI_RESPONSES_MODELS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    let r = router.clone();
    let rows = run_matrix(targets, move |t| {
        let r = r.clone();
        async move { run_stream(r, t).await }
    })
    .await;
    print_summary("OpenAI-Responses", "stream", &rows);
}
