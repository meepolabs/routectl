//! Live native Google Gemini matrix (kind = "gemini", x-goog-api-key header).

use super::*;

// -- Native Google Gemini (kind = "gemini") ---------------------------------
//
// Hits the public Gemini REST endpoint
// (`https://generativelanguage.googleapis.com/v1beta`) using an API key
// sent as the `x-goog-api-key` header. Exercises the native provider
// (NOT the openai-compat shim): native systemInstruction / contents /
// functionDeclarations / generationConfig.thinkingConfig and the
// usageMetadata cached-content + thoughts token accounting.
//
// Required env var:
//   GEMINI_API_KEY -- a Google AI Studio API key.
//
// Skips cleanly when GEMINI_API_KEY is unset / empty, exactly like the
// other key-gated providers (keyless CI / sandbox is a clean SKIP, not a
// failure).
//
// Run:
//   GEMINI_API_KEY=... cargo test -p routectl-cli \
//     --features live-integration --release \
//     --test live_matrix gemini -- --nocapture --test-threads=1

const GEMINI_MODELS: &[&str] = &["gemini-2.5-flash", "gemini-2.5-pro"];

async fn build_gemini_test_router(targets: &[&str]) -> Option<Arc<Router>> {
    let key = std::env::var("GEMINI_API_KEY").ok()?;
    if key.trim().is_empty() {
        return None;
    }

    // MemoryStore resolves env:// refs by reading the process env directly,
    // so we don't need to write the key into the store.
    let store: std::sync::Arc<dyn routectl_auth::SecretStore> =
        std::sync::Arc::new(MemoryStore::new());

    let provider_name = "gemini";
    let mut providers = BTreeMap::new();
    providers.insert(
        provider_name.to_string(),
        ProviderEntry::gemini("env://GEMINI_API_KEY"),
    );

    let mut models = BTreeMap::new();
    let mut aliases = BTreeMap::new();
    for t in targets {
        let nickname = sanitize_provider_name(t);
        models.insert(
            nickname.clone(),
            ModelEntry::new(provider_name.to_string(), (*t).to_string()),
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
            .expect("build resolved gemini models");
    assert!(failed.is_empty(), "unexpected build failures: {failed:?}");
    router.install_resolved_models(resolved_models);
    Some(Arc::new(router))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gemini_complete_matrix() {
    let Some(router) = build_gemini_test_router(GEMINI_MODELS).await else {
        eprintln!("skip: GEMINI_API_KEY not set");
        return;
    };

    let targets: Vec<String> = GEMINI_MODELS.iter().map(|s| (*s).to_string()).collect();
    let total = targets.len();
    let r = router.clone();
    let rows = run_matrix(targets, move |t| {
        let r = r.clone();
        async move { run_complete(r, t).await }
    })
    .await;
    print_summary("Gemini", "complete", &rows);

    let pass = rows.iter().filter(|r| r.ok).count();
    assert!(
        pass > 0,
        "Gemini: 0/{total} models passed -- routectl or provider broken"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gemini_stream_matrix() {
    let Some(router) = build_gemini_test_router(GEMINI_MODELS).await else {
        eprintln!("skip: GEMINI_API_KEY not set");
        return;
    };

    let targets: Vec<String> = GEMINI_MODELS.iter().map(|s| (*s).to_string()).collect();
    let r = router.clone();
    let rows = run_matrix(targets, move |t| {
        let r = r.clone();
        async move { run_stream(r, t).await }
    })
    .await;
    print_summary("Gemini", "stream", &rows);
}
