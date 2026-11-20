//! The `User-Agent` the Anthropic usage call presents: the `user_agent` of
//! the provider entry the seat is reported under, else the compiled default,
//! read from the live config on every poll. Every request goes to a wiremock
//! stub behind a TLS front for the vendor host; the router is built from the
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

use super::client::{UsagePollClient, anthropic_user_agent};
use super::rig::{ANTHROPIC_HOST, ManualClock, TOKEN_MARKER, now_ms, unix_now, write_credentials};
use super::schedule::RandomJitter;
use super::{UsagePollJob, UsagePollState, WallClock};
use crate::server::reload::handle_config_reload;
use crate::server::router_build::build_router_from_config;
use crate::server::seat_driver::{ManagedSeat, SeatJob, managed_seats, sweep};
use crate::server::test_support::never_shutdown;

const SEAT: &str = "anthropic#a";
const SHARED_SEAT: &str = "anthropic#shared";
/// Past the boot spread and every failure backoff the stub's 500 earns.
const PAST_EVERY_DEADLINE_MS: i64 = 20 * 60_000;
const CONFIGURED_UA: &str = "claude-cli/9.9.1 (external, cli)";
const RELOADED_UA: &str = "claude-cli/9.9.2 (external, cli)";

/// One `[providers]` entry: name, credential and optional `user_agent`.
type Entry<'a> = (&'a str, &'a str, Option<&'a str>);

/// `entries` plus one model per entry, named so that model `m_<entry>`
/// sorts in the order the entries are given.
fn config_text(entries: &[Entry<'_>], usage_db: &Path) -> String {
    let mut text = format!(
        "version = {CURRENT_CONFIG_VERSION}\n[usage]\ndb_path = \"{}\"\n\n",
        usage_db.display()
    );
    for (index, (name, credential, user_agent)) in entries.iter().enumerate() {
        text.push_str(&format!(
            "[providers.{name}]\nformat = \"anthropic-messages\"\nbase_url = \"https://api.anthropic.com\"\ncredential = \"{credential}\"\nauth = \"bearer\"\n"
        ));
        if let Some(user_agent) = user_agent {
            text.push_str(&format!("user_agent = \"{user_agent}\"\n"));
        }
        text.push_str(&format!(
            "\n[models.m{index}_{name}]\nprovider = \"{name}\"\nupstream = \"claude-sonnet-4-6\"\n\n"
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
    _front: TlsFront,
    _dir: tempfile::TempDir,
}

impl UaRig {
    /// A daemon-built router over `entries`, `account` logged in with a
    /// token far from expiry, and the Anthropic usage host answering 500
    /// (any answer will do: only the request is under test).
    async fn new(entries: &[Entry<'_>], account: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let creds = dir.path().join("credentials.json");
        write_credentials(&creds, &[(account, TOKEN_MARKER, unix_now() + 3600, None)]);
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
        let front = TlsFront::start(ANTHROPIC_HOST, *stub.address()).await;
        let client = UsagePollClient::with_fronts_for_tests(&[(ANTHROPIC_HOST, &front)]);
        let clock = Arc::new(ManualClock::starting_at(now_ms()));
        let wall: Arc<dyn WallClock> = clock.clone();
        let job = UsagePollJob::with_parts(
            store,
            client,
            routectl_usage::handle_with_closed_channel(),
            UsagePollState::new(RandomJitter::seeded(5), wall),
        );
        Self {
            config_path,
            usage_db,
            config,
            secrets,
            swap: Arc::new(ArcSwap::from_pointee(router)),
            clock,
            stub,
            job,
            _front: front,
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

#[tokio::test]
async fn the_usage_call_sends_the_seat_entrys_user_agent_or_the_default() {
    // (row, the entry's user_agent, the exact header bytes sent)
    let rows = [
        ("configured", Some(CONFIGURED_UA), CONFIGURED_UA),
        ("non-ASCII", Some("agent/caf\u{e9}"), "agent/caf\u{e9}"),
        ("unset", None, default_ua()),
    ];
    for (row, configured, expected) in rows {
        // Arrange
        let rig = UaRig::new(&[("seat_a", "oauth://anthropic#a", configured)], SEAT).await;

        // Act
        rig.poll().await;

        // Assert
        assert_eq!(rig.user_agents().await, vec![one_header(expected)], "{row}");
    }
}

#[test]
fn with_no_configured_value_the_header_is_the_compiled_default_byte_for_byte() {
    let resolved = anthropic_user_agent(None);

    assert_eq!(resolved.value.as_bytes(), default_ua().as_bytes());
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
    // Arrange: the seat as the config would carry it, run directly so the
    // router factory never sees the value.
    let rig = UaRig::new(&[("seat_a", "oauth://anthropic#a", None)], SEAT).await;
    let mut seat = managed_seats(&rig.swap.load().config)
        .into_iter()
        .find(|seat| seat.account == SEAT)
        .expect("the managed seat");
    seat.user_agent = configured(INVALID_UA);

    // Act
    let (_, events) = routectl_testkit::with_capture(async { rig.job.run(&seat).await }).await;

    // Assert: the default went out, alone, and nothing logged the value.
    assert_eq!(rig.user_agents().await, vec![one_header(default_ua())]);
    let refused: Vec<_> = events
        .iter()
        .filter(|event| event.field("event") == Some("usage_poll_user_agent_refused"))
        .collect();
    let [refused] = refused.as_slice() else {
        panic!("one refusal line, saw {}", refused.len());
    };
    assert_eq!(refused.level, tracing::Level::WARN);
    assert_eq!(refused.field("reason"), Some("invalid_header_value"));
    assert_eq!(refused.field("account"), Some(SEAT));
    let leaked: Vec<String> = events
        .iter()
        .flat_map(|event| {
            std::iter::once(event.message.clone()).chain(
                event
                    .fields
                    .iter()
                    .map(|(name, value)| format!("{name}={value}")),
            )
        })
        .filter(|text| text.contains("synthetic-ua-marker") || text.contains("agent/1.0"))
        .collect();
    assert_eq!(leaked, Vec::<String>::new());
}

#[test]
fn a_seat_and_its_user_agent_never_print_the_value_in_debug() {
    let seat = ManagedSeat {
        entry: "seat_a".into(),
        provider: "anthropic".into(),
        label: Some("a".into()),
        account: SEAT.into(),
        user_agent: configured(CONFIGURED_UA),
    };

    let rendered = format!(
        "{seat:?} {:?} {:?}",
        anthropic_user_agent(Some(CONFIGURED_UA)),
        anthropic_user_agent(Some(INVALID_UA)),
    );

    assert!(rendered.contains(SEAT), "premise: the seat rendered");
    assert!(!rendered.contains("9.9.1"), "{rendered}");
    assert!(!rendered.contains("synthetic-ua-marker"), "{rendered}");
}

fn configured(value: &str) -> Option<crate::server::seat_driver::ConfiguredUserAgent> {
    let config = routectl_router::parse_config(&config_text(
        &[(
            "seat_a",
            "oauth://anthropic#a",
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
