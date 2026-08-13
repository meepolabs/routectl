//! The ready-to-paste `[providers.<name>]` block `routectl login`
//! prints on success.
//!
//! Minting a credential does not make it reachable: the token lands in
//! the managed store and nothing in `config.toml` consumes it until an
//! operator writes a provider entry by hand. This module renders that
//! entry so the required shape is discoverable from the login output
//! alone. It MUTATES NOTHING -- the caller prints the string.
//!
//! The rendered block carries no credential material: every value is
//! either a static token or a secret REFERENCE (`oauth://<id>[#label]`),
//! built through `SecretRef`'s own `Display` so the labelled form cannot
//! drift from the parser that reads it back.

use routectl_auth::SecretRef;
use routectl_router::provider_kind_for_oauth_id;

/// The auth-selector field and endpoint an `oauth://<id>` credential must
/// be consumed with, keyed by login-provider id.
///
/// This table MUST track the factory's requirements
/// (`routectl-router/src/factory/build.rs` and `factory/validate.rs`) --
/// a provider entry that omits one of these fields is rejected at
/// provider-build time, or worse, authenticates on the wrong surface.
/// It deliberately owns only what the router carries nowhere as data:
/// the `kind` comes from `provider_kind_for_oauth_id`, the single map the
/// activation path already reads, rather than being restated here.
///
/// Per-provider grounding:
/// - `anthropic` (anthropic-api): `auth_kind = "oauth-bearer"` selects
///   the `Authorization: Bearer` surface for a subscription access
///   token; the default `api-key` would send `x-api-key` and 401.
///   `base_url` defaults to the Anthropic origin.
/// - `codex` (openai-responses): `auth_kind = "chatgpt-oauth"` is also
///   the serde default, and an `oauth://` bearer lets `account_id_ref`
///   be derived from the session, so the field is emitted for clarity
///   rather than necessity. `base_url` is picked per auth kind by the
///   factory when unset.
/// - `xai` (openai-compat): that variant carries NO auth-selector field
///   at all (the entry is `deny_unknown_fields`, so adding one fails the
///   parse), and its `base_url` is REQUIRED non-empty by validation.
///   The endpoint value is the one field here with no code-side
///   constant -- it comes from the xAI section of `docs/CONFIGURATION.md`.
/// - `antigravity` (gemini): `auth_mode = "cloud-code"` selects the
///   Cloud Code surface, which additionally REQUIRES the `api_key_ref`
///   be an `oauth://` reference. `base_url` is left unset so the
///   cloud-code default applies -- pinning the public api-key endpoint
///   here would point the bearer at the wrong host.
fn auth_shape_for_oauth_id(oauth_id: &str) -> Option<AuthShape> {
    match oauth_id {
        "anthropic" => Some(AuthShape {
            auth_field: Some(("auth_kind", "oauth-bearer")),
            base_url: None,
        }),
        "codex" => Some(AuthShape {
            auth_field: Some(("auth_kind", "chatgpt-oauth")),
            base_url: None,
        }),
        "xai" => Some(AuthShape {
            auth_field: None,
            base_url: Some("https://api.x.ai/v1"),
        }),
        "antigravity" => Some(AuthShape {
            auth_field: Some(("auth_mode", "cloud-code")),
            base_url: None,
        }),
        _ => None,
    }
}

/// The provider-shape facts [`auth_shape_for_oauth_id`] owns: which
/// auth-selector key/value the entry needs (if any), and whether
/// `base_url` must be written out (`None` = leave it at its default).
struct AuthShape {
    auth_field: Option<(&'static str, &'static str)>,
    base_url: Option<&'static str>,
}

/// A rendered `[providers.<name>]` entry for one logged-in seat.
pub struct ProviderBlock {
    name: String,
    kind: &'static str,
    auth_field: Option<(&'static str, &'static str)>,
    base_url: Option<&'static str>,
    api_key_ref: String,
}

/// Build the provider entry that consumes the seat `routectl login
/// <oauth_id> [--label <label>]` just minted, or `None` for an id with no
/// known provider shape (unreachable through the CLI, whose accepted set
/// is the login registry itself).
///
/// The `api_key_ref` carries the `#<label>` fragment exactly when a label
/// was passed. Emitting a bare ref for a labelled login would silently
/// point the entry at the DEFAULT seat -- a wrong-credential failure that
/// presents as a config typo -- and inventing a fragment for an
/// unlabelled login would reference a seat that does not exist.
#[must_use]
pub fn provider_block(oauth_id: &str, label: Option<&str>) -> Option<ProviderBlock> {
    let kind = provider_kind_for_oauth_id(oauth_id)?;
    let shape = auth_shape_for_oauth_id(oauth_id)?;
    let api_key_ref = SecretRef::OAuth {
        provider: oauth_id.to_string(),
        label: label.map(str::to_string),
    }
    .to_string();
    Some(ProviderBlock {
        name: block_name(oauth_id, label),
        kind,
        auth_field: shape.auth_field,
        base_url: shape.base_url,
        api_key_ref,
    })
}

/// Suggested provider name for the entry. A labelled seat gets a
/// label-suffixed name so pasting the blocks for two seats of the same
/// upstream does not collide on one `[providers.<name>]` key.
fn block_name(oauth_id: &str, label: Option<&str>) -> String {
    match label {
        Some(l) => format!("{oauth_id}-{l}"),
        None => oauth_id.to_string(),
    }
}

impl ProviderBlock {
    /// Render the entry as pasteable TOML: a table header plus one
    /// `key = "value"` line per field, `=` aligned.
    #[must_use]
    pub fn render(&self) -> String {
        let rows = self.rows();
        let width = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
        let mut out = format!("[providers.{}]\n", toml_key(&self.name));
        for (key, value) in rows {
            out.push_str(&format!("{key:<width$} = {}\n", toml_string(&value)));
        }
        out
    }

    /// Fields in emission order. `kind` first (it is the tag the parser
    /// dispatches on), then the auth selector, then the endpoint, then
    /// the credential reference.
    fn rows(&self) -> Vec<(&'static str, String)> {
        let mut rows = vec![("kind", self.kind.to_string())];
        if let Some((key, value)) = self.auth_field {
            rows.push((key, value.to_string()));
        }
        if let Some(url) = self.base_url {
            rows.push(("base_url", url.to_string()));
        }
        rows.push(("api_key_ref", self.api_key_ref.clone()));
        rows
    }
}

/// Render `name` as a TOML table key: bare when it is made only of
/// characters a bare key permits, quoted otherwise. An operator label is
/// only checked for non-emptiness upstream, so it can carry a space or a
/// dot that would split the key path if written bare.
fn toml_key(name: &str) -> String {
    let is_bare = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if is_bare {
        name.to_string()
    } else {
        toml_string(name)
    }
}

/// Render `value` as a TOML basic string, escaping the two characters
/// that would otherwise terminate or reinterpret it.
fn toml_string(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::{provider_block, toml_key};

    fn rendered(oauth_id: &str, label: Option<&str>) -> String {
        provider_block(oauth_id, label)
            .expect("known login id must have a provider block")
            .render()
    }

    #[test]
    fn every_login_provider_has_a_printable_block() {
        // Arrange: the login registry is the accepted set of `routectl
        // login <provider>`, so an id without a block would leave that
        // login with no success output.
        let ids = routectl_auth::oauth::known_provider_ids();

        // Act + Assert
        for id in ids {
            assert!(
                provider_block(id, None).is_some(),
                "login id `{id}` has no provider block"
            );
        }
    }

    #[test]
    fn antigravity_block_carries_the_gemini_kind_cloud_code_mode_and_oauth_ref() {
        // Arrange + Act
        let block = rendered("antigravity", None);

        // Assert: all three non-obvious fields, and the kind is `gemini`
        // -- NOT the login id.
        assert!(block.contains(r#"kind        = "gemini""#), "{block}");
        assert!(block.contains(r#"auth_mode   = "cloud-code""#), "{block}");
        assert!(
            block.contains(r#"api_key_ref = "oauth://antigravity""#),
            "{block}"
        );
        assert!(!block.contains(r#""antigravity""#), "kind leaked: {block}");
    }

    #[test]
    fn antigravity_block_leaves_base_url_unset() {
        // The cloud-code surface derives its own endpoint; a written
        // base_url would pin the bearer to the api-key host.
        let block = rendered("antigravity", None);

        assert!(!block.contains("base_url"), "{block}");
    }

    #[test]
    fn labelled_login_reference_carries_the_label_fragment() {
        // Arrange + Act
        let block = rendered("anthropic", Some("seat-b"));

        // Assert: without the fragment the entry silently consumes the
        // default seat instead of the one just minted.
        assert!(
            block.contains(r#"api_key_ref = "oauth://anthropic#seat-b""#),
            "{block}"
        );
    }

    #[test]
    fn labelled_login_suggests_a_label_scoped_provider_name() {
        let block = rendered("anthropic", Some("seat-b"));

        assert!(
            block.starts_with("[providers.anthropic-seat-b]\n"),
            "{block}"
        );
    }

    #[test]
    fn unlabelled_login_invents_no_fragment() {
        // Arrange + Act
        let block = rendered("anthropic", None);

        // Assert: a `#` anywhere in the ref would name a seat that was
        // never created.
        assert!(
            block.contains(r#"api_key_ref = "oauth://anthropic""#),
            "{block}"
        );
        assert!(!block.contains('#'), "invented a fragment: {block}");
    }

    #[test]
    fn anthropic_block_selects_the_oauth_bearer_surface() {
        let block = rendered("anthropic", None);

        assert!(
            block.contains(r#"kind        = "anthropic-api""#),
            "{block}"
        );
        assert!(block.contains(r#"auth_kind   = "oauth-bearer""#), "{block}");
    }

    #[test]
    fn codex_block_selects_the_chatgpt_oauth_surface() {
        let block = rendered("codex", None);

        assert!(
            block.contains(r#"kind        = "openai-responses""#),
            "{block}"
        );
        assert!(
            block.contains(r#"auth_kind   = "chatgpt-oauth""#),
            "{block}"
        );
        assert!(
            block.contains(r#"api_key_ref = "oauth://codex""#),
            "{block}"
        );
    }

    #[test]
    fn xai_block_writes_the_required_endpoint_and_no_auth_selector() {
        // openai-compat validation rejects an empty base_url, and the
        // variant has no auth-selector field to write.
        let block = rendered("xai", None);

        assert!(
            block.contains(r#"kind        = "openai-compat""#),
            "{block}"
        );
        assert!(
            block.contains(r#"base_url    = "https://api.x.ai/v1""#),
            "{block}"
        );
        assert!(!block.contains("auth_kind"), "{block}");
        assert!(!block.contains("auth_mode"), "{block}");
    }

    #[test]
    fn no_block_carries_credential_material() {
        // Every emitted value must be a static token or a secret
        // REFERENCE. Scan for the token prefixes the login flows mint
        // plus the generic bearer/secret words.
        let needles = [
            "sk-ant-",
            "sk-",
            "ya29.",
            "eyJ",
            "access_token",
            "refresh_token",
            "Bearer ",
        ];
        for id in routectl_auth::oauth::known_provider_ids() {
            for label in [None, Some("seat-b")] {
                let block = rendered(id, label);
                for needle in needles {
                    assert!(
                        !block.contains(needle),
                        "`{needle}` in block for `{id}`: {block}"
                    );
                }
            }
        }
    }

    #[test]
    fn an_unknown_login_id_yields_no_block() {
        assert!(provider_block("not-a-provider", None).is_none());
    }

    #[test]
    fn every_emitted_block_parses_as_a_provider_entry_of_the_stated_kind() {
        // The entries are `deny_unknown_fields`, so an auth-selector key
        // written on a variant that has none fails the parse here rather
        // than at the operator's next startup.
        for id in routectl_auth::oauth::known_provider_ids() {
            for label in [None, Some("seat-b")] {
                let block = provider_block(id, label).expect("block");
                let name = block.name.clone();
                let expected_kind = block.kind;

                let cfg: routectl_router::Config = toml::from_str(&block.render())
                    .unwrap_or_else(|e| panic!("block for `{id}` must parse: {e}"));

                let entry = cfg
                    .providers
                    .get(&name)
                    .unwrap_or_else(|| panic!("block for `{id}` must define `{name}`"));
                assert_eq!(entry.kind_str(), expected_kind, "id `{id}`");
            }
        }
    }

    #[test]
    fn the_antigravity_block_parses_into_a_cloud_code_gemini_entry() {
        // Arrange + Act: the worst case -- the kind, the auth mode, and
        // the oauth ref are each a separate startup rejection when wrong.
        let cfg: routectl_router::Config =
            toml::from_str(&rendered("antigravity", None)).expect("parse antigravity block");

        // Assert
        let entry = cfg.providers.get("antigravity").expect("entry");
        assert_eq!(entry.kind_str(), "gemini");
        assert_eq!(entry.api_key_ref(), Some("oauth://antigravity"));
        assert!(
            format!("{entry:?}").contains("CloudCode"),
            "auth_mode must be cloud-code: {entry:?}"
        );
    }

    #[test]
    fn a_label_with_toml_punctuation_is_quoted_in_the_table_key() {
        // A dot written bare would split the key path into a nested
        // table; a space would not parse at all.
        assert_eq!(toml_key("plain-1_x"), "plain-1_x");
        assert_eq!(toml_key("seat b"), r#""seat b""#);
        assert_eq!(toml_key("a.b"), r#""a.b""#);
        assert_eq!(toml_key(r#"a"b"#), r#""a\"b""#);

        let block = rendered("anthropic", Some("seat b"));
        assert!(
            block.starts_with(r#"[providers."anthropic-seat b"]"#),
            "{block}"
        );
    }
}
