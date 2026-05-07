//! `routectl config <subcommand>` -- check, show, example.

use std::collections::BTreeMap;

use routectl_auth::{MemoryStore, SecretRef, SecretStore};
use routectl_core::{Error, Result};
use routectl_router::{Config, ProviderEntry};

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

// Silence unused-import warning when only this fn is used.
#[allow(dead_code)]
fn _unused() -> BTreeMap<String, String> {
    BTreeMap::new()
}
