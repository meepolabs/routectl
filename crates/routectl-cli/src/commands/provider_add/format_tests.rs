//! `--format` / `--auth` / `--login` entry shapes. Loaded via
//! `#[cfg(test)] #[path = "format_tests.rs"] mod format_tests;` in `mod.rs`.

use super::*;

use routectl_router::{Auth, CURRENT_CONFIG_VERSION, ProviderEntry};

use crate::commands::edit_pipeline::gate;

const BASE: &str = "\
[server]
host = \"127.0.0.1\"
port = 8787

[providers.fast]
format = \"openai-chat\"
base_url = \"http://127.0.0.1:1\"
credential = \"env://ROUTECTL_TEST_KEY\"
auth = \"bearer\"

[models.gpt]
provider = \"fast\"
upstream = \"gpt-4o\"

[aliases]
default = \"gpt\"
";

fn write_base(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("config.toml");
    std::fs::write(&path, format!("version = {CURRENT_CONFIG_VERSION}\n{BASE}")).unwrap();
    path
}

fn format_args(format: &str, credential: Option<&str>) -> ProviderAddArgs {
    ProviderAddArgs {
        format: Some(format.to_string()),
        login: None,
        label: None,
        auth: None,
        name: "added".to_string(),
        base_url: None,
        api_key_env: None,
        secret_ref: credential.map(str::to_string),
        api_key_stdin: false,
        credential_source: None,
        overwrite: false,
        yes: true,
    }
}

fn login_args(vendor: &str) -> ProviderAddArgs {
    ProviderAddArgs {
        format: None,
        login: Some(vendor.to_string()),
        ..format_args("unused", None)
    }
}

fn labelled_login_args(vendor: &str, label: &str) -> ProviderAddArgs {
    ProviderAddArgs {
        label: Some(label.to_string()),
        ..login_args(vendor)
    }
}

fn with_base_url(mut args: ProviderAddArgs, url: &str) -> ProviderAddArgs {
    args.base_url = Some(url.to_string());
    args
}

fn with_auth(mut args: ProviderAddArgs, auth: &str) -> ProviderAddArgs {
    args.auth = Some(auth.to_string());
    args
}

fn forwarded_args() -> ProviderAddArgs {
    let mut args = format_args("anthropic-messages", None);
    args.credential_source = Some("forwarded".to_string());
    args
}

/// A login stub: records each vendor and seat label and succeeds, touching
/// nothing.
#[derive(Default)]
struct LoginIo {
    logins: std::sync::Mutex<Vec<(String, Option<String>)>>,
}

#[async_trait]
impl AddIo for LoginIo {
    fn stdin_is_terminal(&self) -> bool {
        false
    }
    fn read_stdin(&self) -> Result<String> {
        Ok(String::new())
    }
    fn confirm_env_offer(&self, _var: &str) -> bool {
        false
    }
    fn prompt_hidden(&self, _provider_name: &str) -> Result<String> {
        Ok(String::new())
    }
    async fn login(&self, provider: &str, label: Option<&str>) -> Result<()> {
        self.logins
            .lock()
            .unwrap()
            .push((provider.to_string(), label.map(str::to_string)));
        Ok(())
    }
}

fn as_json(entry: &ProviderEntry) -> serde_json::Value {
    serde_json::to_value(entry).expect("entry serializes")
}

const KEY: &str = "env://ROUTECTL_FORMAT_TEST_KEY";

/// The login (vendor, seat label) a row expects the command to run.
type ExpectedLogin = Option<(&'static str, Option<&'static str>)>;

/// Each row: name, args, the entry those args must write (built here from
/// the router's constructors, independently of the command), and the login
/// the command must run.
fn shape_rows() -> Vec<(&'static str, ProviderAddArgs, ProviderEntry, ExpectedLogin)> {
    vec![
        (
            "anthropic-messages, default host",
            format_args("anthropic-messages", Some(KEY)),
            ProviderEntry::anthropic_api(KEY),
            None,
        ),
        (
            "anthropic-messages over base_url",
            with_base_url(
                format_args("anthropic-messages", Some(KEY)),
                "https://api.deepseek.com/anthropic",
            ),
            ProviderEntry::anthropic_api(KEY).with_base_url("https://api.deepseek.com/anthropic"),
            None,
        ),
        (
            "openai-chat over base_url",
            with_base_url(
                format_args("openai-chat", Some(KEY)),
                "https://api.x.example/v1",
            ),
            ProviderEntry::openai_compat("https://api.x.example/v1", KEY),
            None,
        ),
        (
            "openai-responses over base_url",
            with_base_url(
                format_args("openai-responses", Some(KEY)),
                "https://api.openai.com/v1",
            ),
            ProviderEntry::openai_responses(KEY).with_base_url("https://api.openai.com/v1"),
            None,
        ),
        (
            "gemini",
            format_args("gemini", Some(KEY)),
            ProviderEntry::gemini(KEY),
            None,
        ),
        (
            "forwarded",
            forwarded_args(),
            ProviderEntry::anthropic_api("forwarded").with_auth(Auth::Bearer),
            None,
        ),
        (
            "--login anthropic",
            login_args("anthropic"),
            ProviderEntry::for_managed_login("anthropic", "oauth://anthropic").unwrap(),
            Some(("anthropic", None)),
        ),
        (
            "--login codex",
            login_args("codex"),
            ProviderEntry::for_managed_login("codex", "oauth://codex").unwrap(),
            Some(("codex", None)),
        ),
        (
            "--login anthropic --label work",
            labelled_login_args("anthropic", "work"),
            ProviderEntry::for_managed_login("anthropic", "oauth://anthropic#work").unwrap(),
            Some(("anthropic", Some("work"))),
        ),
        (
            "--login codex --label work",
            labelled_login_args("codex", "work"),
            ProviderEntry::for_managed_login("codex", "oauth://codex#work").unwrap(),
            Some(("codex", Some("work"))),
        ),
    ]
}

#[tokio::test]
async fn each_format_and_login_writes_a_gated_entry_equal_to_the_built_one() {
    for (row, args, expected, login) in shape_rows() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = write_base(dir.path());
        let io = LoginIo::default();

        // Act
        let result = run_with_io(&path, args, &io).await;

        // Assert
        assert_eq!(result.ok(), Some(AddResult::Written), "row `{row}`");
        let text = std::fs::read_to_string(&path).unwrap();
        let config = gate(&text).unwrap_or_else(|e| panic!("row `{row}`: gate refused: {e:?}"));
        let written = config.providers.get("added").expect("entry written");
        assert_eq!(as_json(written), as_json(&expected), "row `{row}`");
        let logins: Vec<(String, Option<String>)> = login
            .into_iter()
            .map(|(vendor, label)| (vendor.to_string(), label.map(str::to_string)))
            .collect();
        assert_eq!(*io.logins.lock().unwrap(), logins, "row `{row}`");
    }
}

/// The default `auth` per format is what the row above writes; this pins
/// the value itself, so a changed default cannot hide behind a constructor
/// that changed with it.
#[tokio::test]
async fn an_omitted_auth_writes_the_format_default() {
    let rows = [
        ("anthropic-messages", None, "x-api-key"),
        ("openai-chat", Some("https://api.x.example/v1"), "bearer"),
        (
            "openai-responses",
            Some("https://api.openai.com/v1"),
            "bearer",
        ),
        ("gemini", None, "x-goog-api-key"),
    ];
    for (format, base_url, auth) in rows {
        let dir = tempfile::tempdir().unwrap();
        let path = write_base(dir.path());
        let mut args = format_args(format, Some(KEY));
        args.base_url = base_url.map(str::to_string);

        run_with_io(&path, args, &LoginIo::default())
            .await
            .unwrap_or_else(|e| panic!("format `{format}`: {e}"));

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains(&format!("auth = \"{auth}\"")),
            "format `{format}` must write auth = {auth}:\n{text}"
        );
    }
}

#[tokio::test]
async fn an_explicit_auth_the_router_accepts_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_base(dir.path());
    let args = with_auth(format_args("anthropic-messages", Some(KEY)), "bearer");

    run_with_io(&path, args, &LoginIo::default())
        .await
        .expect("bearer is accepted on anthropic-messages");

    let config = gate(&std::fs::read_to_string(&path).unwrap()).expect("gate passes");
    assert_eq!(config.providers.get("added").unwrap().auth, Auth::Bearer);
}

#[tokio::test]
async fn an_explicit_auth_the_router_refuses_leaves_the_file_byte_identical() {
    let rows = [
        (
            "x-goog-api-key on openai-chat",
            with_base_url(
                with_auth(format_args("openai-chat", Some(KEY)), "x-goog-api-key"),
                "https://api.x.example/v1",
            ),
        ),
        (
            "x-api-key on gemini",
            with_auth(format_args("gemini", Some(KEY)), "x-api-key"),
        ),
        (
            "x-api-key on forwarded",
            with_auth(forwarded_args(), "x-api-key"),
        ),
    ];
    for (row, args) in rows {
        let dir = tempfile::tempdir().unwrap();
        let path = write_base(dir.path());
        let before = std::fs::read(&path).unwrap();

        let result = run_with_io(&path, args, &LoginIo::default()).await;

        assert!(result.is_err(), "row `{row}` must be refused");
        assert_eq!(std::fs::read(&path).unwrap(), before, "row `{row}`");
    }
}

#[tokio::test]
async fn format_and_login_select_exactly_one_target() {
    let both = ProviderAddArgs {
        login: Some("anthropic".to_string()),
        ..format_args("anthropic-messages", None)
    };
    let neither = ProviderAddArgs {
        format: None,
        ..format_args("anthropic-messages", Some(KEY))
    };
    let rows = [
        ("both", both, "only one of"),
        ("neither", neither, "--format"),
        (
            "login with --auth",
            with_auth(login_args("anthropic"), "bearer"),
            "--auth",
        ),
        ("unknown login", login_args("xai"), "supported logins"),
        (
            "login with --api-key-env",
            ProviderAddArgs {
                api_key_env: Some("ROUTECTL_FORMAT_TEST_KEY".to_string()),
                ..login_args("anthropic")
            },
            "drop the `--api-key-env`",
        ),
        (
            "login with --secret-ref",
            ProviderAddArgs {
                secret_ref: Some(KEY.to_string()),
                ..login_args("anthropic")
            },
            "drop the `--api-key-env`",
        ),
        (
            "login with --api-key-stdin",
            ProviderAddArgs {
                api_key_stdin: true,
                ..login_args("anthropic")
            },
            "drop the `--api-key-env`",
        ),
        (
            "login with --credential-source",
            ProviderAddArgs {
                credential_source: Some("own".to_string()),
                ..login_args("anthropic")
            },
            "`--credential-source` is not supported",
        ),
        (
            "--label with --format",
            ProviderAddArgs {
                label: Some("work".to_string()),
                api_key_stdin: true,
                ..format_args("anthropic-messages", None)
            },
            "only valid with `--login",
        ),
        (
            "--label with neither target",
            ProviderAddArgs {
                format: None,
                label: Some("work".to_string()),
                ..format_args("anthropic-messages", Some(KEY))
            },
            "only valid with `--login",
        ),
        (
            "blank --label with --login",
            labelled_login_args("anthropic", "  "),
            "--label must not be empty",
        ),
    ];
    for (row, args, needle) in rows {
        let dir = tempfile::tempdir().unwrap();
        let path = write_base(dir.path());
        let before = std::fs::read(&path).unwrap();
        let io = LoginIo::default();

        let err = run_with_io(&path, args, &io)
            .await
            .expect_err(&format!("row `{row}` must be refused"));

        assert!(err.to_string().contains(needle), "row `{row}`: {err}");
        assert_eq!(std::fs::read(&path).unwrap(), before, "row `{row}`");
        assert!(io.logins.lock().unwrap().is_empty(), "row `{row}`");
    }
}
