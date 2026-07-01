//! `routectl config <subcommand>` -- check, show, example.

use routectl_auth::{SecretRef, SecretStore};
use routectl_core::{Error, Result};
use routectl_router::{
    Config, ProviderEntry, validate_alias_chain_targets, validate_alias_patterns,
    validate_bedrock_global_config, validate_overrides, validate_reasoning_defaults,
    validate_registry_patterns, validate_retry_policy,
};

use crate::server::CompositeStore;

/// Validate the loaded config: parse syntax (already done by main.rs), resolve
/// every secret reference (env / file / literal), and report any aliases that
/// reference unknown providers.
pub async fn check(config: &Config) -> Result<()> {
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

    // v0.6.0: every [models.X].provider must reference a known
    // [providers] entry. Surfaces typos at startup instead of as a
    // confusing UnknownProvider at first dispatch.
    for (nickname, model) in &config.models {
        if !config.providers.contains_key(&model.provider) {
            errors.push(format!(
                "model `{nickname}` references unknown provider `{}` (not in [providers])",
                model.provider,
            ));
        }
    }

    // Run the same startup validators that `serve` and `test` invoke
    // before building providers. Without this, an operator running
    // `routectl config check` against a TOML carrying `thinking = ""`,
    // a chain of unknown nicknames, or an incoherent
    // `[bedrock] allowed_body_fields` would see "ok" and only discover
    // the failure when starting the server. Surface the same errors here.
    if let Err(e) = validate_alias_chain_targets(config) {
        errors.push(e.to_string());
    }
    if let Err(e) = validate_alias_patterns(config) {
        errors.push(e.to_string());
    }
    if let Err(e) = validate_reasoning_defaults(config) {
        errors.push(e.to_string());
    }
    if let Err(e) = validate_bedrock_global_config(config) {
        errors.push(e.to_string());
    }
    if let Err(e) = validate_retry_policy(config) {
        errors.push(e.to_string());
    }
    if let Err(e) = validate_registry_patterns(config) {
        errors.push(e.to_string());
    }
    if let Err(e) = validate_overrides(&config.cache_pricing) {
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
    let s = toml::to_string_pretty(&redacted)
        .map_err(|e| Error::Internal(format!("serialize: {e}")))?;
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
