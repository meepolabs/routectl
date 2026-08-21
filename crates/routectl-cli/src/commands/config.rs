//! `routectl config <subcommand>` -- check, show, example.

use routectl_auth::{OAuthStore, SecretRef, SecretStore};
use routectl_core::{Error, Result, sanitize_for_log_with_cap};
use routectl_router::{Config, ProviderEntry, collect_config_validation, locate_dotted_path};

use crate::commands::login_provider_block::auth_selector_gap;
use crate::commands::seat_report::{
    pool_rows, seat_pool_lines, seat_reference_is_stored, stored_seat_pool_rows,
};
use crate::server::CompositeStore;

/// Validate the loaded config: parse syntax (already done by main.rs), resolve
/// every secret reference (env / file; `oauth://` refs are checked for
/// PRESENCE only, and `literal:` refs are rejected), and report any aliases
/// that reference unknown providers.
///
/// `raw_text` is the config file's original TOML, threaded through so a
/// semantic error whose message names a config key/path can be rendered with
/// the source line it came from. It is `None` when the text was unreadable;
/// every error then falls back to its plain message. Locating a line is a
/// presentation nicety and never turns into a load error.
///
/// The OAuth seat-pool block is purely informational: it never enters
/// [`CheckReport`] and never moves the exit code, so an unreadable credential
/// store still reports `ok` (with the pool counts rendered as unknown).
pub async fn check(config: &Config, raw_text: Option<&str>) -> Result<()> {
    let secrets = CompositeStore::open_default().await?;
    let mut errors: Vec<String> = secret_ref_parse_errors(config, raw_text);
    let stored = stored_seat_keys().await;
    let mut warnings: Vec<String> = secret_ref_resolution_warnings(config, &secrets, &stored).await;

    // v0.6.0: every [models.X].provider must reference a known
    // [providers] entry, plus the shared startup validator suite that
    // `serve` and `test` also run -- both rendered with source lines.
    // Without the suite here, an operator running `routectl config check`
    // against a TOML carrying `thinking = ""`, a chain of unknown
    // nicknames, or an incoherent `[bedrock] allowed_body_fields` would
    // see "ok" and only discover the failure when starting the server.
    // The bespoke secret-ref resolution above is check-specific and stays
    // here.
    let report = validation_report(config, raw_text);
    errors.extend(report.errors);
    warnings.extend(report.warnings);

    println!("config check:");
    println!("  providers: {}", config.providers.len());
    println!("  aliases:   {}", config.aliases.len());
    println!(
        "  bind:      http://{}:{}",
        config.server.host, config.server.port
    );

    let seat_lines = seat_pool_lines(
        &pool_rows(config, stored.as_deref()),
        &stored_seat_pool_rows(config, stored.as_deref()),
    );
    if !seat_lines.is_empty() {
        println!();
        for line in &seat_lines {
            println!("{line}");
        }
    }

    if !warnings.is_empty() {
        println!("\nwarnings ({}):", warnings.len());
        for w in &warnings {
            println!("  - {}", safe_line(w));
        }
    }

    if !errors.is_empty() {
        println!("\nerrors ({}):", errors.len());
        for e in &errors {
            println!("  - {}", safe_line(e));
        }
        return Err(Error::Config(format!("{} config error(s)", errors.len())));
    }

    println!("\nok.");
    Ok(())
}

/// Longest reported error/warning line, in characters. Every validator
/// formats operator-written TOML keys into its message, and the longest
/// legitimate one (a `[bedrock] allowed_body_fields` remediation) runs past
/// 300 chars -- so the shared 256-char log-field budget would truncate a
/// benign message. This ceiling bounds the line without reaching any real
/// one.
///
/// Shared with the doctor render of the same validator messages so both
/// report surfaces truncate at one ceiling.
pub(crate) const MAX_REPORTED_LINE_CHARS: usize = 512;

/// One-line rendering of a reported error or warning.
///
/// Every validator interpolates operator-written table keys and values into
/// its message, so this one render point control-char-filters the whole
/// suite's output: a config key bearing a newline plus an ANSI sequence would
/// otherwise span lines and forge a fabricated report entry. Mirrors the
/// filtering the doctor render applies to the same validator messages.
fn safe_line(message: &str) -> String {
    sanitize_for_log_with_cap(message, MAX_REPORTED_LINE_CHARS)
}

/// Read-only resolution check of every provider's secret refs.
///
/// `oauth://` refs are checked against `stored_seat_keys` for PRESENCE
/// instead of being resolved: `SecretStore::get` refreshes a near-expiry
/// token, which means a network POST to the provider's token endpoint plus a
/// credentials-file write during a command an operator expects to only read.
/// Presence answers the question check actually asks -- is the credential
/// this ref names there -- and cannot mutate anything. Non-oauth refs
/// (`env://` / `file://`) resolve as before: their resolution is a local read
/// with no side effect.
///
/// The three-way taxonomy for an `oauth://` ref:
///   - store readable and a named seat is stored -> silent;
///   - store readable, no seat matches -> unresolved warning;
///   - store unreadable (`stored_seat_keys` is `None`) -> a static
///     cannot-verify warning, since absence is unknowable.
///
/// Every message names the SCHEME only -- never the ref, the seat, or the
/// store path.
async fn secret_ref_resolution_warnings(
    config: &Config,
    secrets: &CompositeStore,
    stored_seat_keys: &Option<Vec<String>>,
) -> Vec<String> {
    let mut warnings = Vec::new();
    for (name, entry) in &config.providers {
        for uri in entry.secret_uris() {
            // Parse errors are collected separately by
            // `secret_ref_parse_errors`; here we only check the refs that
            // parse, so an unparseable ref is skipped rather than
            // double-reported.
            let Ok(parsed) = SecretRef::parse(uri) else {
                continue;
            };
            if let SecretRef::OAuth { provider, label } = &parsed {
                if let Some(warning) =
                    oauth_presence_warning(name, provider, label.as_deref(), stored_seat_keys)
                {
                    warnings.push(warning);
                }
                continue;
            }
            if let Err(e) = secrets.get(&parsed).await {
                warnings.push(format!(
                    "provider `{name}`: cannot resolve secret-ref (scheme `{}`): {e}",
                    scheme_of(uri),
                ));
            }
        }
    }
    warnings
}

/// The presence verdict for one `oauth://` ref, or `None` when the store
/// holds a seat the ref names (the silent, healthy case).
fn oauth_presence_warning(
    entry: &str,
    provider: &str,
    label: Option<&str>,
    stored_seat_keys: &Option<Vec<String>>,
) -> Option<String> {
    match stored_seat_keys {
        // Unknowable, not absent: claiming "unresolved" here would send the
        // operator to `routectl login` for a credential that may well be
        // stored. The seat-pool block above renders the same distinction.
        None => Some(format!(
            "provider `{entry}`: cannot verify secret-ref (scheme `oauth://`): \
             the credential store could not be opened"
        )),
        Some(keys) if seat_reference_is_stored(provider, label, keys) => None,
        Some(_) => Some(format!(
            "provider `{entry}`: unresolved secret-ref (scheme `oauth://`): \
             no stored credential for the seat it names; run `routectl login` for it"
        )),
    }
}

/// Snapshot of the credential store's seat keys for the informational
/// seat-pool block, or `None` when the store could not be OPENED.
///
/// Opened directly through [`OAuthStore`] rather than through the composite
/// store on purpose: the composite's `list_seats` echoes a pinned ref back as
/// a one-element list when its oauth arm is absent, which would report a
/// confident "1 seat" for exactly the unreadable-store case this block exists
/// to make visible. A merely-missing credentials file opens as an empty store,
/// so "nothing logged in" stays distinguishable from "cannot tell".
async fn stored_seat_keys() -> Option<Vec<String>> {
    let store = OAuthStore::open_default().await.ok()?;
    Some(store.list().await.into_iter().map(|(key, _)| key).collect())
}

/// The rendered semantic findings of `config check`: error and warning lines
/// with source-line prefixes already applied. Secret-ref resolution (which
/// needs the async secret store) stays in [`check`]; everything derivable
/// from the parsed config plus raw text lives here so the line rendering is
/// exercisable without touching the store.
pub struct CheckReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// Build the [`CheckReport`] for `config`: the model->provider reference
/// check plus the shared startup validator suite, each error prefixed with
/// its source line via `locate` when `raw_text` names one. The validator
/// suite returns bare messages; the `config: ` prefix is re-added so each
/// listed error reads the same as when these validators surfaced through
/// `Error::Config` directly.
///
/// The suite's own warnings are carried through and joined by
/// `oauth_auth_selector_warnings`, the one finding that cannot live in the
/// router's shared suite (it reads the CLI-side auth-shape table). `check`
/// and `doctor` render the warning half; the gate caller
/// (`edit_pipeline::gate`) reads `errors` alone, so an advisory finding
/// never blocks a write.
pub fn validation_report(config: &Config, raw_text: Option<&str>) -> CheckReport {
    let mut errors: Vec<String> = Vec::new();

    for (nickname, model) in &config.models {
        // A `[models.X] provider` value resolves against providers AND pools
        // in ONE namespace (the factory's build walk reads it that way), so a
        // pool name here is a valid target, not an unknown provider.
        if !config.providers.contains_key(&model.provider)
            && !config.pools.contains_key(&model.provider)
        {
            errors.push(locate(
                raw_text,
                format!(
                    "model `{nickname}` references unknown provider or pool `{}` (not in \
                     [providers] or [pools])",
                    model.provider,
                ),
            ));
        }
    }

    let validation = collect_config_validation(config);
    errors.extend(
        validation
            .errors
            .into_iter()
            .map(|e| locate(raw_text, format!("config: {e}"))),
    );

    let mut warnings = validation.warnings;
    warnings.extend(oauth_auth_selector_warnings(config, raw_text));

    CheckReport { errors, warnings }
}

/// Advisory findings for a provider entry that consumes an `oauth://`
/// credential without the auth selector that surface needs: the bearer would
/// be dispatched on the API-key header and rejected upstream.
///
/// Lives CLI-side, next to the rest of the config-check additions, because
/// the auth-shape authority it reads (`login_provider_block`'s table, via
/// `auth_selector_gap`) lives here -- moving the check into the router's
/// shared suite would mean either duplicating that table or lifting it into
/// the router purely to satisfy the call direction. Everything the check
/// needs is derivable from a parsed `Config`, so `config check`, `doctor`,
/// and the edit-pipeline gate all see it through `validation_report`.
///
/// WARN, not error: `auth_kind` was not always enforced, so configs written
/// before it exists in the wild carry this shape on entries an operator may
/// have long stopped routing traffic to. A hard failure would refuse to load
/// such a config outright -- taking down every other provider in it -- for a
/// defect that is per-entry and only bites on dispatch.
fn oauth_auth_selector_warnings(config: &Config, raw_text: Option<&str>) -> Vec<String> {
    config
        .providers
        .iter()
        .filter_map(|(name, entry)| {
            let gap = auth_selector_gap(entry)?;
            let found = match &gap.found {
                Some(value) => format!("is `{value}`"),
                None => "is unset".to_string(),
            };
            Some(locate(
                raw_text,
                format!(
                    "provider `{name}`: api_key_ref names the managed `{}` OAuth seat but \
                     `{}` {found}; that dispatches the bearer on the API-key surface, which \
                     the upstream rejects with 401. Set `{} = \"{}\"`.",
                    gap.family, gap.key, gap.key, gap.required,
                ),
            ))
        })
        .collect()
}

/// Collect the secret-ref PARSE errors across every provider entry: the
/// store-independent half of [`check`]'s secret-ref pass. Each entry's
/// [`secret_uris`](ProviderEntry::secret_uris) are run through
/// `SecretRef::parse`; an unrecognized scheme is an error. Secret
/// RESOLUTION (which needs the async store and yields warnings, not errors)
/// stays in [`check`]. Kept sync and store-free so the full config-check
/// error path is exercisable in tests without a `SecretStore`.
///
/// The rendered scheme -- never the raw URI -- names the failure so a
/// `literal:hunter2` in the TOML cannot leak into shell history or CI logs
/// via this stdout path.
fn secret_ref_parse_errors(config: &Config, raw_text: Option<&str>) -> Vec<String> {
    let mut errors = Vec::new();
    for (name, entry) in &config.providers {
        for uri in entry.secret_uris() {
            if let Err(e) = SecretRef::parse(uri) {
                errors.push(locate(
                    raw_text,
                    format!(
                        "provider `{name}`: secret-ref parse failed (scheme `{}`): {e}",
                        scheme_of(uri),
                    ),
                ));
            }
        }
    }
    errors
}

/// Print the resolved config with secrets redacted.
///
/// The output is deliberately NOT round-trippable, and says so in a leading
/// TOML comment: operators paste this into bug reports, so `base_url` is
/// reduced to its origin (dropping any credential in userinfo, path, or query)
/// and literal key refs become sentinels.
pub fn show(config: &Config) -> Result<()> {
    println!("{}", render_show(config)?);
    Ok(())
}

/// The exact text `show` prints, so the redaction boundary is testable without
/// capturing stdout.
fn render_show(config: &Config) -> Result<String> {
    let mut redacted = config.clone();
    for entry in redacted.providers.values_mut() {
        redact_entry(entry);
    }
    let body = toml::to_string_pretty(&redacted)
        .map_err(|e| Error::Internal(format!("serialize: {e}")))?;
    Ok(format!(
        "# Secrets are redacted and each provider base_url is reduced to its origin \
         (scheme://host:port), so this output is NOT round-trippable back into a config \
         file -- see your own config.toml for verbatim values.\n{body}"
    ))
}

fn redact_entry(entry: &mut ProviderEntry) {
    entry.redact_secrets();
}

/// Prefix `message` with `(line N): ` when the config text is available and
/// the message names a config key/path that resolves to a source line.
/// Everything else -- no raw text, an unrecognizable message, or a path that
/// does not resolve -- returns `message` unchanged. Locating a line is a
/// display nicety; it must never mutate the error taxonomy or fail the check.
fn locate(raw_text: Option<&str>, message: String) -> String {
    let line = raw_text.and_then(|raw| {
        derive_dotted_path(&message).and_then(|path| locate_dotted_path(raw, &path))
    });
    match line {
        Some(n) => format!("(line {n}): {message}"),
        None => message,
    }
}

/// Derive the dotted config key/path a semantic error refers to, or `None`
/// when the message names no location this can resolve unambiguously.
///
/// Conservative by design -- only the two clearly-anchored message shapes the
/// validators and check produce are recognized:
///
///   - a leading TOML header, `[a.b.c] ...` -> `a.b.c` (the `[retry]`,
///     `[aliases.X]`, `[models.X]`, `[registry.X]`,
///     `[retry.classes.feature-unsupported]`, and
///     `[providers.X.class_overrides]` validator errors);
///   - a leading ``alias `X` ``/``model `X` ``/``provider `X` `` clause ->
///     `aliases.X` / `models.X` / `providers.X` (the alias-chain, model->
///     provider-reference, and secret-ref checks).
///
/// A leading `config: ` wrapper (added for validator errors) is stripped
/// first so both wrapped and bare messages derive the same path. Anything
/// else returns `None` and the caller keeps the plain message.
fn derive_dotted_path(message: &str) -> Option<String> {
    let message = message.strip_prefix("config: ").unwrap_or(message);

    if let Some(rest) = message.strip_prefix('[') {
        let header = rest.split(']').next().filter(|h| !h.is_empty())?;
        return Some(header.to_string());
    }

    for (prefix, table) in [
        ("alias `", "aliases"),
        ("model `", "models"),
        ("provider `", "providers"),
    ] {
        if let Some(rest) = message.strip_prefix(prefix) {
            let name = rest.split('`').next().filter(|n| !n.is_empty())?;
            return Some(format!("{table}.{name}"));
        }
    }

    None
}

/// The committed example config, embedded once for every consumer: `config
/// example` prints it and `init --scaffold` writes it. A second
/// `include_str!` of the same file elsewhere would be a copy that no
/// compiler check keeps in step with this one, so callers name this
/// constant instead.
pub(crate) const EXAMPLE_CONFIG: &str = include_str!("../../../../examples/config.toml");

/// Print the example config to stdout.
pub fn example() -> Result<()> {
    print!("{EXAMPLE_CONFIG}");
    Ok(())
}

/// Render the scheme prefix of a SecretRef URI without revealing
/// the underlying name or value. Used in `check`'s user-facing
/// error/warning lines so a `literal:hunter2` in a TOML doesn't
/// leak into shell history or CI logs via stdout.
fn scheme_of(uri: &str) -> &'static str {
    if uri.starts_with("env://") {
        "env://"
    } else if uri.starts_with("file://") {
        "file://"
    } else if uri.starts_with("literal:") {
        "literal:"
    } else if uri.starts_with("oauth://") {
        "oauth://"
    } else {
        "unknown"
    }
}

#[cfg(test)]
mod emitted_example_tests {
    use super::*;

    /// Whatever `config example` emits, THIS build must be able to load --
    /// `config example | routectl config check` is the documented
    /// copy-to-config-dir flow, and an emitted provider kind the running
    /// binary cannot deserialize breaks it at the first step. The assertion
    /// carries no feature gate on purpose: it must hold on every feature
    /// combination the binary can be built with, so a future kind added to
    /// the example behind a cfg gate fails here instead of in an operator's
    /// terminal.
    ///
    /// Scope, stated precisely: this runs TYPED DESERIALIZATION plus the
    /// shared semantic validators (`parse_config` + `validation_report`,
    /// via `gate`). That is exactly what catches the failure this guards --
    /// an `unknown variant` rejection when the example documents a provider
    /// kind the build does not compile.
    ///
    /// It is NOT the full runtime load path: startup additionally runs
    /// `load_effective_config` and then constructs the router and its
    /// providers. So a kind that parses but cannot be BUILT in some feature
    /// build would still pass here (see the repo learning that `config
    /// check` passing does not prove `serve` can build the config). Widening
    /// this to router construction is deliberately not done: it would pull
    /// provider credential resolution into a unit test for no gain against
    /// the parse-shaped defect this pins.
    #[test]
    fn the_emitted_example_config_loads_on_this_build() {
        // Arrange: exactly the bytes `config example` writes to stdout.
        let emitted = EXAMPLE_CONFIG;

        // Act
        let gated = crate::commands::edit_pipeline::gate(emitted);

        // Assert
        let config = gated.unwrap_or_else(|errors| {
            panic!(
                "this build emits an example it cannot load; enabled provider \
                 kinds must cover every kind the example documents: {errors:?}"
            )
        });
        assert!(
            !config.providers.is_empty(),
            "the emitted example must declare providers"
        );
    }

    /// The shipped example must pass `routectl config check` with ZERO
    /// errors on a clean machine -- an operator's first command after
    /// copying it in is that check, and a bundled example the repo's own
    /// validator rejects teaches the wrong shape before it teaches
    /// anything else.
    ///
    /// Both error-producing halves of the check are asserted, because they
    /// fail independently: [`secret_ref_parse_errors`] rejects a ref whose
    /// SCHEME does not parse (a `file://<local-path>` placeholder, a
    /// `literal:` inline key), and [`validation_report`] owns the semantic
    /// suite (unknown model targets, pool coherence, namespace collisions).
    ///
    /// Hermetic by construction: both are sync and store-free, so this
    /// reads no env var, opens no credential store, and touches no HOME.
    /// That is also the boundary of what it proves -- an `env://VAR` that
    /// is unset and an `oauth://` seat with nothing logged in are
    /// WARNINGS, and the example is deliberately shipped in exactly that
    /// state: it passes the check without being boot-ready.
    #[test]
    fn the_shipped_example_config_passes_check_as_shipped() {
        // Arrange
        let text = EXAMPLE_CONFIG;
        let config: Config = toml::from_str(text).expect("the shipped example must parse");

        // Act
        let parse_errors = secret_ref_parse_errors(&config, Some(text));
        let report = validation_report(&config, Some(text));

        // Assert
        assert!(
            parse_errors.is_empty(),
            "every credential ref in examples/config.toml must parse: {parse_errors:?}"
        );
        assert!(
            report.errors.is_empty(),
            "examples/config.toml must pass the shared validator suite: {:?}",
            report.errors
        );
    }
}

#[cfg(test)]
mod mantle_config_check_tests {
    use super::*;

    /// A Bedrock mantle entry authenticates with `bedrock_mantle.creds` and
    /// REQUIRES an empty `api_key_ref`. The full config-check path -- the
    /// secret-ref parse walk plus the shared validator suite -- must accept
    /// it end-to-end for both OpenAI-shape lanes. The parse walk previously
    /// surfaced the empty `api_key_ref` through `SecretRef::parse` and failed
    /// with a spurious "unrecognized scheme" error before the mantle
    /// validator (which requires it empty) was ever consulted.
    #[test]
    fn openai_mantle_entries_pass_the_full_config_check() {
        let toml_text = r#"
[providers.compat-mantle]
kind = "openai-compat"
api_key_ref = ""

[providers.compat-mantle.bedrock_mantle]
region = "us-west-2"

[providers.compat-mantle.bedrock_mantle.creds]
kind = "bearer-key"
key_ref = "file:///tmp/whatever"

[providers.responses-mantle]
kind = "openai-responses"
api_key_ref = ""
auth_kind = "bedrock-mantle"

[providers.responses-mantle.bedrock_mantle]
region = "us-west-2"

[providers.responses-mantle.bedrock_mantle.creds]
kind = "bearer-key"
key_ref = "file:///tmp/whatever"
"#;
        let config: Config = toml::from_str(toml_text).expect("mantle config must parse");

        let parse_errors = secret_ref_parse_errors(&config, Some(toml_text));
        assert!(
            parse_errors.is_empty(),
            "mantle api_key_ref must not surface to the secret-ref parse walk: {parse_errors:?}"
        );

        let report = validation_report(&config, Some(toml_text));
        assert!(
            report.errors.is_empty(),
            "mantle entries must pass the shared validator suite: {:?}",
            report.errors
        );
    }

    /// All three mantle-bearing lanes surface `bedrock_mantle.creds` refs to
    /// the config-check parse walk: a valid creds ref scheme passes both the
    /// parse walk and the shared validator suite end-to-end.
    #[test]
    fn mantle_creds_refs_pass_config_check_on_all_lanes() {
        let toml_text = r#"
[providers.anthropic-mantle]
kind = "anthropic-api"
bedrock_mantle = { region = "us-west-2", creds = { kind = "bearer-key", key_ref = "file:///tmp/whatever" } }

[providers.compat-mantle]
kind = "openai-compat"
api_key_ref = ""

[providers.compat-mantle.bedrock_mantle]
region = "us-west-2"

[providers.compat-mantle.bedrock_mantle.creds]
kind = "static"
access_key_ref = "env://AWS_ACCESS_KEY_ID"
secret_key_ref = "env://AWS_SECRET_ACCESS_KEY"

[providers.responses-mantle]
kind = "openai-responses"
api_key_ref = ""
auth_kind = "bedrock-mantle"

[providers.responses-mantle.bedrock_mantle]
region = "us-west-2"

[providers.responses-mantle.bedrock_mantle.creds]
kind = "bearer-key"
key_ref = "file:///tmp/whatever"
"#;
        let config: Config = toml::from_str(toml_text).expect("mantle config must parse");

        let parse_errors = secret_ref_parse_errors(&config, Some(toml_text));
        assert!(
            parse_errors.is_empty(),
            "valid mantle creds refs must pass the parse walk: {parse_errors:?}"
        );

        let report = validation_report(&config, Some(toml_text));
        assert!(
            report.errors.is_empty(),
            "mantle entries must pass the shared validator suite: {:?}",
            report.errors
        );
    }

    /// A malformed `bedrock_mantle.creds` ref scheme FAILS config check on
    /// every mantle lane -- proof the creds descriptor is actually walked. If
    /// the refs were not surfaced the bogus scheme would slip through to
    /// build/probe instead of the parse walk.
    #[test]
    fn malformed_mantle_creds_ref_scheme_fails_config_check() {
        let toml_text = r#"
[providers.anthropic-mantle]
kind = "anthropic-api"
bedrock_mantle = { region = "us-west-2", creds = { kind = "bearer-key", key_ref = "bogus://key" } }

[providers.compat-mantle]
kind = "openai-compat"
api_key_ref = ""

[providers.compat-mantle.bedrock_mantle]
region = "us-west-2"

[providers.compat-mantle.bedrock_mantle.creds]
kind = "static"
access_key_ref = "bogus://access"
secret_key_ref = "env://AWS_SECRET_ACCESS_KEY"

[providers.responses-mantle]
kind = "openai-responses"
api_key_ref = ""
auth_kind = "bedrock-mantle"

[providers.responses-mantle.bedrock_mantle]
region = "us-west-2"

[providers.responses-mantle.bedrock_mantle.creds]
kind = "bearer-key"
key_ref = "bogus://key"
"#;
        let config: Config = toml::from_str(toml_text).expect("mantle config must parse");

        let parse_errors = secret_ref_parse_errors(&config, Some(toml_text));
        for provider in ["anthropic-mantle", "compat-mantle", "responses-mantle"] {
            assert!(
                parse_errors.iter().any(|e| e.contains(provider)),
                "malformed creds ref on `{provider}` must fail the parse walk: {parse_errors:?}"
            );
        }
    }

    /// Ref-less and empty creds fields do not break the walk: a profile name
    /// is not a secret ref, `default-chain` carries none, and an empty
    /// optional `session_token_ref` is skipped (mirroring the empty-api-key
    /// regression). None of these surface a spurious parse error.
    #[test]
    fn refless_and_empty_mantle_creds_fields_do_not_break_the_walk() {
        let toml_text = r#"
[providers.anthropic-mantle]
kind = "anthropic-api"
bedrock_mantle = { region = "us-west-2", creds = { kind = "profile", name = "bedrock-prod" } }

[providers.compat-mantle]
kind = "openai-compat"
api_key_ref = ""

[providers.compat-mantle.bedrock_mantle]
region = "us-west-2"

[providers.compat-mantle.bedrock_mantle.creds]
kind = "default-chain"

[providers.responses-mantle]
kind = "openai-responses"
api_key_ref = ""
auth_kind = "bedrock-mantle"

[providers.responses-mantle.bedrock_mantle]
region = "us-west-2"

[providers.responses-mantle.bedrock_mantle.creds]
kind = "static"
access_key_ref = "env://AWS_ACCESS_KEY_ID"
secret_key_ref = "env://AWS_SECRET_ACCESS_KEY"
session_token_ref = ""
"#;
        let config: Config = toml::from_str(toml_text).expect("mantle config must parse");

        let parse_errors = secret_ref_parse_errors(&config, Some(toml_text));
        assert!(
            parse_errors.is_empty(),
            "ref-less / empty creds fields must not surface a parse error: {parse_errors:?}"
        );
    }
}

#[cfg(test)]
mod parse_error_scheme_wording_tests {
    use super::*;

    /// A malformed `oauth://` ref must name its own scheme in the
    /// parse-error line. The line is the only thing an operator sees (the
    /// raw ref is deliberately withheld), so rendering `unknown` for a
    /// scheme the parser knows about points them at nothing.
    #[test]
    fn an_oauth_parse_error_names_the_oauth_scheme() {
        // Arrange
        let toml_text = "[providers.claude]\n\
                         kind = \"anthropic-api\"\n\
                         api_key_ref = \"oauth://anthropic#\"\n";
        let config: Config = toml::from_str(toml_text).expect("config must parse");

        // Act
        let errors = secret_ref_parse_errors(&config, Some(toml_text));

        // Assert
        let error = errors
            .iter()
            .find(|e| e.contains("claude"))
            .unwrap_or_else(|| panic!("expected a parse error for the entry: {errors:?}"));
        assert!(
            error.contains("secret-ref parse failed (scheme `oauth://`)"),
            "{error}"
        );
    }

    /// The control: an existing scheme's wording is unchanged, so the
    /// oauth arm added no shift to the rest of the taxonomy.
    #[test]
    fn an_env_parse_error_still_names_the_env_scheme() {
        // Arrange
        let toml_text = "[providers.compat]\n\
                         kind = \"openai-compat\"\n\
                         base_url = \"https://example.invalid/v1\"\n\
                         api_key_ref = \"env://\"\n";
        let config: Config = toml::from_str(toml_text).expect("config must parse");

        // Act
        let errors = secret_ref_parse_errors(&config, Some(toml_text));

        // Assert
        let error = errors
            .iter()
            .find(|e| e.contains("compat"))
            .unwrap_or_else(|| panic!("expected a parse error for the entry: {errors:?}"));
        assert!(
            error.contains("secret-ref parse failed (scheme `env://`)"),
            "{error}"
        );
    }

    /// A ref with no recognizable scheme still renders `unknown` -- the
    /// fallthrough is what keeps an unprefixed, possibly-secret value out
    /// of the message.
    #[test]
    fn a_schemeless_ref_still_renders_as_unknown() {
        // Arrange
        let toml_text = "[providers.compat]\n\
                         kind = \"openai-compat\"\n\
                         base_url = \"https://example.invalid/v1\"\n\
                         api_key_ref = \"sk-not-a-ref\"\n";
        let config: Config = toml::from_str(toml_text).expect("config must parse");

        // Act
        let errors = secret_ref_parse_errors(&config, Some(toml_text));

        // Assert
        let error = errors
            .iter()
            .find(|e| e.contains("compat"))
            .unwrap_or_else(|| panic!("expected a parse error for the entry: {errors:?}"));
        assert!(
            error.contains("secret-ref parse failed (scheme `unknown`)"),
            "{error}"
        );
        assert!(!error.contains("sk-not-a-ref"), "{error}");
    }
}

#[cfg(test)]
mod show_redaction_tests {
    use super::render_show;
    use routectl_router::Config;

    /// `config show` ADVERTISES redaction, and operators paste its output into
    /// bug reports -- so a credential embedded in a `base_url` (accepted config:
    /// only the `[mitm]` validator rejects userinfo) must appear NOWHERE in the
    /// rendered text, in any position.
    #[test]
    fn show_never_renders_a_credential_carried_by_a_base_url() {
        let toml_text = r#"
[providers.relay]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-FAKE-KEY"
base_url = "https://svc:sk-userinfo-FAKE@internal.example:8443/v1?key=sk-query-FAKE"
"#;
        let config: Config = toml::from_str(toml_text).expect("config must parse");

        let rendered = render_show(&config).expect("render must succeed");

        for secret in [
            "sk-userinfo-FAKE",
            "sk-query-FAKE",
            "sk-ant-FAKE-KEY",
            "internal.example:8443/v1",
            "svc:",
            "?key=",
        ] {
            assert!(
                !rendered.contains(secret),
                "`{secret}` must not survive redaction; rendered:\n{rendered}"
            );
        }

        assert!(
            rendered.contains("https://internal.example:8443"),
            "the origin must still be shown so the dump stays diagnostic; rendered:\n{rendered}"
        );
        assert!(
            rendered.contains("literal:[REDACTED]"),
            "the literal key ref must be sentinelled; rendered:\n{rendered}"
        );
    }

    /// The leading comment is what tells the operator the dump cannot be pasted
    /// back into a config file, which is the price of the reduction.
    #[test]
    fn show_leads_with_a_not_round_trippable_notice() {
        let config = Config::default();
        let rendered = render_show(&config).expect("render must succeed");
        let first = rendered.lines().next().expect("at least one line");
        assert!(
            first.starts_with('#'),
            "the notice must be a TOML comment; got: {first}"
        );
        assert!(
            first.contains("NOT round-trippable"),
            "the notice must say the dump is not round-trippable; got: {first}"
        );
    }
}

#[cfg(test)]
mod oauth_auth_selector_tests {
    use super::validation_report;
    use routectl_router::Config;

    fn report_of(toml_text: &str) -> super::CheckReport {
        let config: Config = toml::from_str(toml_text).expect("config must parse");
        validation_report(&config, Some(toml_text))
    }

    /// `config check` must SURFACE the mismatch: without this the entry
    /// reports `ok` and only fails on the first dispatched request.
    #[test]
    fn config_check_warns_on_an_oauth_ref_missing_its_auth_selector() {
        // Arrange
        let toml_text = "[providers.claude]\n\
                         kind = \"anthropic-api\"\n\
                         api_key_ref = \"oauth://anthropic\"\n";

        // Act
        let report = report_of(toml_text);

        // Assert: a warning naming the entry and the field to set, and NOT
        // an error -- the config still loads.
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let warning = report
            .warnings
            .iter()
            .find(|w| w.contains("claude"))
            .unwrap_or_else(|| panic!("no warning for the entry: {:?}", report.warnings));
        assert!(warning.contains("auth_kind"), "{warning}");
        assert!(warning.contains("oauth-bearer"), "{warning}");
    }

    /// The negative: the correctly shaped entry produces NO warning at all,
    /// so an existing valid config is unaffected.
    #[test]
    fn config_check_stays_silent_on_a_correct_oauth_bearer_entry() {
        let toml_text = "[providers.claude]\n\
                         kind = \"anthropic-api\"\n\
                         api_key_ref = \"oauth://anthropic\"\n\
                         auth_kind = \"oauth-bearer\"\n";

        let report = report_of(toml_text);

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    /// The warning carries the source line, like every other located
    /// finding, so an operator can jump straight to the entry.
    #[test]
    fn the_warning_carries_the_offending_source_line() {
        let toml_text = "[providers.other]\n\
                         kind = \"openai-compat\"\n\
                         base_url = \"https://example.invalid/v1\"\n\
                         api_key_ref = \"env://KEY\"\n\
                         \n\
                         [providers.claude]\n\
                         kind = \"anthropic-api\"\n\
                         api_key_ref = \"oauth://anthropic\"\n";

        let report = report_of(toml_text);

        let warning = report
            .warnings
            .first()
            .unwrap_or_else(|| panic!("expected one warning: {:?}", report.warnings));
        assert!(warning.starts_with("(line 6): "), "{warning}");
    }

    /// The shipped example documents the shape operators copy: it must not
    /// itself trip the warning.
    #[test]
    fn the_shipped_example_config_trips_no_auth_selector_warning() {
        let report = report_of(super::EXAMPLE_CONFIG);

        let tripped: Vec<&String> = report
            .warnings
            .iter()
            .filter(|w| w.contains("api_key_ref names the managed"))
            .collect();
        assert!(tripped.is_empty(), "{tripped:?}");
    }
}
