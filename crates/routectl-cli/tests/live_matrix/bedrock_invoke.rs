//! Live Bedrock matrix: Anthropic-on-Bedrock via InvokeModel + bearer-key auth.

use super::*;

// -- Bedrock live matrix ----------------------------------------------------
//
// Anthropic-on-Bedrock via InvokeModel + bearer-key auth. Reads
// `AWS_BEARER_TOKEN_BEDROCK` (short-term Bedrock console API key) and
// `AWS_REGION`. Skips cleanly when either is unset.
//
// Run:
//   cargo test -p routectl-cli --features live-integration,bedrock --release \
//     --test live_matrix bedrock -- --nocapture --test-threads=1
//
// Cross-region inference profiles (`us.`-prefixed) are used because they
// have the broadest streaming-permission surface across AWS accounts.

const BEDROCK_MODELS: &[&str] = &[
    "us.anthropic.claude-3-5-haiku-20241022-v1:0",
    "us.anthropic.claude-sonnet-4-20250514-v1:0",
    "us.anthropic.claude-opus-4-20250514-v1:0",
    "us.anthropic.claude-haiku-4-5-20251001-v1:0",
    "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
];

async fn build_bedrock_test_router(targets: &[&str]) -> Option<Arc<Router>> {
    use routectl_providers::bedrock::{
        BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth as bedrock_auth,
    };

    let key = std::env::var("AWS_BEARER_TOKEN_BEDROCK").ok()?;
    if key.trim().is_empty() {
        return None;
    }
    let region = std::env::var("AWS_REGION")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "us-east-1".into());

    // v0.6.0 wiring detour: the live Bedrock tests bypass
    // `build_resolved_models` because they need to construct
    // `BedrockProvider` directly (with a custom `allowed_*` allowlist)
    // before handing it to the router. We build one provider per
    // target -- mirroring the per-model Arc fan-out that v0.6's
    // factory produces -- then install a synthetic `ResolvedModel`
    // table so dispatch walks the per-nickname Arc.
    use routectl_router::ResolvedModel;
    use std::sync::Arc as ArcAlias;

    let mut providers = BTreeMap::new();
    let mut aliases = BTreeMap::new();
    let mut models = BTreeMap::new();
    let mut resolved_models: BTreeMap<String, ArcAlias<ResolvedModel>> = BTreeMap::new();

    for model_id in targets {
        let provider_name = format!("bedrock-{}", sanitize_provider_name(model_id));
        let creds = BedrockCreds::BearerKey { key: key.clone() };
        let resolved = bedrock_auth::resolve(&creds, &region)
            .await
            .expect("resolve bedrock bearer creds");
        let cfg = BedrockConfig {
            id: format!("bedrock:{provider_name}"),
            region: region.clone(),
            model_id: (*model_id).to_string(),
            api_shape: BedrockApiShape::Invoke,
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

        // Placeholder provider entry so Router::new sees the provider
        // name in its config (used for runtime-state lookups). The
        // actual provider Arc is installed via the resolved-models
        // table below.
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
async fn bedrock_complete_matrix() {
    let Some(router) = build_bedrock_test_router(BEDROCK_MODELS).await else {
        eprintln!("skip: AWS_BEARER_TOKEN_BEDROCK not set");
        return;
    };

    let targets: Vec<String> = BEDROCK_MODELS.iter().map(|s| (*s).to_string()).collect();
    let total = targets.len();
    let r = router.clone();
    let rows = run_matrix(targets, move |t| {
        let r = r.clone();
        async move { run_complete(r, t).await }
    })
    .await;
    print_summary("Bedrock", "complete", &rows);

    let pass = rows.iter().filter(|r| r.ok).count();
    assert!(
        pass > 0,
        "Bedrock: 0/{total} models passed -- routectl or provider broken"
    );
}

// -- Bedrock ingress-through-bedrock end-to-end ----------------------------
//
// These tests exercise the v0.4.0 hub-and-spoke seam: a wire-format
// request body parsed by an ingress adapter, run through the Router,
// and rendered back into wire format. Both ingress dialects feed the
// same bedrock egress, so this proves N+M (not NxM) translation is
// real and verifies cache_control + anthropic_beta round-trip
// losslessly on a live Anthropic-on-Bedrock request.

const BEDROCK_INGRESS_MODEL: &str = "us.anthropic.claude-haiku-4-5-20251001-v1:0";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn openai_ingress_through_bedrock() {
    use axum::http::HeaderMap;
    use routectl_cli::ingress::{IngressAdapter, openai::OpenAiIngress};
    use serde_json::json;

    let Some(router) = build_bedrock_test_router(&[BEDROCK_INGRESS_MODEL]).await else {
        eprintln!("skip: AWS_BEARER_TOKEN_BEDROCK not set");
        return;
    };

    let body = json!({
        "model": BEDROCK_INGRESS_MODEL,
        "max_tokens": 32,
        "messages": [
            {"role": "system", "content": "Reply with one short word."},
            {"role": "user", "content": "Say pong."}
        ]
    });

    let ingress = OpenAiIngress;
    let req = ingress
        .parse_request(&HeaderMap::new(), body)
        .expect("parse openai body");
    let resp = router.complete(req).await.expect("complete via bedrock");
    let wire = ingress.render_response(resp).expect("render openai");

    // OpenAI shape: choices[0].message.content. Note: canonical
    // ChatResponse has no `object` field today, so the wire body
    // omits `object: "chat.completion"`. Most clients tolerate this
    // (Anthropic's official sdk does, opencode does, claude-code does).
    // Tracking the wire-format completeness as a separate follow-up.
    let choices = wire["choices"].as_array().expect("choices array");
    assert!(!choices.is_empty(), "expected at least one choice");
    let content = choices[0]["message"]["content"]
        .as_str()
        .expect("string content");
    assert!(!content.trim().is_empty(), "expected non-empty content");
    println!("openai-ingress -> bedrock: content={content:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn anthropic_ingress_through_bedrock_cache_and_beta() {
    use axum::http::HeaderMap;
    use routectl_cli::ingress::{IngressAdapter, anthropic::AnthropicIngress};
    use serde_json::json;

    // Use sonnet-4-5 here: its prompt-cache minimum is 1024 tokens,
    // vs haiku models which require 2048+. The matrix already proves
    // haiku works end-to-end; this test specifically verifies the
    // cache_control round-trip, so we use the model with the lower
    // threshold to keep the test reliable.
    let cache_model = "us.anthropic.claude-sonnet-4-5-20250929-v1:0";
    let Some(router) = build_bedrock_test_router(&[cache_model]).await else {
        eprintln!("skip: AWS_BEARER_TOKEN_BEDROCK not set");
        return;
    };

    // System prompt above the 1024-token cache minimum. Repetition
    // is fine -- we only need the byte count to clear the threshold.
    let big_filler = "You are a careful assistant. ".repeat(400);
    let system_text = format!("{big_filler}\n\nRespond to every user message with a single word.");

    let body = json!({
        "model": cache_model,
        "max_tokens": 16,
        "anthropic_beta": ["interleaved-thinking-2025-05-14"],
        "system": [
            {
                "type": "text",
                "text": system_text,
                "cache_control": {"type": "ephemeral"}
            }
        ],
        "messages": [
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": "Say pong"}
                ]
            }
        ]
    });

    let ingress = AnthropicIngress;

    // First call: should create the cache entry.
    let req1 = ingress
        .parse_request(&HeaderMap::new(), body.clone())
        .expect("parse anthropic body 1");
    let resp1 = router.complete(req1).await.expect("complete 1");
    let wire1 = ingress.render_response(resp1).expect("render anthropic 1");

    assert_eq!(wire1["type"], "message");
    let content1 = wire1["content"].as_array().expect("content array");
    assert!(!content1.is_empty(), "non-empty content");
    assert_eq!(content1[0]["type"], "text");
    let text1 = content1[0]["text"].as_str().expect("text");
    assert!(!text1.trim().is_empty(), "non-empty text");
    let cache_creation_1 = wire1["usage"]
        .get("cache_creation_input_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let cache_read_1 = wire1["usage"]
        .get("cache_read_input_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    println!(
        "anthropic-ingress -> bedrock call#1: text={text1:?} cache_create={cache_creation_1} cache_read={cache_read_1}"
    );

    // Second call (same body): should hit the cache.
    let req2 = ingress
        .parse_request(&HeaderMap::new(), body)
        .expect("parse anthropic body 2");
    let resp2 = router.complete(req2).await.expect("complete 2");
    let wire2 = ingress.render_response(resp2).expect("render anthropic 2");

    let cache_read_2 = wire2["usage"]
        .get("cache_read_input_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let cache_creation_2 = wire2["usage"]
        .get("cache_creation_input_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    println!(
        "anthropic-ingress -> bedrock call#2: cache_create={cache_creation_2} cache_read={cache_read_2}"
    );

    // Cache fields must be present in the rendered Anthropic usage
    // (round-trip from upstream usage to ingress wire). On the second
    // call we expect cache_read > 0 unless Bedrock decided the prompt
    // is too small (in which case cache_creation would also be 0 on
    // the first call, surfacing the misconfiguration).
    let any_cache_tokens =
        cache_creation_1 > 0 || cache_read_1 > 0 || cache_creation_2 > 0 || cache_read_2 > 0;
    assert!(
        any_cache_tokens,
        "expected non-zero cache tokens on at least one call -- \
         cache_control did not round-trip to Bedrock or prompt was too small"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn anthropic_ingress_streaming_through_bedrock() {
    use axum::http::HeaderMap;
    use futures::StreamExt;
    use routectl_cli::ingress::{
        IngressAdapter, StreamRequestContext, anthropic::AnthropicIngress,
    };
    use serde_json::json;

    let Some(router) = build_bedrock_test_router(&[BEDROCK_INGRESS_MODEL]).await else {
        eprintln!("skip: AWS_BEARER_TOKEN_BEDROCK not set");
        return;
    };

    let body = json!({
        "model": BEDROCK_INGRESS_MODEL,
        "max_tokens": 24,
        "stream": true,
        "messages": [
            {"role": "user", "content": "Say pong."}
        ]
    });

    let ingress = AnthropicIngress;
    let req = ingress
        .parse_request(&HeaderMap::new(), body)
        .expect("parse anthropic body");

    let mut state = ingress.new_stream_state(&StreamRequestContext::default());
    let mut upstream = router.stream(req).await.expect("stream via bedrock");
    let mut events: Vec<(Option<String>, String)> = Vec::new();
    while let Some(item) = upstream.next().await {
        let chunk = item.expect("upstream chunk ok");
        for ev in ingress
            .render_chunk(chunk, state.as_mut())
            .expect("render chunk")
        {
            events.push((ev.event, ev.data));
        }
    }
    for ev in ingress.render_eos(state.as_mut()) {
        events.push((ev.event, ev.data));
    }

    let names: Vec<&str> = events
        .iter()
        .filter_map(|(name, _)| name.as_deref())
        .collect();
    println!("anthropic-ingress streaming events: {names:?}");

    assert!(
        names.contains(&"message_start"),
        "expected message_start event, got {names:?}"
    );
    assert!(
        names.contains(&"content_block_start"),
        "expected content_block_start, got {names:?}"
    );
    assert!(
        names.contains(&"content_block_delta"),
        "expected at least one content_block_delta, got {names:?}"
    );
    assert!(
        names.contains(&"message_stop"),
        "expected message_stop, got {names:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bedrock_stream_subset() {
    // Stream against the smallest models to keep cost tiny.
    let subset_static: &[&str] = &[
        "us.anthropic.claude-haiku-4-5-20251001-v1:0",
        "us.anthropic.claude-3-5-haiku-20241022-v1:0",
    ];
    let Some(router) = build_bedrock_test_router(subset_static).await else {
        eprintln!("skip: AWS_BEARER_TOKEN_BEDROCK not set");
        return;
    };

    let subset: Vec<String> = subset_static.iter().map(|s| (*s).to_string()).collect();
    let r = router.clone();
    let rows = run_matrix(subset, move |t| {
        let r = r.clone();
        async move { run_stream(r, t).await }
    })
    .await;
    print_summary("Bedrock", "stream", &rows);
}
