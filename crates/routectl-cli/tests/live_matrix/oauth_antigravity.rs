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
use routectl_providers::gemini::GeminiAuthMode;

/// Env var carrying the real antigravity OAuth bearer. Mirrors the
/// `OPENAI_OAUTH_ACCESS_TOKEN` convention used by `oauth_codex`
/// (raw token, trimmed, skipped on empty).
const ENV_BEARER: &str = "GEMINI_OAUTH_ACCESS_TOKEN";
const MODELS: &[&str] = &["gemini-2.5-flash", "gemini-2.5-pro"];
const TIMEOUT_SECS: u64 = 60;

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
