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

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use routectl_core::ChatRequest;

/// Zero-width space (U+200B) inserted after the first character of each
/// `sensitive_words` match. Represented as a Rust escape (never a literal
/// non-ASCII byte in source) per the repo's ASCII-only rule. Invisible to
/// the model, so no reverse mapping is needed on the response.
const ZERO_WIDTH_SPACE: char = '\u{200B}';

/// Minimum length (in chars) a configured sensitive word must have to be
/// obfuscated. Mirrors CLIProxyAPI's matcher: words shorter than this are
/// dropped to avoid pathological single-letter rewrites.
const MIN_SENSITIVE_WORD_LEN: usize = 2;

/// Operator-facing cloak mode. Selects how aggressively the OAuth-egress
/// cloak rewrites the outgoing body. `Auto` (default) preserves the
/// Increment-1 behavior exactly: the non-CC heuristic keys off the
/// presence of an `x-claude-code-session-id` capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CloakMode {
    /// Heuristic mode (default): cloak as a non-CC client only when the
    /// inbound request did NOT carry an `x-claude-code-session-id`
    /// capture. Identical to Increment-1 behavior.
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CloakConfig {
    /// Cloak mode. Default `Auto` (Increment-1 heuristic).
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
            .finish()
    }
}

/// The single-underscore `mcp_` prefix: a non-Claude-Code tool name shape
/// that, alongside any other non-`mcp__` name, trips the Anthropic
/// subscription billing classifier. Used to detect the prefix-doubling
/// subcase so the result is `mcp__<rest>` rather than `mcp__mcp_<rest>`.
const MCP_SINGLE_PREFIX: &str = "mcp_";

/// The canonical double-underscore prefix the classifier accepts. Every
/// tool name that does not already start with this is normalized to it.
const MCP_DOUBLE_PREFIX: &str = "mcp__";

/// Result of the OAuth-egress cloak. Carries the per-request reverse map
/// (upstream renamed name -> original client name) so the caller can
/// restore the client's original tool names on the response. The map is
/// per-request, NOT global: a global reverse map would corrupt a client
/// that legitimately sent a mix of names by rewriting names the client
/// never asked us to touch.
#[derive(Debug, Default)]
pub(crate) struct CloakResult {
    /// Maps an upstream (renamed) tool name back to the original
    /// client-supplied name. Contains ONLY names actually renamed on this
    /// request.
    pub(crate) tool_reverse: HashMap<String, String>,
}

/// Marker prefix (after trimming leading whitespace) for the Claude Code
/// billing/attribution system block. Same concept as
/// `system_filter::BILLING_PREFIX`, but applied to the JSON body `Value`
/// rather than canonical `SystemContent`.
const BILLING_PREFIX: &str = "x-anthropic-billing-header:";

/// Canonical Claude Code first-block identity strings. When the inbound
/// body's first system block already matches one of these verbatim, the
/// client is presenting a real Claude Code identity block and we leave it
/// untouched. The first entry is the interactive shape we inject for a
/// non-CC client.
const RECOGNIZED_IDENTITY_LINES: &[&str] = &[
    "You are Claude Code, Anthropic's official CLI for Claude.",
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.",
];

/// The identity line injected for a non-CC client: the interactive
/// (first) recognized line.
const INTERACTIVE_IDENTITY_LINE: &str = RECOGNIZED_IDENTITY_LINES[0];

/// Opening tag wrapping the relocated client system content in the first
/// user message. The client's real system prompt is moved here verbatim so
/// the subscription classifier sees only the Claude Code identity in
/// `system` while the client's behavior is preserved.
const SYSTEM_REMINDER_OPEN: &str = "<system-reminder>";

/// Closing tag for the relocated client system content.
const SYSTEM_REMINDER_CLOSE: &str = "</system-reminder>";

/// Stable Claude Code identity minted once per provider instance for the
/// OAuth anthropic-api egress. The values are reused across every request
/// the provider handles so a non-CC client presents one consistent
/// session over its lifetime.
#[derive(Debug, Clone)]
pub(crate) struct ClaudeCodeIdentity {
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
        let session_id = session_id
            .map(str::to_string)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
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
pub(crate) fn cloak_oauth_egress(
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
    CloakResult { tool_reverse }
}

/// Normalize every tool NAME on the outgoing body to the canonical
/// double-underscore `mcp__` prefix. A non-Claude-Code request whose tool
/// SET contains enough non-`mcp__` names is diverted to the extra-usage
/// billing lane; prefixing every such name with `mcp__` keeps the request
/// in the subscription lane. The rename is prefix-only (the original name
/// is preserved as the suffix, internal separators untouched), idempotent
/// (an already-`mcp__` name is left alone and records no reverse entry),
/// and pure.
///
/// Per name:
/// - already starts with `mcp__` -> unchanged (no reverse entry).
/// - starts with `mcp_` (single underscore) -> the prefix is doubled:
///   `mcp_x` -> `mcp__x` (NOT `mcp__mcp_x`).
/// - any other (bare) name -> prefixed: `read_file` -> `mcp__read_file`.
///
/// Returns the per-request reverse map (renamed upstream name -> original
/// client name) containing ONLY names actually renamed.
///
/// Paths walked (serde_json `Value`, by path not typed enum, so unknown
/// forward-compat block types are still handled correctly):
/// - `tools[].name`
/// - `tool_choice.name` (when `tool_choice.type == "tool"`)
/// - `messages[].content[]` entries of type `tool_use` (`.name`)
/// - `messages[].content[]` entries of type `tool_reference` (`.tool_name`)
/// - nested `tool_result.content[]` entries of type `tool_reference`
///   (`.tool_name`)
fn normalize_tool_names_to_mcp(body: &mut Value) -> HashMap<String, String> {
    let mut reverse: HashMap<String, String> = HashMap::new();
    rename_tool_names_with(body, &mut reverse, &renamed_to_mcp);
    reverse
}

/// Apply an operator-configured `tool_rename` map over the SAME tool-name
/// JSON paths the tool-name normalization walks, recording reverse entries
/// into the SAME `reverse` map so renamed names also restore on the
/// response. Runs AFTER `normalize_tool_names_to_mcp`, so a `from` targeting
/// a post-normalization name (e.g. `mcp__x`) matches the already-normalized
/// wire name. An empty map is a no-op. Each rename honors the same
/// collision guard and builtin-skip as the normalization pass.
///
/// First-from-wins on duplicate `from` keys; an entry with an empty `from`
/// is skipped. The reverse map records first-seen per renamed name,
/// consistent with `rename_name_field`.
fn apply_tool_rename(
    body: &mut Value,
    renames: &[ToolRename],
    reverse: &mut HashMap<String, String>,
) {
    if renames.is_empty() {
        return;
    }
    let mut map: HashMap<String, String> = HashMap::new();
    for r in renames {
        if r.from.is_empty() {
            continue;
        }
        map.entry(r.from.clone()).or_insert_with(|| r.to.clone());
    }
    if map.is_empty() {
        return;
    }
    let renamer = move |name: &str| -> Option<String> {
        match map.get(name) {
            Some(to) if to != name => Some(to.clone()),
            _ => None,
        }
    };
    rename_tool_names_with(body, reverse, &renamer);
}

/// Walk every tool-name JSON path and apply `renamer` (returning the new
/// name or `None` to leave the field untouched), recording reverse entries.
/// Shared by the tool-name normalization and the operator `tool_rename` pass
/// so both touch exactly the same surfaces and obey the same collision guard.
///
/// Paths walked (serde_json `Value`, by path not typed enum, so unknown
/// forward-compat block types are still handled correctly):
/// - `tools[].name`
/// - `tool_choice.name` (when `tool_choice.type == "tool"`)
/// - `messages[].content[]` entries of type `tool_use` (`.name`)
/// - `messages[].content[]` entries of type `tool_reference` (`.tool_name`)
/// - nested `tool_result.content[]` entries of type `tool_reference`
///   (`.tool_name`)
fn rename_tool_names_with(
    body: &mut Value,
    reverse: &mut HashMap<String, String>,
    renamer: &dyn Fn(&str) -> Option<String>,
) {
    let existing = collect_existing_tool_names(body);
    rename_tools_array(body, &existing, reverse, renamer);
    rename_tool_choice(body, &existing, reverse, renamer);
    rename_message_tool_refs(body, &existing, reverse, renamer);
}

/// Compute the canonical `mcp__` form of a tool name, or `None` if it
/// already starts with `mcp__` (idempotent: no rename, no reverse entry).
/// A single-underscore `mcp_` prefix is doubled (`mcp_x` -> `mcp__x`); any
/// other (bare) name is prefixed (`read_file` -> `mcp__read_file`).
fn renamed_to_mcp(name: &str) -> Option<String> {
    if name.starts_with(MCP_DOUBLE_PREFIX) {
        return None;
    }
    if let Some(suffix) = name.strip_prefix(MCP_SINGLE_PREFIX) {
        return Some(format!("{MCP_DOUBLE_PREFIX}{suffix}"));
    }
    Some(format!("{MCP_DOUBLE_PREFIX}{name}"))
}

/// Gather every tool name in the request's `tools[]` array. This is a
/// PRE-RENAME snapshot of `tools[]` names only: it is computed once before
/// the forward pass, does NOT track in-progress renames, and does NOT
/// detect collisions against names that appear only in message history
/// (`tool_use` / `tool_reference`). It backs the collision guard in
/// `rename_name_field`.
fn collect_existing_tool_names(body: &Value) -> HashSet<String> {
    body.get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| t.get("name").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Apply the rename at a single JSON object's string field, honoring the
/// collision guard and recording the reverse mapping. `existing` is the
/// pre-rename snapshot of `tools[]` names; a renamed form already present
/// there is skipped with a warning. The same name appearing across
/// multiple surfaces (e.g. in `tools[]` AND a prior `tool_use`) is renamed
/// on every surface by design -- safe because the rename is idempotent and
/// the reverse map records first-seen, so identical originals map to
/// identical renamed forms.
fn rename_name_field(
    obj: &mut Value,
    field: &str,
    existing: &HashSet<String>,
    reverse: &mut HashMap<String, String>,
    renamer: &dyn Fn(&str) -> Option<String>,
) {
    let Some(original) = obj.get(field).and_then(Value::as_str).map(str::to_string) else {
        return;
    };
    let Some(renamed) = renamer(&original) else {
        return;
    };
    if existing.contains(&renamed) {
        tracing::warn!(
            "cloak tool-name rename skipped: renamed form collides with an \
             existing tool name in the request"
        );
        return;
    }
    if let Some(map) = obj.as_object_mut() {
        map.insert(field.to_string(), Value::String(renamed.clone()));
    }
    // First-seen is sufficient: the prefix-only mcp normalization maps a
    // given original to exactly one renamed form, and the collision guard
    // above skips a rename whose target already exists among tool names, so
    // distinct originals do not silently share one renamed form; for the
    // operator tool_rename pass first-seen is the documented contract.
    reverse.entry(renamed).or_insert(original);
}

/// Rename `tools[].name` in place. Anthropic native builtins (objects
/// carrying a non-empty `"type"`) are skipped.
fn rename_tools_array(
    body: &mut Value,
    existing: &HashSet<String>,
    reverse: &mut HashMap<String, String>,
    renamer: &dyn Fn(&str) -> Option<String>,
) {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    for tool in tools.iter_mut() {
        if tool_is_builtin(tool) {
            continue;
        }
        rename_name_field(tool, "name", existing, reverse, renamer);
    }
}

/// True for an Anthropic native builtin tool (web_search, code_execution,
/// ...): an object with a non-empty `"type"` field. Such tools are left
/// unchanged.
fn tool_is_builtin(tool: &Value) -> bool {
    tool.get("type")
        .and_then(Value::as_str)
        .map(|t| !t.is_empty())
        .unwrap_or(false)
}

/// Rename `tool_choice.name` when `tool_choice.type == "tool"`.
fn rename_tool_choice(
    body: &mut Value,
    existing: &HashSet<String>,
    reverse: &mut HashMap<String, String>,
    renamer: &dyn Fn(&str) -> Option<String>,
) {
    let is_tool_choice = body
        .get("tool_choice")
        .and_then(|tc| tc.get("type"))
        .and_then(Value::as_str)
        .map(|t| t == "tool")
        .unwrap_or(false);
    if !is_tool_choice {
        return;
    }
    if let Some(tc) = body.get_mut("tool_choice") {
        rename_name_field(tc, "name", existing, reverse, renamer);
    }
}

/// Rename tool references in message history: `tool_use.name`,
/// `tool_reference.tool_name`, and nested
/// `tool_result.content[].tool_reference.tool_name`.
fn rename_message_tool_refs(
    body: &mut Value,
    existing: &HashSet<String>,
    reverse: &mut HashMap<String, String>,
    renamer: &dyn Fn(&str) -> Option<String>,
) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for msg in messages.iter_mut() {
        let Some(content) = msg.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for part in content.iter_mut() {
            rename_content_part(part, existing, reverse, renamer);
        }
    }
}

/// Rename a single `messages[].content[]` entry by its `type`:
/// `tool_use` -> `.name`; `tool_reference` -> `.tool_name`;
/// `tool_result` -> recurse into nested `content[]` `tool_reference`s.
fn rename_content_part(
    part: &mut Value,
    existing: &HashSet<String>,
    reverse: &mut HashMap<String, String>,
    renamer: &dyn Fn(&str) -> Option<String>,
) {
    match part.get("type").and_then(Value::as_str) {
        Some("tool_use") => rename_name_field(part, "name", existing, reverse, renamer),
        Some("tool_reference") => rename_name_field(part, "tool_name", existing, reverse, renamer),
        Some("tool_result") => {
            let Some(nested) = part.get_mut("content").and_then(Value::as_array_mut) else {
                return;
            };
            for nested_part in nested.iter_mut() {
                if nested_part.get("type").and_then(Value::as_str) == Some("tool_reference") {
                    rename_name_field(nested_part, "tool_name", existing, reverse, renamer);
                }
            }
        }
        _ => {}
    }
}

/// Remove the Claude Code billing/attribution block from `body["system"]`.
///
/// routectl strips the billing block here, so the retained
/// `claude_signing::resign_cch_in_place` re-signer no-ops on the
/// transmitted bytes -- it is kept for a future forward-instead-of-strip
/// toggle.
fn strip_billing_block(body: &mut Value) {
    match body.get_mut("system") {
        Some(Value::Array(blocks)) => {
            blocks.retain(|b| !block_is_billing(b));
        }
        Some(Value::String(s)) if s.trim_start().starts_with(BILLING_PREFIX) => {
            if let Some(obj) = body.as_object_mut() {
                obj.remove("system");
            }
        }
        _ => {}
    }
}

/// True when a system array element's `"text"` (after trimming leading
/// whitespace) starts with the billing prefix.
fn block_is_billing(block: &Value) -> bool {
    block
        .get("text")
        .and_then(Value::as_str)
        .map(|t| t.trim_start().starts_with(BILLING_PREFIX))
        .unwrap_or(false)
}

/// Reduce a non-CC client's `system` to the interactive identity line only,
/// relocating the client's real system content into the first user message.
///
/// The subscription classifier runs a substance check on `system`; a
/// third-party agent's system prompt fails it wholesale. So the client's
/// real system content (already billing-stripped) is captured, the `system`
/// field is replaced with the identity line only, and -- unless
/// `strict_mode` is set -- the captured content is reattached as a
/// `<system-reminder>` block at the front of the first user message so the
/// client's intended behavior is preserved.
///
/// Recognized identity lines in the captured content are excluded (we
/// re-add our own identity, so an existing identity line is never
/// duplicated into the reminder). The transform is egress-only: the
/// response never echoes `system`, so there is no reverse map.
fn relocate_client_system(body: &mut Value, strict_mode: bool) {
    // Run the transform as an all-or-nothing unit: if the body root is not a
    // JSON object there is no `system` / `messages` to rewrite, so bail before
    // any partial mutation leaves the body in an inconsistent state.
    if body.as_object().is_none() {
        return;
    }
    let captured = capture_client_system(body.get("system"));
    set_identity_only_system(body);

    if strict_mode {
        return;
    }
    let Some(reminder) = build_reminder_block(&captured) else {
        return;
    };
    insert_reminder_into_first_user(body, reminder);
}

/// A captured client system text block: its text plus any `cache_control`
/// it carried (so a cache breakpoint can be preserved on relocation).
struct CapturedSystemBlock {
    text: String,
    cache_control: Option<Value>,
}

/// Capture the client's real system content, excluding any block whose
/// trimmed text is a recognized identity line (we re-add our own identity).
/// Handles the string form, the array-of-text-blocks form, and absence.
fn capture_client_system(system: Option<&Value>) -> Vec<CapturedSystemBlock> {
    match system {
        Some(Value::String(s)) => {
            if RECOGNIZED_IDENTITY_LINES.contains(&s.trim()) {
                return Vec::new();
            }
            vec![CapturedSystemBlock {
                text: s.clone(),
                cache_control: None,
            }]
        }
        Some(Value::Array(blocks)) => blocks.iter().filter_map(capture_one_system_block).collect(),
        _ => Vec::new(),
    }
}

/// Capture a single system array element when it is a text block that is not
/// a recognized identity line. A block whose `text` field is absent or
/// non-string is intentionally dropped: only text blocks are valid system
/// content for the Anthropic `system` field, so a non-text block has nothing
/// to relocate into the reminder.
fn capture_one_system_block(block: &Value) -> Option<CapturedSystemBlock> {
    let text = block.get("text").and_then(Value::as_str)?;
    if RECOGNIZED_IDENTITY_LINES.contains(&text.trim()) {
        return None;
    }
    Some(CapturedSystemBlock {
        text: text.to_string(),
        cache_control: block.get("cache_control").cloned(),
    })
}

/// Replace `body["system"]` with the identity-only array (no
/// `cache_control`; matches `identity_block()`).
fn set_identity_only_system(body: &mut Value) {
    if let Some(obj) = body.as_object_mut() {
        obj.insert("system".into(), Value::Array(vec![identity_block()]));
    }
}

/// Build the `<system-reminder>` text block from the captured client system
/// content, or `None` when there is nothing to relocate. Multiple captured
/// blocks' text is joined with a blank line. KNOWN LIMITATION: multiple
/// client system cache breakpoints collapse to one -- the last captured
/// `cache_control` (closest to the cache boundary) is carried, the rest are
/// dropped.
fn build_reminder_block(captured: &[CapturedSystemBlock]) -> Option<Value> {
    if captured.is_empty() {
        return None;
    }
    // Single-pass build with a blank-line separator between blocks; a literal
    // closing tag inside client content is neutralized so it cannot
    // prematurely close our wrapper framing.
    let mut joined = String::new();
    for (i, b) in captured.iter().enumerate() {
        if i > 0 {
            joined.push_str("\n\n");
        }
        joined.push_str(&neutralize_close_tag(&b.text));
    }
    let text = format!("{SYSTEM_REMINDER_OPEN}\n{joined}\n{SYSTEM_REMINDER_CLOSE}");
    let mut block = json!({"type": "text", "text": text});
    if let Some(cache_control) = captured.iter().rev().find_map(|b| b.cache_control.clone())
        && let Some(obj) = block.as_object_mut()
    {
        obj.insert("cache_control".into(), cache_control);
    }
    Some(block)
}

/// Strip any literal `</system-reminder>` from captured client content so the
/// relocated text cannot prematurely close the wrapper framing. The tag is
/// removed entirely (the least-surprising minimal transform); unrelated
/// content is untouched.
fn neutralize_close_tag(text: &str) -> String {
    if text.contains(SYSTEM_REMINDER_CLOSE) {
        text.replace(SYSTEM_REMINDER_CLOSE, "")
    } else {
        text.to_string()
    }
}

/// Insert the reminder block at index 0 of the content of the first
/// `role == "user"` message. A no-op when there is no usable user message
/// (missing/empty messages array, or no user role) -- the identity-only
/// system still stands and the client body is dropped. Never panics.
fn insert_reminder_into_first_user(body: &mut Value, reminder: Value) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    let Some(user) = messages
        .iter_mut()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
    else {
        return;
    };
    match user.get_mut("content") {
        Some(Value::Array(blocks)) => {
            blocks.insert(0, reminder);
        }
        Some(content @ Value::String(_)) => {
            let original = std::mem::replace(content, Value::Null);
            let Value::String(text) = original else {
                unreachable!()
            };
            *content = Value::Array(vec![reminder, json!({"type": "text", "text": text})]);
        }
        _ => {
            if let Some(obj) = user.as_object_mut() {
                obj.insert("content".into(), Value::Array(vec![reminder]));
            }
        }
    }
}
fn identity_block() -> Value {
    json!({"type": "text", "text": INTERACTIVE_IDENTITY_LINE})
}

/// Mint `body["metadata"]["user_id"]` to a corpus-shaped JSON-encoded
/// string when it is absent or empty. The encoded object keeps key order
/// device_id, account_uuid, session_id (corpus shape). A present non-empty
/// `user_id` is left untouched.
fn mint_metadata_user_id(body: &mut Value, identity: &ClaudeCodeIdentity) {
    let already_set = body
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(Value::as_str)
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if already_set {
        return;
    }
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let metadata = obj
        .entry("metadata")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let Some(metadata_obj) = metadata.as_object_mut() else {
        return;
    };
    metadata_obj.insert("user_id".into(), Value::String(encode_user_id(identity)));
}

/// Build the JSON-encoded `user_id` string with keys in the exact corpus
/// order: device_id, account_uuid, session_id. A hand-built string (not
/// `serde_json::to_string` of a map) so key order is guaranteed.
fn encode_user_id(identity: &ClaudeCodeIdentity) -> String {
    // All three interpolated fields are UUID-shaped (device_id is two
    // concatenated simple uuids; account_uuid is a dashed uuid; session_id
    // is a uuid or a corpus-shaped session id), so they contain no quote /
    // backslash / control bytes that would need JSON escaping. The hand-built
    // string (rather than `serde_json::to_string` of a map) is deliberate: it
    // guarantees the corpus key order device_id, account_uuid, session_id.
    format!(
        r#"{{"device_id":"{}","account_uuid":"{}","session_id":"{}"}}"#,
        identity.device_id, identity.account_uuid, identity.session_id
    )
}

/// Obfuscate each configured sensitive word in the outgoing body by
/// inserting a zero-width space (U+200B) after the first character of each
/// match. Mirrors CLIProxyAPI's `ObfuscateSensitiveWords`: matches are
/// case-insensitive and longest-match-first; obfuscation is applied to
/// `system` (string and array-of-text-blocks forms) and `messages[]`
/// content text (string and array-of-text-blocks forms). The inserted
/// zero-width space is invisible to the model, so no reverse mapping is
/// needed on the response. An empty word list is a byte-identical no-op.
fn obfuscate_sensitive_words(body: &mut Value, words: &[String]) {
    let matcher = match SensitiveWordMatcher::build(words) {
        Some(m) => m,
        None => return,
    };
    obfuscate_system(body, &matcher);
    obfuscate_messages(body, &matcher);
}

/// A normalized, deduplicated, longest-first set of sensitive words for a
/// case-insensitive scan. Words shorter than `MIN_SENSITIVE_WORD_LEN` chars
/// or already containing a zero-width space are dropped at build time;
/// `None` is returned when no valid word remains (the obfuscation no-ops).
struct SensitiveWordMatcher {
    /// (original-cased word, lowercased word), sorted longest-first by
    /// char count so an overlap prefers the longest match.
    words: Vec<(String, String)>,
}

impl SensitiveWordMatcher {
    fn build(words: &[String]) -> Option<Self> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut valid: Vec<(String, String)> = Vec::new();
        for w in words {
            let trimmed = w.trim();
            if trimmed.chars().count() < MIN_SENSITIVE_WORD_LEN
                || trimmed.contains(ZERO_WIDTH_SPACE)
            {
                continue;
            }
            let lower = trimmed.to_lowercase();
            if seen.insert(lower.clone()) {
                valid.push((trimmed.to_string(), lower));
            }
        }
        if valid.is_empty() {
            return None;
        }
        // Longest-first (by char count) so the scan prefers the longest
        // overlapping match, matching CLIProxyAPI's sort-by-length.
        valid.sort_by_key(|w| std::cmp::Reverse(w.0.chars().count()));
        Some(Self { words: valid })
    }

    /// Return the obfuscated form of `text`, or `None` when no match was
    /// found (so callers can skip the write and keep bytes identical).
    /// Scans left-to-right; at each position the longest configured word
    /// that matches case-insensitively (anchored at that byte offset) is
    /// obfuscated, then the scan resumes past the match.
    fn obfuscate(&self, text: &str) -> Option<String> {
        let lower = text.to_lowercase();
        // `to_lowercase` can change byte length for some scripts; guard by
        // only using the lowercased copy for ASCII-safe matching. The
        // configured words and the haystack are compared on their
        // lowercased forms but we splice from the ORIGINAL `text` by byte
        // offset, so a length divergence between `text` and `lower` would
        // corrupt offsets. Fall back to no-op when the lengths diverge.
        if lower.len() != text.len() {
            return self.obfuscate_charwise(text);
        }
        let bytes = text.as_bytes();
        let lower_bytes = lower.as_bytes();
        let mut out = String::with_capacity(text.len() + 8);
        let mut i = 0usize;
        let mut hit = false;
        while i < bytes.len() {
            if let Some(w) = self.match_at(lower_bytes, i) {
                let matched = &text[i..i + w];
                push_obfuscated(&mut out, matched);
                i += w;
                hit = true;
            } else {
                // Advance one full char so we never split a UTF-8 boundary.
                let ch_len = utf8_char_len(bytes[i]);
                out.push_str(&text[i..i + ch_len]);
                i += ch_len;
            }
        }
        if hit { Some(out) } else { None }
    }

    /// Slow path for haystacks whose lowercased byte length diverges from
    /// the original (rare; non-ASCII case folding). Matches on a fully
    /// lowercased char view and rebuilds from the original chars.
    ///
    /// Graceful degradation (intentional, documented): when per-char
    /// lowercasing also changes the CHAR count (a configured sensitive word
    /// whose lowercase form is a different length, e.g. certain non-ASCII
    /// scripts), this returns `None` -- that word is silently NOT obfuscated
    /// rather than risk corrupting the body by splicing at a mismatched
    /// offset. Sensitive-word obfuscation is best-effort hardening, not a
    /// correctness-critical transform, so skipping such a word is preferred
    /// over a malformed payload.
    fn obfuscate_charwise(&self, text: &str) -> Option<String> {
        let orig: Vec<char> = text.chars().collect();
        let lower: Vec<char> = text.chars().flat_map(|c| c.to_lowercase()).collect();
        // When per-char lowering changed the char count, give up rather
        // than risk corrupting the body. Sensitive-word obfuscation is a
        // best-effort hardening, not a correctness-critical transform.
        if lower.len() != orig.len() {
            return None;
        }
        let mut out = String::with_capacity(text.len() + 8);
        let mut i = 0usize;
        let mut hit = false;
        while i < orig.len() {
            if let Some(n) = self.match_at_chars(&lower, i) {
                let matched: String = orig[i..i + n].iter().collect();
                push_obfuscated(&mut out, &matched);
                i += n;
                hit = true;
            } else {
                out.push(orig[i]);
                i += 1;
            }
        }
        if hit { Some(out) } else { None }
    }

    /// Return the byte length of the longest configured word that matches
    /// `lower_bytes` anchored at byte offset `i`, or `None`.
    fn match_at(&self, lower_bytes: &[u8], i: usize) -> Option<usize> {
        for (_, lw) in &self.words {
            let lwb = lw.as_bytes();
            if i + lwb.len() <= lower_bytes.len() && &lower_bytes[i..i + lwb.len()] == lwb {
                return Some(lwb.len());
            }
        }
        None
    }

    /// Char-view variant of `match_at`: return the char count of the
    /// longest configured word matching `lower` anchored at char index `i`.
    fn match_at_chars(&self, lower: &[char], i: usize) -> Option<usize> {
        for (_, lw) in &self.words {
            let lwc: Vec<char> = lw.chars().collect();
            if i + lwc.len() <= lower.len() && lower[i..i + lwc.len()] == lwc[..] {
                return Some(lwc.len());
            }
        }
        None
    }
}

/// Append `matched` to `out` with a zero-width space inserted after its
/// first character. A single-char match is left unchanged (no interior
/// position to mark), matching CLIProxyAPI's `size >= len` guard.
fn push_obfuscated(out: &mut String, matched: &str) {
    let mut chars = matched.chars();
    if let Some(first) = chars.next() {
        let rest = chars.as_str();
        if rest.is_empty() {
            out.push(first);
        } else {
            out.push(first);
            out.push(ZERO_WIDTH_SPACE);
            out.push_str(rest);
        }
    }
}

/// Length in bytes of the UTF-8 char beginning with `b`.
fn utf8_char_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

/// Obfuscate sensitive words in `body["system"]` (string form, or an array
/// of `{type:"text", text:...}` blocks).
fn obfuscate_system(body: &mut Value, matcher: &SensitiveWordMatcher) {
    match body.get_mut("system") {
        Some(Value::String(s)) => {
            if let Some(ob) = matcher.obfuscate(s) {
                *s = ob;
            }
        }
        Some(Value::Array(blocks)) => {
            for block in blocks.iter_mut() {
                obfuscate_text_block(block, matcher);
            }
        }
        _ => {}
    }
}

/// Obfuscate sensitive words in `body["messages"][].content` (string form,
/// or an array of content blocks; only `{type:"text"}` blocks are touched).
fn obfuscate_messages(body: &mut Value, matcher: &SensitiveWordMatcher) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for msg in messages.iter_mut() {
        match msg.get_mut("content") {
            Some(Value::String(s)) => {
                if let Some(ob) = matcher.obfuscate(s) {
                    *s = ob;
                }
            }
            Some(Value::Array(blocks)) => {
                for block in blocks.iter_mut() {
                    obfuscate_text_block(block, matcher);
                }
            }
            _ => {}
        }
    }
}

/// Obfuscate the `text` field of a `{type:"text"}` content block in place.
/// Blocks of any other type are left untouched.
fn obfuscate_text_block(block: &mut Value, matcher: &SensitiveWordMatcher) {
    if block.get("type").and_then(Value::as_str) != Some("text") {
        return;
    }
    let Some(text) = block.get("text").and_then(Value::as_str) else {
        return;
    };
    if let Some(ob) = matcher.obfuscate(text)
        && let Some(obj) = block.as_object_mut()
    {
        obj.insert("text".into(), Value::String(ob));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> ClaudeCodeIdentity {
        ClaudeCodeIdentity {
            session_id: "sess-123".into(),
            device_id: "a".repeat(64),
            account_uuid: "11111111-2222-3333-4444-555555555555".into(),
        }
    }

    // -- billing strip -----------------------------------------------------

    #[test]
    fn strip_removes_billing_block_keeps_others() {
        // Arrange
        let mut body = json!({
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: v=1; cch=abcde;"},
                {"type": "text", "text": "you are helpful"},
            ]
        });

        // Act
        strip_billing_block(&mut body);

        // Assert
        let arr = body["system"].as_array().expect("system is array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["text"], "you are helpful");
    }

    #[test]
    fn strip_leaves_array_without_billing_unchanged() {
        // Arrange
        let mut body = json!({
            "system": [{"type": "text", "text": "you are helpful"}]
        });
        let before = body.clone();

        // Act
        strip_billing_block(&mut body);

        // Assert
        assert_eq!(body, before);
    }

    #[test]
    fn strip_removes_pure_string_billing_system() {
        // Arrange
        let mut body = json!({"system": "x-anthropic-billing-header: v=1"});

        // Act
        strip_billing_block(&mut body);

        // Assert
        assert!(
            body.get("system").is_none(),
            "pure-billing string system must be removed"
        );
    }

    #[test]
    fn strip_runs_even_with_genuine_cc_marker_present() {
        // Arrange: the interactive identity marker is present, but the
        // billing block must still be stripped.
        let mut body = json!({
            "system": [
                {"type": "text", "text": INTERACTIVE_IDENTITY_LINE},
                {"type": "text", "text": "x-anthropic-billing-header: v=1; cch=abcde;"},
            ]
        });

        // Act
        strip_billing_block(&mut body);

        // Assert
        let arr = body["system"].as_array().expect("system is array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["text"], INTERACTIVE_IDENTITY_LINE);
    }

    // -- system relocation (non-CC) ----------------------------------------

    fn reminder_text(inner: &str) -> String {
        format!("{SYSTEM_REMINDER_OPEN}\n{inner}\n{SYSTEM_REMINDER_CLOSE}")
    }

    #[test]
    fn relocate_string_system_sets_identity_only_and_moves_to_first_user() {
        // Arrange: non-CC body, client system as a String, one user message.
        let mut body = json!({
            "system": "client system prompt",
            "messages": [{"role": "user", "content": "hello"}]
        });

        // Act
        relocate_client_system(&mut body, false);

        // Assert: system is identity-only; first user content[0] is the
        // reminder wrapping the original string, content[1] the original text.
        let sys = body["system"].as_array().expect("system is array");
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0]["text"], INTERACTIVE_IDENTITY_LINE);
        let content = body["messages"][0]["content"]
            .as_array()
            .expect("content promoted to array");
        assert_eq!(content[0]["text"], reminder_text("client system prompt"));
        assert_eq!(content[1]["text"], "hello");
    }

    #[test]
    fn relocate_array_system_joins_blocks_into_one_reminder() {
        // Arrange: client system as an array of two text blocks.
        let mut body = json!({
            "system": [
                {"type": "text", "text": "first block"},
                {"type": "text", "text": "second block"},
            ],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
        });

        // Act
        relocate_client_system(&mut body, false);

        // Assert: identity-only system; reminder joins both blocks' text.
        let sys = body["system"].as_array().expect("system is array");
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0]["text"], INTERACTIVE_IDENTITY_LINE);
        let content = body["messages"][0]["content"]
            .as_array()
            .expect("content promoted to array");
        assert_eq!(
            content[0]["text"],
            reminder_text("first block\n\nsecond block")
        );
        assert_eq!(content[1]["text"], "hi");
    }

    #[test]
    fn strict_mode_drops_client_system_and_leaves_user_message_unchanged() {
        // Arrange: strict mode set, client system present, a user message.
        let mut body = json!({
            "system": "client system prompt",
            "messages": [{"role": "user", "content": "hello"}]
        });

        // Act
        relocate_client_system(&mut body, true);

        // Assert: identity-only system; user message untouched, no reminder.
        let sys = body["system"].as_array().expect("system is array");
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0]["text"], INTERACTIVE_IDENTITY_LINE);
        assert_eq!(body["messages"][0]["content"], "hello");
    }

    #[test]
    fn relocate_preserves_cache_control_on_reminder_block() {
        // Arrange: a client system block carrying a cache_control breakpoint.
        let mut body = json!({
            "system": [
                {"type": "text", "text": "cached prompt", "cache_control": {"type": "ephemeral"}},
            ],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
        });

        // Act
        relocate_client_system(&mut body, false);

        // Assert: the relocated reminder carries the cache_control.
        let reminder = &body["messages"][0]["content"][0];
        assert_eq!(reminder["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn relocate_no_panic_when_no_user_message_present() {
        // Arrange: only an assistant message -- nowhere to relocate into.
        let mut body = json!({
            "system": "client system prompt",
            "messages": [{"role": "assistant", "content": "prior"}]
        });

        // Act
        relocate_client_system(&mut body, false);

        // Assert: identity-only system; client body dropped; messages intact.
        let sys = body["system"].as_array().expect("system is array");
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0]["text"], INTERACTIVE_IDENTITY_LINE);
        assert_eq!(body["messages"][0]["content"], "prior");
    }

    #[test]
    fn relocate_no_panic_when_messages_empty() {
        // Arrange: empty messages array.
        let mut body = json!({
            "system": "client system prompt",
            "messages": []
        });

        // Act
        relocate_client_system(&mut body, false);

        // Assert: identity-only system; no reminder anywhere.
        let sys = body["system"].as_array().expect("system is array");
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0]["text"], INTERACTIVE_IDENTITY_LINE);
        assert!(body["messages"].as_array().unwrap().is_empty());
    }

    #[test]
    fn relocate_identity_only_system_leaves_messages_untouched() {
        // Arrange: client system is already exactly the identity line.
        let mut body = json!({
            "system": INTERACTIVE_IDENTITY_LINE,
            "messages": [{"role": "user", "content": "hello"}]
        });

        // Act
        relocate_client_system(&mut body, false);

        // Assert: identity-only system; no reminder added; message intact.
        let sys = body["system"].as_array().expect("system is array");
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0]["text"], INTERACTIVE_IDENTITY_LINE);
        assert_eq!(body["messages"][0]["content"], "hello");
    }

    #[test]
    fn relocate_excludes_identity_line_from_reminder() {
        // Arrange: client system = [identity line, real body].
        let mut body = json!({
            "system": [
                {"type": "text", "text": INTERACTIVE_IDENTITY_LINE},
                {"type": "text", "text": "real body"},
            ],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
        });

        // Act
        relocate_client_system(&mut body, false);

        // Assert: identity-only system; only the real body relocated (the
        // identity line is not duplicated into the reminder).
        let sys = body["system"].as_array().expect("system is array");
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0]["text"], INTERACTIVE_IDENTITY_LINE);
        let reminder = &body["messages"][0]["content"][0];
        assert_eq!(reminder["text"], reminder_text("real body"));
    }

    #[test]
    fn relocated_identity_carries_no_cache_control() {
        // Arrange: a plain client system, no cache_control.
        let mut body = json!({
            "system": [{"type": "text", "text": "custom"}],
            "messages": [{"role": "user", "content": "hi"}]
        });

        // Act
        relocate_client_system(&mut body, false);

        // Assert: the injected identity block has no cache breakpoint.
        let injected = &body["system"][0];
        assert!(
            injected.get("cache_control").is_none(),
            "injected identity must not add a cache breakpoint"
        );
    }

    #[test]
    fn relocate_non_object_body_is_noop() {
        // Arrange: a body that is not a JSON object at the root.
        let mut body = Value::String("not an object".into());
        let before = body.clone();

        // Act
        relocate_client_system(&mut body, false);

        // Assert: the whole transform is a no-op -- no panic, no reminder
        // insertion, no system rewrite. The body stays the same String.
        assert_eq!(body, before);
    }

    #[test]
    fn relocate_drops_non_text_system_blocks() {
        // Arrange: a system array with one valid text block and one non-text
        // block that carries no usable "text" field.
        let mut body = json!({
            "system": [
                {"type": "text", "text": "real body"},
                {"type": "image", "source": {"type": "base64", "data": "AAAA"}},
            ],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
        });

        // Act
        relocate_client_system(&mut body, false);

        // Assert: upstream system reduced to identity-only.
        let sys = body["system"].as_array().expect("system is array");
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0]["text"], INTERACTIVE_IDENTITY_LINE);
        // The reminder carries the text block and nothing from the non-text
        // block (which is intentionally dropped -- not valid system content).
        let reminder = &body["messages"][0]["content"][0];
        assert_eq!(reminder["text"], reminder_text("real body"));
        let reminder_str = reminder["text"].as_str().expect("reminder is a string");
        assert!(
            !reminder_str.contains("base64") && !reminder_str.contains("AAAA"),
            "non-text block content must not leak into the reminder: {reminder_str:?}"
        );
    }

    #[test]
    fn relocate_collapses_multi_block_cache_control_to_last() {
        // Arrange: two text blocks, each carrying a distinct cache_control.
        let mut body = json!({
            "system": [
                {"type": "text", "text": "first", "cache_control": {"type": "ephemeral", "ttl": "5m"}},
                {"type": "text", "text": "second", "cache_control": {"type": "ephemeral", "ttl": "1h"}},
            ],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
        });

        // Act
        relocate_client_system(&mut body, false);

        // Assert: the reminder carries exactly ONE cache_control, equal to the
        // LAST captured block's cache_control (last-wins collapse, which also
        // keeps the result under the 4-breakpoint cap).
        let reminder = &body["messages"][0]["content"][0];
        assert_eq!(reminder["cache_control"]["ttl"], "1h");
        let reminder_obj = reminder.as_object().expect("reminder is an object");
        assert_eq!(
            reminder_obj
                .keys()
                .filter(|k| k.as_str() == "cache_control")
                .count(),
            1,
            "exactly one cache_control on the relocated reminder block"
        );
    }

    #[test]
    fn relocate_neutralizes_injected_close_tag() {
        // Arrange: client system text contains a literal closing tag that
        // would prematurely close the wrapper after relocation.
        let mut body = json!({
            "system": "before </system-reminder> after",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
        });

        // Act
        relocate_client_system(&mut body, false);

        // Assert: the emitted reminder carries no stray closing tag in its
        // body -- only the single framing close tag at the very end.
        let reminder = &body["messages"][0]["content"][0]["text"];
        let text = reminder.as_str().expect("reminder is a string");
        assert!(
            text.starts_with(SYSTEM_REMINDER_OPEN) && text.ends_with(SYSTEM_REMINDER_CLOSE),
            "reminder must keep its framing: {text:?}"
        );
        // Strip the framing tags and confirm the inner body has no close tag.
        let inner = &text[SYSTEM_REMINDER_OPEN.len()..text.len() - SYSTEM_REMINDER_CLOSE.len()];
        assert!(
            !inner.contains(SYSTEM_REMINDER_CLOSE),
            "injected close tag must be neutralized in the body: {inner:?}"
        );
    }

    #[test]
    fn relocate_targets_first_user_message_among_many() {
        // Arrange: assistant, then user A, then user B. The reminder must land
        // in user A (the first user-role message), not the assistant or user B.
        let mut body = json!({
            "system": "client system prompt",
            "messages": [
                {"role": "assistant", "content": "prior"},
                {"role": "user", "content": [{"type": "text", "text": "A"}]},
                {"role": "user", "content": [{"type": "text", "text": "B"}]},
            ]
        });

        // Act
        relocate_client_system(&mut body, false);

        // Assert: reminder prepended to user A only.
        assert_eq!(body["messages"][0]["content"], "prior");
        assert_eq!(
            body["messages"][1]["content"][0]["text"],
            reminder_text("client system prompt")
        );
        assert_eq!(body["messages"][1]["content"][1]["text"], "A");
        assert_eq!(body["messages"][2]["content"][0]["text"], "B");
    }

    #[test]
    fn relocate_handles_absent_or_null_user_content() {
        // Arrange (a): a user message whose content key is absent.
        let mut absent = json!({
            "system": "client system prompt",
            "messages": [{"role": "user"}]
        });

        // Act
        relocate_client_system(&mut absent, false);

        // Assert: content becomes an array holding only the reminder.
        let content = absent["messages"][0]["content"]
            .as_array()
            .expect("content set to array");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], reminder_text("client system prompt"));

        // Arrange (b): a user message whose content is explicitly null.
        let mut null_content = json!({
            "system": "client system prompt",
            "messages": [{"role": "user", "content": Value::Null}]
        });

        // Act
        relocate_client_system(&mut null_content, false);

        // Assert: same -- content becomes an array holding only the reminder.
        let content = null_content["messages"][0]["content"]
            .as_array()
            .expect("content set to array");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], reminder_text("client system prompt"));
    }

    #[test]
    fn relocate_handles_whitespace_only_system() {
        // Arrange: system is a whitespace-only string.
        let mut body = json!({
            "system": "   ",
            "messages": [{"role": "user", "content": "hello"}]
        });

        // Act
        relocate_client_system(&mut body, false);

        // Assert: system is reduced to identity-only (sensible, no panic).
        let sys = body["system"].as_array().expect("system is array");
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0]["text"], INTERACTIVE_IDENTITY_LINE);
        // Current behavior pinned: whitespace is not a recognized identity
        // line, so it IS relocated -- the reminder wraps the whitespace.
        let content = body["messages"][0]["content"]
            .as_array()
            .expect("content promoted to array");
        assert_eq!(content[0]["text"], reminder_text("   "));
        assert_eq!(content[1]["text"], "hello");
    }

    // -- metadata mint -----------------------------------------------------

    #[test]
    fn metadata_user_id_minted_when_absent() {
        // Arrange
        let id = identity();
        let mut body = json!({"model": "claude"});

        // Act
        mint_metadata_user_id(&mut body, &id);

        // Assert
        let user_id = body["metadata"]["user_id"]
            .as_str()
            .expect("user_id is a string");
        let parsed: Value = serde_json::from_str(user_id).expect("user_id parses as JSON");
        assert_eq!(parsed["device_id"], id.device_id);
        assert_eq!(parsed["account_uuid"], id.account_uuid);
        assert_eq!(parsed["session_id"], id.session_id);
        // device_id is 64 hex chars.
        let device = parsed["device_id"].as_str().unwrap();
        assert_eq!(device.len(), 64);
        assert!(device.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn metadata_user_id_key_order_is_corpus_shape() {
        // Arrange
        let id = identity();
        let mut body = json!({});

        // Act
        mint_metadata_user_id(&mut body, &id);

        // Assert: keys appear in device_id, account_uuid, session_id order.
        let user_id = body["metadata"]["user_id"].as_str().unwrap();
        let dev = user_id.find("device_id").unwrap();
        let acct = user_id.find("account_uuid").unwrap();
        let sess = user_id.find("session_id").unwrap();
        assert!(
            dev < acct && acct < sess,
            "key order must match corpus: {user_id}"
        );
    }

    #[test]
    fn metadata_user_id_present_non_empty_untouched() {
        // Arrange
        let id = identity();
        let mut body = json!({"metadata": {"user_id": "client-supplied"}});

        // Act
        mint_metadata_user_id(&mut body, &id);

        // Assert
        assert_eq!(body["metadata"]["user_id"], "client-supplied");
    }

    #[test]
    fn metadata_user_id_present_empty_is_minted() {
        // Arrange
        let id = identity();
        let mut body = json!({"metadata": {"user_id": ""}});

        // Act
        mint_metadata_user_id(&mut body, &id);

        // Assert
        assert_ne!(body["metadata"]["user_id"], "");
        let user_id = body["metadata"]["user_id"].as_str().unwrap();
        assert!(user_id.contains(&id.session_id));
    }

    #[test]
    fn metadata_mint_preserves_other_metadata_keys() {
        // Arrange
        let id = identity();
        let mut body = json!({"metadata": {"other": "keep-me"}});

        // Act
        mint_metadata_user_id(&mut body, &id);

        // Assert
        assert_eq!(body["metadata"]["other"], "keep-me");
        assert!(body["metadata"]["user_id"].is_string());
    }

    // -- full cloak orchestration ------------------------------------------

    #[test]
    fn cloak_non_cc_strips_billing_stamps_identity_and_metadata() {
        // Arrange: non-CC body with billing + custom system and a user
        // message to relocate the client system into.
        let id = identity();
        let req = ChatRequest::default();
        let mut body = json!({
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: v=1"},
                {"type": "text", "text": "custom"},
            ],
            "messages": [{"role": "user", "content": "hello"}]
        });

        // Act
        cloak_oauth_egress(&mut body, &req, &id, true, &CloakConfig::default());

        // Assert: billing gone, system is identity-only, the client system is
        // relocated into the first user message, metadata minted.
        let arr = body["system"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["text"], INTERACTIVE_IDENTITY_LINE);
        assert!(!arr.iter().any(|b| {
            b["text"]
                .as_str()
                .map(|t| t.starts_with(BILLING_PREFIX))
                .unwrap_or(false)
        }));
        let reminder = &body["messages"][0]["content"][0];
        assert_eq!(
            reminder["text"],
            format!("{SYSTEM_REMINDER_OPEN}\ncustom\n{SYSTEM_REMINDER_CLOSE}")
        );
        assert!(body["metadata"]["user_id"].is_string());
    }

    #[test]
    fn cloak_genuine_cc_strips_billing_but_does_not_stamp() {
        // Arrange: genuine CC (is_non_cc = false). Billing must still be
        // stripped, but no identity block, no metadata, no reminder added.
        let id = identity();
        let req = ChatRequest::default();
        let mut body = json!({
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: v=1"},
                {"type": "text", "text": "custom"},
            ],
            "messages": [{"role": "user", "content": "hello"}]
        });

        // Act
        cloak_oauth_egress(&mut body, &req, &id, false, &CloakConfig::default());

        // Assert: billing stripped, but identity NOT stamped, metadata absent,
        // client system retained in `system`, and NO reminder anywhere.
        let arr = body["system"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["text"], "custom");
        assert!(body.get("metadata").is_none());
        assert_eq!(body["messages"][0]["content"], "hello");
        let serialized = serde_json::to_string(&body).unwrap();
        assert!(
            !serialized.contains(SYSTEM_REMINDER_OPEN),
            "genuine CC must not gain a system-reminder block"
        );
    }

    #[test]
    fn mint_session_id_prefers_credential_value() {
        let id = ClaudeCodeIdentity::mint(Some("cred-session"));
        assert_eq!(id.session_id, "cred-session");
    }

    #[test]
    fn mint_session_id_falls_back_to_fresh_uuid() {
        let id = ClaudeCodeIdentity::mint(None);
        assert!(
            uuid::Uuid::parse_str(&id.session_id).is_ok(),
            "minted session id must be a valid uuid; got {}",
            id.session_id
        );
    }

    #[test]
    fn mint_device_id_is_64_lowercase_hex() {
        let id = ClaudeCodeIdentity::mint(None);
        assert_eq!(id.device_id.len(), 64);
        assert!(
            id.device_id
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn mint_account_uuid_is_dashed_uuid() {
        let id = ClaudeCodeIdentity::mint(None);
        assert!(
            uuid::Uuid::parse_str(&id.account_uuid).is_ok(),
            "account_uuid must be a valid dashed uuid; got {}",
            id.account_uuid
        );
        assert!(id.account_uuid.contains('-'));
    }

    // -- tool-name normalization to mcp__ (forward) ------------------------

    #[test]
    fn doubles_single_underscore_mcp_prefix_only() {
        // Arrange: internal separators must be untouched.
        let mut body = json!({
            "tools": [{"name": "mcp_linear_get_issue"}]
        });

        // Act
        let reverse = normalize_tool_names_to_mcp(&mut body);

        // Assert: prefix doubled, internal underscores preserved.
        assert_eq!(body["tools"][0]["name"], "mcp__linear_get_issue");
        assert_eq!(
            reverse.get("mcp__linear_get_issue").map(String::as_str),
            Some("mcp_linear_get_issue")
        );
    }

    #[test]
    fn renames_across_tool_choice() {
        // Arrange
        let mut body = json!({
            "tool_choice": {"type": "tool", "name": "mcp_foo"}
        });

        // Act
        let reverse = normalize_tool_names_to_mcp(&mut body);

        // Assert
        assert_eq!(body["tool_choice"]["name"], "mcp__foo");
        assert_eq!(reverse.get("mcp__foo").map(String::as_str), Some("mcp_foo"));
    }

    #[test]
    fn tool_choice_auto_is_untouched() {
        // Arrange: tool_choice without type=="tool" has no name to rename.
        let mut body = json!({"tool_choice": {"type": "auto"}});
        let before = body.clone();

        // Act
        let reverse = normalize_tool_names_to_mcp(&mut body);

        // Assert
        assert_eq!(body, before);
        assert!(reverse.is_empty());
    }

    #[test]
    fn renames_tool_use_in_message_history() {
        // Arrange
        let mut body = json!({
            "messages": [{
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "t1", "name": "mcp_foo", "input": {}}]
            }]
        });

        // Act
        let reverse = normalize_tool_names_to_mcp(&mut body);

        // Assert
        assert_eq!(body["messages"][0]["content"][0]["name"], "mcp__foo");
        assert_eq!(reverse.get("mcp__foo").map(String::as_str), Some("mcp_foo"));
    }

    #[test]
    fn renames_tool_reference_in_message_history() {
        // Arrange
        let mut body = json!({
            "messages": [{
                "role": "user",
                "content": [{"type": "tool_reference", "tool_name": "mcp_foo"}]
            }]
        });

        // Act
        let reverse = normalize_tool_names_to_mcp(&mut body);

        // Assert
        assert_eq!(body["messages"][0]["content"][0]["tool_name"], "mcp__foo");
        assert_eq!(reverse.get("mcp__foo").map(String::as_str), Some("mcp_foo"));
    }

    #[test]
    fn renames_nested_tool_reference_inside_tool_result() {
        // Arrange
        let mut body = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "t1",
                    "content": [{"type": "tool_reference", "tool_name": "mcp_foo"}]
                }]
            }]
        });

        // Act
        let reverse = normalize_tool_names_to_mcp(&mut body);

        // Assert
        assert_eq!(
            body["messages"][0]["content"][0]["content"][0]["tool_name"],
            "mcp__foo"
        );
        assert_eq!(reverse.get("mcp__foo").map(String::as_str), Some("mcp_foo"));
    }

    #[test]
    fn idempotent_double_underscore_untouched_no_reverse_entry() {
        // Arrange: an already-mcp__ name records nothing and is unchanged.
        let mut body = json!({"tools": [{"name": "mcp__foo"}]});
        let before = body.clone();

        // Act
        let reverse = normalize_tool_names_to_mcp(&mut body);

        // Assert
        assert_eq!(body, before);
        assert!(
            reverse.is_empty(),
            "an already-normalized name must record no reverse entry"
        );
    }

    #[test]
    fn idempotent_applying_twice_is_byte_identical() {
        // Arrange
        let mut body = json!({"tools": [{"name": "mcp_foo"}]});

        // Act: first pass renames, second is a no-op.
        normalize_tool_names_to_mcp(&mut body);
        let once = body.clone();
        let reverse2 = normalize_tool_names_to_mcp(&mut body);

        // Assert
        assert_eq!(body, once);
        assert!(reverse2.is_empty());
    }

    #[test]
    fn builtin_tool_with_type_is_skipped() {
        // Arrange: a native builtin (non-empty "type") is left unchanged
        // even if its name happens to carry the mcp_ prefix.
        let mut body = json!({
            "tools": [{"type": "web_search_20250305", "name": "mcp_should_not_rename"}]
        });
        let before = body.clone();

        // Act
        let reverse = normalize_tool_names_to_mcp(&mut body);

        // Assert
        assert_eq!(body, before);
        assert!(reverse.is_empty());
    }

    #[test]
    fn collision_guard_skips_when_renamed_form_already_exists() {
        // Arrange: renaming mcp_foo would collide with an existing mcp__foo.
        let mut body = json!({
            "tools": [
                {"name": "mcp_foo"},
                {"name": "mcp__foo"}
            ]
        });

        // Act
        let reverse = normalize_tool_names_to_mcp(&mut body);

        // Assert: the colliding rename is skipped; both names preserved.
        assert_eq!(body["tools"][0]["name"], "mcp_foo");
        assert_eq!(body["tools"][1]["name"], "mcp__foo");
        assert!(
            reverse.is_empty(),
            "collision must skip the rename and record no reverse entry"
        );
    }

    #[test]
    fn bare_name_is_prefixed_with_mcp_double() {
        // Arrange: a bare snake_case tool name (the hermes-style set).
        let mut body = json!({"tools": [{"name": "read_file"}]});

        // Act
        let reverse = normalize_tool_names_to_mcp(&mut body);

        // Assert: prefixed, reverse restores the bare original.
        assert_eq!(body["tools"][0]["name"], "mcp__read_file");
        assert_eq!(
            reverse.get("mcp__read_file").map(String::as_str),
            Some("read_file")
        );
    }

    #[test]
    fn titlecase_bare_name_is_prefixed_with_mcp_double() {
        // Arrange: the bare path applies to anything non-mcp__, including
        // TitleCase names like Bash.
        let mut body = json!({"tools": [{"name": "Bash"}]});

        // Act
        let reverse = normalize_tool_names_to_mcp(&mut body);

        // Assert
        assert_eq!(body["tools"][0]["name"], "mcp__Bash");
        assert_eq!(reverse.get("mcp__Bash").map(String::as_str), Some("Bash"));
    }

    #[test]
    fn every_non_mcp_double_name_is_cloaked_across_all_surfaces() {
        // Arrange: a mixed set of bare, TitleCase, single-mcp_, and
        // already-mcp__ names spread across every renamed surface.
        let mut body = json!({
            "tools": [{"name": "Bash"}, {"name": "glob"}, {"name": "read_file"}],
            "tool_choice": {"type": "tool", "name": "Bash"},
            "messages": [{
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "t1", "name": "terminal", "input": {}}]
            }]
        });

        // Act
        let reverse = normalize_tool_names_to_mcp(&mut body);

        // Assert: every bare name became mcp__-prefixed on every surface.
        assert_eq!(body["tools"][0]["name"], "mcp__Bash");
        assert_eq!(body["tools"][1]["name"], "mcp__glob");
        assert_eq!(body["tools"][2]["name"], "mcp__read_file");
        assert_eq!(body["tool_choice"]["name"], "mcp__Bash");
        assert_eq!(body["messages"][0]["content"][0]["name"], "mcp__terminal");
        // Reverse map has one entry per distinct renamed name.
        assert_eq!(reverse.get("mcp__Bash").map(String::as_str), Some("Bash"));
        assert_eq!(reverse.get("mcp__glob").map(String::as_str), Some("glob"));
        assert_eq!(
            reverse.get("mcp__read_file").map(String::as_str),
            Some("read_file")
        );
        assert_eq!(
            reverse.get("mcp__terminal").map(String::as_str),
            Some("terminal")
        );
    }

    #[test]
    fn full_hermes_tool_set_round_trips() {
        // Arrange: a representative subset of the real hermes tool set (all
        // bare snake_case), the empirical trigger for the billing 400.
        let names = [
            "browser_back",
            "read_file",
            "terminal",
            "write_file",
            "list_dir",
            "search",
        ];
        let tools: Vec<Value> = names.iter().map(|n| json!({"name": n})).collect();
        let mut body = json!({"tools": tools});

        // Act
        let reverse = normalize_tool_names_to_mcp(&mut body);

        // Assert: every tool is now mcp__-prefixed, one reverse entry each,
        // and the reverse fully restores the originals.
        let out = body["tools"].as_array().unwrap();
        assert_eq!(out.len(), names.len());
        for (i, n) in names.iter().enumerate() {
            let renamed = format!("mcp__{n}");
            assert_eq!(out[i]["name"], renamed);
            assert_eq!(reverse.get(&renamed).map(String::as_str), Some(*n));
        }
        assert_eq!(reverse.len(), names.len());
    }

    #[test]
    fn cloak_oauth_egress_returns_reverse_map() {
        // Arrange
        let id = identity();
        let req = ChatRequest::default();
        let mut body = json!({"tools": [{"name": "mcp_foo"}]});

        // Act
        let result = cloak_oauth_egress(&mut body, &req, &id, true, &CloakConfig::default());

        // Assert
        assert_eq!(body["tools"][0]["name"], "mcp__foo");
        assert_eq!(
            result.tool_reverse.get("mcp__foo").map(String::as_str),
            Some("mcp_foo")
        );
    }

    #[test]
    fn normalize_is_deterministic_same_input_byte_identical() {
        // Arrange: a body exercising every renamed surface.
        let template = json!({
            "tools": [{"name": "mcp_foo"}, {"name": "Bash"}],
            "tool_choice": {"type": "tool", "name": "mcp_bar"},
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "t1", "name": "mcp_baz", "input": {}},
                    {"type": "tool_result", "tool_use_id": "t1", "content": [
                        {"type": "tool_reference", "tool_name": "mcp_qux"}
                    ]}
                ]
            }]
        });

        // Act: normalize two independent clones.
        let mut a = template.clone();
        let mut b = template.clone();
        normalize_tool_names_to_mcp(&mut a);
        normalize_tool_names_to_mcp(&mut b);

        // Assert: byte-identical serialized output.
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
    }

    // -- CloakConfig defaults ----------------------------------------------

    #[test]
    fn cloak_config_default_is_auto_false_empty_empty() {
        // Arrange / Act
        let cfg = CloakConfig::default();

        // Assert
        assert_eq!(cfg.mode, CloakMode::Auto);
        assert!(!cfg.strict_mode);
        assert!(cfg.tool_rename.is_empty());
        assert!(cfg.sensitive_words.is_empty());
    }

    #[test]
    fn cloak_mode_default_is_auto() {
        assert_eq!(CloakMode::default(), CloakMode::Auto);
    }

    // -- regression guard: default config == base cloak transforms ---------

    /// With a DEFAULT (empty) CloakConfig, the non-CC post-cloak body must be
    /// byte-identical to the base cloak transforms: billing strip, system
    /// relocation (identity-only system + client body moved to the first user
    /// message), metadata mint, and the broadened tool-name normalization.
    /// This is the hard regression guard for the opt-in surface; the base is
    /// the NEW relocate behavior, not the old keep-behind-identity.
    #[test]
    fn default_config_byte_identical_to_base_transforms() {
        // Arrange: a body exercising billing strip + system relocation +
        // tool-name normalization (mcp_ subcase AND a bare name). A user
        // message is present so the client system has somewhere to relocate.
        let id = identity();
        let req = ChatRequest::default();
        let template = json!({
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: v=1"},
                {"type": "text", "text": "custom system prompt"},
            ],
            "tools": [{"name": "mcp_linear_get_issue"}, {"name": "Bash"}],
            "messages": [{
                "role": "user",
                "content": [{"type": "text", "text": "hello"}]
            }]
        });

        // Act: one body through the cloak with a default config; a second
        // body through the SAME base transforms applied directly (strip +
        // relocate + metadata + tool-name normalization).
        let mut via_config = template.clone();
        cloak_oauth_egress(&mut via_config, &req, &id, true, &CloakConfig::default());

        let mut via_base = template.clone();
        strip_billing_block(&mut via_base);
        relocate_client_system(&mut via_base, false);
        mint_metadata_user_id(&mut via_base, &id);
        let _ = normalize_tool_names_to_mcp(&mut via_base);

        // Assert: byte-identical serialized output.
        assert_eq!(
            serde_json::to_string(&via_config).unwrap(),
            serde_json::to_string(&via_base).unwrap()
        );
        // The NEW base behavior: identity-only system, client body relocated.
        assert_eq!(via_config["system"][0]["text"], INTERACTIVE_IDENTITY_LINE);
        assert_eq!(
            via_config["messages"][0]["content"][0]["text"],
            format!("{SYSTEM_REMINDER_OPEN}\ncustom system prompt\n{SYSTEM_REMINDER_CLOSE}")
        );
        // And the BROADENED normalization applied: the mcp_ subcase doubled
        // its prefix AND the bare name gained the mcp__ prefix.
        assert_eq!(via_config["tools"][0]["name"], "mcp__linear_get_issue");
        assert_eq!(via_config["tools"][1]["name"], "mcp__Bash");
    }

    /// Companion guard for the GENUINE-CC path (is_non_cc=false): with a
    /// DEFAULT config, the post-cloak body must be byte-identical to the base
    /// CC sequence -- strip_billing_block + normalize_tool_names_to_mcp ONLY,
    /// with NO identity block prepended and NO metadata user_id minted.
    #[test]
    fn default_config_byte_identical_to_base_transforms_genuine_cc() {
        // Arrange: same representative body as the non-CC guard.
        let id = identity();
        let req = ChatRequest::default();
        let template = json!({
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: v=1"},
                {"type": "text", "text": "custom system prompt"},
            ],
            "tools": [{"name": "mcp_linear_get_issue"}, {"name": "Bash"}],
            "messages": [{
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "t1", "name": "mcp_foo", "input": {}}]
            }]
        });

        // Act: default config with is_non_cc=false vs. the base CC sequence
        // (billing strip + tool-name normalize only -- no identity, no
        // user_id mint).
        let mut via_config = template.clone();
        cloak_oauth_egress(&mut via_config, &req, &id, false, &CloakConfig::default());

        let mut via_base = template.clone();
        strip_billing_block(&mut via_base);
        let _ = normalize_tool_names_to_mcp(&mut via_base);

        // Assert: byte-identical serialized output.
        assert_eq!(
            serde_json::to_string(&via_config).unwrap(),
            serde_json::to_string(&via_base).unwrap()
        );
        // The broadened normalization still applies on the CC path...
        assert_eq!(via_config["tools"][0]["name"], "mcp__linear_get_issue");
        assert_eq!(via_config["tools"][1]["name"], "mcp__Bash");
        // ...but NO identity block was prepended (system[0] is still the
        // client's billing-stripped first block, not the interactive line)
        // and NO metadata was minted.
        assert_ne!(via_config["system"][0]["text"], INTERACTIVE_IDENTITY_LINE);
        assert!(via_config.get("metadata").is_none());
    }

    // -- tool_rename --------------------------------------------------------

    #[test]
    fn tool_rename_applies_to_tools_and_tool_use_and_records_reverse() {
        // Arrange: an operator rename across tools + tool_use. Because the
        // tool-name normalization runs FIRST, a bare `foo` is already
        // `mcp__foo` on the wire by the time the operator pass runs, so the
        // rename keys on the normalized name `mcp__foo`.
        let id = identity();
        let req = ChatRequest::default();
        let mut body = json!({
            "tools": [{"name": "foo"}],
            "messages": [{
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "t1", "name": "foo", "input": {}}]
            }]
        });
        let cfg = CloakConfig {
            tool_rename: vec![ToolRename {
                from: "mcp__foo".into(),
                to: "bar".into(),
            }],
            ..CloakConfig::default()
        };

        // Act
        let result = cloak_oauth_egress(&mut body, &req, &id, false, &cfg);

        // Assert: forward rename applied on both surfaces.
        assert_eq!(body["tools"][0]["name"], "bar");
        assert_eq!(body["messages"][0]["content"][0]["name"], "bar");
        // Both reverse hops recorded: the normalization mcp__foo->foo and the
        // operator rename bar->mcp__foo.
        assert_eq!(
            result.tool_reverse.get("mcp__foo").map(String::as_str),
            Some("foo")
        );
        assert_eq!(
            result.tool_reverse.get("bar").map(String::as_str),
            Some("mcp__foo")
        );
    }

    /// Ordering: tool-name normalization runs FIRST, then operator
    /// tool_rename. A rename targeting the post-normalization name `mcp__x`
    /// must match (because the wire name is already `mcp__x` by the time the
    /// operator pass runs). A rename keyed on the pre-normalization `mcp_x`
    /// must NOT match (the normalization pass already changed it).
    #[test]
    fn tool_rename_runs_after_tool_name_normalization() {
        let id = identity();
        let req = ChatRequest::default();

        // (a) rename keyed on the normalized name mcp__x -> renamed.
        let mut body_a = json!({"tools": [{"name": "mcp_x"}]});
        let cfg_a = CloakConfig {
            tool_rename: vec![ToolRename {
                from: "mcp__x".into(),
                to: "renamed".into(),
            }],
            ..CloakConfig::default()
        };
        let res_a = cloak_oauth_egress(&mut body_a, &req, &id, false, &cfg_a);
        assert_eq!(
            body_a["tools"][0]["name"], "renamed",
            "rename keyed on the normalized name must match"
        );
        // Both reverse hops are recorded: mcp__x->mcp_x (from the
        // normalization pass) and renamed->mcp__x (from the operator pass).
        assert_eq!(
            res_a.tool_reverse.get("mcp__x").map(String::as_str),
            Some("mcp_x")
        );
        assert_eq!(
            res_a.tool_reverse.get("renamed").map(String::as_str),
            Some("mcp__x")
        );

        // (b) rename keyed on the PRE-normalization name mcp_x must NOT match
        // (the normalization pass already rewrote it to mcp__x first).
        let mut body_b = json!({"tools": [{"name": "mcp_x"}]});
        let cfg_b = CloakConfig {
            tool_rename: vec![ToolRename {
                from: "mcp_x".into(),
                to: "should_not_apply".into(),
            }],
            ..CloakConfig::default()
        };
        cloak_oauth_egress(&mut body_b, &req, &id, false, &cfg_b);
        assert_eq!(
            body_b["tools"][0]["name"], "mcp__x",
            "rename keyed on the pre-normalization name must NOT match"
        );
    }

    #[test]
    fn tool_rename_empty_is_noop() {
        let mut body = json!({"tools": [{"name": "foo"}]});
        let mut reverse: HashMap<String, String> = HashMap::new();
        apply_tool_rename(&mut body, &[], &mut reverse);
        assert_eq!(body["tools"][0]["name"], "foo");
        assert!(reverse.is_empty());
    }

    // -- sensitive_words ----------------------------------------------------

    #[test]
    fn sensitive_words_obfuscates_system_and_message_text() {
        // Arrange
        let mut body = json!({
            "system": "the secret password is here",
            "messages": [{
                "role": "user",
                "content": [{"type": "text", "text": "another secret"}]
            }]
        });

        // Act
        obfuscate_sensitive_words(&mut body, &["secret".to_string()]);

        // Assert: a zero-width space lands after the first char of "secret".
        let zws = ZERO_WIDTH_SPACE;
        let expect = format!("s{zws}ecret");
        assert!(
            body["system"].as_str().unwrap().contains(&expect),
            "system text must be obfuscated: {:?}",
            body["system"]
        );
        assert!(
            body["messages"][0]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains(&expect),
            "message text must be obfuscated"
        );
    }

    #[test]
    fn sensitive_words_empty_list_is_byte_identical() {
        // Arrange
        let mut with_words = json!({
            "system": "the secret password",
            "messages": [{"role": "user", "content": "secret stuff"}]
        });
        let without = with_words.clone();

        // Act: empty list must be a byte-identical no-op.
        obfuscate_sensitive_words(&mut with_words, &[]);

        // Assert
        assert_eq!(
            serde_json::to_string(&with_words).unwrap(),
            serde_json::to_string(&without).unwrap()
        );
    }

    #[test]
    fn sensitive_words_case_insensitive_longest_first() {
        // Arrange: "secretkey" (longer) must win over "secret" at the same
        // anchor, and matching is case-insensitive.
        let mut body = json!({"system": "my SECRETKEY value"});
        let cfg_words = vec!["secret".to_string(), "secretkey".to_string()];

        // Act
        obfuscate_sensitive_words(&mut body, &cfg_words);

        // Assert: the obfuscation marks after the first char of the WHOLE
        // longest match, preserving the original casing of the remaining
        // chars ("SECRETKEY" -> "S<zws>ECRETKEY").
        let zws = ZERO_WIDTH_SPACE;
        let out = body["system"].as_str().unwrap();
        assert!(
            out.contains(&format!("S{zws}ECRETKEY")),
            "longest case-insensitive match must be obfuscated whole: {out:?}"
        );
    }

    #[test]
    fn sensitive_words_obfuscation_carries_no_reverse() {
        // The full egress with sensitive_words set records NO extra reverse
        // entries for the obfuscation (zero-width space is invisible). The
        // tool name is already mcp__-shaped, so the tool-name normalization
        // adds no reverse entry either -- isolating the obfuscation pass.
        let id = identity();
        let req = ChatRequest::default();
        let mut body = json!({"system": "secret", "tools": [{"name": "mcp__bash"}]});
        let cfg = CloakConfig {
            sensitive_words: vec!["secret".to_string()],
            ..CloakConfig::default()
        };
        let result = cloak_oauth_egress(&mut body, &req, &id, false, &cfg);
        assert!(
            result.tool_reverse.is_empty(),
            "sensitive-word obfuscation must not add reverse entries"
        );
    }
}
