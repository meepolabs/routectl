//! `routectl refresh <provider> [--label <name>]` -- force a token
//! refresh through the per-seat single-flight gate, regardless of expiry.
//! Useful when the operator suspects a token has been revoked
//! server-side or wants to cycle a fresh access token before a
//! long-running session. Without `--label`, refreshes the default
//! (unlabeled) seat -- today's behavior. With `--label`, refreshes only
//! that one seat.

use std::path::Path;

use routectl_auth::oauth::types::{seat_key, unix_now};
use routectl_auth::{ForcedRefreshError, OAuthStore};
use routectl_core::{Error, Result, render_operator_text};

use crate::commands::rerun::{RerunCommand, chosen_config_path, instruction};
use crate::commands::seat::{shell_word, validate_label};

/// `config_path` is the file the global `--config` flag or `ROUTECTL_CONFIG`
/// named, `None` when neither was given. The refresh itself never reads it;
/// a failure that needs a fresh login names it in the rerun, because that
/// login offers its config change to the same file.
pub async fn run(provider: &str, label: Option<&str>, config_path: Option<&Path>) -> Result<()> {
    let label = validate_label(label)?;
    let store = OAuthStore::open_default()
        .await
        .map_err(|e| Error::Auth(e.to_string()))?;
    refresh_with(&store, provider, label, config_path).await
}

/// [`run`] against an already-open `store`.
pub async fn refresh_with(
    store: &OAuthStore,
    provider: &str,
    label: Option<&str>,
    config_path: Option<&Path>,
) -> Result<()> {
    let new_rec = store
        .force_refresh(provider, label)
        .await
        .map_err(|e| refresh_error(e, provider, label, config_path))?;
    let now = unix_now();
    let remaining = new_rec.expires_at_unix.saturating_sub(now);
    let human = humanize_remaining(remaining);
    let seat = seat_key(provider, label);
    println!(
        "Refreshed {seat}. Access token expires in {human} (expires_at_unix={}).",
        new_rec.expires_at_unix
    );
    Ok(())
}

/// The error a failed refresh surfaces. A failure only a fresh login of the
/// seat cures (the identity provider refused its refresh token, or no login
/// is stored for it) keeps the cause and adds the `routectl login` command
/// for the same seat; every other failure is the cause alone, since a login
/// would not cure it.
fn refresh_error(
    failure: ForcedRefreshError,
    provider: &str,
    label: Option<&str>,
    config_path: Option<&Path>,
) -> Error {
    if failure.relogin.is_none() {
        return failure.error;
    }
    let rerun = login_rerun(provider, label, config_path.and_then(chosen_config_path));
    match failure.error {
        Error::Credential {
            detail,
            reason,
            parked_until_ms,
        } => Error::Credential {
            detail: format!("{detail}; {rerun}"),
            reason,
            parked_until_ms,
        },
        Error::Auth(detail) => Error::Auth(format!("{detail}; {rerun}")),
        other => other,
    }
}

/// How to log the refreshed seat in again: the exact `routectl login`
/// command against `config_path` (`None` for the default file), else an
/// instruction naming the vendor alone when a value has no exact printable
/// form.
fn login_rerun(provider: &str, label: Option<&str>, config_path: Option<&Path>) -> String {
    instruction(
        RerunCommand::routectl(config_path)
            .word("login")
            .value(provider)
            .option("--label", label)
            .render(),
        &format!(
            "routectl login {}",
            shell_word(&render_operator_text(provider))
        ),
    )
}

/// Render seconds-from-now as a short human-readable string. Operators
/// running `routectl refresh` to verify a token is alive should see
/// "expires in ~58m" not "expires in 3520s".
fn humanize_remaining(secs: u64) -> String {
    if secs == 0 {
        return "now".to_string();
    }
    let mins = secs / 60;
    if mins < 1 {
        return format!("{secs}s");
    }
    let hours = mins / 60;
    if hours < 1 {
        return format!("~{mins}m");
    }
    let days = hours / 24;
    if days < 1 {
        let extra_mins = mins % 60;
        if extra_mins == 0 {
            return format!("~{hours}h");
        }
        return format!("~{hours}h{extra_mins}m");
    }
    let extra_hours = hours % 24;
    if extra_hours == 0 {
        format!("~{days}d")
    } else {
        format!("~{days}d{extra_hours}h")
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use routectl_auth::OAuthStore;
    use routectl_auth::oauth::testing::{ScriptedRefresh, ScriptedRefreshFlow};
    use routectl_auth::oauth::types::{seat_key, unix_now};
    use routectl_testkit::ScopedEnv;

    use super::{humanize_remaining, refresh_with};

    /// A credentials file at `path` holding one expired seat.
    fn write_expired_seat(path: &Path, seat: &str) {
        let doc = serde_json::json!({
            "schema_version": 1,
            "providers": {
                seat: {
                    "access_token": "seeded-access-token",
                    "refresh_token": "seeded-refresh-token",
                    "token_type": "Bearer",
                    "expires_at_unix": unix_now() - 60,
                    "scopes": ["user:inference"],
                    "account": { "email": null, "account_id": null },
                    "obtained_at_unix": 0
                }
            }
        });
        std::fs::write(path, serde_json::to_vec_pretty(&doc).expect("json")).expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        }
    }

    /// The message `routectl refresh anthropic [--label <label>]` exits with
    /// when the token endpoint answers `script` for that seat.
    async fn refresh_message(
        label: Option<&str>,
        config_path: Option<&Path>,
        script: ScriptedRefresh,
    ) -> String {
        let (msg, posts) = refresh_failure(label, config_path, Some(script)).await;
        assert_eq!(posts, 1, "the refresh reached the token endpoint");
        msg
    }

    /// The message and token-endpoint POST count of a failed
    /// `routectl refresh anthropic [--label <label>]`. `script` `None`
    /// stores no login for the seat.
    async fn refresh_failure(
        label: Option<&str>,
        config_path: Option<&Path>,
        script: Option<ScriptedRefresh>,
    ) -> (String, u32) {
        let dir = tempfile::tempdir().expect("tempdir");
        let creds = dir.path().join("credentials.json");
        if script.is_some() {
            write_expired_seat(&creds, &seat_key("anthropic", label));
        }
        let flow = Arc::new(ScriptedRefreshFlow::new(
            script.unwrap_or(ScriptedRefresh::RefreshExpired),
        ));
        let store = OAuthStore::open_with_scripted_refresh_for_tests(&creds, flow.clone())
            .await
            .expect("open the scripted store");

        let err = refresh_with(&store, "anthropic", label, config_path)
            .await
            .expect_err("the scripted refresh fails");

        (err.to_string(), flow.post_count())
    }

    /// A refused refresh token, or a seat with no stored login, names the
    /// `routectl login` command for the same seat exactly once, and no
    /// command when the label has no exact printable form. The cause names
    /// the seat key only when it prints exactly as stored.
    #[tokio::test]
    async fn a_refused_refresh_names_the_login_for_the_same_seat() {
        const REFUSED: Option<ScriptedRefresh> = Some(ScriptedRefresh::RefreshExpired);
        const UNPRINTABLE: &str = "the seat for `anthropic` whose label holds characters \
                                   that cannot be printed";
        let rows = [
            (
                "refused, labeled",
                REFUSED,
                Some("work"),
                "oauth refresh failed: refresh token for `anthropic#work` expired or revoked",
                "re-run `routectl login anthropic --label work`",
            ),
            (
                "refused, unlabeled",
                REFUSED,
                None,
                "oauth refresh failed: refresh token for `anthropic` expired or revoked",
                "re-run `routectl login anthropic`",
            ),
            (
                "refused, control in label",
                REFUSED,
                Some("wo\u{1b}[2Jrk"),
                &format!(
                    "oauth refresh failed: refresh token for {UNPRINTABLE} expired or revoked"
                ),
                "re-run the same `routectl login anthropic` command",
            ),
            (
                "refused, CSI before label",
                REFUSED,
                Some("\u{1b}[2Jwork"),
                &format!(
                    "oauth refresh failed: refresh token for {UNPRINTABLE} expired or revoked"
                ),
                "re-run the same `routectl login anthropic` command",
            ),
            (
                "refused, non-ASCII label",
                REFUSED,
                Some("Jos\u{e9}"),
                "oauth refresh failed: refresh token for `anthropic#Jos\u{e9}` expired or revoked",
                "re-run `routectl login anthropic --label 'Jos\u{e9}'`",
            ),
            (
                "not logged in, labeled",
                None,
                Some("work"),
                "no credentials for `anthropic#work`",
                "re-run `routectl login anthropic --label work`",
            ),
            (
                "not logged in, unlabeled",
                None,
                None,
                "no credentials for `anthropic`",
                "re-run `routectl login anthropic`",
            ),
            (
                "not logged in, control in label",
                None,
                Some("wo\u{1b}[2Jrk"),
                &format!("no credentials for {UNPRINTABLE}"),
                "re-run the same `routectl login anthropic` command",
            ),
            (
                "not logged in, CSI before label",
                None,
                Some("\u{1b}[2Jwork"),
                &format!("no credentials for {UNPRINTABLE}"),
                "re-run the same `routectl login anthropic` command",
            ),
        ];
        for (row, script, label, cause, rerun) in rows {
            // Act
            let is_stored = script.is_some();
            let (msg, posts) = refresh_failure(label, None, script).await;

            // Assert
            assert_eq!(posts, u32::from(is_stored), "row `{row}`");
            assert_eq!(msg, format!("auth: {cause}; {rerun}"), "row `{row}`");
            assert_eq!(
                msg.contains("`anthropic#work`"),
                label == Some("work"),
                "row `{row}`: names only its own seat: {msg}"
            );
            assert!(!msg.chars().any(char::is_control), "row `{row}`: {msg}");
            assert_eq!(msg.matches("re-run").count(), 1, "row `{row}`: {msg}");
            assert_eq!(
                msg.matches("routectl login").count(),
                1,
                "row `{row}`: {msg}"
            );
            assert_eq!(
                msg.contains("--label"),
                rerun.contains("--label"),
                "row `{row}`: {msg}"
            );
        }
    }

    /// The rerun names the config file the refresh was given, so the login
    /// offers its config change to that same file.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_refused_refresh_rerun_names_a_chosen_config_file() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path().join("default"));
        let _env = ScopedEnv::unset("ROUTECTL_CONFIG");
        let config = dir.path().join("my configs").join("config.toml");

        // Act
        let msg =
            refresh_message(Some("work"), Some(&config), ScriptedRefresh::RefreshExpired).await;

        // Assert
        assert!(
            msg.ends_with(&format!(
                "re-run `routectl --config '{}' login anthropic --label work`",
                config.display()
            )),
            "{msg}"
        );
    }

    /// A failure a fresh login would not cure is the cause alone.
    #[tokio::test]
    async fn a_transient_refresh_failure_names_no_login() {
        let msg = refresh_message(Some("work"), None, ScriptedRefresh::Transient).await;

        assert!(msg.contains("`anthropic#work`"), "{msg}");
        assert!(!msg.contains("routectl login"), "{msg}");
    }

    #[test]
    fn humanize_zero() {
        assert_eq!(humanize_remaining(0), "now");
    }

    #[test]
    fn humanize_seconds() {
        assert_eq!(humanize_remaining(45), "45s");
    }

    #[test]
    fn humanize_minutes() {
        assert_eq!(humanize_remaining(180), "~3m");
    }

    #[test]
    fn humanize_hours() {
        assert_eq!(humanize_remaining(3600), "~1h");
        assert_eq!(humanize_remaining(3600 + 600), "~1h10m");
    }

    #[test]
    fn humanize_days() {
        assert_eq!(humanize_remaining(86_400), "~1d");
        assert_eq!(humanize_remaining(86_400 + 3600 * 5), "~1d5h");
    }
}
