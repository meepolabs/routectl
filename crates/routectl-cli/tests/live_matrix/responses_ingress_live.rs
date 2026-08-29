//! Live OpenAI Responses INGRESS over HTTP (POST /v1/responses via axum).

// -- OpenAI Responses INGRESS (POST /v1/responses over HTTP) ----------------
//
// Operator-gated end-to-end row for the Responses ingress: spins up the
// real axum server via `serve_on_listener` and
// sends a Responses-shaped body to `POST /v1/responses`, exercising the
// full ingress -> canonical -> egress -> ingress-render stack over HTTP
// (the other matrix rows call the Router directly; this one is the only
// HTTP-path row because the ingress wiring it covers lives in the server
// router, not the Router).
//
// Required env vars:
//   RESPONSES_LIVE_TARGET   -- alias/nickname for the upstream model
//   RESPONSES_LIVE_BASE_URL -- openai-compat base URL of the upstream
//   RESPONSES_LIVE_API_KEY  -- API key for that upstream (read via env://)
//
// Skips cleanly when any of the three is unset / empty. The server
// resolves the `env://` key through the same CompositeStore `routectl
// serve` builds.
//
// Run:
//   RESPONSES_LIVE_TARGET=gpt-4.1-mini \
//   RESPONSES_LIVE_BASE_URL=https://api.openai.com/v1 \
//   RESPONSES_LIVE_API_KEY="$OPENAI_API_KEY" \
//     cargo test -p routectl-cli --features live-integration --release \
//       --test live_matrix responses_ingress -- --nocapture --test-threads=1

use super::*;
use tokio::net::TcpListener;

const TIMEOUT_SECS: u64 = 60;
const API_KEY_ENV: &str = "RESPONSES_LIVE_API_KEY";

fn read_gate() -> Option<(String, String)> {
    let target = std::env::var("RESPONSES_LIVE_TARGET").ok()?;
    let base_url = std::env::var("RESPONSES_LIVE_BASE_URL").ok()?;
    let key = std::env::var(API_KEY_ENV).ok()?;
    if target.trim().is_empty() || base_url.trim().is_empty() || key.trim().is_empty() {
        return None;
    }
    Some((target, base_url))
}

/// Build a single-target openai-compat config and boot the server on
/// an ephemeral port. The API key is referenced via `env://` so the
/// server's CompositeStore resolves it from the process env at boot.
async fn spawn_server(target: &str, base_url: &str) -> String {
    let provider_name = format!("responses-live-{}", sanitize_provider_name(target));
    let nickname = alias_nickname(target);

    let mut providers = BTreeMap::new();
    providers.insert(
        provider_name.clone(),
        ProviderEntry::openai_compat(base_url.to_string(), format!("env://{API_KEY_ENV}")),
    );
    let mut models = BTreeMap::new();
    models.insert(
        nickname.clone(),
        ModelEntry::new(provider_name, target.to_string()),
    );
    let mut aliases = BTreeMap::new();
    aliases.insert(target.to_string(), AliasValue::Single(nickname));

    let cfg = Arc::new(Config {
        server: Default::default(),
        providers,
        aliases,
        models,
        retry: Default::default(),
        ..Default::default()
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    tokio::spawn(async move {
        routectl_cli::server::serve_on_listener(cfg, listener, None)
            .await
            .expect("server failed");
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    base
}

fn responses_body(target: &str, stream: bool) -> serde_json::Value {
    serde_json::json!({
        "model": target,
        "stream": stream,
        "max_output_tokens": 64,
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": SHORT_PROMPT}]
        }]
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_ingress_http_complete_and_stream() {
    let Some((target, base_url)) = read_gate() else {
        eprintln!(
            "skip: set RESPONSES_LIVE_TARGET + RESPONSES_LIVE_BASE_URL + \
             RESPONSES_LIVE_API_KEY to run the /v1/responses live row"
        );
        return;
    };

    let base = spawn_server(&target, &base_url).await;
    let client = reqwest::Client::new();

    // Non-streaming: assert HTTP 200 and output[0].type == "message".
    let resp = tokio::time::timeout(
        Duration::from_secs(TIMEOUT_SECS),
        client
            .post(format!("{base}/v1/responses"))
            .json(&responses_body(&target, false))
            .send(),
    )
    .await
    .expect("non-stream request timed out")
    .expect("non-stream request failed");
    assert_eq!(
        resp.status(),
        200,
        "non-stream /v1/responses must return 200, got {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().await.expect("non-stream body parses");
    assert_eq!(
        body["output"][0]["type"], "message",
        "expected output[0].type=message: {body}"
    );
    eprintln!("PASS responses-ingress complete target={target}");

    // Streaming: collect the SSE text and find `response.completed`.
    let resp = tokio::time::timeout(
        Duration::from_secs(TIMEOUT_SECS),
        client
            .post(format!("{base}/v1/responses"))
            .json(&responses_body(&target, true))
            .send(),
    )
    .await
    .expect("stream request timed out")
    .expect("stream request failed");
    assert_eq!(
        resp.status(),
        200,
        "stream /v1/responses must return 200, got {}",
        resp.status()
    );
    let sse = tokio::time::timeout(Duration::from_secs(TIMEOUT_SECS), resp.text())
        .await
        .expect("stream body timed out")
        .expect("stream body read failed");
    assert!(
        sse.contains("response.completed"),
        "stream SSE missing the response.completed event; got:\n{sse}"
    );
    eprintln!("PASS responses-ingress stream target={target}");
}
