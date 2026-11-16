//! Every provider role an operator writes survives the full config path on
//! the flat `[providers.X]` shape: it parses, validates with no error, builds
//! against a scripted in-memory store, serializes, and reparses to the same
//! providers table.
//!
//! The roles are synthetic. AWS rows use `aws://profile/<name>` over temp
//! shared-credentials and config files, so no ambient AWS credential and no
//! network is consulted. AWS state is process-global, so the test is serial.

#![cfg(all(feature = "bedrock", feature = "openai-responses", feature = "gemini"))]

use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use routectl_auth::{CredentialProbeFailure, SecretRef, SecretStore};
use routectl_router::{
    BuildOptions, Config, build_resolved_models_reported, collect_config_validation, parse_config,
};
use routectl_testkit::ScopedEnv;

const AWS_PROFILE: &str = "routectl-round-trip";

/// One role: its name, the provider (and pool) tables, and the model that
/// routes to it.
struct Role {
    name: &'static str,
    tables: &'static str,
    model_provider: &'static str,
    upstream: &'static str,
}

const ROLES: &[Role] = &[
    Role {
        name: "loopback file-key passthrough",
        tables: r#"
[providers.passthrough]
format     = "anthropic-messages"
base_url   = "http://127.0.0.1:8889"
credential = "file:///nonexistent/routectl-round-trip/key"
auth       = "x-api-key"
"#,
        model_provider: "passthrough",
        upstream: "claude-sonnet-4-6",
    },
    Role {
        name: "managed anthropic pool",
        tables: r#"
[providers.anthropic-default]
format        = "anthropic-messages"
base_url      = "https://api.anthropic.com"
credential    = "oauth://anthropic"
auth          = "bearer"
header_extras = { "anthropic-beta" = "oauth-2025-04-20" }

[providers.anthropic-work]
format     = "anthropic-messages"
base_url   = "https://api.anthropic.com"
credential = "oauth://anthropic#work"
auth       = "bearer"

[pools.anthropic]
members = ["anthropic-default", "anthropic-work"]
"#,
        model_provider: "anthropic",
        upstream: "claude-opus-4-7",
    },
    Role {
        name: "managed codex",
        tables: r#"
[providers.codex]
format     = "openai-responses"
base_url   = "https://chatgpt.com/backend-api/codex"
credential = "oauth://codex"
auth       = "bearer"
"#,
        model_provider: "codex",
        upstream: "gpt-5.4",
    },
    Role {
        name: "cloud-code seat in an antigravity pool",
        tables: r#"
[providers.antigravity-default]
format     = "gemini-cloud-code"
base_url   = "https://daily-cloudcode-pa.googleapis.com"
credential = "oauth://antigravity"
auth       = "bearer"

[pools.antigravity]
members = ["antigravity-default"]
"#,
        model_provider: "antigravity",
        upstream: "gemini-3-pro",
    },
    Role {
        name: "env bearer to openrouter",
        tables: r#"
[providers.openrouter]
format     = "anthropic-messages"
base_url   = "https://openrouter.ai/api"
credential = "env://ROUTECTL_ROUND_TRIP_OPENROUTER_KEY"
auth       = "bearer"
"#,
        model_provider: "openrouter",
        upstream: "anthropic/claude-opus-4-7",
    },
    Role {
        name: "env openai-chat",
        tables: r#"
[providers.chat-host]
format     = "openai-chat"
base_url   = "https://chat.example.com/v1"
credential = "env://ROUTECTL_ROUND_TRIP_CHAT_KEY"
auth       = "bearer"
"#,
        model_provider: "chat-host",
        upstream: "some-chat-model",
    },
    Role {
        name: "env gemini",
        tables: r#"
[providers.gemini]
format     = "gemini"
base_url   = "https://generativelanguage.googleapis.com/v1beta"
credential = "env://ROUTECTL_ROUND_TRIP_GEMINI_KEY"
auth       = "x-goog-api-key"
"#,
        model_provider: "gemini",
        upstream: "gemini-2.5-pro",
    },
    Role {
        name: "bedrock invoke",
        tables: r#"
[providers.bedrock-invoke]
format     = "bedrock-invoke"
cloud      = { name = "aws", region = "us-west-2" }
credential = "aws://profile/routectl-round-trip"
auth       = "aws-sigv4"
"#,
        model_provider: "bedrock-invoke",
        upstream: "us.anthropic.claude-haiku-4-5-20251001-v1:0",
    },
    Role {
        name: "bedrock converse",
        tables: r#"
[providers.bedrock-converse]
format     = "bedrock-converse"
cloud      = { name = "aws", region = "us-east-1" }
credential = "aws://profile/routectl-round-trip"
auth       = "aws-sigv4"
"#,
        model_provider: "bedrock-converse",
        upstream: "us.anthropic.claude-haiku-4-5-20251001-v1:0",
    },
    Role {
        name: "mantle anthropic-messages",
        tables: r#"
[providers.mantle-anthropic]
format     = "anthropic-messages"
cloud      = { name = "aws", region = "us-east-1" }
credential = "aws://profile/routectl-round-trip"
auth       = "aws-sigv4"
"#,
        model_provider: "mantle-anthropic",
        upstream: "claude-haiku-4-5-20251001-v1:0",
    },
    Role {
        name: "mantle openai-chat",
        tables: r#"
[providers.mantle-chat]
format     = "openai-chat"
cloud      = { name = "aws", region = "us-east-1" }
credential = "aws://profile/routectl-round-trip"
auth       = "aws-sigv4"
"#,
        model_provider: "mantle-chat",
        upstream: "openai.gpt-oss-20b",
    },
    Role {
        name: "mantle openai-responses",
        tables: r#"
[providers.mantle-responses]
format     = "openai-responses"
cloud      = { name = "aws", region = "us-east-1" }
credential = "aws://profile/routectl-round-trip"
auth       = "aws-sigv4"
"#,
        model_provider: "mantle-responses",
        upstream: "openai.gpt-oss-120b",
    },
];

/// The number of roles this test pins; a dropped row fails the count.
const EXPECTED_ROLES: usize = 12;

/// Answers every credential read with a fixed value and a stored login with
/// an account id, as a logged-in OAuth store would.
struct ScriptedStore;

#[async_trait]
impl SecretStore for ScriptedStore {
    async fn get(&self, _: &SecretRef) -> routectl_core::Result<String> {
        Ok("scripted-credential".into())
    }
    async fn set(&self, _: &SecretRef, _: &str) -> routectl_core::Result<()> {
        Ok(())
    }
    async fn delete(&self, _: &SecretRef) -> routectl_core::Result<()> {
        Ok(())
    }
    async fn probe_credential(&self, _: &SecretRef) -> Result<(), CredentialProbeFailure> {
        Ok(())
    }
    async fn account_id(&self, _: &SecretRef) -> routectl_core::Result<Option<String>> {
        Ok(Some("scripted-account".into()))
    }
}

fn config_text(role: &Role) -> String {
    format!(
        "{}\n[models.m]\nprovider = \"{}\"\nupstream = \"{}\"\n\n[aliases]\ndefault = \"m\"\n",
        role.tables, role.model_provider, role.upstream
    )
}

/// Points the AWS SDK at temp shared files holding one static profile, with
/// every inherited `AWS_*` variable removed and IMDS off, so only the files
/// written here can answer.
fn hermetic_aws_profile(dir: &Path) -> Vec<ScopedEnv> {
    let inherited: Vec<OsString> = std::env::vars_os()
        .map(|(key, _)| key)
        .filter(|key| key.to_string_lossy().starts_with("AWS_"))
        .collect();
    let mut guards: Vec<ScopedEnv> = inherited.iter().map(ScopedEnv::unset).collect();

    let credentials = dir.join("credentials");
    let config = dir.join("config");
    std::fs::write(
        &credentials,
        format!(
            "[{AWS_PROFILE}]\naws_access_key_id = ROUNDTRIPACCESSKEY\n\
             aws_secret_access_key = round-trip-secret-key\n"
        ),
    )
    .expect("write shared credentials");
    std::fs::write(
        &config,
        format!("[profile {AWS_PROFILE}]\nregion = us-east-1\n"),
    )
    .expect("write shared config");

    guards.push(ScopedEnv::set("AWS_EC2_METADATA_DISABLED", "true"));
    guards.push(ScopedEnv::set("AWS_SHARED_CREDENTIALS_FILE", &credentials));
    guards.push(ScopedEnv::set("AWS_CONFIG_FILE", &config));
    guards
}

/// The `[providers]` table of a TOML document, as a structural value.
fn providers_table(text: &str) -> toml::Value {
    let doc: toml::Value = toml::from_str(text).expect("document is TOML");
    doc.get("providers")
        .cloned()
        .expect("document has [providers]")
}

fn assert_validates(role: &Role, config: &Config) {
    let validation = collect_config_validation(config);
    assert!(
        validation.errors.is_empty(),
        "`{}` must validate: {:?}",
        role.name,
        validation.errors
    );
}

async fn assert_builds(role: &Role, config: &Config) {
    let store: Arc<dyn SecretStore> = Arc::new(ScriptedStore);
    let opts = BuildOptions::new().with_mint_installation_id(false);
    let built = build_resolved_models_reported(config, store, opts)
        .await
        .unwrap_or_else(|e| panic!("`{}` must build: {e}", role.name));
    assert!(
        built.failed.is_empty(),
        "`{}`: {:?}",
        role.name,
        built.failed
    );
    assert!(
        built.models.contains_key("m"),
        "`{}`: the model resolves to a built provider",
        role.name
    );
}

/// Serializes, reparses and compares: the reserialized providers table
/// equals the one the operator wrote, key for key.
fn assert_round_trips(role: &Role, text: &str, config: &Config) {
    let serialized =
        toml::to_string(config).unwrap_or_else(|e| panic!("`{}` must serialize: {e}", role.name));
    let reparsed =
        parse_config(&serialized).unwrap_or_else(|e| panic!("`{}` must reparse: {e}", role.name));
    let reserialized = toml::to_string(&reparsed).expect("reparsed config serializes");

    assert_eq!(
        providers_table(&serialized),
        providers_table(text),
        "`{}`: the serialized providers table is the one written",
        role.name
    );
    assert_eq!(
        serialized, reserialized,
        "`{}`: serialization is stable",
        role.name
    );
}

/// Runs `body` on a fresh current-thread runtime. The caller sets the
/// process environment before calling, so no runtime thread exists while it
/// changes, and drops its guards after this returns, once the runtime (and
/// any thread it started) is gone.
fn block_on<F: std::future::Future>(body: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(body)
}

#[test]
#[serial_test::serial]
fn every_provider_role_parses_validates_builds_and_round_trips() {
    // Arrange: environment first, before any runtime exists
    let aws_dir = tempfile::tempdir().expect("temp aws dir");
    let xdg = tempfile::tempdir().expect("temp config dir");
    let _aws = hermetic_aws_profile(aws_dir.path());
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", xdg.path());

    assert_eq!(ROLES.len(), EXPECTED_ROLES, "one row per role");

    // Act + Assert, one role per row
    block_on(async {
        for role in ROLES {
            let text = config_text(role);
            let config =
                parse_config(&text).unwrap_or_else(|e| panic!("`{}` must parse: {e}", role.name));
            assert_validates(role, &config);
            assert_builds(role, &config).await;
            assert_round_trips(role, &text, &config);
        }
    });
}
