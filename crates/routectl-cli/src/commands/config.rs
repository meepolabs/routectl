//! `routectl config <subcommand>` -- check, show, example.

use routectl_auth::{MemoryStore, SecretRef, SecretStore};
use routectl_core::{Error, Result};
use routectl_router::{
    validate_bedrock_global_config, validate_reasoning_defaults, Config, ProviderEntry,
};

/// Validate the loaded config: parse syntax (already done by main.rs), resolve
/// every secret reference (env / file / literal), and report any aliases that
/// reference unknown providers.
pub async fn check(config: &Config) -> Result<()> {
    let secrets = MemoryStore::new();
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
                    errors.push(format!(
                        "provider `{name}`: secret-ref parse failed (scheme `{}`): {e}",
                        scheme_of(uri),
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

    for (alias, entry) in &config.aliases {
        if entry.chain.is_empty() {
            errors.push(format!(
                "alias `{alias}`: chain is empty -- an alias with no targets resolves \
                 to UnknownAlias at request time, which is the same as not declaring \
                 the alias at all"
            ));
            continue;
        }
        for target in &entry.chain {
            let provider_name = target.split_once(':').map(|(p, _)| p).unwrap_or(target);
            if !config.providers.contains_key(provider_name) {
                errors.push(format!(
                    "alias `{alias}`: target `{target}` references unknown provider `{provider_name}`"
                ));
            }
        }
    }

    // default_model accepts either an alias key OR a `provider:model`
    // literal (mirrors the wire `model` field). Reject any other
    // shape at startup -- otherwise the misconfiguration only surfaces
    // at first request-time as a runtime WARN + UnknownAlias, which
    // is the wrong place to find out about a typo'd alias name.
    if let Some(default) = config.default_model.as_deref() {
        if default.is_empty() {
            errors.push("default_model is set but empty; remove the field or set a valid alias / provider:model literal".into());
        } else if let Some((provider_name, _)) = default.split_once(':') {
            if !config.providers.contains_key(provider_name) {
                errors.push(format!(
                    "default_model `{default}` is a provider:model literal but provider `{provider_name}` is not in [providers]"
                ));
            }
        } else if !config.aliases.contains_key(default) {
            errors.push(format!(
                "default_model `{default}` is neither an [aliases] key nor a provider:model literal"
            ));
        }
    }

    // Run the same startup validators that `serve` and `test` invoke
    // before building providers. Without this, an operator running
    // `routectl config check` against a TOML carrying `thinking = ""`
    // or an incoherent `[bedrock] allowed_body_fields` would see "ok"
    // and only discover the failure when starting the server. Surface
    // the same errors here.
    if let Err(e) = validate_reasoning_defaults(config) {
        errors.push(e.to_string());
    }
    if let Err(e) = validate_bedrock_global_config(config) {
        errors.push(e.to_string());
    }

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

/// Print the resolved config with secrets redacted.
pub fn show(config: &Config) -> Result<()> {
    let mut redacted = config.clone();
    for (_, entry) in redacted.providers.iter_mut() {
        redact_entry(entry);
    }
    let s =
        toml::to_string_pretty(&redacted).map_err(|e| Error::Config(format!("serialize: {e}")))?;
    println!("{s}");
    Ok(())
}

fn redact_entry(entry: &mut ProviderEntry) {
    entry.redact_secrets();
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
