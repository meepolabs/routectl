//! `--region`: entries on the AWS cloud carrier (Bedrock and Bedrock's
//! managed surface). Loaded via `#[cfg(test)] #[path = "carrier_tests.rs"]
//! mod carrier_tests;` in `mod.rs`.

use super::*;

use routectl_router::{Auth, CURRENT_CONFIG_VERSION, Format, ProviderEntry};

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

const REGION: &str = "us-west-2";
const KEY: &str = "env://ROUTECTL_CARRIER_TEST_KEY";
const INLINE_CLOUD: &str = "cloud = { name = \"aws\", region = \"us-west-2\" }";

fn write_base(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("config.toml");
    std::fs::write(&path, format!("version = {CURRENT_CONFIG_VERSION}\n{BASE}")).unwrap();
    path
}

fn cloud_args(format: &str, credential: &str) -> ProviderAddArgs {
    ProviderAddArgs {
        format: Some(format.to_string()),
        login: None,
        label: None,
        pool: None,
        auth: None,
        name: "added".to_string(),
        base_url: None,
        region: Some(REGION.to_string()),
        api_key_env: None,
        secret_ref: Some(credential.to_string()),
        api_key_stdin: false,
        credential_source: None,
        overwrite: false,
        yes: true,
    }
}

/// No terminal, no login: a refused credential has no other way in.
struct NoTty;

#[async_trait]
impl AddIo for NoTty {
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
    async fn login(&self, _provider: &str, _label: Option<&str>) -> Result<()> {
        panic!("no carrier row runs a login")
    }
}

fn as_json(entry: &ProviderEntry) -> serde_json::Value {
    serde_json::to_value(entry).expect("entry serializes")
}

/// Each row: name, args, and the entry those args must write, built here
/// from the router's constructor rather than by the command.
fn cloud_rows() -> Vec<(&'static str, ProviderAddArgs, ProviderEntry)> {
    let mut rows = Vec::new();
    for (format_name, format) in [
        ("bedrock-invoke", Format::BedrockInvoke),
        ("bedrock-converse", Format::BedrockConverse),
    ] {
        for credential in ["aws://profile/p", "aws://chain"] {
            rows.push((
                format_name,
                cloud_args(format_name, credential),
                ProviderEntry::cloud_aws(format, REGION, credential, Auth::AwsSigv4),
            ));
        }
    }
    for (row, format) in [
        ("mantle anthropic-messages", Format::AnthropicMessages),
        ("mantle openai-chat", Format::OpenaiChat),
        ("mantle openai-responses", Format::OpenaiResponses),
    ] {
        rows.push((
            row,
            cloud_args(format.as_str(), KEY),
            ProviderEntry::cloud_aws(format, REGION, KEY, Auth::Bearer),
        ));
    }
    rows
}

#[tokio::test]
async fn each_cloud_row_writes_a_gated_inline_cloud_entry_equal_to_the_built_one() {
    for (row, args, expected) in cloud_rows() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = write_base(dir.path());
        let credential = args.secret_ref.clone().unwrap();

        // Act
        let result = run_with_io(&path, args, &NoTty).await;

        // Assert
        assert_eq!(
            result.as_ref().ok(),
            Some(&AddResult::Written),
            "row `{row}` {credential}: {result:?}"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        let config =
            gate(&text).unwrap_or_else(|e| panic!("row `{row}` {credential}: gate refused: {e:?}"));
        let written = config.providers.get("added").expect("entry written");
        assert_eq!(
            as_json(written),
            as_json(&expected),
            "row `{row}` {credential}"
        );
        assert!(
            text.lines().any(|line| line == INLINE_CLOUD),
            "row `{row}` {credential}: inline cloud line missing:\n{text}"
        );
        assert!(
            !text.contains("[providers.added.cloud]"),
            "row `{row}` {credential}: cloud written as a sub-table:\n{text}"
        );
    }
}

/// The default `auth` on the cloud is pinned by value: an `aws://`
/// credential signs, a key is a bearer (Anthropic Messages included, which
/// defaults to `x-api-key` only over a base URL).
#[tokio::test]
async fn an_omitted_auth_on_the_cloud_follows_the_credential() {
    let rows = [
        ("bedrock-invoke", "aws://chain", "aws-sigv4"),
        ("anthropic-messages", "aws://profile/p", "aws-sigv4"),
        ("anthropic-messages", KEY, "bearer"),
    ];
    for (format, credential, auth) in rows {
        let dir = tempfile::tempdir().unwrap();
        let path = write_base(dir.path());

        run_with_io(&path, cloud_args(format, credential), &NoTty)
            .await
            .unwrap_or_else(|e| panic!("{format} {credential}: {e}"));

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains(&format!("auth = \"{auth}\"")),
            "{format} {credential} must write auth = {auth}:\n{text}"
        );
    }
}

#[tokio::test]
async fn a_carrier_refusal_leaves_the_file_byte_identical() {
    let with = |mut args: ProviderAddArgs, edit: fn(&mut ProviderAddArgs)| {
        edit(&mut args);
        args
    };
    let rows = [
        (
            "--region with --base-url",
            with(cloud_args("openai-chat", KEY), |a| {
                a.base_url = Some("https://api.x.example/v1".into());
            }),
            "only one of `--base-url",
        ),
        (
            "bedrock-invoke without --region",
            with(cloud_args("bedrock-invoke", "aws://chain"), |a| {
                a.region = None;
            }),
            "--region <REGION>",
        ),
        (
            "bedrock-converse without --region",
            with(cloud_args("bedrock-converse", "aws://chain"), |a| {
                a.region = None;
            }),
            "--region <REGION>",
        ),
        (
            "gemini with --region",
            cloud_args("gemini", KEY),
            "no AWS cloud form",
        ),
        (
            "forwarded with --region",
            with(cloud_args("anthropic-messages", KEY), |a| {
                a.secret_ref = None;
                a.credential_source = Some("forwarded".into());
            }),
            "`--region` is not supported",
        ),
        (
            "--login with --region",
            with(cloud_args("anthropic-messages", KEY), |a| {
                a.format = None;
                a.secret_ref = None;
                a.login = Some("anthropic".into());
            }),
            "`--region` is not supported",
        ),
        (
            "--secret-ref forwarded",
            cloud_args("anthropic-messages", "forwarded"),
            "--credential-source forwarded",
        ),
        (
            "aws-sigv4 with a key",
            with(cloud_args("bedrock-invoke", KEY), |a| {
                a.auth = Some("aws-sigv4".into());
            }),
            "config error",
        ),
        (
            "x-api-key on the cloud",
            with(cloud_args("anthropic-messages", KEY), |a| {
                a.auth = Some("x-api-key".into());
            }),
            "config error",
        ),
    ];
    for (row, args, needle) in rows {
        let dir = tempfile::tempdir().unwrap();
        let path = write_base(dir.path());
        let before = std::fs::read(&path).unwrap();

        let err = run_with_io(&path, args, &NoTty)
            .await
            .expect_err(&format!("row `{row}` must be refused"));

        assert!(err.to_string().contains(needle), "row `{row}`: {err}");
        assert_eq!(std::fs::read(&path).unwrap(), before, "row `{row}`");
    }
}

/// A credential that does not parse is refused by the credential parser,
/// before the gate, and the error carries no byte of what was given. The accepted control
/// shows the same marker reaches the file when the credential is valid.
#[tokio::test]
async fn an_unparseable_credential_is_refused_without_echoing_it() {
    let rows = [
        ("aws://bogus", "aws://profile/<name> or aws://chain"),
        ("aws://profile/a/bogus", "exactly one profile name segment"),
        (
            "literal:x-bogus-secret",
            "literal: secret refs are not accepted",
        ),
        ("bogus-pasted-key", "unrecognized credential"),
    ];
    let marker = "bogus";
    for (credential, reason) in rows {
        let dir = tempfile::tempdir().unwrap();
        let path = write_base(dir.path());
        let before = std::fs::read(&path).unwrap();

        let err = run_with_io(&path, cloud_args("bedrock-invoke", credential), &NoTty)
            .await
            .expect_err(&format!("`{credential}` must be refused"));

        assert!(err.to_string().contains(reason), "`{credential}`: {err}");
        assert!(
            !err.to_string().contains(marker),
            "`{credential}` echoed: {err}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before, "`{credential}`");
    }

    let dir = tempfile::tempdir().unwrap();
    let path = write_base(dir.path());
    run_with_io(
        &path,
        cloud_args("bedrock-invoke", "aws://profile/bogus"),
        &NoTty,
    )
    .await
    .expect("positive control: a valid profile is accepted");
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("aws://profile/bogus"),
        "positive control: the accepted credential is written"
    );
}

/// A terminal that records every env offer, accepts it, and refuses the
/// hidden prompt so a run that skips the offer ends there.
#[derive(Default)]
struct OfferTty {
    offers: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl AddIo for OfferTty {
    fn stdin_is_terminal(&self) -> bool {
        true
    }
    fn read_stdin(&self) -> Result<String> {
        Ok(String::new())
    }
    fn confirm_env_offer(&self, var: &str) -> bool {
        self.offers.lock().unwrap().push(var.to_string());
        true
    }
    fn prompt_hidden(&self, _provider_name: &str) -> Result<String> {
        Err(routectl_core::Error::Config("prompted".into()))
    }
    async fn login(&self, _provider: &str, _label: Option<&str>) -> Result<()> {
        panic!("no carrier row runs a login")
    }
}

/// On the AWS cloud the key is not the vendor's own, so the vendor's
/// conventional variable is not offered; over a base URL (the control) the
/// same format offers it.
#[tokio::test]
#[serial_test::serial]
async fn the_cloud_skips_the_vendor_key_offer_that_a_base_url_makes() {
    let _key = routectl_testkit::ScopedEnv::set("ANTHROPIC_API_KEY", "offer-test-not-real");
    let mut cloud = cloud_args("anthropic-messages", KEY);
    cloud.secret_ref = None;
    let base_url = ProviderAddArgs {
        region: None,
        secret_ref: None,
        ..cloud_args("anthropic-messages", KEY)
    };

    let dir = tempfile::tempdir().unwrap();
    let path = write_base(dir.path());
    let io = OfferTty::default();
    let err = run_with_io(&path, cloud, &io)
        .await
        .expect_err("the cloud row reaches the prompt");
    assert!(err.to_string().contains("prompted"), "{err}");
    assert!(io.offers.lock().unwrap().is_empty(), "cloud offered a var");

    let dir = tempfile::tempdir().unwrap();
    let path = write_base(dir.path());
    let io = OfferTty::default();
    run_with_io(&path, base_url, &io)
        .await
        .expect("control: the base-URL row takes the offer");
    assert_eq!(*io.offers.lock().unwrap(), ["ANTHROPIC_API_KEY"]);
}
