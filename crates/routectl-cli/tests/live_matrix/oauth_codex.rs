//! Live OpenAI Responses via routectl-managed oauth://codex bearer source.

// -- OpenAI Responses (oauth://codex bearer source) -------------------------
//
// Mirrors `openai_responses_complete_matrix` but proves the
// routectl-managed OAuth bearer source: the test seeds a
// `credentials.json` into a tempdir, opens a `CompositeStore` over it
// (the same store `routectl serve` uses), and lets the factory
// auto-derive the ChatGPT account id from the JWT claim recorded in
// the `TokenRecord` (`account_id_ref` left unset).
//
// Required env var:
//   OPENAI_OAUTH_ACCESS_TOKEN -- a real, currently-valid ChatGPT
//     bearer JWT. The test extracts the `chatgpt_account_id` claim
//     from the JWT payload (production parity for the codex login
//     flow's `decode_token_response`) and seeds the same value into
//     the persisted `account.account_id` field.
//
// Skips cleanly when the env var is unset / empty / the JWT is
// malformed / the JWT lacks the `chatgpt_account_id` claim. The
// tempdir-scoped credentials.json keeps the operator's real
// `~/.config/routectl/credentials.json` untouched.
//
// Run:
//   OPENAI_OAUTH_ACCESS_TOKEN="$(jq -r '.providers.codex.access_token' \
//     ~/.config/routectl/credentials.json)" \
//   cargo test -p routectl-cli --features live-integration --release \
//     --test live_matrix oauth_codex -- --nocapture --test-threads=1

use super::*;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use routectl_auth::OAuthStore;
use routectl_cli::server::CompositeStore;
use routectl_providers::openai_responses::AuthKind as OpenaiResponsesAuthKind;

/// Env var carrying the real ChatGPT bearer JWT. Mirrors the
/// `ROUTECTL_TEST_CLAUDE_OAUTH_TOKEN_FILE` convention used by
/// `live_anthropic_oauth.rs` (raw token, trimmed, skipped on empty).
const ENV_BEARER: &str = "OPENAI_OAUTH_ACCESS_TOKEN";
/// Cheapest model on the chatgpt-oauth surface; matches the entry
/// recorded in `docs/TESTED_MODELS.md` for this test.
const MODEL: &str = "gpt-5.4-mini";
const TIMEOUT_SECS: u64 = 60;

fn read_bearer() -> Option<String> {
    let raw = std::env::var(ENV_BEARER).ok()?;
    let token = raw.trim().to_string();
    if token.is_empty() { None } else { Some(token) }
}

/// Decode `chatgpt_account_id` out of an OpenAI access-token JWT.
/// Returns `None` if the token is not a 3-segment JWT, the payload
/// is not valid base64url-no-pad, the payload is not JSON, or the
/// claim is absent. Mirrors the production `decode_jwt_payload`
/// helper in `routectl-auth/src/oauth/providers/codex.rs` (no
/// signature verification -- the upstream is the verifier).
fn extract_chatgpt_account_id(jwt: &str) -> Option<String> {
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    let payload_bytes = URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&payload_bytes).ok()?;
    v.get("https://api.openai.com/auth")
        .and_then(|a| a.get("chatgpt_account_id"))
        .and_then(|c| c.as_str())
        .map(String::from)
}

/// Write a minimal `credentials.json` containing one `codex`
/// `TokenRecord`. Keeps the file at `chmod 0600` (Unix) so
/// `OAuthStore::open` accepts it. The record format mirrors the
/// shape produced by a real `routectl login codex`:
///   - `access_token` is a JWT-shaped string (the real bearer)
///   - `refresh_token` is a placeholder (refresh path is not
///     exercised because `expires_at_unix` is far in the future)
///   - `account.account_id` carries the JWT-derived claim, which
///     the factory reads via `OAuthStore::peek_account_id`.
fn seed_credentials_file(
    path: &std::path::Path,
    bearer: &str,
    account_id: &str,
) -> std::io::Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let expires_at = now + 3600;
    // Hand-rolled JSON literal: TokenRecord is `#[non_exhaustive]`
    // so it cannot be built with a struct literal from this crate.
    // Mirrors the shape OAuthStore::open expects on disk.
    let record = serde_json::json!({
        "schema_version": 1,
        "providers": {
            "codex": {
                "access_token": bearer,
                "refresh_token": "rtok-test-placeholder",
                "token_type": "Bearer",
                "expires_at_unix": expires_at,
                "scopes": [],
                "account": {
                    "email": null,
                    "account_id": account_id,
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

/// Build a single-model router whose `gpt-5.4-mini` provider entry
/// resolves its bearer through `oauth://codex` against the
/// supplied `SecretStore`. `account_id_ref` is left unset so the
/// factory auto-derives the ChatGPT account id from the stored
/// session (the path under test).
async fn build_router_for_oauth_codex(store: Arc<dyn routectl_auth::SecretStore>) -> Arc<Router> {
    let provider_name = format!("gpt-{}", sanitize_provider_name(MODEL));
    let nickname = sanitize_provider_name(MODEL);

    let mut providers = BTreeMap::new();
    providers.insert(
        provider_name.clone(),
        ProviderEntry::openai_responses("oauth://codex")
            .with_openai_responses_base_url(OPENAI_RESPONSES_BASE)
            .with_openai_responses_auth_kind(OpenaiResponsesAuthKind::ChatgptOauth),
    );
    let mut models = BTreeMap::new();
    models.insert(
        nickname.clone(),
        ModelEntry::new(provider_name, MODEL.to_string()),
    );
    let mut aliases = BTreeMap::new();
    aliases.insert(MODEL.to_string(), AliasValue::Single(nickname));

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
        .expect("build_resolved_models for oauth://codex");
    assert!(
        failed.is_empty(),
        "factory must build with oauth://codex bearer + auto-derived account_id: {failed:?}",
    );
    router.install_resolved_models(resolved_models);
    Arc::new(router)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oauth_codex_complete_via_seeded_record() {
    let Some(bearer) = read_bearer() else {
        eprintln!(
            "skip: {ENV_BEARER} not set or empty. \
             Set it to a real ChatGPT bearer JWT (e.g. \
             `jq -r '.providers.codex.access_token' \
             ~/.config/routectl/credentials.json`)."
        );
        return;
    };

    let Some(account_id) = extract_chatgpt_account_id(&bearer) else {
        eprintln!(
            "skip: JWT bearer in {ENV_BEARER} has no `chatgpt_account_id` \
             claim under `https://api.openai.com/auth`. Provide a real \
             codex CLI access token (re-run `routectl login codex` if \
             needed)."
        );
        return;
    };

    let dir = tempfile::tempdir().expect("create tempdir");
    let creds_path = dir.path().join("credentials.json");
    seed_credentials_file(&creds_path, &bearer, &account_id).expect("seed credentials.json");

    // Pin: OAuthStore must surface the JWT-derived account id via
    // peek_account_id. This is the read path the factory exercises
    // when `account_id_ref` is unset.
    let oauth = OAuthStore::open(&creds_path)
        .await
        .expect("open OAuthStore over tempdir");
    let peek = oauth.peek_account_id("codex").await;
    assert_eq!(
        peek.as_deref(),
        Some(account_id.as_str()),
        "OAuthStore::peek_account_id must return the JWT-derived account id",
    );
    drop(oauth);

    // CompositeStore mirrors what `routectl serve` builds: oauth://
    // refs land on the OAuthStore arm, env:// / file:// / literal:
    // refs land on the MemoryStore arm.
    let store: Arc<dyn routectl_auth::SecretStore> = Arc::new(
        CompositeStore::open_at(&creds_path)
            .await
            .expect("open CompositeStore over tempdir"),
    );

    let router = build_router_for_oauth_codex(store).await;

    let req = make_request(MODEL, MAX_TOKENS_COMPLETE, false);
    let result = tokio::time::timeout(Duration::from_secs(TIMEOUT_SECS), router.complete(req))
        .await
        .expect("oauth-codex completion timed out");
    let resp = result.expect("oauth-codex completion failed");

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
        "PASS oauth-codex complete model={MODEL} account_id={account_id} \
         tokens={tokens} content={preview:?}"
    );
    assert!(!preview.is_empty(), "expected non-empty completion text");
}
