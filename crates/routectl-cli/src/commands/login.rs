//! `routectl login <provider>` -- run the OAuth 2.0 PKCE flow, persist
//! tokens to `~/.config/routectl/credentials.json`, and (for a direct
//! `routectl login`) offer the config change that makes the new seat
//! reachable.
//!
//! Providers: `anthropic` (claude.ai) and `codex` (OpenAI
//! ChatGPT/Codex). The accepted set is validated at the clap layer
//! against the auth registry; this body is provider-id-generic except
//! for the `--print-url` guard below.
//!
//! The config auto-surface is a CALLER's choice ([`ConfigSurface`]), not a
//! property of logging in: `provider add` drives this same flow between its
//! own byte snapshot and its own commit, so a config-writing login there
//! would make every oauth `provider add` fail its snapshot check.

use std::path::Path;

use routectl_auth::{LoginOptions, OAuthStore};
use routectl_core::{Error, Result};

use crate::commands::login_provider_block::provider_block;
use crate::commands::login_surface;
use crate::commands::seat::validate_label;

/// Whether a login offers to write the config change its new seat implies.
///
/// The auto-surface is login-specific. A caller that is ITSELF mid-edit of
/// `config.toml` passes [`ConfigSurface::Skip`]: two writers between one
/// snapshot and one commit is a guaranteed conflict, and that caller is
/// already writing the entry the surface would propose.
///
/// `Auto` carries the file it may edit and the skip-confirm flag, so a
/// `Skip` caller has no path to hand over and cannot supply a placeholder
/// one by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSurface<'a> {
    /// Plan the config delta, show it, and write it on confirmation.
    Auto {
        /// The `config.toml` to offer the delta against.
        config_path: &'a Path,
        /// Skip the confirmation PROMPT. Never the printed delta.
        yes: bool,
    },
    /// Print the pasteable entry only; touch no config file.
    Skip,
}

pub async fn run(
    provider: &str,
    print_url: bool,
    callback_port: Option<u16>,
    label: Option<&str>,
    surface: ConfigSurface<'_>,
) -> Result<()> {
    if print_url && !provider_supports_print_url(provider) {
        return Err(Error::Auth(format!(
            "{provider} login does not support --print-url; run the default \
             browser flow (SSH users can port-forward 1455)"
        )));
    }
    let label = validate_label(label)?;
    let store = OAuthStore::open_default()
        .await
        .map_err(|e| Error::Auth(e.to_string()))?;
    let opts = LoginOptions::new()
        .with_print_url(print_url)
        .with_callback_port(callback_port)
        .with_label(label.map(str::to_string));
    routectl_auth::oauth::run_login(provider, &store, opts)
        .await
        .map_err(|e| Error::Auth(e.to_string()))?;
    match surface {
        ConfigSurface::Auto { config_path, yes } => {
            login_surface::surface(config_path, provider, label, yes)?;
        }
        ConfigSurface::Skip => print_next_step(provider, label),
    }
    Ok(())
}

/// Print the provider entry that consumes the seat just minted -- the
/// [`ConfigSurface::Skip`] path's whole output.
///
/// The credential alone routes no traffic: nothing in `config.toml` reaches
/// it until an operator adds this block. On the `Skip` path that is the
/// caller's own job (it is mid-edit of the same file), so a printed block
/// is all this can offer.
///
/// An id with no rendered block (not reachable via the CLI, whose accepted
/// set IS the login registry) prints nothing rather than a partial hint.
fn print_next_step(provider: &str, label: Option<&str>) {
    let Some(block) = provider_block(provider, label) else {
        return;
    };
    println!(
        "\nCredential stored. Nothing routes to it yet -- add this entry to \
         your config.toml:\n\n{}",
        block.render()
    );
}

/// Whether `provider` has a real headless `--print-url` landing page.
///
/// The `--print-url` flow prints the auth URL and reads a `code#state`
/// pair pasted back from the provider's success page. That only works
/// when the upstream serves such a page: claude.ai does, codex does not
/// (its `manual_redirect_url` is just the authorize URL placeholder, so
/// the flow always runs through the local callback server). Codex
/// operators on SSH should port-forward the fixed callback port (1455)
/// and use the default browser flow instead.
///
/// Kept as a CLI-local predicate rather than an `OAuthFlow` trait method:
/// it is a UX concern of this one command, and the registry stays the
/// single source of truth for *which* providers exist (clap validates
/// against `known_provider_ids()`); this only governs *how* a known
/// provider may be driven.
fn provider_supports_print_url(provider: &str) -> bool {
    !matches!(provider, "codex")
}

#[cfg(test)]
mod tests {
    use super::provider_supports_print_url;

    #[test]
    fn anthropic_supports_print_url() {
        assert!(provider_supports_print_url("anthropic"));
    }

    #[test]
    fn codex_does_not_support_print_url() {
        assert!(!provider_supports_print_url("codex"));
    }
}
