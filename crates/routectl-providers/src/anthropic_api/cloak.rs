//! Claude Code identity cloak for the OAuth anthropic-api egress.
//!
//! On the OauthBearer + api.anthropic.com surface, routectl talks to
//! Anthropic with a Claude Code subscription identity. A genuine Claude
//! Code client supplies its own session-id header, the canonical identity
//! system block, and a metadata `user_id`; a non-CC client (a bare
//! OpenAI/Anthropic SDK, a custom script) supplies none of these. This
//! module mints a stable per-provider identity once and rewrites the
//! outgoing body so a non-CC client inherits the same shape: the `system`
//! field is reduced to the interactive identity line only, the client's
//! real system content is relocated verbatim into the first user message as
//! a `<system-reminder>` block (so client behavior is preserved without the
//! client fingerprint reaching the subscription classifier), and a
//! corpus-shaped metadata `user_id` is minted. The billing/attribution
//! block is always stripped (CC or not) so the client fingerprint never
//! reaches the upstream.
//!
//! Shapes here are fixed from empirical capture against the live endpoint;
//! do not redesign them.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use routectl_core::ChatRequest;

mod billing;
mod identity;
mod obfuscate;
mod tool_rename;
mod tool_sort;

use billing::strip_billing_block;
use identity::{mint_metadata_user_id, relocate_client_system};
use obfuscate::obfuscate_sensitive_words;
use tool_rename::{apply_tool_rename, normalize_tool_names_to_mcp};
use tool_sort::sort_custom_tools_by_name;

#[cfg(test)]
use billing::BILLING_PREFIX;
#[cfg(test)]
use identity::{INTERACTIVE_IDENTITY_LINE, SYSTEM_REMINDER_CLOSE, SYSTEM_REMINDER_OPEN};
#[cfg(test)]
use obfuscate::ZERO_WIDTH_SPACE;

/// Operator-facing cloak mode. Selects how aggressively the OAuth-egress
/// cloak rewrites the outgoing body. `Auto` (default) preserves the
/// original heuristic behavior exactly: the non-CC heuristic keys off the
/// presence of an `x-claude-code-session-id` capture.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum CloakMode {
    /// Heuristic mode (default): cloak as a non-CC client only when the
    /// inbound request did NOT carry an `x-claude-code-session-id`
    /// capture. Identical to the original heuristic behavior.
    #[default]
    Auto,
    /// Always cloak as a non-CC client: stamp the identity block and a
    /// minted metadata `user_id` regardless of the session-id header.
    Always,
    /// Skip the cloak entirely: no billing strip, no identity, no
    /// metadata, no tool-name normalization, no tool rename, no
    /// sensitive-word obfuscation. The body goes upstream untouched.
    Never,
}

/// A single operator-configured tool-name rename. Applied AFTER the
/// always-on tool-name `mcp__` normalization, over the same tool-name JSON
/// paths, recording reverse entries so renamed names restore on the
/// response.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ToolRename {
    /// The tool name as the client sends it (the rename source).
    pub from: String,
    /// The tool name routectl rewrites it to on the wire (the rename
    /// target).
    pub to: String,
}

/// Opt-in, empty-default configuration surface for the OAuth-egress cloak.
/// `CloakConfig::default()` selects the standard cloak: mode `Auto`,
/// `strict_mode` false (relocate the client system into the first user
/// message rather than dropping it), no operator tool renames, no sensitive
/// words.
///
/// `Debug` is hand-rolled (NOT derived) to print mode + strict_mode +
/// COUNTS only. The configured `tool_rename` pairs and `sensitive_words`
/// are operator content and must never enter Debug output or logs -- a
/// derived `Debug` would leak them through a future `dbg!(&cfg.cloak)` or
/// `tracing::debug!(?cloak)`. `ProviderEntry::AnthropicApi` and
/// `AnthropicApiConfig` both Debug-format this, so the impl is mandatory.
#[derive(Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct CloakConfig {
    /// Cloak mode. Default `Auto` (original heuristic).
    pub mode: CloakMode,
    /// How the non-CC client's real `system` content is handled after the
    /// `system` field is reduced to the identity line only. Default false:
    /// relocate the client system verbatim into the first user message as a
    /// `<system-reminder>` block (client behavior preserved). When true:
    /// drop the client system entirely (identity-only `system`, no
    /// reminder). Egress-only: the response never echoes `system`, so there
    /// is no reverse map for this transform.
    pub strict_mode: bool,
    /// Operator tool renames, applied after the tool-name normalization.
    /// Default empty (no renames). IMPORTANT: because normalization runs
    /// first and prefixes every non-`mcp__` tool with `mcp__`, the `from`
    /// key must be the POST-normalization wire name (e.g. `mcp__read_file`,
    /// not the client's original `read_file`) or it will silently no-match.
    pub tool_rename: Vec<ToolRename>,
    /// Words to obfuscate (zero-width-space insertion) in system blocks
    /// and message text. Default empty (no obfuscation).
    pub sensitive_words: Vec<String>,
    /// Tool-array canonicalization switch. NOT an operator-facing `[cloak]`
    /// key: it is `#[serde(skip)]` so it never appears in the config schema,
    /// and is stamped programmatically from the global `[cache]
    /// normalize_tools` switch by the router factory. Default `true`.
    #[serde(skip, default = "default_true_bool")]
    pub normalize_tools: bool,
}

/// Field default for `CloakConfig::normalize_tools` on deserialize (the
/// field is serde-skipped, so this is the value a parsed config lands with).
const fn default_true_bool() -> bool {
    true
}

impl Default for CloakConfig {
    fn default() -> Self {
        Self {
            mode: CloakMode::default(),
            strict_mode: false,
            tool_rename: Vec::new(),
            sensitive_words: Vec::new(),
            normalize_tools: true,
        }
    }
}

impl std::fmt::Debug for CloakConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Counts/mode ONLY: the tool_rename from/to pairs and the
        // sensitive_words strings are operator content and must never reach
        // Debug output or logs (log-hygiene rule).
        f.debug_struct("CloakConfig")
            .field("mode", &self.mode)
            .field("strict_mode", &self.strict_mode)
            .field("tool_rename_count", &self.tool_rename.len())
            .field("sensitive_words_count", &self.sensitive_words.len())
            .field("normalize_tools", &self.normalize_tools)
            .finish()
    }
}

/// Result of the OAuth-egress cloak. Carries the per-request reverse map
/// (upstream renamed name -> original client name) so the caller can
/// restore the client's original tool names on the response. The map is
/// per-request, NOT global: a global reverse map would corrupt a client
/// that legitimately sent a mix of names by rewriting names the client
/// never asked us to touch.
#[derive(Debug, Default)]
pub struct CloakResult {
    /// Maps an upstream (renamed) tool name back to the original
    /// client-supplied name. Contains ONLY names actually renamed on this
    /// request.
    pub(crate) tool_reverse: HashMap<String, String>,
}

/// Stable Claude Code identity minted once per provider instance for the
/// OAuth anthropic-api egress. The values are reused across every request
/// the provider handles so a non-CC client presents one consistent
/// session over its lifetime.
#[derive(Debug, Clone)]
pub struct ClaudeCodeIdentity {
    /// Logical session id. Prefers the credential's `session_id` (minted
    /// at login); otherwise a fresh uuid stable for the provider's life.
    /// Stamped as `x-claude-code-session-id` in `build_headers`.
    pub(crate) session_id: String,
    /// 64 lowercase hex chars (corpus shape), embedded in the minted
    /// metadata `user_id`.
    pub(crate) device_id: String,
    /// Standard dashed uuid (corpus shape), embedded in the minted
    /// metadata `user_id`.
    pub(crate) account_uuid: String,
}

impl ClaudeCodeIdentity {
    /// Mint a fresh identity. `session_id` prefers the credential value
    /// when present, else a fresh uuid (mint-when-absent). `device_id` is
    /// two concatenated simple uuids (32 hex each) for 64 lowercase hex
    /// chars without adding a dependency. `account_uuid` is a standard
    /// dashed uuid.
    pub(crate) fn mint(session_id: Option<&str>) -> Self {
        let session_id =
            session_id.map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_string);
        let device_id = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let account_uuid = uuid::Uuid::new_v4().to_string();
        Self {
            session_id,
            device_id,
            account_uuid,
        }
    }
}

/// Apply the full cloak to the outgoing body on the OAuth anthropic-api
/// surface. The billing block is stripped unconditionally (even for a
/// genuine CC client). For a non-CC client the `system` field is reduced to
/// the interactive identity line only, the client's real system content is
/// relocated into the first user message as a `<system-reminder>` block
/// (unless `strict_mode` drops it), and a metadata `user_id` is minted.
///
/// Order is load-bearing and cache-safe: identity/billing transforms
/// first, then the always-on tool-name normalization (every non-`mcp__`
/// name to `mcp__`), then operator `tool_rename` over the SAME tool-name
/// paths (recording reverse entries into the SAME map), then
/// `sensitive_words` obfuscation over system and message text. With a
/// default (empty) `config`, the output adds nothing beyond the base
/// cloak transforms.
pub fn cloak_oauth_egress(
    body: &mut Value,
    _req: &ChatRequest,
    identity: &ClaudeCodeIdentity,
    is_non_cc: bool,
    config: &CloakConfig,
) -> CloakResult {
    strip_billing_block(body);
    if is_non_cc {
        relocate_client_system(body, config.strict_mode);
        mint_metadata_user_id(body, identity);
    }
    let mut tool_reverse = normalize_tool_names_to_mcp(body);
    apply_tool_rename(body, &config.tool_rename, &mut tool_reverse);
    obfuscate_sensitive_words(body, &config.sensitive_words);
    // Tool-array canonicalization: stable-sort `tools[]` by name so a non-CC
    // client that shuffles tool order request-to-request presents a stable
    // cache prefix. Gated on the SAME `is_non_cc` branch the identity rewrite
    // uses (never looser) plus the operator kill switch. Runs LAST so it
    // orders the FINAL wire names -- ordering the pre-normalization names
    // would not be idempotent (a second cloak pass sorts the mcp__-prefixed
    // names and could reorder differently). All-or-nothing: any opaque tool,
    // missing name, or duplicate name stands the whole sort down.
    if is_non_cc && config.normalize_tools {
        sort_custom_tools_by_name(body);
    }
    CloakResult { tool_reverse }
}

#[cfg(test)]
#[path = "cloak_tests.rs"]
mod tests;
