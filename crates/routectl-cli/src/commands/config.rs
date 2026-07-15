//! `routectl config <subcommand>` -- check, show, example.

use routectl_auth::{SecretRef, SecretStore};
use routectl_core::{Error, Result};
use routectl_router::{Config, ProviderEntry, collect_config_validation, locate_dotted_path};

use crate::server::CompositeStore;

/// Validate the loaded config: parse syntax (already done by main.rs), resolve
/// every secret reference (env / file / oauth; `literal:` refs are rejected),
/// and report any aliases that reference unknown providers.
///
/// `raw_text` is the config file's original TOML, threaded through so a
/// semantic error whose message names a config key/path can be rendered with
/// the source line it came from. It is `None` when the text was unreadable;
/// every error then falls back to its plain message. Locating a line is a
/// presentation nicety and never turns into a load error.
pub async fn check(config: &Config, raw_text: Option<&str>) -> Result<()> {
    let secrets = CompositeStore::open_default().await?;
    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    for (name, entry) in &config.providers {
        for uri in entry.secret_uris() {
            let parsed = match SecretRef::parse(uri) {
                Ok(r) => r,
                Err(e) => {
                    // Render the parsed scheme rather than echoing
                    // the raw URI. A `literal:hunter2` in the TOML
                    // would otherwise land in shell history and CI
                    // logs verbatim via this stdout path.
                    errors.push(locate(
                        raw_text,
                        format!(
                            "provider `{name}`: secret-ref parse failed (scheme `{}`): {e}",
                            scheme_of(uri),
                        ),
                    ));
                    continue;
                }
            };
            if let Err(e) = secrets.get(&parsed).await {
                warnings.push(format!(
                    "provider `{name}`: cannot resolve secret-ref (scheme `{}`): {e}",
                    scheme_of(uri),
                ));
            }
        }
    }

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

    if !warnings.is_empty() {
        println!("\nwarnings ({}):", warnings.len());
        for w in &warnings {
            println!("  - {w}");
        }
    }

    if !errors.is_empty() {
        println!("\nerrors ({}):", errors.len());
        for e in &errors {
            println!("  - {e}");
        }
        return Err(Error::Config(format!("{} config error(s)", errors.len())));
    }

    println!("\nok.");
    Ok(())
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
/// its source line via [`locate`] when `raw_text` names one. The validator
/// suite returns bare messages; the `config: ` prefix is re-added so each
/// listed error reads the same as when these validators surfaced through
/// `Error::Config` directly.
pub fn validation_report(config: &Config, raw_text: Option<&str>) -> CheckReport {
    let mut errors: Vec<String> = Vec::new();

    for (nickname, model) in &config.models {
        if !config.providers.contains_key(&model.provider) {
            errors.push(locate(
                raw_text,
                format!(
                    "model `{nickname}` references unknown provider `{}` (not in [providers])",
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

    CheckReport {
        errors,
        warnings: validation.warnings,
    }
}

/// Print the resolved config with secrets redacted.
pub fn show(config: &Config) -> Result<()> {
    let mut redacted = config.clone();
    for entry in redacted.providers.values_mut() {
        redact_entry(entry);
    }
    let s = toml::to_string_pretty(&redacted)
        .map_err(|e| Error::Internal(format!("serialize: {e}")))?;
    println!("{s}");
    Ok(())
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

/// Print the example config to stdout.
pub fn example() -> Result<()> {
    print!("{}", include_str!("../../../../examples/config.toml"));
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
    } else {
        "unknown"
    }
}
