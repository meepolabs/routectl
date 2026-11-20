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

use routectl_auth::{LoginOptions, OAuthError, OAuthStore};
use routectl_core::{Error, Result, render_operator_text};

use crate::commands::login_provider_block::provider_block;
use crate::commands::login_surface;
use crate::commands::rerun::{RerunCommand, chosen_config_path, instruction};
use crate::commands::seat::{shell_word, validate_label};

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
    Skip {
        /// How a login that must be repeated says to repeat the caller's
        /// invocation, which is not `routectl login`. `None` when the
        /// caller's own error names that itself.
        rerun: Option<&'a str>,
    },
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
    let invocation = LoginInvocation {
        provider,
        label,
        print_url,
        callback_port,
    };
    routectl_auth::oauth::run_login(provider, &store, opts)
        .await
        .map_err(|e| login_error(e, &invocation, surface))?;
    match surface {
        ConfigSurface::Auto { config_path, yes } => {
            login_surface::surface(config_path, provider, label, yes)?;
        }
        ConfigSurface::Skip { .. } => print_next_step(provider, label),
    }
    Ok(())
}

/// The flags of one login run, as the operator gave them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LoginInvocation<'a> {
    pub(crate) provider: &'a str,
    pub(crate) label: Option<&'a str>,
    pub(crate) print_url: bool,
    pub(crate) callback_port: Option<u16>,
}

/// The error a failed login surfaces. A login that must simply be run again
/// (it timed out, or the callback's state did not match) keeps the cause and
/// adds how to repeat the invocation that ran it, when there is one to add;
/// every other failure is the cause alone.
pub(crate) fn login_error(
    err: OAuthError,
    invocation: &LoginInvocation<'_>,
    surface: ConfigSurface<'_>,
) -> Error {
    let rerun = match err {
        OAuthError::LoginTimeout(_) | OAuthError::StateMismatch(_) => {
            failed_login_rerun(invocation, surface)
        }
        _ => None,
    };
    match rerun {
        Some(rerun) => Error::Auth(format!("{err}; {rerun}")),
        None => Error::Auth(err.to_string()),
    }
}

/// How to repeat the failed login: a `Skip` caller's own phrase, else the
/// exact `routectl login` command line, against the same config file.
fn failed_login_rerun(
    invocation: &LoginInvocation<'_>,
    surface: ConfigSurface<'_>,
) -> Option<String> {
    match surface {
        ConfigSurface::Skip { rerun } => rerun.map(str::to_string),
        ConfigSurface::Auto { config_path, yes } => Some(instruction(
            login_command(chosen_config_path(config_path), invocation, yes),
            &format!(
                "routectl login {}",
                shell_word(&render_operator_text(invocation.provider))
            ),
        )),
    }
}

/// The `routectl login` command line of `invocation` against `config_path`
/// (`None` for the default file); `None` when a value has no exact
/// printable form.
fn login_command(
    config_path: Option<&Path>,
    invocation: &LoginInvocation<'_>,
    yes: bool,
) -> Option<String> {
    let port = invocation.callback_port.map(|port| port.to_string());
    RerunCommand::routectl(config_path)
        .word("login")
        .value(invocation.provider)
        .option("--label", invocation.label)
        .switch("--print-url", invocation.print_url)
        .option("--callback-port", port.as_deref())
        .switch("--yes", yes)
        .render()
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
    use std::path::{Path, PathBuf};

    use routectl_auth::OAuthError;
    use routectl_testkit::ScopedEnv;

    use super::{ConfigSurface, LoginInvocation, login_error, provider_supports_print_url};

    type Failure = fn(String) -> OAuthError;

    const MUST_REPEAT: [(&str, Failure, &str); 2] = [
        ("timeout", OAuthError::LoginTimeout, "timed out"),
        (
            "state mismatch",
            OAuthError::StateMismatch,
            "state mismatch",
        ),
    ];

    fn invocation(label: Option<&str>) -> LoginInvocation<'_> {
        LoginInvocation {
            provider: "anthropic",
            label,
            print_url: false,
            callback_port: None,
        }
    }

    /// The message `routectl login` exits with when `failure` ends a login
    /// run with `invocation` against `config_path`.
    fn message(
        failure: OAuthError,
        invocation: &LoginInvocation<'_>,
        config_path: &Path,
    ) -> String {
        let surface = ConfigSurface::Auto {
            config_path,
            yes: false,
        };
        login_error(failure, invocation, surface).to_string()
    }

    /// Point the default config resolution at `root` with no
    /// `ROUTECTL_CONFIG`; returns the default file's path.
    fn default_config(root: &Path) -> (PathBuf, [ScopedEnv; 2]) {
        let env = [
            ScopedEnv::set("XDG_CONFIG_HOME", root),
            ScopedEnv::unset("ROUTECTL_CONFIG"),
        ];
        let folder = routectl_core::config_dir::config_dir().expect("an absolute XDG_CONFIG_HOME");
        (folder.join("config.toml"), env)
    }

    /// A login that must be repeated names the exact `routectl login`
    /// invocation, `--config` and `--label` included, and keeps the cause.
    #[test]
    #[serial_test::serial]
    fn a_login_that_must_be_repeated_names_the_exact_login_rerun() {
        for (row, failure, cause) in MUST_REPEAT {
            // Arrange: a chosen file in a folder the shell would split.
            let dir = tempfile::tempdir().expect("tempdir");
            let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path().join("default"));
            let _env = ScopedEnv::unset("ROUTECTL_CONFIG");
            let path = dir.path().join("my configs").join("config.toml");

            // Act
            let msg = message(
                failure("anthropic".into()),
                &invocation(Some("work")),
                &path,
            );

            // Assert
            let expected = format!(
                "re-run `routectl --config '{}' login anthropic --label work`",
                path.display()
            );
            assert!(msg.contains(cause), "row `{row}`: {msg}");
            assert!(msg.ends_with(&expected), "row `{row}`: {msg}");
            assert_eq!(msg.matches("re-run").count(), 1, "row `{row}`: {msg}");
        }
    }

    /// The default file is left unnamed only when a bare `routectl` reaches
    /// it; `ROUTECTL_CONFIG` pointing elsewhere keeps `--config`.
    #[test]
    #[serial_test::serial]
    fn a_login_rerun_names_the_config_only_when_it_is_not_the_default() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, _env) = default_config(dir.path());
        let failure = || OAuthError::LoginTimeout("anthropic".into());

        // Act
        let bare = message(failure(), &invocation(Some("work")), &path);
        let _redirect = ScopedEnv::set("ROUTECTL_CONFIG", dir.path().join("other.toml"));
        let redirected = message(failure(), &invocation(Some("work")), &path);

        // Assert
        assert!(
            bare.ends_with("re-run `routectl login anthropic --label work`"),
            "{bare}"
        );
        let config = crate::commands::seat::shell_word(path.to_str().expect("utf8 tempdir"));
        assert!(
            redirected.ends_with(&format!(
                "re-run `routectl --config {config} login anthropic --label work`"
            )),
            "{redirected}"
        );
    }

    /// The rerun repeats every flag the login ran with, so the pasted
    /// command takes the same flow and the same confirmation choice.
    #[test]
    #[serial_test::serial]
    fn a_login_rerun_repeats_the_flow_flags() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, _env) = default_config(dir.path());
        let flow = LoginInvocation {
            provider: "anthropic",
            label: Some("my seat"),
            print_url: true,
            callback_port: Some(9000),
        };
        let surface = ConfigSurface::Auto {
            config_path: &path,
            yes: true,
        };

        // Act
        let msg =
            login_error(OAuthError::LoginTimeout("anthropic".into()), &flow, surface).to_string();

        // Assert
        assert!(
            msg.ends_with(
                "re-run `routectl login anthropic --label 'my seat' --print-url \
                 --callback-port 9000 --yes`"
            ),
            "{msg}"
        );
    }

    /// A label or config path with no exact printable form prints no
    /// literal command; the message still says what failed. The printable
    /// row is the positive control.
    #[test]
    #[serial_test::serial]
    fn a_login_rerun_with_a_control_character_value_prints_no_literal_command() {
        let rows = [
            ("printable", "work", "my configs", true),
            ("escape in label", "wo\u{1b}[2Jrk", "my configs", false),
            ("bell in config folder", "work", "my\u{7}configs", false),
        ];
        for (row, label, folder, is_literal) in rows {
            // Arrange
            let dir = tempfile::tempdir().expect("tempdir");
            let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path().join("default"));
            let _env = ScopedEnv::unset("ROUTECTL_CONFIG");
            let path = dir.path().join(folder).join("config.toml");

            for (failure_row, failure, cause) in MUST_REPEAT {
                // Act
                let msg = message(failure("anthropic".into()), &invocation(Some(label)), &path);

                // Assert
                let at = format!("row `{row}` `{failure_row}`");
                assert!(msg.contains(cause), "{at}: {msg}");
                assert_eq!(
                    msg.ends_with("re-run the same `routectl login anthropic` command"),
                    !is_literal,
                    "{at}: {msg}"
                );
                for word in ["--label", "--config"] {
                    assert_eq!(msg.contains(word), is_literal, "{at} `{word}`: {msg}");
                }
            }
        }
    }

    /// A caller that names its own rerun gets that phrase, never a
    /// `routectl login` command; one that names none gets the cause alone.
    #[test]
    fn a_skip_caller_gets_its_own_rerun_or_none() {
        for (row, failure, cause) in MUST_REPEAT {
            let own = login_error(
                failure("anthropic".into()),
                &invocation(Some("work")),
                ConfigSurface::Skip {
                    rerun: Some("re-run `routectl provider add --login anthropic`"),
                },
            )
            .to_string();
            let none = login_error(
                failure("anthropic".into()),
                &invocation(Some("work")),
                ConfigSurface::Skip { rerun: None },
            )
            .to_string();

            assert!(
                own.ends_with("re-run `routectl provider add --login anthropic`"),
                "row `{row}`: {own}"
            );
            assert!(none.contains(cause), "row `{row}`: {none}");
            for msg in [&own, &none] {
                assert!(!msg.contains("routectl login"), "row `{row}`: {msg}");
                assert_eq!(msg.matches("re-run").count(), usize::from(msg == &own));
            }
        }
    }

    /// A failure that re-running the same command would not cure is the
    /// cause alone: no rerun is added.
    #[test]
    fn other_login_failures_add_no_rerun() {
        let path = Path::new("/srv/routectl/config.toml");
        let failure = OAuthError::TokenEndpoint("invalid_grant: bad code".into());

        let msg = message(failure, &invocation(Some("work")), path);

        assert_eq!(
            msg,
            "auth: token endpoint returned an error: invalid_grant: bad code"
        );
    }

    #[test]
    fn anthropic_supports_print_url() {
        assert!(provider_supports_print_url("anthropic"));
    }

    #[test]
    fn codex_does_not_support_print_url() {
        assert!(!provider_supports_print_url("codex"));
    }
}
