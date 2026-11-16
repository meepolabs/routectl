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
use routectl_router::seat_naming::account_entry_name;
use routectl_router::{Auth, Format};
use toml_edit::Table;

/// The `format`, `auth` and `base_url` an `oauth://<id>` credential must be
/// consumed with, keyed by login-provider id.
///
/// Each row is the vendor's own lane as the router's provider descriptor
/// states it: config validation refuses a managed login on any other
/// format, auth or host, so a row that drifts from the descriptor yields a
/// block that fails at the operator's next startup.
///
/// - `anthropic`: `anthropic-messages` over `https://api.anthropic.com`,
///   sent as `bearer` (a subscription token on `x-api-key` would 401).
/// - `codex`: `openai-responses` over the ChatGPT Codex base, `bearer`;
///   `account_id_ref` is derived from the login, so it is not written.
/// - `xai`: `openai-chat` over `https://api.x.ai/v1`, `bearer`.
/// - `antigravity`: `gemini-cloud-code` over the daily Cloud Code base,
///   `bearer`.
fn auth_shape_for_oauth_id(oauth_id: &str) -> Option<AuthShape> {
    let (format, base_url) = match oauth_id {
        "anthropic" => (Format::AnthropicMessages, "https://api.anthropic.com"),
        "codex" => (
            Format::OpenaiResponses,
            "https://chatgpt.com/backend-api/codex",
        ),
        "xai" => (Format::OpenaiChat, "https://api.x.ai/v1"),
        "antigravity" => (
            Format::GeminiCloudCode,
            "https://daily-cloudcode-pa.googleapis.com",
        ),
        _ => return None,
    };
    Some(AuthShape {
        format,
        auth: Auth::Bearer,
        base_url,
    })
}

/// The provider-shape facts [`auth_shape_for_oauth_id`] owns.
struct AuthShape {
    format: Format,
    auth: Auth,
    base_url: &'static str,
}

/// A rendered `[providers.<name>]` entry for one logged-in seat.
///
/// `Debug` is safe here: every field is a static token, a name, or a
/// secret REFERENCE -- the block never holds credential material.
#[derive(Debug)]
pub struct ProviderBlock {
    name: String,
    format: Format,
    auth: Auth,
    base_url: &'static str,
    credential: String,
}

/// Build the provider entry that consumes the seat `routectl login
/// <oauth_id> [--label <label>]` just minted, or `None` for an id with no
/// known provider shape (unreachable through the CLI, whose accepted set
/// is the login registry itself).
///
/// The `credential` carries the `#<label>` fragment exactly when a label
/// was passed. Emitting a bare ref for a labelled login would silently
/// point the entry at the DEFAULT seat -- a wrong-credential failure that
/// presents as a config typo -- and inventing a fragment for an
/// unlabelled login would reference a seat that does not exist.
#[must_use]
pub fn provider_block(oauth_id: &str, label: Option<&str>) -> Option<ProviderBlock> {
    let shape = auth_shape_for_oauth_id(oauth_id)?;
    let credential = SecretRef::OAuth {
        provider: oauth_id.to_string(),
        label: label.map(str::to_string),
    }
    .to_string();
    Some(ProviderBlock {
        name: block_name(oauth_id, label),
        format: shape.format,
        auth: shape.auth,
        base_url: shape.base_url,
        credential,
    })
}

/// Suggested provider name for the entry, taken from the shared naming
/// convention so the name PRINTED here and the name a config write picks
/// are one string. Two names for one seat would make reconciliation by
/// ref disagree with reconciliation by name.
///
/// The convention refuses tokens it cannot render verbatim (an unusable
/// family or label, the reserved label `default`). Those cases still get a
/// printable suggestion -- the block is a hint an operator edits, and
/// printing nothing would leave a successful login with no output at all
/// -- so they fall back to the plain label-suffixed derivation, whose
/// TOML-key quoting [`toml_key`] already handles.
fn block_name(oauth_id: &str, label: Option<&str>) -> String {
    account_entry_name(oauth_id, label).unwrap_or_else(|_| match label {
        Some(l) => format!("{oauth_id}-{l}"),
        None => oauth_id.to_string(),
    })
}

/// The auth-shape fields a provider entry consuming `oauth://<oauth_id>`
/// MUST carry: its `format` and its `auth`.
///
/// Read by the login auto-surface to check an existing entry it matched by
/// ref for auth drift. Deliberately narrower than the full auth shape: an
/// operator's `base_url` override is legitimate configuration (validation
/// owns which hosts a managed login may reach), while a wrong `format` or
/// `auth` means the entry authenticates on the wrong surface.
pub struct RequiredAuthFields {
    /// The `format` the entry must carry.
    pub format: Format,
    /// The `auth` the entry must carry.
    pub auth: Auth,
}

/// The required auth-shape fields for `oauth_id`, or `None` for an id with
/// no known provider shape.
#[must_use]
pub fn required_auth_fields(oauth_id: &str) -> Option<RequiredAuthFields> {
    let shape = auth_shape_for_oauth_id(oauth_id)?;
    Some(RequiredAuthFields {
        format: shape.format,
        auth: shape.auth,
    })
}

impl ProviderBlock {
    /// Rename the entry this block writes and prints, keeping every auth
    /// field. Used when the naming authority for the write is the config
    /// being edited (a pool's existing member name) rather than the
    /// convention's fresh derivation.
    #[must_use]
    pub fn with_entry_name(mut self, name: String) -> Self {
        self.name = name;
        self
    }

    /// The entry as a `toml_edit` table, ready for
    /// `edit_pipeline::insert_provider_block`.
    ///
    /// Built from the SAME `rows` the printed block renders, so the
    /// written entry and the printed one can never carry different fields.
    /// No render-then-parse round trip: nothing here needs quoting or
    /// escaping decisions made twice.
    #[must_use]
    pub fn entry_table(&self) -> Table {
        let mut table = Table::new();
        table.set_implicit(false);
        for (key, value) in self.rows() {
            table.insert(key, toml_edit::value(value));
        }
        table
    }

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

    /// Fields in the entry's documented key order: `format`, `base_url`,
    /// `credential`, `auth`.
    fn rows(&self) -> Vec<(&'static str, String)> {
        vec![
            ("format", self.format.as_str().to_string()),
            ("base_url", self.base_url.to_string()),
            ("credential", self.credential.clone()),
            ("auth", self.auth.as_str().to_string()),
        ]
    }
}

/// Render `name` as a TOML table key: bare when it is made only of
/// characters a bare key permits, quoted otherwise. An operator label is
/// only checked for non-emptiness upstream, so it can carry a space or a
/// dot that would split the key path if written bare.
pub(crate) fn toml_key(name: &str) -> String {
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

/// Render `value` as a TOML basic string.
///
/// TOML forbids raw control characters inside a basic string outright, not
/// just the quote and backslash that would terminate or reinterpret it. A
/// member or label name carrying a newline is reachable -- pool member names
/// come from operator-written config, and only non-emptiness is checked
/// upstream -- and a raw one would make the PRINTED delta unparseable while
/// the committed file (written through `toml_edit`, which escapes properly)
/// stayed valid. An operator pasting the shown block would then hit a syntax
/// error routectl appeared to have authored.
///
/// The three whitespace controls get their short escapes; every other C0
/// control plus DEL takes the `\uXXXX` form.
pub(crate) fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::{block_name, provider_block, toml_key};

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
    fn antigravity_block_carries_the_cloud_code_format_bearer_auth_and_oauth_ref() {
        // Arrange + Act
        let block = rendered("antigravity", None);

        // Assert: all three non-obvious fields, and the format is
        // `gemini-cloud-code` -- NOT the login id.
        assert!(
            block.contains(r#"format     = "gemini-cloud-code""#),
            "{block}"
        );
        assert!(block.contains(r#"auth       = "bearer""#), "{block}");
        assert!(
            block.contains(r#"credential = "oauth://antigravity""#),
            "{block}"
        );
        assert!(
            !block.contains(r#""antigravity""#),
            "format leaked: {block}"
        );
    }

    #[test]
    fn antigravity_block_writes_the_daily_cloud_code_base() {
        // The carrier is an always-written identity key; the Cloud Code
        // surface is written as its daily base, never the api-key host.
        let block = rendered("antigravity", None);

        assert!(
            block.contains(r#"base_url   = "https://daily-cloudcode-pa.googleapis.com""#),
            "{block}"
        );
        assert!(!block.contains("generativelanguage"), "{block}");
    }

    #[test]
    fn labelled_login_reference_carries_the_label_fragment() {
        // Arrange + Act
        let block = rendered("anthropic", Some("seat-b"));

        // Assert: without the fragment the entry silently consumes the
        // default seat instead of the one just minted.
        assert!(
            block.contains(r#"credential = "oauth://anthropic#seat-b""#),
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
            block.contains(r#"credential = "oauth://anthropic""#),
            "{block}"
        );
        assert!(!block.contains('#'), "invented a fragment: {block}");
    }

    #[test]
    fn anthropic_block_selects_the_bearer_surface() {
        let block = rendered("anthropic", None);

        assert!(
            block.contains(r#"format     = "anthropic-messages""#),
            "{block}"
        );
        assert!(
            block.contains(r#"base_url   = "https://api.anthropic.com""#),
            "{block}"
        );
        assert!(block.contains(r#"auth       = "bearer""#), "{block}");
    }

    #[test]
    fn codex_block_selects_the_chatgpt_codex_surface() {
        let block = rendered("codex", None);

        assert!(
            block.contains(r#"format     = "openai-responses""#),
            "{block}"
        );
        assert!(
            block.contains(r#"base_url   = "https://chatgpt.com/backend-api/codex""#),
            "{block}"
        );
        assert!(block.contains(r#"auth       = "bearer""#), "{block}");
        assert!(block.contains(r#"credential = "oauth://codex""#), "{block}");
        assert!(!block.contains("account_id_ref"), "{block}");
    }

    #[test]
    fn xai_block_writes_the_required_endpoint_and_bearer_auth() {
        // The carrier is required on every entry; openai-chat takes only
        // the bearer form.
        let block = rendered("xai", None);

        assert!(block.contains(r#"format     = "openai-chat""#), "{block}");
        assert!(
            block.contains(r#"base_url   = "https://api.x.ai/v1""#),
            "{block}"
        );
        assert!(block.contains(r#"auth       = "bearer""#), "{block}");
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
    fn every_emitted_block_parses_as_a_provider_entry_of_the_stated_format() {
        // The entry is `deny_unknown_fields`, so a key the flat entry does
        // not take fails the parse here rather than at the operator's next
        // startup.
        for id in routectl_auth::oauth::known_provider_ids() {
            for label in [None, Some("seat-b")] {
                let block = provider_block(id, label).expect("block");
                let name = block.name.clone();
                let expected_format = block.format;
                let expected_auth = block.auth;

                let cfg: routectl_router::Config = toml::from_str(&block.render())
                    .unwrap_or_else(|e| panic!("block for `{id}` must parse: {e}"));

                let entry = cfg
                    .providers
                    .get(&name)
                    .unwrap_or_else(|| panic!("block for `{id}` must define `{name}`"));
                assert_eq!(entry.format(), expected_format, "id `{id}`");
                assert_eq!(entry.auth, expected_auth, "id `{id}`");
            }
        }
    }

    #[test]
    fn the_antigravity_block_parses_into_a_cloud_code_gemini_entry() {
        // Arrange + Act: the worst case -- the format, the auth, and the
        // oauth ref are each a separate startup rejection when wrong.
        let cfg: routectl_router::Config =
            toml::from_str(&rendered("antigravity", None)).expect("parse antigravity block");

        // Assert
        let entry = cfg.providers.get("antigravity-default").expect("entry");
        assert_eq!(entry.kind_str(), "gemini");
        assert_eq!(entry.api_key_ref(), Some("oauth://antigravity"));
        assert_eq!(entry.format(), routectl_router::Format::GeminiCloudCode);
        assert_eq!(entry.auth, routectl_router::Auth::Bearer);
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

    /// The printed name and the name a config write picks must be ONE
    /// string: two names for one seat make reconciliation by ref disagree
    /// with reconciliation by name, which is what mints duplicate entries.
    #[test]
    fn the_block_name_is_the_naming_convention_for_every_login_id() {
        for id in routectl_auth::oauth::known_provider_ids() {
            for label in [None, Some("work")] {
                let expected = routectl_router::seat_naming::account_entry_name(id, label)
                    .expect("a login id and a usable label derive a name");
                assert_eq!(block_name(id, label), expected, "id `{id}` label {label:?}");
            }
        }
    }

    /// An unlabelled login prints the convention's `<family>-default`, not
    /// the bare family name -- the bare name is the POOL's, and a provider
    /// entry holding it makes the pool unnameable.
    #[test]
    fn an_unlabelled_login_takes_the_default_suffix_not_the_bare_family_name() {
        let block = rendered("anthropic", None);

        assert!(
            block.starts_with("[providers.anthropic-default]\n"),
            "{block}"
        );
    }

    /// The convention refuses tokens it cannot render verbatim; the block
    /// is still printable, because a successful login with no output at
    /// all is worse than a name the operator edits.
    #[test]
    fn a_name_the_convention_refuses_falls_back_to_a_printable_derivation() {
        // Arrange: `default` is reserved, and a space is unusable.
        assert!(
            routectl_router::seat_naming::account_entry_name("anthropic", Some("default")).is_err()
        );

        // Act / Assert
        assert_eq!(
            block_name("anthropic", Some("default")),
            "anthropic-default"
        );
        assert_eq!(block_name("anthropic", Some("seat b")), "anthropic-seat b");
    }

    /// The WRITTEN entry and the PRINTED entry come from one row set, so a
    /// field added to the auth table cannot reach one and miss the other.
    #[test]
    fn the_entry_table_carries_exactly_the_fields_the_rendered_block_prints() {
        for id in routectl_auth::oauth::known_provider_ids() {
            let block = provider_block(id, Some("work")).expect("block");

            let table = block.entry_table();
            let printed: routectl_router::Config =
                toml::from_str(&block.render()).expect("rendered block parses");
            let written: routectl_router::Config =
                toml::from_str(&format!("[providers.x]\n{table}")).expect("entry table parses");

            let printed_entry = printed.providers.get(&block.name).expect("printed entry");
            let written_entry = written.providers.get("x").expect("written entry");
            assert_eq!(
                format!("{printed_entry:?}"),
                format!("{written_entry:?}"),
                "id `{id}`"
            );
        }
    }

    /// A rename keeps every auth field: only the key the entry is written
    /// under changes.
    #[test]
    fn with_entry_name_renames_the_key_and_keeps_the_auth_fields() {
        // Arrange
        let original = provider_block("anthropic", None).expect("block");
        let original_rows = original.rows();

        // Act
        let renamed = provider_block("anthropic", None)
            .expect("block")
            .with_entry_name("claude-sub".into());

        // Assert
        assert_eq!(renamed.rows(), original_rows);
        assert!(
            renamed.render().starts_with("[providers.claude-sub]\n"),
            "{}",
            renamed.render()
        );
    }
}

#[cfg(test)]
mod printed_block_validation_tests {
    use super::provider_block;
    use routectl_router::{Config, collect_config_validation};

    /// Every block login prints must load: the startup validator suite
    /// (managed-credential containment included) accepts it as written.
    #[test]
    fn every_printed_login_block_passes_config_validation() {
        for id in routectl_auth::oauth::known_provider_ids() {
            for label in [None, Some("seat-b")] {
                let block = provider_block(id, label).expect("block");
                let cfg: Config = toml::from_str(&block.render()).expect("block parses");

                let validation = collect_config_validation(&cfg);

                assert!(
                    validation.errors.is_empty(),
                    "login block for `{id}` ({label:?}) is refused: {:?}",
                    validation.errors
                );
            }
        }
    }
}
