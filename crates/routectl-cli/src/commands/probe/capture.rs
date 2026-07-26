//! Env-gated Bedrock envelope-capture harness (CLI-only, never reachable
//! from the serving listener).
//!
//! Sends three purpose-built canary requests against one targeted Bedrock
//! Invoke-shape provider and asserts each is rejected with a flat AWS
//! `ValidationException` (`{"__type", "message"}`). On success the raw,
//! byte-exact response bodies are written to an operator-specified output
//! directory for downstream fixture capture.
//!
//! Two gates guard the path, checked before any config load or network:
//!   1. `ROUTECTL_BEDROCK_ENVELOPE_CAPTURE=1` must be set.
//!   2. Exactly one explicit target (`--provider` or `--alias`) must be
//!      given -- the harness never runs against a default or a whole set.
//!
//! The harness writes ONLY to the output directory: it never mutates
//! config, catalog, usage DB, or breaker state.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use routectl_auth::{SecretRef, SecretStore};
use routectl_providers::bedrock::auth::{self, ResolvedCreds};
use routectl_providers::bedrock::{BedrockCreds, endpoint, signing};
use routectl_router::{BedrockApiShapeConfig, BedrockCredsConfig, Config, ProviderEntry};

use crate::server::CompositeStore;

/// Env var that arms the capture path. Any value other than exactly `1`
/// leaves the harness inert.
const CAPTURE_ENV: &str = "ROUTECTL_BEDROCK_ENVELOPE_CAPTURE";

/// The Bedrock-required `anthropic_version` body field -- a public AWS
/// protocol constant, not an internal identifier.
const BEDROCK_ANTHROPIC_VERSION: &str = "bedrock-2023-05-31";

/// Synthetic, deliberately non-date/non-version-shaped canary values. The
/// point of each is to be UNRECOGNIZED by the upstream schema so it 400s.
const UNKNOWN_BETA_VALUE: &str = "envelope-probe-unrecognized-beta";
const UNKNOWN_BODY_FIELD: &str = "routectl_envelope_probe_field";
const ADVISOR_TOOL_TYPE: &str = "advisor";

/// One canary class. Each maps a distinct schema-drift surface AWS is
/// expected to reject with a flat `ValidationException`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanaryKind {
    /// Unrecognized `anthropic_beta` array value.
    UnknownBeta,
    /// Unrecognized top-level body field.
    UnknownBodyField,
    /// Unrecognized advisor tool entry.
    AdvisorTool,
}

const ALL_CANARIES: [CanaryKind; 3] = [
    CanaryKind::UnknownBeta,
    CanaryKind::UnknownBodyField,
    CanaryKind::AdvisorTool,
];

impl CanaryKind {
    const fn slug(self) -> &'static str {
        match self {
            Self::UnknownBeta => "unknown-beta",
            Self::UnknownBodyField => "unknown-body-field",
            Self::AdvisorTool => "advisor-tool",
        }
    }

    fn file_name(self) -> String {
        format!("{}.json", self.slug())
    }

    /// The Anthropic-Messages-shape Invoke body carrying this canary's
    /// offending element on top of a minimal valid base.
    fn body(self) -> Value {
        let mut body = json!({
            "anthropic_version": BEDROCK_ANTHROPIC_VERSION,
            "max_tokens": 1,
            "messages": [{ "role": "user", "content": "envelope capture probe" }],
        });
        let obj = body
            .as_object_mut()
            .expect("canary base body is a JSON object");
        match self {
            Self::UnknownBeta => {
                obj.insert("anthropic_beta".into(), json!([UNKNOWN_BETA_VALUE]));
            }
            Self::UnknownBodyField => {
                obj.insert(UNKNOWN_BODY_FIELD.into(), json!(true));
            }
            Self::AdvisorTool => {
                obj.insert(
                    "tools".into(),
                    json!([{ "type": ADVISOR_TOOL_TYPE, "name": ADVISOR_TOOL_TYPE }]),
                );
            }
        }
        body
    }
}

/// Parsed arguments for the capture subcommand.
#[derive(Debug, Clone)]
pub struct CaptureArgs {
    /// Target a Bedrock `[providers.X]` key directly (model id resolved
    /// from the single selectable model referencing it).
    pub provider: Option<String>,
    /// Target a `[models.X]` nickname (resolves both provider and model
    /// id).
    pub alias: Option<String>,
    /// Directory the byte-exact response bodies are written to.
    pub out: PathBuf,
}

/// True only when the arming env var is set to exactly `1`.
fn capture_enabled() -> bool {
    std::env::var(CAPTURE_ENV).ok().as_deref() == Some("1")
}

/// Reject an unscoped or over-scoped invocation. Exactly one explicit
/// target must be named.
fn require_scoped(args: &CaptureArgs) -> Result<(), String> {
    match (&args.provider, &args.alias) {
        (None, None) => Err("an explicit --provider or --alias target is required".into()),
        (Some(_), Some(_)) => Err("pass exactly one of --provider or --alias, not both".into()),
        _ => Ok(()),
    }
}

/// Assert an upstream response is a flat AWS `ValidationException`: HTTP
/// 400 with a top-level string `__type` naming a ValidationException and a
/// non-empty string `message`, and NOT the nested Anthropic-shape error
/// envelope. Returns the failure reason on any mismatch.
fn classify_validation(status: u16, body: &[u8]) -> Result<(), String> {
    if status != 400 {
        return Err(format!("expected HTTP 400, got HTTP {status}"));
    }
    let value: Value = serde_json::from_slice(body)
        .map_err(|e| format!("response body is not valid JSON: {e}"))?;
    let obj = value
        .as_object()
        .ok_or_else(|| "response body is not a JSON object".to_string())?;
    if obj.contains_key("error") {
        return Err("response carries a nested `error` object, not the flat AWS envelope".into());
    }
    let type_field = obj
        .get("__type")
        .and_then(Value::as_str)
        .ok_or_else(|| "response has no string `__type` field".to_string())?;
    if !type_field.contains("ValidationException") {
        return Err(format!(
            "`__type` is `{type_field}`, not a ValidationException"
        ));
    }
    let message = obj
        .get("message")
        .and_then(Value::as_str)
        .ok_or_else(|| "response has no string `message` field".to_string())?;
    if message.is_empty() {
        return Err("`message` field is empty".into());
    }
    Ok(())
}

/// A resolved Bedrock send target.
struct BedrockTarget {
    region: String,
    model_id: String,
    creds: BedrockCredsConfig,
    user_agent: Option<String>,
}

/// Resolve the scoped target to a Bedrock Invoke-shape send target.
fn resolve_target(config: &Config, args: &CaptureArgs) -> Result<BedrockTarget, String> {
    let (provider_name, model_id) = super::resolve::resolve_provider_and_model(
        config,
        args.provider.as_deref(),
        args.alias.as_deref(),
    )?;
    let entry = config
        .providers
        .get(&provider_name)
        .ok_or_else(|| format!("no provider named `{provider_name}` is configured"))?;
    match entry {
        ProviderEntry::Bedrock {
            region,
            api_shape,
            creds,
            user_agent,
            ..
        } => {
            if !matches!(api_shape, BedrockApiShapeConfig::Invoke) {
                return Err(format!(
                    "provider `{provider_name}` is Converse-shaped; the capture harness targets \
                     Invoke-shape providers"
                ));
            }
            Ok(BedrockTarget {
                region: region.clone(),
                model_id,
                creds: creds.clone(),
                user_agent: user_agent.clone(),
            })
        }
        _ => Err(format!(
            "provider `{provider_name}` is not a Bedrock provider"
        )),
    }
}

/// Resolve one secret reference through the composite store.
async fn resolve_ref(store: &CompositeStore, uri: &str) -> Result<String, String> {
    let secret_ref = SecretRef::parse(uri).map_err(|e| format!("invalid secret reference: {e}"))?;
    store
        .get(&secret_ref)
        .await
        .map_err(|e| format!("failed to resolve credential: {e}"))
}

/// Resolve the declarative Bedrock creds config to concrete send-time
/// credentials, mirroring the router factory's credential mapping.
async fn resolve_bedrock_creds(
    store: &CompositeStore,
    creds: &BedrockCredsConfig,
) -> Result<BedrockCreds, String> {
    Ok(match creds {
        BedrockCredsConfig::BearerKey { key_ref } => BedrockCreds::BearerKey {
            key: resolve_ref(store, key_ref).await?,
        },
        BedrockCredsConfig::Static {
            access_key_ref,
            secret_key_ref,
            session_token_ref,
        } => {
            let access_key = resolve_ref(store, access_key_ref).await?;
            let secret_key = resolve_ref(store, secret_key_ref).await?;
            let session_token = match session_token_ref {
                Some(t) => Some(resolve_ref(store, t).await?),
                None => None,
            };
            BedrockCreds::Static {
                access_key,
                secret_key,
                session_token,
            }
        }
        BedrockCredsConfig::Profile { name } => BedrockCreds::Profile { name: name.clone() },
        BedrockCredsConfig::DefaultChain => BedrockCreds::DefaultChain,
        _ => return Err("unsupported Bedrock credential kind".into()),
    })
}

/// Sign and send one canary, returning the byte-exact response body only
/// if it is a flat 400 ValidationException AND did not echo any of the
/// request's own credential material; otherwise a failure reason.
async fn capture_one(
    client: &reqwest::Client,
    url: &str,
    resolved: &ResolvedCreds,
    region: &str,
    user_agent: Option<&str>,
    kind: CanaryKind,
    configured_secrets: &[String],
) -> Result<Vec<u8>, String> {
    let body_bytes = serde_json::to_vec(&kind.body())
        .map_err(|e| format!("failed to serialize canary body: {e}"))?;

    let mut builder = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "application/json");
    if let Some(ua) = user_agent {
        builder = builder.header(reqwest::header::USER_AGENT, ua);
    }
    let mut request = builder
        .body(body_bytes)
        .build()
        .map_err(|e| format!("failed to build request: {e}"))?;

    signing::apply(&mut request, resolved, region)
        .await
        .map_err(|e| format!("failed to sign request: {e}"))?;

    // Gather the request's own transmitted credential material BEFORE the
    // request is consumed by execute: the signed Authorization value and any
    // session-token header. Combined with the configured key material, this
    // is the set the response body must not echo back.
    let mut secrets: Vec<String> = configured_secrets.to_vec();
    secrets.extend(signed_header_secrets(&request));

    let resp = client
        .execute(request)
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status().as_u16();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("failed to read response body: {e}"))?
        .to_vec();

    classify_validation(status, &bytes).map_err(|why| format!("{} canary: {why}", kind.slug()))?;
    assert_no_credential_echo(&bytes, &secrets, &kind.file_name())?;
    Ok(bytes)
}

/// The request's configured raw credential material -- the key id/secret/
/// token or bearer key the signer used. Combined with the on-wire signed
/// headers, this is the set a captured body must not contain. Variants that
/// resolve inside the signer (profile, default-chain, or any future kind)
/// expose no raw material here; their on-wire form is still covered by the
/// signed-header scan. Never logged.
fn configured_secret_material(creds: &BedrockCreds) -> Vec<String> {
    match creds {
        BedrockCreds::BearerKey { key } => vec![key.clone()],
        BedrockCreds::Static {
            access_key,
            secret_key,
            session_token,
        } => {
            let mut v = vec![access_key.clone(), secret_key.clone()];
            if let Some(token) = session_token {
                v.push(token.clone());
            }
            v
        }
        _ => Vec::new(),
    }
}

/// The request's transmitted credential material, read from the signed
/// headers before the request is sent: the full `Authorization` value (SigV4
/// signature + credential scope, or the bearer key) and any
/// `x-amz-security-token` (session token). Used only to refuse persisting a
/// body that echoed it; never logged.
fn signed_header_secrets(request: &reqwest::Request) -> Vec<String> {
    ["authorization", "x-amz-security-token"]
        .into_iter()
        .filter_map(|name| {
            request
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        })
        .collect()
}

/// True when `haystack` contains the contiguous byte sequence `needle`.
/// Byte-level so it works on a response body that is not valid UTF-8.
fn body_contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && needle.len() <= haystack.len()
        && haystack.windows(needle.len()).any(|w| w == needle)
}

/// Refuse to persist a captured body that echoed any of the request's own
/// credential material. A malicious endpoint could echo the Authorization
/// header or session token back inside extra JSON fields, satisfy the loose
/// flat-400 shape check, and land credential material in a fixture file. On
/// any match the harness hard-fails, naming the file it did NOT write; the
/// offending value is never itself logged.
fn assert_no_credential_echo(
    body: &[u8],
    secrets: &[String],
    file_name: &str,
) -> Result<(), String> {
    for secret in secrets {
        if body_contains(body, secret.as_bytes()) {
            return Err(format!(
                "response body echoed request credential material; refusing to write `{file_name}`"
            ));
        }
    }
    Ok(())
}

/// Write each captured body byte-exact to `<out>/<canary>.json`.
fn write_bodies(out_dir: &Path, captured: &[(CanaryKind, Vec<u8>)]) -> Result<(), String> {
    std::fs::create_dir_all(out_dir).map_err(|e| {
        format!(
            "cannot create output directory `{}`: {e}",
            out_dir.display()
        )
    })?;
    for (kind, bytes) in captured {
        let path = out_dir.join(kind.file_name());
        std::fs::write(&path, bytes)
            .map_err(|e| format!("cannot write `{}`: {e}", path.display()))?;
    }
    Ok(())
}

/// Run the capture harness. Returns the process exit code: `0` on full
/// success, `2` for a gate rejection (disabled / unscoped), `1` for any
/// resolution, network, or canary-shape failure.
pub async fn run(config_path: &Path, args: CaptureArgs) -> i32 {
    if !capture_enabled() {
        eprintln!("error: set {CAPTURE_ENV}=1 to run the Bedrock envelope-capture harness");
        return 2;
    }
    if let Err(msg) = require_scoped(&args) {
        eprintln!("error: {msg}");
        return 2;
    }

    let config = match crate::server::load_effective_config_unvalidated(config_path) {
        Ok(loaded) => loaded.config,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let target = match resolve_target(&config, &args) {
        Ok(target) => target,
        Err(msg) => {
            eprintln!("error: {msg}");
            return 1;
        }
    };

    let store = match CompositeStore::open_default().await {
        Ok(store) => store,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let bedrock_creds = match resolve_bedrock_creds(&store, &target.creds).await {
        Ok(creds) => creds,
        Err(msg) => {
            eprintln!("error: {msg}");
            return 1;
        }
    };
    let configured_secrets = configured_secret_material(&bedrock_creds);
    let resolved = match auth::resolve(&bedrock_creds, &target.region).await {
        Ok(resolved) => resolved,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    let client = reqwest::Client::new();
    let url = endpoint::invoke_url(&target.region, &target.model_id, false);

    let mut captured: Vec<(CanaryKind, Vec<u8>)> = Vec::with_capacity(ALL_CANARIES.len());
    for kind in ALL_CANARIES {
        match capture_one(
            &client,
            &url,
            &resolved,
            &target.region,
            target.user_agent.as_deref(),
            kind,
            &configured_secrets,
        )
        .await
        {
            Ok(bytes) => captured.push((kind, bytes)),
            Err(why) => {
                eprintln!("error: {why}");
                return 1;
            }
        }
    }

    if let Err(msg) = write_bodies(&args.out, &captured) {
        eprintln!("error: {msg}");
        return 1;
    }
    for (kind, _) in &captured {
        println!(
            "captured {} envelope -> {}",
            kind.slug(),
            args.out.join(kind.file_name()).display()
        );
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_testkit::ScopedEnv;

    // -----------------------------------------------------------------
    // Canary construction (no live AWS).
    // -----------------------------------------------------------------

    fn base_fields_present(body: &Value) {
        let obj = body.as_object().expect("canary body is an object");
        assert_eq!(
            obj.get("anthropic_version").and_then(Value::as_str),
            Some(BEDROCK_ANTHROPIC_VERSION)
        );
        assert!(obj.contains_key("max_tokens"));
        assert!(obj.contains_key("messages"));
    }

    #[test]
    fn unknown_beta_canary_carries_the_unrecognized_beta_value() {
        let body = CanaryKind::UnknownBeta.body();
        base_fields_present(&body);
        let betas = body["anthropic_beta"].as_array().expect("beta array");
        assert_eq!(betas, &[json!(UNKNOWN_BETA_VALUE)]);
    }

    #[test]
    fn unknown_body_field_canary_carries_the_unrecognized_top_level_key() {
        let body = CanaryKind::UnknownBodyField.body();
        base_fields_present(&body);
        assert_eq!(body[UNKNOWN_BODY_FIELD], json!(true));
    }

    #[test]
    fn advisor_tool_canary_carries_the_advisor_tool_entry() {
        let body = CanaryKind::AdvisorTool.body();
        base_fields_present(&body);
        let tools = body["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], json!(ADVISOR_TOOL_TYPE));
    }

    #[test]
    fn every_canary_has_a_distinct_slug_and_file_name() {
        let slugs: Vec<&str> = ALL_CANARIES.iter().map(|k| k.slug()).collect();
        assert_eq!(
            slugs,
            vec!["unknown-beta", "unknown-body-field", "advisor-tool"]
        );
        for kind in ALL_CANARIES {
            assert_eq!(kind.file_name(), format!("{}.json", kind.slug()));
        }
    }

    // -----------------------------------------------------------------
    // 400-shape assertion against synthetic responses.
    // -----------------------------------------------------------------

    #[test]
    fn classify_accepts_flat_validation_400() {
        let body = br#"{"__type":"com.amazon.coral.validate#ValidationException","message":"model does not support the anthropic_beta value"}"#;
        assert!(classify_validation(400, body).is_ok());
    }

    #[test]
    fn classify_rejects_non_400_status() {
        let body = br#"{"__type":"ValidationException","message":"x"}"#;
        let err = classify_validation(403, body).expect_err("403 must be rejected");
        assert!(err.contains("400"), "reason: {err}");
    }

    #[test]
    fn classify_rejects_nested_anthropic_error_envelope() {
        let body = br#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#;
        let err = classify_validation(400, body).expect_err("nested envelope must be rejected");
        assert!(err.contains("nested"), "reason: {err}");
    }

    #[test]
    fn classify_rejects_non_validation_type() {
        let body = br#"{"__type":"ThrottlingException","message":"slow down"}"#;
        let err =
            classify_validation(400, body).expect_err("non-ValidationException must be rejected");
        assert!(err.contains("ValidationException"), "reason: {err}");
    }

    #[test]
    fn classify_rejects_missing_message() {
        let body = br#"{"__type":"ValidationException"}"#;
        let err = classify_validation(400, body).expect_err("missing message must be rejected");
        assert!(err.contains("message"), "reason: {err}");
    }

    #[test]
    fn classify_rejects_empty_message() {
        let body = br#"{"__type":"ValidationException","message":""}"#;
        let err = classify_validation(400, body).expect_err("empty message must be rejected");
        assert!(err.contains("empty"), "reason: {err}");
    }

    #[test]
    fn classify_rejects_non_json_body() {
        let err = classify_validation(400, b"<html>gateway timeout</html>").expect_err("non-JSON");
        assert!(err.contains("JSON"), "reason: {err}");
    }

    // -----------------------------------------------------------------
    // Scope gate: rejects unscoped and over-scoped invocation.
    // -----------------------------------------------------------------

    fn args(provider: Option<&str>, alias: Option<&str>) -> CaptureArgs {
        CaptureArgs {
            provider: provider.map(str::to_string),
            alias: alias.map(str::to_string),
            out: PathBuf::from("/dev/null"),
        }
    }

    #[test]
    fn require_scoped_rejects_no_target() {
        let err = require_scoped(&args(None, None)).expect_err("no target must be rejected");
        assert!(err.contains("--provider or --alias"), "reason: {err}");
    }

    #[test]
    fn require_scoped_rejects_both_targets() {
        let err =
            require_scoped(&args(Some("p"), Some("a"))).expect_err("both targets must be rejected");
        assert!(err.contains("exactly one"), "reason: {err}");
    }

    #[test]
    fn require_scoped_accepts_a_single_target() {
        assert!(require_scoped(&args(Some("p"), None)).is_ok());
        assert!(require_scoped(&args(None, Some("a"))).is_ok());
    }

    // -----------------------------------------------------------------
    // Env gate: inert without ROUTECTL_BEDROCK_ENVELOPE_CAPTURE=1, and
    // an armed-but-unscoped invocation is refused before any IO.
    // -----------------------------------------------------------------

    #[tokio::test]
    #[serial_test::serial]
    async fn run_is_inert_without_the_env_gate() {
        let _env = ScopedEnv::set(CAPTURE_ENV, "0");
        let code = run(Path::new("/nonexistent/config.toml"), args(None, Some("a"))).await;
        assert_eq!(code, 2, "a disabled harness must refuse before any work");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn run_armed_but_unscoped_is_refused() {
        let _env = ScopedEnv::set(CAPTURE_ENV, "1");
        let code = run(Path::new("/nonexistent/config.toml"), args(None, None)).await;
        assert_eq!(code, 2, "an unscoped invocation must be refused");
    }

    // -----------------------------------------------------------------
    // Byte-exact write on success.
    // -----------------------------------------------------------------

    #[test]
    fn write_bodies_persists_byte_exact_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let beta_body = br#"{"__type":"ValidationException","message":"beta"}"#.to_vec();
        let tool_body = br#"{"__type":"ValidationException","message":"tool"}"#.to_vec();
        let captured = vec![
            (CanaryKind::UnknownBeta, beta_body.clone()),
            (CanaryKind::AdvisorTool, tool_body.clone()),
        ];

        write_bodies(dir.path(), &captured).expect("write must succeed");

        assert_eq!(
            std::fs::read(dir.path().join("unknown-beta.json")).unwrap(),
            beta_body
        );
        assert_eq!(
            std::fs::read(dir.path().join("advisor-tool.json")).unwrap(),
            tool_body
        );
    }

    // -----------------------------------------------------------------
    // Credential-echo guard: refuse to persist a body that echoed the
    // request's own credential material.
    // -----------------------------------------------------------------

    #[test]
    fn assert_no_credential_echo_refuses_body_that_echoed_a_secret() {
        let secrets = vec!["synthetic-access-key-id".to_string()];
        let body =
            br#"{"__type":"ValidationException","message":"echoed synthetic-access-key-id back"}"#;
        let err = assert_no_credential_echo(body, &secrets, "unknown-beta.json")
            .expect_err("an echoed credential must be refused");
        assert!(
            err.contains("unknown-beta.json"),
            "reason must name the unwritten file: {err}"
        );
        assert!(err.contains("refusing to write"), "reason: {err}");
        assert!(
            !err.contains("synthetic-access-key-id"),
            "the secret value must never appear in the error: {err}"
        );
    }

    #[test]
    fn assert_no_credential_echo_allows_a_clean_body() {
        let secrets = vec![
            "synthetic-access-key-id".to_string(),
            "synthetic-session-token".to_string(),
        ];
        let body =
            br#"{"__type":"ValidationException","message":"model does not support the anthropic_beta value"}"#;
        assert!(assert_no_credential_echo(body, &secrets, "unknown-beta.json").is_ok());
    }

    #[test]
    fn assert_no_credential_echo_ignores_empty_secrets() {
        // An empty configured slot must never match every body.
        let secrets = vec![String::new()];
        let body = br#"{"__type":"ValidationException","message":"x"}"#;
        assert!(assert_no_credential_echo(body, &secrets, "unknown-beta.json").is_ok());
    }

    #[test]
    fn configured_secret_material_gathers_static_and_bearer_creds() {
        assert_eq!(
            configured_secret_material(&BedrockCreds::BearerKey { key: "bk".into() }),
            vec!["bk".to_string()]
        );
        assert_eq!(
            configured_secret_material(&BedrockCreds::Static {
                access_key: "ak".into(),
                secret_key: "sk".into(),
                session_token: Some("st".into()),
            }),
            vec!["ak".to_string(), "sk".to_string(), "st".to_string()]
        );
        assert!(configured_secret_material(&BedrockCreds::DefaultChain).is_empty());
        assert!(configured_secret_material(&BedrockCreds::Profile { name: "p".into() }).is_empty());
    }
}
