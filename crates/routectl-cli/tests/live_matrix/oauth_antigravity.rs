//! Live Cloud Code Gemini via routectl-managed oauth://antigravity bearer.

// -- Cloud Code Gemini (oauth://antigravity bearer source) ------------------
//
// Mirrors `gemini_complete_matrix` / `gemini_stream_matrix` but proves the
// Cloud Code ("antigravity") egress path: bearer auth against the
// cloudcode-pa `/v1internal:*` surface rather than an API key against
// generativelanguage. The test seeds a `credentials.json` into a tempdir,
// opens a `CompositeStore` over it (the same store `routectl serve` uses),
// and lets the factory build the provider with
// `GeminiAuthMode::CloudCode`. The project id is resolved live (via
// loadCodeAssist, falling back to onboardUser) against the real
// cloudcode-pa endpoint, so the seeded record leaves `cloud_project_id`
// unset (None).
//
// Required env var:
//   GEMINI_OAUTH_ACCESS_TOKEN -- a real, currently-valid antigravity
//     OAuth bearer. Obtain it via a one-time `routectl login antigravity`
//     (live Google consent in a browser), then extract it from the
//     persisted credentials:
//       jq -r '.providers.antigravity.access_token' \
//         ~/.config/routectl/credentials.json
//
// Skips cleanly when the env var is unset / empty (keyless CI / sandbox is
// a clean SKIP, not a failure). The tempdir-scoped credentials.json keeps
// the operator's real `~/.config/routectl/credentials.json` untouched.
//
// Run:
//   GEMINI_OAUTH_ACCESS_TOKEN="$(jq -r '.providers.antigravity.access_token' \
//     ~/.config/routectl/credentials.json)" \
//   cargo test -p routectl-cli --features live-integration --release \
//     --test live_matrix oauth_antigravity -- --nocapture --test-threads=1

use super::*;
use routectl_cli::server::CompositeStore;
use routectl_core::{CustomTool, ReasoningConfig, ToolDef};
use routectl_providers::gemini::GeminiAuthMode;

/// Env var carrying the real antigravity OAuth bearer. Mirrors the
/// `OPENAI_OAUTH_ACCESS_TOKEN` convention used by `oauth_codex`
/// (raw token, trimmed, skipped on empty).
const ENV_BEARER: &str = "GEMINI_OAUTH_ACCESS_TOKEN";
const MODELS: &[&str] = &["gemini-2.5-flash", "gemini-2.5-pro"];
/// Claude model ids served by the same Cloud Code surface. Kept in a
/// separate constant so the gemini tests above keep indexing `MODELS`
/// unchanged -- adding a claude id must never repoint `MODELS[0]`.
const CLAUDE_MODELS: &[&str] = &["claude-sonnet-4-6", "claude-opus-4-6-thinking"];
const CLAUDE_TOOL_MODEL: &str = CLAUDE_MODELS[0];
const CLAUDE_THINKING_MODEL: &str = CLAUDE_MODELS[1];
const TIMEOUT_SECS: u64 = 60;

/// Prompt that gives the model no way to answer except by calling the
/// tool: the round-trip assertion needs a `tool_calls` entry, not prose.
const TOOL_PROMPT: &str = "What is the weather in Paris? Use the get_weather tool.";
const THINKING_PROMPT: &str = "A bat and a ball cost 1.10 together. The bat costs 1.00 more than the ball. \
     How much is the ball? Think it through, then answer.";
const MAX_TOKENS_TOOL: u32 = 256;
const MAX_TOKENS_THINKING: u32 = 2048;
const THINKING_EFFORT: &str = "low";

fn read_bearer() -> Option<String> {
    let raw = std::env::var(ENV_BEARER).ok()?;
    let token = raw.trim().to_string();
    if token.is_empty() { None } else { Some(token) }
}

/// Write a minimal `credentials.json` containing one `antigravity`
/// `TokenRecord`. Keeps the file at `chmod 0600` (Unix) so
/// `OAuthStore::open` accepts it. The record format mirrors the shape
/// produced by a real `routectl login antigravity`:
///   - `access_token` is the real OAuth bearer
///   - `refresh_token` is a placeholder (refresh path is not
///     exercised because `expires_at_unix` is far in the future)
///   - `cloud_project_id` is left unset (None): the project id is
///     resolved live via loadCodeAssist / onboardUser against the
///     real cloudcode-pa surface and then cached back into the record.
fn seed_credentials_file(path: &std::path::Path, bearer: &str) -> std::io::Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    // Far in the future so no refresh fires during the test.
    let expires_at = now + 365 * 24 * 3600;
    // Hand-rolled JSON literal: TokenRecord is `#[non_exhaustive]`
    // so it cannot be built with a struct literal from this crate.
    // Mirrors the shape OAuthStore::open expects on disk. The
    // `antigravity` provider key is what `oauth://antigravity` routes
    // to. `cloud_project_id` is omitted on purpose (resolved live).
    let record = serde_json::json!({
        "schema_version": 1,
        "providers": {
            "antigravity": {
                "access_token": bearer,
                "refresh_token": "rtok-test-placeholder",
                "token_type": "Bearer",
                "expires_at_unix": expires_at,
                "scopes": [],
                "account": {
                    "email": null,
                    "account_id": null,
                },
                "obtained_at_unix": now,
            }
        }
    });
    std::fs::write(path, record.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Build a router whose Gemini provider entry resolves its bearer
/// through `oauth://antigravity` against the supplied `SecretStore`
/// and runs in `GeminiAuthMode::CloudCode`. The base URL is left at
/// its default (the real cloudcode-pa endpoint).
async fn build_router_for_oauth_antigravity(
    store: Arc<dyn routectl_auth::SecretStore>,
    targets: &[&str],
) -> Arc<Router> {
    let provider_name = "gemini-cloud-code";
    let mut providers = BTreeMap::new();
    providers.insert(
        provider_name.to_string(),
        ProviderEntry::gemini("oauth://antigravity")
            .with_gemini_auth_mode(GeminiAuthMode::CloudCode),
    );

    let mut models = BTreeMap::new();
    let mut aliases = BTreeMap::new();
    for t in targets {
        // The nickname must differ from the alias key: an alias whose
        // value equals its own key is a self-referential chain and
        // resolution fails with "alias chain recursion exceeded depth".
        // `sanitize_provider_name` is a no-op on ids that carry no dot
        // (every claude id), so the prefix is what keeps them distinct.
        let nickname = format!("cc-{}", sanitize_provider_name(t));
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
    let (resolved_models, failed) = build_resolved_models(&cfg, store, BuildOptions::default())
        .await
        .expect("build_resolved_models for oauth://antigravity");
    assert!(
        failed.is_empty(),
        "factory must build the cloud-code provider with oauth://antigravity bearer: {failed:?}",
    );
    router.install_resolved_models(resolved_models);
    Arc::new(router)
}

async fn build_router_or_skip(targets: &[&str]) -> Option<Arc<Router>> {
    let bearer = read_bearer()?;
    let dir = tempfile::tempdir().expect("create tempdir");
    let creds_path = dir.path().join("credentials.json");
    seed_credentials_file(&creds_path, &bearer).expect("seed credentials.json");

    // CompositeStore mirrors what `routectl serve` builds: oauth://
    // refs land on the OAuthStore arm.
    let store: Arc<dyn routectl_auth::SecretStore> = Arc::new(
        CompositeStore::open_at(&creds_path)
            .await
            .expect("open CompositeStore over tempdir"),
    );
    // Keep the tempdir alive for the lifetime of the router: leak the
    // handle so the backing credentials.json is not removed while the
    // store still reads it during request resolution.
    std::mem::forget(dir);

    Some(build_router_for_oauth_antigravity(store, targets).await)
}

fn user_message(prompt: &str) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Text(prompt.to_string()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

/// Request carrying one custom tool definition. The shared `make_request`
/// helper is text-only, so tool and thinking coverage needs its own
/// builders.
fn make_tool_request(target: &str, stream: bool) -> ChatRequest {
    let tool = ToolDef::Custom(CustomTool {
        name: "get_weather".to_string(),
        description: Some("Get the current weather for a city.".to_string()),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "city": {"type": "string", "description": "City name"}
            },
            "required": ["city"],
        }),
        cache_control: None,
        defer_loading: None,
        strict: None,
        type_tag: None,
    });
    ChatRequest {
        model: target.to_string(),
        messages: vec![user_message(TOOL_PROMPT)].into(),
        max_tokens: Some(MAX_TOKENS_TOOL),
        stream: Some(stream),
        tools: Some(vec![tool]),
        ..Default::default()
    }
}

/// Request asking the upstream for reasoning output via the unified
/// `reasoning.effort` knob.
fn make_thinking_request(target: &str, stream: bool) -> ChatRequest {
    ChatRequest {
        model: target.to_string(),
        messages: vec![user_message(THINKING_PROMPT)].into(),
        max_tokens: Some(MAX_TOKENS_THINKING),
        stream: Some(stream),
        reasoning: Some(ReasoningConfig {
            effort: Some(THINKING_EFFORT.to_string()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oauth_antigravity_complete_via_seeded_record() {
    let Some(router) = build_router_or_skip(MODELS).await else {
        eprintln!(
            "skip: {ENV_BEARER} not set or empty. Set it to a real \
             antigravity OAuth bearer (e.g. `jq -r \
             '.providers.antigravity.access_token' \
             ~/.config/routectl/credentials.json` after a one-time \
             `routectl login antigravity`)."
        );
        return;
    };

    let model = MODELS[0];
    let req = make_request(model, MAX_TOKENS_COMPLETE, false);
    let result = tokio::time::timeout(Duration::from_secs(TIMEOUT_SECS), router.complete(req))
        .await
        .expect("oauth-antigravity completion timed out");
    let resp = result.expect("oauth-antigravity completion failed");

    let preview = resp
        .choices
        .first()
        .map(|c| match &c.message.content {
            MessageContent::Text(t) => t.clone(),
            _ => "<non-text>".into(),
        })
        .unwrap_or_default();
    let tokens = resp.usage.as_ref().map_or(0, |u| u.total_tokens);
    eprintln!("PASS oauth-antigravity complete model={model} tokens={tokens} content={preview:?}");
    assert!(!preview.is_empty(), "expected non-empty completion text");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oauth_antigravity_stream_via_seeded_record() {
    let Some(router) = build_router_or_skip(MODELS).await else {
        eprintln!(
            "skip: {ENV_BEARER} not set or empty. Set it to a real \
             antigravity OAuth bearer (see \
             oauth_antigravity_complete_via_seeded_record)."
        );
        return;
    };

    let model = MODELS[0];
    let req = make_request(model, MAX_TOKENS_STREAM, true);
    let mut stream = tokio::time::timeout(Duration::from_secs(TIMEOUT_SECS), router.stream(req))
        .await
        .expect("oauth-antigravity stream open timed out")
        .expect("oauth-antigravity stream open failed");

    let mut text = String::new();
    let mut chunks = 0usize;
    while let Ok(Some(item)) =
        tokio::time::timeout(Duration::from_secs(TIMEOUT_SECS), stream.next()).await
    {
        let chunk = item.expect("oauth-antigravity stream chunk error");
        chunks += 1;
        for ch in &chunk.choices {
            if let Some(c) = ch.delta.content.as_deref() {
                text.push_str(c);
            }
        }
    }
    eprintln!("PASS oauth-antigravity stream model={model} chunks={chunks} content={text:?}");
    assert!(!text.is_empty(), "expected non-empty streamed text");
}

// -- Claude ids on the Cloud Code surface -----------------------------------
//
// The antigravity surface serves Anthropic model ids alongside the gemini
// ones through the same `/v1internal:*` envelope. These tests exercise
// `CLAUDE_MODELS`; they never touch `MODELS`, so the gemini cases above
// stay pinned to the ids they were written against.

/// Render an error's wire evidence (status + upstream classifiers) so a
/// live failure is diagnosable from the test output alone -- a bare
/// `Display` drops `upstream_type` / `upstream_code`, which are exactly
/// what decides whether a failure is a request-shape problem.
fn wire_evidence(err: &routectl_core::Error) -> String {
    match err {
        routectl_core::Error::Upstream {
            status,
            upstream_type,
            upstream_code,
            body,
            ..
        } => format!(
            "status={status} upstream_type={:?} upstream_code={:?} body={:?}",
            upstream_type.as_deref(),
            upstream_code.as_deref(),
            body.chars().take(400).collect::<String>(),
        ),
        other => format!("status=- non-upstream error: {other}"),
    }
}

fn skip_notice(case: &str) {
    eprintln!(
        "skip[{case}]: {ENV_BEARER} not set or empty (see \
         oauth_antigravity_complete_via_seeded_record for how to mint it)."
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oauth_antigravity_claude_complete() {
    let Some(router) = build_router_or_skip(CLAUDE_MODELS).await else {
        skip_notice("claude-complete");
        return;
    };

    let model = CLAUDE_MODELS[0];
    let req = make_request(model, MAX_TOKENS_COMPLETE, false);
    let result = tokio::time::timeout(Duration::from_secs(TIMEOUT_SECS), router.complete(req))
        .await
        .expect("claude complete timed out");
    let resp = match result {
        Ok(r) => r,
        Err(e) => panic!("claude complete failed model={model} {}", wire_evidence(&e)),
    };

    let preview = resp
        .choices
        .first()
        .map(|c| match &c.message.content {
            MessageContent::Text(t) => t.clone(),
            _ => "<non-text>".into(),
        })
        .unwrap_or_default();
    let tokens = resp.usage.as_ref().map_or(0, |u| u.total_tokens);
    eprintln!(
        "PASS oauth-antigravity claude complete model={model} tokens={tokens} content={preview:?}"
    );
    assert!(!preview.is_empty(), "expected non-empty completion text");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oauth_antigravity_claude_stream() {
    let Some(router) = build_router_or_skip(CLAUDE_MODELS).await else {
        skip_notice("claude-stream");
        return;
    };

    let model = CLAUDE_MODELS[0];
    let req = make_request(model, MAX_TOKENS_STREAM, true);
    let opened = tokio::time::timeout(Duration::from_secs(TIMEOUT_SECS), router.stream(req))
        .await
        .expect("claude stream open timed out");
    let mut stream = match opened {
        Ok(s) => s,
        Err(e) => panic!(
            "claude stream open failed model={model} {}",
            wire_evidence(&e)
        ),
    };

    let mut text = String::new();
    let mut chunks = 0usize;
    while let Ok(Some(item)) =
        tokio::time::timeout(Duration::from_secs(TIMEOUT_SECS), stream.next()).await
    {
        let chunk = match item {
            Ok(c) => c,
            Err(e) => panic!(
                "claude stream chunk error model={model} after {chunks} chunks {}",
                wire_evidence(&e)
            ),
        };
        chunks += 1;
        for ch in &chunk.choices {
            if let Some(c) = ch.delta.content.as_deref() {
                text.push_str(c);
            }
        }
    }
    eprintln!(
        "PASS oauth-antigravity claude stream model={model} chunks={chunks} content={text:?}"
    );
    assert!(!text.is_empty(), "expected non-empty streamed text");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oauth_antigravity_claude_tool_call_round_trip() {
    let Some(router) = build_router_or_skip(CLAUDE_MODELS).await else {
        skip_notice("claude-tool-call");
        return;
    };

    let model = CLAUDE_TOOL_MODEL;
    let req = make_tool_request(model, false);
    let result = tokio::time::timeout(Duration::from_secs(TIMEOUT_SECS), router.complete(req))
        .await
        .expect("claude tool-call completion timed out");
    let resp = match result {
        Ok(r) => r,
        Err(e) => panic!(
            "claude tool-call failed model={model} mode=complete {}",
            wire_evidence(&e)
        ),
    };

    let choice = resp.choices.first().expect("expected one choice");
    let calls = choice.message.tool_calls.as_deref().unwrap_or_default();
    let finish = choice.finish_reason.clone();
    eprintln!(
        "oauth-antigravity claude tool-call model={model} finish={finish:?} \
         calls={} payload={:?}",
        calls.len(),
        calls,
    );
    assert!(
        !calls.is_empty(),
        "expected a tool_calls entry on the response; got finish={finish:?} content={:?}",
        choice.message.content,
    );
    let name = calls[0]
        .get("function")
        .and_then(|f| f.get("name"))
        .or_else(|| calls[0].get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or_default();
    assert_eq!(
        name, "get_weather",
        "tool call must name the tool we defined; got {:?}",
        calls[0]
    );
    eprintln!("PASS oauth-antigravity claude tool-call model={model} tool={name}");
}

/// Reasoning round-trip on the Cloud Code lane, asserted against a
/// GEMINI id: this is the positive control that proves the thinking
/// request builder and the response lift both work through this
/// transport. Without it, the claude thinking observation below could
/// not be distinguished from a broken builder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oauth_antigravity_gemini_thinking_round_trip() {
    let Some(router) = build_router_or_skip(MODELS).await else {
        skip_notice("gemini-thinking");
        return;
    };

    let model = MODELS[0];
    let req = make_thinking_request(model, false);
    let result = tokio::time::timeout(Duration::from_secs(TIMEOUT_SECS), router.complete(req))
        .await
        .expect("gemini thinking completion timed out");
    let resp = match result {
        Ok(r) => r,
        Err(e) => panic!(
            "gemini thinking failed model={model} mode=complete {}",
            wire_evidence(&e)
        ),
    };

    let observed = observe_reasoning(&resp);
    eprintln!("oauth-antigravity gemini thinking model={model} {observed}");
    assert!(
        observed.round_tripped(),
        "positive control: expected reasoning back for {model}; got {observed}",
    );
    eprintln!("PASS oauth-antigravity gemini thinking model={model}");
}

/// Claude thinking on the Cloud Code lane. The request must be ACCEPTED
/// with the thinking config attached (that is the shared-translation
/// assertion: the config reaches upstream and is understood). Whether
/// reasoning comes BACK is recorded, not asserted: the lane does not
/// return thought parts for claude ids today, and the gemini positive
/// control above proves that is upstream behavior rather than a
/// translation defect. The recorded line flips to PASS on its own if the
/// lane starts serving thoughts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oauth_antigravity_claude_thinking_request_accepted() {
    let Some(router) = build_router_or_skip(CLAUDE_MODELS).await else {
        skip_notice("claude-thinking");
        return;
    };

    let model = CLAUDE_THINKING_MODEL;
    let req = make_thinking_request(model, false);
    let result = tokio::time::timeout(Duration::from_secs(TIMEOUT_SECS), router.complete(req))
        .await
        .expect("claude thinking completion timed out");
    let resp = match result {
        Ok(r) => r,
        Err(e) => panic!(
            "claude thinking failed model={model} mode=complete {}",
            wire_evidence(&e)
        ),
    };

    let answer = resp
        .choices
        .first()
        .map(|c| match &c.message.content {
            MessageContent::Text(t) => t.clone(),
            _ => "<non-text>".into(),
        })
        .unwrap_or_default();
    let observed = observe_reasoning(&resp);
    assert!(
        !answer.is_empty(),
        "thinking request must be accepted and answered; got empty content, {observed}",
    );
    if observed.round_tripped() {
        eprintln!("PASS oauth-antigravity claude thinking round-trip model={model} {observed}");
    } else {
        eprintln!(
            "GAP oauth-antigravity claude thinking model={model} status=200 \
             accepted=yes reasoning_returned=no {observed} \
             -- upstream returns no thought parts for claude ids on this lane"
        );
    }
}

/// What a response carried back on the reasoning channel.
struct ObservedReasoning {
    detail_count: usize,
    format: String,
    reasoning_tokens: Option<u32>,
}

impl ObservedReasoning {
    fn round_tripped(&self) -> bool {
        self.detail_count > 0 || self.reasoning_tokens.is_some_and(|t| t > 0)
    }
}

impl std::fmt::Display for ObservedReasoning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "rd={} fmt={} reasoning_tokens={:?}",
            self.detail_count, self.format, self.reasoning_tokens
        )
    }
}

fn observe_reasoning(resp: &routectl_core::ChatResponse) -> ObservedReasoning {
    let message = resp.choices.first().map(|c| &c.message);
    ObservedReasoning {
        detail_count: message.map_or(0, |m| m.reasoning_details.len()),
        format: message
            .and_then(|m| m.reasoning_details.first())
            .and_then(|d| d.format.as_deref())
            .unwrap_or("-")
            .to_string(),
        reasoning_tokens: resp.usage.as_ref().and_then(|u| u.reasoning_tokens),
    }
}
