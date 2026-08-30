//! Front-proxy reachability, derived from the MITM host pin.
//!
//! The MITM front is Anthropic-only PERMANENTLY, and that is a
//! structural limit rather than a coverage gap: `validate_mitm_config`
//! rejects any `upstream_origin` other than exactly
//! `https://<pinned host>` and any `mitm_host` other than the pinned
//! host, because a forwarded full-scope claude.ai token must never reach
//! a non-Anthropic egress. So every non-Anthropic front-proxy cell is
//! unreachable BY CONSTRUCTION and must be reported naming the pin,
//! never as a missing credential.
//!
//! The answer is DERIVED from the pin rather than from a hand-listed
//! exclusion set: widening or narrowing the pin moves the reachable set
//! automatically. The pin's constant is private to `routectl-router`, so
//! it is read out of that crate's source by declaration -- the same
//! cross-language weld `lane::resolve_site_symbol` and
//! `tests/scrub_gate_provider_shapes.rs` use.
//!
//! Reading is FAIL-CLOSED in every direction. An unreadable source, an
//! absent constant, two competing declarations, a pinned host outside
//! the settled constraint, or a validator that no longer exists are all
//! errors rather than a default answer: a derivation that silently
//! defaulted would report a permanent structural limit as a coverage
//! gap, which is exactly the confusion this module exists to prevent.
//!
//! The textual parse is deliberately narrow rather than syntactic. `syn`
//! reaches this workspace only as a transitive proc-macro dependency
//! without its `full` feature, so item-level parsing would mean a new
//! direct dependency for a one-line declaration. Instead the scan strips
//! comments and string literals before matching and accepts the
//! declaration only at brace depth 0 with `const` as the line's first
//! token -- so a copy of the declaration text inside a block comment, a
//! raw string, or a nested `mod`/`fn` body cannot satisfy it. The tests
//! carry a negative control for each of those three shapes.

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

use super::lane::{SymbolError, resolve_site_symbol, workspace_root};

/// Workspace-relative source that declares the pin and the validator
/// enforcing it.
pub const MITM_PIN_SITE_PATH: &str = "crates/routectl-router/src/factory/validate.rs";

/// Name of the private constant carrying the pinned host.
pub const MITM_PIN_CONST: &str = "MITM_REQUIRED_HOST";

/// The validator whose rejection makes the limit structural. Named in
/// every unreachable reason so a reader can go read the rejection.
pub const MITM_VALIDATOR_SYMBOL: &str = "validate_mitm_config";

/// The host the pin is settled at, and the ingress reachable under it.
///
/// This pair is the SETTLED CONSTRAINT, not a heuristic: the front is
/// permanently Anthropic-only, so the mapping is stated once and any
/// other parsed pin is a loud [`PinError::PinHostUnexpected`] rather
/// than a guess derived from the new host's shape.
pub const SETTLED_PIN: (&str, &str) = ("api.anthropic.com", "anthropic");

/// Why the pin could not be derived from source. Every variant is a hard
/// failure: none of them has a safe default, because the safe-looking
/// default ("nothing is pinned") is the one answer that misreports a
/// structural limit as a gap.
#[derive(Debug, Error)]
pub enum PinError {
    /// The workspace root could not be located.
    #[error("cannot locate the workspace to read the MITM host pin: {0}")]
    WorkspaceUnavailable(#[source] SymbolError),
    /// The declaring source could not be read.
    #[error("cannot read {path} to derive the MITM host pin: {source}")]
    SourceUnreadable {
        /// The path that was read.
        path: String,
        /// The underlying io failure.
        #[source]
        source: std::io::Error,
    },
    /// The source carries no such constant. An empty parse result is a
    /// FAILED parse, never an empty pin.
    #[error(
        "{path} declares no `const {constant}`; the pin was renamed, moved, or \
         removed -- re-derive this weld rather than defaulting, because a \
         default would report a structural limit as a coverage gap"
    )]
    PinNotDeclared {
        /// The path that was searched.
        path: String,
        /// The constant that was looked for.
        constant: String,
    },
    /// The source declares the constant more than once, so no single
    /// declaration is the pin.
    #[error("{path} declares `const {constant}` {count} times; no single declaration is the pin")]
    PinAmbiguous {
        /// The path that was searched.
        path: String,
        /// The constant that was looked for.
        constant: String,
        /// How many declarations were found.
        count: usize,
    },
    /// The declaration parsed to an empty host.
    #[error("{path} declares `const {constant}` as an empty string, which pins nothing")]
    PinEmpty {
        /// The path that was searched.
        path: String,
        /// The constant that was looked for.
        constant: String,
    },
    /// The parsed pin is not the host this derivation was settled
    /// against. Deliberately an ERROR and not an all-unreachable answer:
    /// the pin being permanently `api.anthropic.com` is a settled
    /// constraint, so a different value means the constraint moved and
    /// the reachable set must be re-settled by a human -- guessing an
    /// ingress from the host's shape would quietly invent policy.
    #[error(
        "pinned host `{host}` is not the settled `{expected}`; the MITM host \
         constraint moved, so the front-proxy reachable set must be re-settled \
         rather than inferred from the new host"
    )]
    PinHostUnexpected {
        /// The host as declared.
        host: String,
        /// The host this derivation was settled against.
        expected: String,
    },
    /// The validator enforcing the pin is gone, so the reason string
    /// would cite a symbol that no longer exists.
    #[error("cannot cite `{symbol}` as the enforcing validator: {source}")]
    ValidatorUnresolved {
        /// The validator that was cited.
        symbol: String,
        /// Why the citation did not resolve.
        #[source]
        source: SymbolError,
    },
}

/// Whether a front-proxy cell for one ingress can exist at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reachability {
    /// The ingress speaks the pinned host's vendor dialect, so a
    /// front-proxy cell for it is reachable.
    Reachable,
    /// No front-proxy cell for this ingress can ever exist. The reason
    /// names the enforcing validator and the pinned host, so a reader
    /// never mistakes it for a missing credential.
    Unreachable {
        /// Human-readable structural reason.
        reason: String,
    },
}

impl Reachability {
    /// Whether this is [`Reachability::Reachable`].
    pub const fn is_reachable(&self) -> bool {
        matches!(self, Self::Reachable)
    }

    /// The structural reason, or `None` when reachable.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Reachable => None,
            Self::Unreachable { reason } => Some(reason),
        }
    }
}

/// Absolute path of the source declaring the pin.
pub fn mitm_pin_site_path() -> Result<PathBuf, PinError> {
    let root = workspace_root().map_err(PinError::WorkspaceUnavailable)?;
    Ok(root.join(MITM_PIN_SITE_PATH))
}

/// The pinned MITM host, read out of the committed router source.
///
/// Also resolves [`MITM_VALIDATOR_SYMBOL`] in the same file: the reason
/// strings cite that validator, and citing a symbol that has since been
/// renamed would send a reader to nothing.
pub fn mitm_pinned_host() -> Result<String, PinError> {
    let path = mitm_pin_site_path()?;
    let host = mitm_pinned_host_at(&path)?;
    resolve_site_symbol(MITM_VALIDATOR_SYMBOL, MITM_PIN_SITE_PATH).map_err(|source| {
        PinError::ValidatorUnresolved {
            symbol: MITM_VALIDATOR_SYMBOL.to_string(),
            source,
        }
    })?;
    Ok(host)
}

/// The pinned MITM host, read out of the source at an explicit path.
pub fn mitm_pinned_host_at(path: &Path) -> Result<String, PinError> {
    let text = fs::read_to_string(path).map_err(|source| PinError::SourceUnreadable {
        path: path.display().to_string(),
        source,
    })?;
    parse_mitm_pinned_host(&text, &path.display().to_string())
}

/// Parse the pinned host out of Rust source: exactly one
/// `const <MITM_PIN_CONST>: &str = "<host>";` ITEM.
///
/// Non-code text is removed before matching and the declaration is
/// accepted only at brace depth 0 with `const` as the line's first
/// token, so a copy of the declaration inside a block comment, inside a
/// raw string, or inside a nested `mod`/`fn` body is not a declaration.
/// Two accepted matches stay a hard [`PinError::PinAmbiguous`].
pub fn parse_mitm_pinned_host(text: &str, path: &str) -> Result<String, PinError> {
    let hosts = top_level_pin_declarations(text);
    match hosts.len() {
        1 => {
            let host = hosts.into_iter().next().unwrap_or_default();
            if host.is_empty() {
                return Err(PinError::PinEmpty {
                    path: path.to_string(),
                    constant: MITM_PIN_CONST.to_string(),
                });
            }
            Ok(host)
        }
        0 => Err(PinError::PinNotDeclared {
            path: path.to_string(),
            constant: MITM_PIN_CONST.to_string(),
        }),
        count => Err(PinError::PinAmbiguous {
            path: path.to_string(),
            constant: MITM_PIN_CONST.to_string(),
            count,
        }),
    }
}

/// Every host literal declared by a top-level `const <MITM_PIN_CONST>`
/// item. Walks the source once, tracking comment / string state and
/// brace depth so only real top-level items are considered.
fn top_level_pin_declarations(text: &str) -> Vec<String> {
    let mut hosts = Vec::new();
    let mut depth: i32 = 0;
    for line in strip_non_code(text).lines() {
        let opens = line.matches('{').count() as i32;
        let closes = line.matches('}').count() as i32;
        if let Some(host) = declared_host(line).filter(|_| depth == 0) {
            hosts.push(host);
        }
        depth = (depth + opens - closes).max(0);
    }
    hosts
}

/// The source with block comments and line comments removed and string
/// literals reduced to a brace-free, newline-free single-line form,
/// newlines otherwise preserved so line structure survives.
///
/// Neutralizing literal bodies is what makes the depth walk trustworthy:
/// a brace, a newline, or a comment opener inside a string would
/// otherwise desync it. A host literal survives intact because a host
/// contains none of the neutralized characters.
fn strip_non_code(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut block_depth: u32 = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if block_depth > 0 {
            if bytes[i..].starts_with(b"/*") {
                block_depth += 1;
                i += 2;
                continue;
            }
            if bytes[i..].starts_with(b"*/") {
                block_depth -= 1;
                i += 2;
                continue;
            }
            if c == '\n' {
                out.push('\n');
            }
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"/*") {
            block_depth = 1;
            i += 2;
            continue;
        }
        if bytes[i..].starts_with(b"//") {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if let Some((literal, next)) = read_string_literal(text, i) {
            out.push('"');
            out.extend(literal.chars().filter(|ch| !NEUTRALIZED.contains(ch)));
            out.push('"');
            i = next;
            continue;
        }
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// Characters dropped from a string-literal body so the body can never
/// desync the brace walk or forge a second line. A host contains none of
/// them, so the pin's own literal survives this filter unchanged.
const NEUTRALIZED: &[char] = &['{', '}', '\n', '\r', '"', '/', '*'];

/// Read a (raw or plain) string literal starting at `at`, returning its
/// BODY and the byte index just past the closing quote.
///
/// Raw strings are read by hash count so an embedded `"` or `\` cannot
/// end them early; plain strings honour backslash escapes.
fn read_string_literal(text: &str, at: usize) -> Option<(String, usize)> {
    let bytes = text.as_bytes();
    let mut i = at;
    if bytes[i] == b'r' {
        let mut hashes = 0;
        let mut j = i + 1;
        while j < bytes.len() && bytes[j] == b'#' {
            hashes += 1;
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b'"' {
            let terminator = format!("\"{}", "#".repeat(hashes));
            let body_start = j + 1;
            let end = text[body_start..].find(&terminator)? + body_start;
            return Some((text[body_start..end].to_string(), end + terminator.len()));
        }
        return None;
    }
    if bytes[i] != b'"' {
        return None;
    }
    i += 1;
    let body_start = i;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some((text[body_start..i].to_string(), i + 1)),
            _ => i += 1,
        }
    }
    None
}

/// The string literal a single top-level code line declares for the pin,
/// if the line IS that declaration. `const` must be the line's first
/// token: `pub` is tolerated ahead of it, nothing else is.
fn declared_host(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let rest = trimmed
        .strip_prefix("pub const ")
        .or_else(|| trimmed.strip_prefix("const "))?;
    let rest = rest.strip_prefix(MITM_PIN_CONST)?;
    let rest = rest.trim_start().strip_prefix(':')?;
    let (_, literal) = rest.split_once('=')?;
    let literal = literal.trim_start().strip_prefix('"')?;
    let (host, _) = literal.split_once('"')?;
    Some(host.to_string())
}

/// Whether a front-proxy cell for `ingress` is reachable, derived from
/// the committed pin.
pub fn front_proxy_reachability(ingress: &str) -> Result<Reachability, PinError> {
    let host = mitm_pinned_host()?;
    reachability_for_pin(ingress, &host)
}

/// Whether a front-proxy cell for `ingress` is reachable under an
/// explicitly supplied pin.
///
/// Keyed on the pin, and fail-closed on anything but the settled one:
/// under [`SETTLED_PIN`] exactly its ingress is reachable, and any other
/// pinned host is [`PinError::PinHostUnexpected`]. An unrecognised pin
/// must never resolve to "everything unreachable" -- that answer is
/// indistinguishable from a correct verdict while actually meaning the
/// constraint moved out from under this derivation.
pub fn reachability_for_pin(ingress: &str, pinned_host: &str) -> Result<Reachability, PinError> {
    let (expected_host, reachable_ingress) = SETTLED_PIN;
    if pinned_host != expected_host {
        return Err(PinError::PinHostUnexpected {
            host: pinned_host.to_string(),
            expected: expected_host.to_string(),
        });
    }
    if ingress == reachable_ingress {
        return Ok(Reachability::Reachable);
    }
    Ok(Reachability::Unreachable {
        reason: format!(
            "front proxy is structurally {reachable_ingress}-only: \
             {MITM_VALIDATOR_SYMBOL} in {MITM_PIN_SITE_PATH} pins both the MITM \
             upstream origin and mitm_host to {pinned_host}, so no `{ingress}` \
             front-proxy cell can exist -- this is a permanent limit, not a \
             missing credential"
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeSet;

    use tempfile::tempdir;

    use super::super::lane::INGRESS_IDS;

    /// The committed pin must be derivable at all: every assertion below
    /// that walks it would be vacuous against an unreadable source.
    #[test]
    fn the_committed_router_source_declares_the_mitm_host_pin() {
        let host = mitm_pinned_host().expect("the MITM host pin must be derivable from source");

        assert!(
            host.contains('.') && !host.contains('/'),
            "pin `{host}` does not look like a bare host",
        );
    }

    /// The reachable set is EXACTLY `{anthropic}`, asserted as a set over
    /// the declared ingress vocabulary rather than as a membership check:
    /// a `contains` assertion would stay green if a second ingress
    /// silently became reachable.
    #[test]
    fn the_reachable_ingress_set_over_the_declared_vocabulary_is_exactly_anthropic() {
        let reachable: BTreeSet<&str> = INGRESS_IDS
            .iter()
            .copied()
            .filter(|ingress| {
                front_proxy_reachability(ingress)
                    .expect("the pin must be derivable")
                    .is_reachable()
            })
            .collect();

        assert_eq!(reachable, BTreeSet::from(["anthropic"]));
    }

    #[test]
    fn every_unreachable_ingress_reason_names_the_pinned_host_and_the_validator() {
        let host = mitm_pinned_host().expect("the pin must be derivable");
        let unreachable: Vec<&str> = INGRESS_IDS
            .iter()
            .copied()
            .filter(|ingress| *ingress != "anthropic")
            .collect();

        assert!(
            !unreachable.is_empty(),
            "the declared ingress vocabulary has no non-Anthropic member, so this \
             assertion would be vacuous",
        );
        for ingress in unreachable {
            let verdict = front_proxy_reachability(ingress).expect("the pin must be derivable");

            let reason = verdict
                .reason()
                .unwrap_or_else(|| panic!("`{ingress}` reported reachable"));
            assert!(
                reason.contains(&host),
                "reason for `{ingress}` does not name the pinned host `{host}`: {reason}",
            );
            assert!(
                reason.contains(MITM_VALIDATOR_SYMBOL),
                "reason for `{ingress}` does not name `{MITM_VALIDATOR_SYMBOL}`: {reason}",
            );
            assert!(
                reason.contains(ingress),
                "reason does not name the ingress it is about: {reason}",
            );
        }
    }

    /// The verdict is keyed on the PARSED pin, not on a constant baked
    /// into the caller: a pin that is not the settled one is a loud error
    /// for every ingress, including the otherwise-reachable one. This is
    /// the control that a hand-listed exclusion set would fail -- such a
    /// list would keep answering `anthropic` regardless of the pin.
    #[test]
    fn a_pin_that_is_not_the_settled_host_errors_for_every_ingress() {
        for ingress in INGRESS_IDS {
            let err = reachability_for_pin(ingress, "api.openai.com").unwrap_err();

            match &err {
                PinError::PinHostUnexpected { host, expected } => {
                    assert_eq!(host, "api.openai.com");
                    assert_eq!(expected, SETTLED_PIN.0);
                }
                other => panic!("expected PinHostUnexpected for `{ingress}`, got {other:?}"),
            }
        }
    }

    /// Positive control for the arm above: the settled pin itself must
    /// NOT error, so the host check cannot be tightened into rejecting
    /// the one value the derivation exists to serve.
    #[test]
    fn the_settled_pin_resolves_without_error() {
        for ingress in INGRESS_IDS {
            assert!(
                reachability_for_pin(ingress, SETTLED_PIN.0).is_ok(),
                "the settled pin must resolve for `{ingress}`",
            );
        }
    }

    /// The committed source's pin must BE the settled host. If the
    /// production pin ever moves, this is the test that says so in one
    /// line instead of leaving the derivation quietly wrong.
    #[test]
    fn the_committed_pin_is_the_settled_host() {
        let host = mitm_pinned_host().expect("the pin must be derivable");

        assert_eq!(host, SETTLED_PIN.0);
    }

    /// The reason strings cite the enforcing validator by name, so that
    /// symbol must actually be defined where they point. The paired
    /// negative proves the citation check is falsifiable rather than
    /// satisfied by any file that happens to be readable.
    #[test]
    fn the_cited_validator_resolves_in_the_cited_file_and_a_bogus_symbol_does_not() {
        assert!(
            resolve_site_symbol(MITM_VALIDATOR_SYMBOL, MITM_PIN_SITE_PATH).is_ok(),
            "`{MITM_VALIDATOR_SYMBOL}` is not defined in {MITM_PIN_SITE_PATH}, so every \
             reason string cites a symbol a reader cannot find",
        );
        assert!(
            resolve_site_symbol(
                "validate_mitm_config_that_does_not_exist",
                MITM_PIN_SITE_PATH
            )
            .is_err(),
        );
    }

    /// FAIL-CLOSED: a source carrying no such constant errors rather than
    /// returning a default host.
    #[test]
    fn fails_closed_against_a_source_that_declares_no_pin() {
        let tmp = tempdir().unwrap();
        let stub = tmp.path().join("validate.rs");
        fs::write(
            &stub,
            "// mentions MITM_REQUIRED_HOST in prose only\n\
             pub fn validate_mitm_config() {}\n",
        )
        .unwrap();

        let err = mitm_pinned_host_at(&stub).unwrap_err();

        assert!(
            matches!(err, PinError::PinNotDeclared { .. }),
            "expected PinNotDeclared, got {err:?}",
        );
    }

    /// Positive control for the parse: the real declaration shape must
    /// read, including the doc comment that names the constant right
    /// above it, so the narrowing below cannot be tightened into
    /// rejecting the line it exists to read.
    #[test]
    fn parses_the_real_declaration_shape() {
        let text = format!(
            "/// Doc comment naming {MITM_PIN_CONST}.\n\
             const {MITM_PIN_CONST}: &str = \"api.anthropic.com\";\n"
        );

        let host = parse_mitm_pinned_host(&text, "validate.rs").unwrap();

        assert_eq!(host, "api.anthropic.com");
    }

    /// The parse must read the COMMITTED file, not merely a hand-built
    /// approximation of it: this is what pins the two together.
    #[test]
    fn parses_the_committed_source_to_the_same_host_as_the_real_declaration() {
        let path = mitm_pin_site_path().expect("the workspace must resolve");

        let host = mitm_pinned_host_at(&path).expect("the committed source must parse");

        assert_eq!(host, "api.anthropic.com");
    }

    /// Non-code copies of the declaration are NOT declarations. Each
    /// stanza is a shape that a line-anchored textual match would have
    /// accepted, so a moved production pin could have resolved to stale
    /// text instead of failing loudly.
    #[test]
    fn a_non_code_copy_of_the_declaration_is_not_a_declaration() {
        let decl = format!("const {MITM_PIN_CONST}: &str = \"api.anthropic.com\";");
        let cases = [
            ("block comment", format!("/*\n{decl}\n*/\n")),
            // Rust block comments NEST, so the inner `*/` closes only the
            // inner comment and the declaration is still commented out. A
            // scanner treating comments as non-nesting reads it as code.
            (
                "after a nested block comment",
                format!("/* /* */ \n{decl}\n */\n"),
            ),
            (
                "raw string",
                format!("const DOC: &str = r#\"\n{decl}\n\"#;\n"),
            ),
            (
                "nested test-module const",
                format!("#[cfg(test)]\nmod tests {{\n    {decl}\n}}\n"),
            ),
            (
                "nested fn body",
                format!("fn helper() {{\n    {decl}\n}}\n"),
            ),
        ];

        for (label, text) in cases {
            let err = match parse_mitm_pinned_host(&text, "validate.rs") {
                Ok(host) => panic!("{label} copy parsed as the pin: {host}"),
                Err(err) => err,
            };

            assert!(
                matches!(err, PinError::PinNotDeclared { .. }),
                "expected PinNotDeclared for the {label} copy, got {err:?}",
            );
        }
    }

    /// Non-code text must not DESYNC the scan either. Each stanza puts
    /// brace-ish or comment-ish content somewhere the walk would
    /// misread it -- an unbalanced brace in a raw string, a `*/` in a
    /// plain string, an escaped quote -- ahead of the REAL declaration,
    /// which must still resolve. The complementary failure to the
    /// false-positive controls above: a desynced walk hides a real pin.
    #[test]
    fn non_code_text_ahead_of_the_declaration_does_not_desync_the_scan() {
        let decl = format!("const {MITM_PIN_CONST}: &str = \"api.anthropic.com\";");
        let cases = [
            (
                "unbalanced brace in a raw string",
                format!("const T: &str = r#\"{{ unclosed \"#;\n{decl}\n"),
            ),
            (
                "raw string holding a quote and a hash",
                format!("const T: &str = r#\"a \" b #\"#;\n{decl}\n"),
            ),
            (
                "comment closer inside a plain string",
                format!("const T: &str = \"*/ not a comment\";\n{decl}\n"),
            ),
            (
                "escaped quote in a plain string",
                format!("const T: &str = \"say \\\" then {{\";\n{decl}\n"),
            ),
        ];

        for (label, text) in cases {
            let host = parse_mitm_pinned_host(&text, "validate.rs")
                .unwrap_or_else(|e| panic!("{label} hid the real declaration: {e}"));

            assert_eq!(host, "api.anthropic.com", "case: {label}");
        }
    }

    /// A non-code copy alongside the REAL declaration must not make the
    /// pin ambiguous: the real one still resolves, alone.
    #[test]
    fn a_commented_out_copy_beside_the_real_declaration_leaves_one_pin() {
        let decl = format!("const {MITM_PIN_CONST}: &str = \"api.anthropic.com\";");
        let text = format!("/*\n{decl}\n*/\n{decl}\n");

        let host = parse_mitm_pinned_host(&text, "validate.rs").unwrap();

        assert_eq!(host, "api.anthropic.com");
    }

    #[test]
    fn fails_closed_when_the_source_cannot_be_read() {
        let tmp = tempdir().unwrap();
        let missing = tmp.path().join("absent.rs");

        let err = mitm_pinned_host_at(&missing).unwrap_err();

        assert!(
            matches!(err, PinError::SourceUnreadable { .. }),
            "expected SourceUnreadable, got {err:?}",
        );
    }

    #[test]
    fn fails_closed_when_two_declarations_compete() {
        let text = format!(
            "const {MITM_PIN_CONST}: &str = \"api.anthropic.com\";\n\
             const {MITM_PIN_CONST}: &str = \"api.example.com\";\n"
        );

        let err = parse_mitm_pinned_host(&text, "validate.rs").unwrap_err();

        assert!(
            matches!(err, PinError::PinAmbiguous { count: 2, .. }),
            "expected PinAmbiguous with count 2, got {err:?}",
        );
    }

    #[test]
    fn fails_closed_when_the_declaration_pins_an_empty_host() {
        let text = format!("const {MITM_PIN_CONST}: &str = \"\";\n");

        let err = parse_mitm_pinned_host(&text, "validate.rs").unwrap_err();

        assert!(
            matches!(err, PinError::PinEmpty { .. }),
            "expected PinEmpty, got {err:?}",
        );
    }

    /// Degenerate hosts are unexpected pins like any other, never a
    /// shape to reason about.
    #[test]
    fn fails_closed_on_a_degenerate_pinned_host() {
        for host in ["localhost", "", ".", "anthropic.com.evil.test"] {
            let err = reachability_for_pin("anthropic", host).unwrap_err();

            assert!(
                matches!(err, PinError::PinHostUnexpected { .. }),
                "expected PinHostUnexpected for {host:?}, got {err:?}",
            );
        }
    }
}
