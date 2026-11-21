//! The `User-Agent` the Anthropic and Codex usage calls present: the
//! `user_agent` of the provider entry the seat is reported under, else the
//! vendor's compiled default, read from the live config on every poll. Every
//! request goes to one wiremock stub behind a TLS front for each vendor
//! host; the router is built from the
//! config text by the daemon's own factory, and reloads go through the
//! daemon's reload handler.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use routectl_auth::{OAuthStore, SecretStore};
use routectl_router::{AccountIdentities, CURRENT_CONFIG_VERSION, Config, Router};
use routectl_testkit::tls_front::TlsFront;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::client::{UsagePollClient, usage_user_agent};
use super::rig::{
    ACCOUNT_ID_MARKER, ANTHROPIC_HOST, CODEX_HOST, ManualClock, TOKEN_MARKER, now_ms, unix_now,
    write_credentials,
};
use super::schedule::RandomJitter;
use super::{UsagePollJob, UsagePollState, WallClock};
use crate::server::reload::handle_config_reload;
use crate::server::router_build::build_router_from_config;
use crate::server::seat_driver::{ManagedSeat, SeatJob, managed_seats, sweep};
use crate::server::test_support::never_shutdown;

const SEAT: &str = "anthropic#a";
const CODEX_SEAT: &str = "codex#c";
const SHARED_SEAT: &str = "anthropic#shared";
/// Past the boot spread and every failure backoff the stub's 500 earns.
const PAST_EVERY_DEADLINE_MS: i64 = 20 * 60_000;
const CONFIGURED_UA: &str = "claude-cli/9.9.1 (external, cli)";
const CODEX_CONFIGURED_UA: &str = "codex_cli_rs/9.9.3 (Linux 6.1; x86_64) synthetic-term";
const RELOADED_UA: &str = "claude-cli/9.9.2 (external, cli)";

/// One `[providers]` entry: name, credential and optional `user_agent`.
/// An `oauth://codex#` credential makes it a Codex entry, any other an
/// Anthropic one.
type Entry<'a> = (&'a str, &'a str, Option<&'a str>);

/// The format, base URL and upstream model of an entry on `credential`.
fn entry_shape(credential: &str) -> (&'static str, &'static str, &'static str) {
    if credential.starts_with("oauth://codex#") {
        (
            "openai-responses",
            "https://chatgpt.com/backend-api/codex",
            "gpt-5",
        )
    } else {
        (
            "anthropic-messages",
            "https://api.anthropic.com",
            "claude-sonnet-4-6",
        )
    }
}

/// `entries` plus one model per entry, named so that model `m_<entry>`
/// sorts in the order the entries are given.
fn config_text(entries: &[Entry<'_>], usage_db: &Path) -> String {
    let mut text = format!(
        "version = {CURRENT_CONFIG_VERSION}\n[usage]\ndb_path = \"{}\"\n\n",
        usage_db.display()
    );
    for (index, (name, credential, user_agent)) in entries.iter().enumerate() {
        let (format, base_url, upstream) = entry_shape(credential);
        text.push_str(&format!(
            "[providers.{name}]\nformat = \"{format}\"\nbase_url = \"{base_url}\"\ncredential = \"{credential}\"\nauth = \"bearer\"\n"
        ));
        if let Some(user_agent) = user_agent {
            text.push_str(&format!("user_agent = \"{user_agent}\"\n"));
        }
        text.push_str(&format!(
            "\n[models.m{index}_{name}]\nprovider = \"{name}\"\nupstream = \"{upstream}\"\n\n"
        ));
    }
    text
}

struct UaRig {
    config_path: PathBuf,
    usage_db: PathBuf,
    config: Arc<Config>,
    secrets: Arc<dyn SecretStore>,
    swap: Arc<ArcSwap<Router>>,
    clock: Arc<ManualClock>,
    stub: MockServer,
    job: UsagePollJob,
    _fronts: [TlsFront; 2],
    _dir: tempfile::TempDir,
}

impl UaRig {
    /// A daemon-built router over `entries`, `account` logged in with a
    /// token far from expiry (and, for Codex, an account id), and both
    /// usage hosts fronting one stub answering 500 (any answer will do:
    /// only the request is under test).
    async fn new(entries: &[Entry<'_>], account: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let creds = dir.path().join("credentials.json");
        let account_id = account.starts_with("codex#").then_some(ACCOUNT_ID_MARKER);
        write_credentials(
            &creds,
            &[(account, TOKEN_MARKER, unix_now() + 3600, account_id)],
        );
        let store = Arc::new(OAuthStore::open(&creds).await.expect("open the store"));
        let usage_db = dir.path().join("usage.db");
        let config_path = dir.path().join("config.toml");
        let text = config_text(entries, &usage_db);
        std::fs::write(&config_path, &text).expect("write config");
        let config = Arc::new(routectl_router::parse_config(&text).expect("fixture parses"));
        let secrets: Arc<dyn SecretStore> = store.clone();
        let router = build_router_from_config(config.clone(), secrets.clone())
            .await
            .expect("router builds");
        let stub = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(500))
            .mount(&stub)
            .await;
        let fronts = [
            TlsFront::start(ANTHROPIC_HOST, *stub.address()).await,
            TlsFront::start(CODEX_HOST, *stub.address()).await,
        ];
        let client = UsagePollClient::with_fronts_for_tests(&[
            (ANTHROPIC_HOST, &fronts[0]),
            (CODEX_HOST, &fronts[1]),
        ]);
        let clock = Arc::new(ManualClock::starting_at(now_ms()));
        let wall: Arc<dyn WallClock> = clock.clone();
        let swap = Arc::new(ArcSwap::from_pointee(router));
        let job = UsagePollJob::with_parts(
            store,
            client,
            routectl_usage::handle_with_closed_channel(),
            UsagePollState::new(RandomJitter::seeded(5), wall),
            Arc::clone(&swap),
        );
        Self {
            config_path,
            usage_db,
            config,
            secrets,
            swap,
            clock,
            stub,
            job,
            _fronts: fronts,
            _dir: dir,
        }
    }

    /// One sweep after every deadline has passed, through the live router.
    async fn poll(&self) {
        let _ = self.job.due(&self.swap.load_full());
        self.clock.set(self.clock.now_ms() + PAST_EVERY_DEADLINE_MS);
        sweep(&self.job, &self.swap.load_full()).await;
    }

    async fn reload_to(&mut self, entries: &[Entry<'_>]) {
        std::fs::write(&self.config_path, config_text(entries, &self.usage_db))
            .expect("rewrite config");
        self.config = Box::pin(handle_config_reload(
            Some(&self.config_path),
            &self.config,
            self.secrets.clone(),
            &self.swap,
            &routectl_usage::handle_with_closed_channel(),
            &mut never_shutdown(),
        ))
        .await
        .expect("the reload applies")
        .expect("a successful reload returns its config");
    }

    /// The path of every request the stub saw.
    async fn paths(&self) -> Vec<String> {
        self.stub
            .received_requests()
            .await
            .expect("recording")
            .iter()
            .map(|request| request.url.path().to_string())
            .collect()
    }

    /// Every `user-agent` header value of every request the stub saw, as
    /// raw bytes, one list per request.
    async fn user_agents(&self) -> Vec<Vec<Vec<u8>>> {
        self.stub
            .received_requests()
            .await
            .expect("recording")
            .iter()
            .map(|request| {
                request
                    .headers
                    .get_all("user-agent")
                    .iter()
                    .map(|value| value.as_bytes().to_vec())
                    .collect()
            })
            .collect()
    }
}

fn one_header(bytes: &str) -> Vec<Vec<u8>> {
    vec![bytes.as_bytes().to_vec()]
}

fn default_ua() -> &'static str {
    routectl_core::identity::anthropic::default_claude_code_user_agent()
}

fn codex_default_ua() -> &'static str {
    routectl_core::identity::codex::codex_user_agent()
}

const ANTHROPIC_PATH: &str = "/api/oauth/usage";
const CODEX_PATH: &str = "/backend-api/wham/usage";

/// One vendor's seat: its entry credential, account key and usage path.
struct Vendor {
    credential: &'static str,
    account: &'static str,
    path: &'static str,
}

const ANTHROPIC: Vendor = Vendor {
    credential: "oauth://anthropic#a",
    account: SEAT,
    path: ANTHROPIC_PATH,
};

const CODEX: Vendor = Vendor {
    credential: "oauth://codex#c",
    account: CODEX_SEAT,
    path: CODEX_PATH,
};

#[tokio::test]
async fn the_usage_call_sends_the_seat_entrys_user_agent_or_the_default() {
    // (row, vendor, the entry's user_agent, the exact header bytes sent)
    let rows = [
        (
            "anthropic configured",
            ANTHROPIC,
            Some(CONFIGURED_UA),
            CONFIGURED_UA,
        ),
        (
            "anthropic non-ASCII",
            ANTHROPIC,
            Some("agent/caf\u{e9}"),
            "agent/caf\u{e9}",
        ),
        ("anthropic unset", ANTHROPIC, None, default_ua()),
        (
            "codex configured",
            CODEX,
            Some(CODEX_CONFIGURED_UA),
            CODEX_CONFIGURED_UA,
        ),
        (
            "codex non-ASCII",
            CODEX,
            Some("agent/caf\u{e9}"),
            "agent/caf\u{e9}",
        ),
        ("codex unset", CODEX, None, codex_default_ua()),
    ];
    for (row, vendor, configured, expected) in rows {
        // Arrange
        let rig = UaRig::new(&[("seat_a", vendor.credential, configured)], vendor.account).await;

        // Act
        rig.poll().await;

        // Assert
        assert_eq!(rig.paths().await, vec![vendor.path.to_string()], "{row}");
        assert_eq!(rig.user_agents().await, vec![one_header(expected)], "{row}");
    }
}

#[test]
fn with_no_configured_value_no_override_is_sent() {
    let resolved = usage_user_agent(None);

    assert_eq!(resolved.configured, None);
    assert!(!resolved.configured_invalid);
}

#[tokio::test]
async fn a_login_two_entries_name_uses_the_entry_status_accounts_reports() {
    // Arrange: the larger entry's model sorts first, so a first-met pick
    // would take it.
    let entries: [Entry<'_>; 2] = [
        ("zeta", "oauth://anthropic#shared", Some(RELOADED_UA)),
        ("alpha", "oauth://anthropic#shared", Some(CONFIGURED_UA)),
    ];
    let rig = UaRig::new(&entries, SHARED_SEAT).await;
    let reported = AccountIdentities::from_config(&rig.config)
        .get(SHARED_SEAT)
        .expect("the shared login is listed")
        .provider_entry
        .clone();
    assert_eq!(reported, "alpha", "premise: the reported entry");

    // Act
    rig.poll().await;

    // Assert
    assert_eq!(rig.user_agents().await, vec![one_header(CONFIGURED_UA)]);
}

#[tokio::test]
async fn a_reload_that_changes_user_agent_changes_the_next_polls_header() {
    // Arrange
    let mut rig = UaRig::new(
        &[("seat_a", "oauth://anthropic#a", Some(CONFIGURED_UA))],
        SEAT,
    )
    .await;
    rig.poll().await;

    // Act: change it, then remove it.
    rig.reload_to(&[("seat_a", "oauth://anthropic#a", Some(RELOADED_UA))])
        .await;
    rig.poll().await;
    rig.reload_to(&[("seat_a", "oauth://anthropic#a", None)])
        .await;
    rig.poll().await;

    // Assert
    assert_eq!(
        rig.user_agents().await,
        vec![
            one_header(CONFIGURED_UA),
            one_header(RELOADED_UA),
            one_header(default_ua()),
        ]
    );
}

/// A configured value no header can carry, which the config parser
/// accepts as a string.
const INVALID_UA: &str = "agent/1.0\r\nx-injected: synthetic-ua-marker-3b7d";

#[tokio::test]
async fn an_invalid_configured_user_agent_sends_the_default_and_logs_no_value() {
    // (row, vendor, the vendor's compiled default)
    let rows = [
        ("anthropic", ANTHROPIC, default_ua()),
        ("codex", CODEX, codex_default_ua()),
    ];
    for (row, vendor, expected) in rows {
        // Arrange: the seat as the config would carry it, run directly so
        // the router factory never sees the value.
        let rig = UaRig::new(&[("seat_a", vendor.credential, None)], vendor.account).await;
        let mut seat = managed_seats(&rig.swap.load().config)
            .into_iter()
            .find(|seat| seat.account == vendor.account)
            .expect("the managed seat");
        seat.user_agent = configured(vendor.credential, INVALID_UA);

        // Act
        let (_, events) = routectl_testkit::with_capture(async { rig.job.run(&seat).await }).await;

        // Assert: the default went out, alone, and nothing logged the value.
        assert_eq!(rig.paths().await, vec![vendor.path.to_string()], "{row}");
        assert_eq!(rig.user_agents().await, vec![one_header(expected)], "{row}");
        let refused: Vec<_> = events
            .iter()
            .filter(|event| event.field("event") == Some("usage_poll_user_agent_refused"))
            .collect();
        let [refused] = refused.as_slice() else {
            panic!("{row}: one refusal line, saw {}", refused.len());
        };
        assert_eq!(refused.level, tracing::Level::WARN, "{row}");
        assert_eq!(
            refused.field("reason"),
            Some("invalid_header_value"),
            "{row}"
        );
        assert_eq!(refused.field("account"), Some(vendor.account), "{row}");
        let flattened: Vec<String> = events
            .iter()
            .flat_map(|event| {
                std::iter::once(event.message.clone()).chain(
                    event
                        .fields
                        .iter()
                        .map(|(name, value)| format!("{name}={value}")),
                )
            })
            .collect();
        let account_field = format!("account={}", vendor.account);
        assert!(
            flattened.contains(&account_field),
            "{row}: premise: the refusal's operator-text field is in the scan: {flattened:?}"
        );
        assert!(
            flattened
                .iter()
                .any(|text| text.contains("configured user_agent")),
            "{row}: premise: the refusal's message is in the scan: {flattened:?}"
        );
        let leaked: Vec<&String> = flattened
            .iter()
            .filter(|text| text.contains("synthetic-ua-marker") || text.contains("agent/1.0"))
            .collect();
        assert_eq!(leaked, Vec::<&String>::new(), "{row}");
    }
}

#[test]
fn a_seat_and_its_user_agent_never_print_the_value_in_debug() {
    let seat = ManagedSeat {
        entry: "seat_a".into(),
        provider: "anthropic".into(),
        label: Some("a".into()),
        account: SEAT.into(),
        user_agent: configured(ANTHROPIC.credential, CONFIGURED_UA),
    };

    // (row, Debug output, a field each output must render)
    let rows = [
        ("seat account", format!("{seat:?}"), SEAT.to_string()),
        (
            "seat user_agent",
            format!("{seat:?}"),
            "user_agent: Some(".to_string(),
        ),
        (
            "valid user agent",
            format!("{:?}", usage_user_agent(Some(CONFIGURED_UA))),
            "configured_invalid: false".to_string(),
        ),
        (
            "invalid user agent",
            format!("{:?}", usage_user_agent(Some(INVALID_UA))),
            "configured_invalid: true".to_string(),
        ),
    ];
    for (row, rendered, control) in rows {
        assert!(rendered.contains(&control), "{row}: premise: {rendered}");
        for marker in ["9.9.1", "synthetic-ua-marker", "agent/1.0"] {
            assert!(!rendered.contains(marker), "{row}: {marker}: {rendered}");
        }
    }
}

fn configured(
    credential: &str,
    value: &str,
) -> Option<crate::server::seat_driver::ConfiguredUserAgent> {
    let config = routectl_router::parse_config(&config_text(
        &[(
            "seat_a",
            credential,
            Some(&value.escape_default().to_string()),
        )],
        Path::new("/nonexistent/usage.db"),
    ))
    .expect("the value parses as a config string");
    managed_seats(&config)
        .into_iter()
        .next()
        .and_then(|seat| seat.user_agent)
}
