//! `routectl config <subcommand>` -- check, show, example.

use std::collections::BTreeMap;

use routectl_auth::{KeyringStore, SecretRef, SecretStore};
use routectl_core::{Error, Result};
use routectl_router::{Config, ProviderEntry};

/// Validate the loaded config: parse syntax (already done by main.rs), resolve
/// every secret reference via the keychain, and report any aliases that
/// reference unknown providers.
pub async fn check(config: &Config) -> Result<()> {
    let secrets = KeyringStore::new();
    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    for (name, entry) in &config.providers {
        for uri in secret_uris(entry) {
            let parsed = match SecretRef::parse(uri) {
                Ok(r) => r,
                Err(e) => {
                    errors.push(format!("provider `{name}`: {uri}: {e}"));
                    continue;
                }
            };
            if let Err(e) = secrets.get(&parsed).await {
                warnings.push(format!("provider `{name}`: cannot resolve `{uri}`: {e}"));
            }
        }
    }

    for (alias, entry) in &config.aliases {
        for target in &entry.chain {
            let provider_name = target.split_once(':').map(|(p, _)| p).unwrap_or(target);
            if !config.providers.contains_key(provider_name) {
                errors.push(format!(
                    "alias `{alias}`: target `{target}` references unknown provider `{provider_name}`"
                ));
            }
        }
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
        .map_err(|e| Error::Config(format!("serialize: {e}")))?;
    println!("{s}");
    Ok(())
}

fn redact_entry(entry: &mut ProviderEntry) {
    match entry {
        ProviderEntry::OpenaiCompat { api_key_ref, .. } => *api_key_ref = redact(api_key_ref),
        ProviderEntry::AnthropicApi { api_key_ref, .. } => *api_key_ref = redact(api_key_ref),
        ProviderEntry::ClaudeCookie { session_ref, .. } => *session_ref = redact(session_ref),
        ProviderEntry::ChatgptCookie { session_ref } => *session_ref = redact(session_ref),
    }
}

fn redact(uri: &str) -> String {
    if let Some(rest) = uri.strip_prefix("literal:") {
        if rest.is_empty() {
            "literal:".into()
        } else {
            "literal:[REDACTED]".into()
        }
    } else {
        uri.to_string()
    }
}

fn secret_uris(entry: &ProviderEntry) -> Vec<&str> {
    match entry {
        ProviderEntry::OpenaiCompat { api_key_ref, .. } => vec![api_key_ref.as_str()],
        ProviderEntry::AnthropicApi { api_key_ref, .. } => vec![api_key_ref.as_str()],
        ProviderEntry::ClaudeCookie { session_ref, .. } => vec![session_ref.as_str()],
        ProviderEntry::ChatgptCookie { session_ref } => vec![session_ref.as_str()],
    }
}

/// Print the example config to stdout.
pub fn example() -> Result<()> {
    print!("{}", include_str!("../../../../examples/config.toml"));
    Ok(())
}

// Silence unused-import warning when only this fn is used.
#[allow(dead_code)]
fn _unused() -> BTreeMap<String, String> {
    BTreeMap::new()
}
