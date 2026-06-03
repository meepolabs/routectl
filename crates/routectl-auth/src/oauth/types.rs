//! On-disk and in-memory shapes for the OAuth credential store.
//!
//! `CredentialsFile` is the JSON document at
//! `~/.config/routectl/credentials.json`. `TokenRecord` is one
//! provider's bundle inside it. The schema is versioned (`SCHEMA_VERSION`);
//! mismatches are a fatal error with operator guidance, not a silent
//! migration -- the file is regenerable via `routectl login`.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zeroize::Zeroize;

/// Bumped on any incompatible schema change. Reader rejects any
/// version it does not understand; the operator is told to
/// re-`routectl login` (the file is always regenerable, so a forced
/// re-login is a strict improvement over silent translation bugs).
pub const SCHEMA_VERSION: u32 = 1;

/// Shared "now in unix seconds" helper for the OAuth subsystem.
/// One implementation, used by `OAuthStore`, the Anthropic flow's
/// token decoder, and `routectl whoami`. Saturates to 0 on systems
/// whose clock is set before 1970 (pathological but possible in
/// containers); a 1970 expiry will look "expired" everywhere, which
/// is the safe direction.
///
/// Wall-clock dependency: reads the system clock, NOT a monotonic
/// source. OAuth `expires_at_unix` decisions depend on this value;
/// if the system clock skews (broken NTP, manual clock change, VM
/// pause/resume), the router may treat valid tokens as expired or
/// expired tokens as valid. In production, ensure NTP sync.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Newtype wrapping a sensitive `String` so that:
/// - `Debug` redacts the inner value (so `tracing::debug!(?record)`
///   never leaks tokens)
/// - `Drop` zeroizes the inner buffer (so freed allocations do not
///   leave token bytes lingering in the heap)
/// - `Serialize`/`Deserialize` round-trip the raw String (the on-disk
///   credentials.json must contain the actual token)
///
/// The accessor `expose` returns `&str` for read sites that genuinely
/// need the value (auth header construction, refresh POST). All other
/// code should treat it as opaque.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct SecretToken(String);

impl SecretToken {
    /// Construct from any `Into<String>`.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Read the raw token. The name is loud on purpose: every call site
    /// is a place a reviewer must check is not a logging or display path.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretToken([REDACTED])")
    }
}

impl Drop for SecretToken {
    fn drop(&mut self) {
        // Zeroize via `zeroize::Zeroize`. Best-effort: the compiler may
        // still leave intermediate copies on the stack of code that
        // moved the SecretToken, but the heap-resident inner buffer
        // (the long-lived copy that lives across the credentials-file
        // load + the lifetime of the in-memory cache) is overwritten.
        self.0.zeroize();
    }
}

impl Serialize for SecretToken {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<'de> Deserialize<'de> for SecretToken {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d).map(SecretToken)
    }
}

/// One provider's token bundle. All fields are stored verbatim from
/// the upstream token endpoint response. `expires_at_unix` is
/// computed at exchange time as `now + expires_in` so a clock jump on
/// disk does not corrupt validity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct TokenRecord {
    pub access_token: SecretToken,
    pub refresh_token: SecretToken,

    #[serde(default = "default_token_type")]
    pub token_type: String,

    /// Absolute Unix timestamp (seconds) of access_token expiry.
    pub expires_at_unix: u64,

    /// Scopes the access_token was granted.
    #[serde(default)]
    pub scopes: Vec<String>,

    /// Operator-facing identity bits (email, account_id) shown by
    /// `routectl whoami`. Pulled from the token endpoint response or
    /// from id_token claims (best-effort -- never trusted for auth,
    /// purely for display).
    #[serde(default)]
    pub account: AccountInfo,

    /// Absolute Unix timestamp of when this record was written.
    /// `routectl whoami` shows it. Diagnostic only.
    pub obtained_at_unix: u64,

    /// Stable per-credential session id (UUIDv4) used in the
    /// `session-id` HTTP header on outbound openai-responses requests
    /// when the bearer is a chatgpt-oauth JWT. Mirrors codex CLI's
    /// `ModelClientState::session_id` -- routectl stamps one value per
    /// credential lifetime so the upstream upstream can correlate a
    /// human's turns under one stable session.
    ///
    /// Generated lazily on first use (factory-driven backfill) for
    /// records minted by routectl < v0.7.1 that pre-date this field;
    /// regenerated only on a fresh `routectl login codex`. Optional in
    /// the on-disk shape so older binaries continue to parse credentials
    /// minted by newer ones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

fn default_token_type() -> String {
    "Bearer".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct AccountInfo {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
}

impl TokenRecord {
    /// True if `expires_at_unix` is within `lead_secs` of `now_unix`,
    /// or already in the past. Saturates at zero on past-expiry so a
    /// clock that jumps far backward does not produce a weird "valid"
    /// signal.
    pub fn near_expiry(&self, lead_secs: u64, now_unix: u64) -> bool {
        self.expires_at_unix.saturating_sub(now_unix) < lead_secs
    }
}

/// On-disk schema for credentials.json. One file holds all providers
/// under a `providers` map; atomic-rename of one file is simpler than
/// coordinating multiple, and `routectl whoami` reads everything in
/// one stat+parse.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CredentialsFile {
    pub schema_version: u32,
    #[serde(default)]
    pub providers: BTreeMap<String, TokenRecord>,
}

impl Default for CredentialsFile {
    fn default() -> Self {
        Self::empty()
    }
}

impl CredentialsFile {
    pub fn empty() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            providers: BTreeMap::new(),
        }
    }

    pub fn get(&self, provider: &str) -> Option<&TokenRecord> {
        self.providers.get(provider)
    }

    pub fn upsert(&mut self, provider: &str, rec: TokenRecord) {
        self.providers.insert(provider.into(), rec);
    }

    pub fn remove(&mut self, provider: &str) -> Option<TokenRecord> {
        self.providers.remove(provider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec_at(expires_at: u64) -> TokenRecord {
        TokenRecord {
            access_token: SecretToken::new("tok"),
            refresh_token: SecretToken::new("rtok"),
            token_type: "Bearer".into(),
            expires_at_unix: expires_at,
            scopes: vec!["user:inference".into()],
            account: AccountInfo::default(),
            obtained_at_unix: 0,
            session_id: None,
        }
    }

    #[test]
    fn near_expiry_true_within_lead() {
        let rec = rec_at(100);
        assert!(rec.near_expiry(60, 50)); // 50s left, 60s lead -> near
    }

    #[test]
    fn near_expiry_false_outside_lead() {
        let rec = rec_at(1000);
        assert!(!rec.near_expiry(60, 50)); // 950s left, 60s lead -> not near
    }

    #[test]
    fn near_expiry_true_when_already_expired() {
        let rec = rec_at(50);
        assert!(rec.near_expiry(60, 100)); // expired -> "near" (saturates to 0)
    }

    #[test]
    fn upsert_and_remove_round_trip() {
        let mut cf = CredentialsFile::empty();
        assert!(cf.get("anthropic").is_none());
        cf.upsert("anthropic", rec_at(1000));
        assert_eq!(cf.get("anthropic").unwrap().expires_at_unix, 1000);
        let removed = cf.remove("anthropic").unwrap();
        assert_eq!(removed.expires_at_unix, 1000);
        assert!(cf.get("anthropic").is_none());
    }

    #[test]
    fn json_round_trip_preserves_all_fields() {
        let mut cf = CredentialsFile::empty();
        cf.upsert(
            "anthropic",
            TokenRecord {
                access_token: SecretToken::new("sk-ant-oat01-XYZ"),
                refresh_token: SecretToken::new("rtok-ABC"),
                token_type: "Bearer".into(),
                expires_at_unix: 1_900_000_000,
                scopes: vec!["user:profile".into(), "user:inference".into()],
                account: AccountInfo {
                    email: Some("u@example.com".into()),
                    account_id: Some("acc-123".into()),
                },
                obtained_at_unix: 1_899_000_000,
                session_id: None,
            },
        );
        let json = serde_json::to_string(&cf).unwrap();
        let parsed: CredentialsFile = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.schema_version, SCHEMA_VERSION);
        assert_eq!(parsed.providers.len(), 1);
        let p = parsed.get("anthropic").unwrap();
        assert_eq!(p.access_token.expose(), "sk-ant-oat01-XYZ");
        assert_eq!(p.scopes.len(), 2);
        assert_eq!(p.account.email.as_deref(), Some("u@example.com"));
    }

    #[test]
    fn unknown_extra_fields_are_tolerated_at_record_level() {
        // Future schema additions (e.g. `id_token`) must round-trip
        // through older binaries without rejecting the file outright.
        // Serde's default is to drop unknown fields silently when
        // deserializing into structs without `deny_unknown_fields`.
        let json = r#"{
            "schema_version": 1,
            "providers": {
                "anthropic": {
                    "access_token": "tok",
                    "refresh_token": "rtok",
                    "expires_at_unix": 100,
                    "obtained_at_unix": 50,
                    "id_token": "future-field-routectl-does-not-know"
                }
            }
        }"#;
        let parsed: CredentialsFile = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.providers["anthropic"].access_token.expose(), "tok");
    }

    #[test]
    fn debug_redacts_secret_token() {
        let t = SecretToken::new("super-secret-token-value");
        let dbg = format!("{t:?}");
        assert!(!dbg.contains("super-secret"), "debug leaked: {dbg}");
        assert!(dbg.contains("REDACTED"), "expected REDACTED, got: {dbg}");
    }

    #[test]
    fn debug_token_record_redacts_tokens() {
        let rec = rec_at(123);
        let dbg = format!("{rec:?}");
        // The inner SecretToken's Debug must not leak the literal
        // value of either the access or refresh token.
        assert!(!dbg.contains("tok\""), "access token leaked in {dbg}");
        assert!(!dbg.contains("rtok\""), "refresh token leaked in {dbg}");
        assert!(dbg.contains("REDACTED"));
    }
}
