//! Claude Code identity cloak for the OAuth anthropic-api egress.
//!
//! On the OauthBearer + api.anthropic.com surface, routectl talks to
//! Anthropic with a Claude Code subscription identity. A genuine Claude
//! Code client supplies its own session-id header, the canonical identity
//! system block, and a metadata `user_id`; a non-CC client (a bare
//! OpenAI/Anthropic SDK, a custom script) supplies none of these. This
//! module mints a stable per-provider identity once and rewrites the
//! outgoing body so a non-CC client inherits the same shape: the
//! interactive identity system block and a corpus-shaped metadata
//! `user_id`. The billing/attribution block is always stripped (CC or
//! not) so the client fingerprint never reaches the upstream.
//!
//! Shapes here are fixed from empirical capture against the live endpoint;
//! do not redesign them.

use serde_json::{json, Value};

use routectl_core::ChatRequest;

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
/// genuine CC client); the identity system block and metadata `user_id`
/// are minted only for a non-CC client.
pub(crate) fn cloak_oauth_egress(
    body: &mut Value,
    _req: &ChatRequest,
    identity: &ClaudeCodeIdentity,
    is_non_cc: bool,
) {
    strip_billing_block(body);
    if is_non_cc {
        ensure_identity_block(body);
        mint_metadata_user_id(body, identity);
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

/// Ensure the body's `system` opens with the interactive Claude Code
/// identity line. A no-op when the first block already is a recognized
/// identity line; otherwise the interactive line is prepended (array) or
/// the whole string is wrapped behind it. Injected WITHOUT
/// `cache_control` -- no cache breakpoint is added.
fn ensure_identity_block(body: &mut Value) {
    if first_block_is_recognized(body.get("system")) {
        return;
    }
    let identity = identity_block();
    let new_system = match body.get_mut("system") {
        Some(Value::Array(blocks)) => {
            blocks.insert(0, identity);
            return;
        }
        Some(Value::String(s)) => {
            let existing = json!({"type": "text", "text": std::mem::take(s)});
            Value::Array(vec![identity, existing])
        }
        _ => Value::Array(vec![identity]),
    };
    if let Some(obj) = body.as_object_mut() {
        obj.insert("system".into(), new_system);
    }
}

/// True when `system` already presents a recognized identity line as its
/// first block (array `[0].text`) or as the whole string.
fn first_block_is_recognized(system: Option<&Value>) -> bool {
    let text = match system {
        Some(Value::Array(blocks)) => blocks
            .first()
            .and_then(|b| b.get("text"))
            .and_then(Value::as_str),
        Some(Value::String(s)) => Some(s.as_str()),
        _ => None,
    };
    text.map(|t| RECOGNIZED_IDENTITY_LINES.contains(&t.trim()))
        .unwrap_or(false)
}

/// The interactive identity system block, without `cache_control`.
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

    // -- identity stamp (non-CC) -------------------------------------------

    #[test]
    fn identity_noop_when_first_block_is_interactive_line() {
        // Arrange
        let mut body = json!({
            "system": [{"type": "text", "text": INTERACTIVE_IDENTITY_LINE}]
        });
        let before = body.clone();

        // Act
        ensure_identity_block(&mut body);

        // Assert
        assert_eq!(body, before);
    }

    #[test]
    fn identity_noop_when_first_block_is_agent_sdk_line() {
        // Arrange
        let agent_line = RECOGNIZED_IDENTITY_LINES[1];
        let mut body = json!({
            "system": [{"type": "text", "text": agent_line}]
        });
        let before = body.clone();

        // Act
        ensure_identity_block(&mut body);

        // Assert
        assert_eq!(body, before);
    }

    #[test]
    fn identity_prepended_to_generic_array() {
        // Arrange
        let mut body = json!({
            "system": [{"type": "text", "text": "custom system prompt"}]
        });

        // Act
        ensure_identity_block(&mut body);

        // Assert
        let arr = body["system"].as_array().expect("system is array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["text"], INTERACTIVE_IDENTITY_LINE);
        assert_eq!(arr[1]["text"], "custom system prompt");
    }

    #[test]
    fn identity_wraps_string_system() {
        // Arrange
        let mut body = json!({"system": "custom system prompt"});

        // Act
        ensure_identity_block(&mut body);

        // Assert
        let arr = body["system"].as_array().expect("system is array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["text"], INTERACTIVE_IDENTITY_LINE);
        assert_eq!(arr[1]["type"], "text");
        assert_eq!(arr[1]["text"], "custom system prompt");
    }

    #[test]
    fn identity_injected_when_system_absent() {
        // Arrange
        let mut body = json!({"model": "claude"});

        // Act
        ensure_identity_block(&mut body);

        // Assert
        let arr = body["system"].as_array().expect("system is array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["text"], INTERACTIVE_IDENTITY_LINE);
    }

    #[test]
    fn injected_identity_carries_no_cache_control() {
        // Arrange
        let mut body = json!({"system": [{"type": "text", "text": "custom"}]});

        // Act
        ensure_identity_block(&mut body);

        // Assert
        let injected = &body["system"][0];
        assert!(
            injected.get("cache_control").is_none(),
            "injected identity must not add a cache breakpoint"
        );
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
        // Arrange
        let id = identity();
        let req = ChatRequest::default();
        let mut body = json!({
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: v=1"},
                {"type": "text", "text": "custom"},
            ]
        });

        // Act
        cloak_oauth_egress(&mut body, &req, &id, true);

        // Assert: billing gone, identity prepended, metadata minted.
        let arr = body["system"].as_array().unwrap();
        assert_eq!(arr[0]["text"], INTERACTIVE_IDENTITY_LINE);
        assert_eq!(arr[1]["text"], "custom");
        assert!(!arr.iter().any(|b| b["text"]
            .as_str()
            .map(|t| t.starts_with(BILLING_PREFIX))
            .unwrap_or(false)));
        assert!(body["metadata"]["user_id"].is_string());
    }

    #[test]
    fn cloak_genuine_cc_strips_billing_but_does_not_stamp() {
        // Arrange: genuine CC (is_non_cc = false). Billing must still be
        // stripped, but no identity block and no metadata are added.
        let id = identity();
        let req = ChatRequest::default();
        let mut body = json!({
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: v=1"},
                {"type": "text", "text": "custom"},
            ]
        });

        // Act
        cloak_oauth_egress(&mut body, &req, &id, false);

        // Assert: billing stripped, but identity NOT stamped, metadata absent.
        let arr = body["system"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["text"], "custom");
        assert!(body.get("metadata").is_none());
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
        assert!(id
            .device_id
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
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
}
