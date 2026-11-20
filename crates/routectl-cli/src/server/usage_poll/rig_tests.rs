//! The usage poll's test rig: a router over one Anthropic and one Codex
//! managed seat whose models answer from stub providers (so a response
//! header reading can be fed without a vendor), a tempdir credential store,
//! one wiremock stub per vendor usage host behind a TLS front, a writer
//! channel the test owns, and a wall clock the test moves.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use futures::stream::BoxStream;
use routectl_auth::OAuthStore;
use routectl_core::upstream_meta::{AnthropicUnifiedQuota, UpstreamMeta};
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Choice, Error, Provider, Result, test_utils,
};
use routectl_router::router::RouteTargetStatus;
use routectl_router::{
    AccountReading, CURRENT_CONFIG_VERSION, ObservationStamp, PoolEligibility, ResolvedModel,
    Router,
};
use routectl_testkit::tls_front::TlsFront;
use routectl_usage::{UsageCounters, UsageHandle, WriterMessage};
use tokio::sync::mpsc;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Respond};

use super::client::UsagePollClient;
use super::schedule::RandomJitter;
use super::{UsagePollJob, UsagePollState, WallClock};

pub(super) const ANTHROPIC_SEAT: &str = "anthropic#a";
pub(super) const CODEX_SEAT: &str = "codex#c";
const ANTHROPIC_MODEL: &str = "claude-a";
const CODEX_MODEL: &str = "codex-c";
pub(super) const ANTHROPIC_HOST: &str = "api.anthropic.com";
pub(super) const CODEX_HOST: &str = "chatgpt.com";

/// Markers that must never reach a log line.
pub(super) const TOKEN_MARKER: &str = "synthetic-poll-token-9f2e";
pub(super) const ACCOUNT_ID_MARKER: &str = "synthetic-poll-account-41bd";
pub(super) const BODY_MARKER: &str = "synthetic-poll-body-c07a";
pub(super) const HEADER_MARKER: &str = "synthetic-poll-header-6e13";

/// The span first-sight due times are spread over.
const BOOT_SPREAD_MS: i64 = 5 * 60_000;

/// Capacity of the writer channel the rig owns.
pub(super) const WRITER_CAPACITY: usize = 8;

pub(super) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a post-epoch clock")
        .as_secs()
}

pub(super) fn now_ms() -> i64 {
    super::history::epoch_ms(SystemTime::now())
}

/// A wall clock that moves only when the test moves it.
pub(super) struct ManualClock(AtomicI64);

impl ManualClock {
    pub(super) const fn starting_at(ms: i64) -> Self {
        Self(AtomicI64::new(ms))
    }

    pub(super) fn set(&self, ms: i64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}

impl WallClock for ManualClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// A stub provider whose every response carries an Anthropic 5-hour
/// header reading at `utilization`, resetting at `reset_secs`.
struct HeaderQuotaProvider {
    utilization: String,
    reset_secs: u64,
}

#[async_trait]
impl Provider for HeaderQuotaProvider {
    fn id(&self) -> &'static str {
        "header-quota"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("header-quota", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        let mut quota = AnthropicUnifiedQuota::default();
        quota.utilization = Some(self.utilization.clone());
        quota.extras = vec![("5h-reset".into(), self.reset_secs.to_string())];
        Ok(ChatResponse {
            choices: vec![Choice {
                index: 0,
                message: test_utils::assistant_text_msg("ok"),
                finish_reason: Some("stop".into()),
                matched_stop_sequence: None,
                logprobs: None,
            }],
            upstream_meta: Some(UpstreamMeta::from_anthropic_unified(quota)),
            ..Default::default()
        })
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::upstream("header-quota", 500, "unused"))
    }
}

fn config_text() -> String {
    format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [providers.seat_a]\nformat = \"anthropic-messages\"\nbase_url = \"https://api.anthropic.com\"\ncredential = \"oauth://anthropic#a\"\nauth = \"bearer\"\n\n\
         [providers.codex_c]\nformat = \"openai-responses\"\nbase_url = \"https://chatgpt.com/backend-api/codex\"\ncredential = \"oauth://codex#c\"\nauth = \"bearer\"\n\n\
         [models.{ANTHROPIC_MODEL}]\nprovider = \"seat_a\"\nupstream = \"claude-sonnet-4-6\"\n\n\
         [models.{CODEX_MODEL}]\nprovider = \"codex_c\"\nupstream = \"gpt-5\"\n"
    )
}

fn model(nickname: &str, entry: &str, provider: &str, label: &str, reset: u64) -> ResolvedModel {
    let stub: Arc<dyn Provider> = Arc::new(HeaderQuotaProvider {
        utilization: "0.60".into(),
        reset_secs: reset,
    });
    ResolvedModel::new(nickname, entry, stub, "upstream").with_auth_secret_ref(
        routectl_auth::SecretRef::OAuth {
            provider: provider.into(),
            label: Some(label.into()),
        },
    )
}

fn build_router(header_reset_secs: u64) -> Router {
    let config = routectl_router::parse_config(&config_text()).expect("fixture parses");
    let mut router = Router::new(Arc::new(config));
    let models: BTreeMap<String, Arc<ResolvedModel>> = [
        (
            ANTHROPIC_MODEL,
            model(
                ANTHROPIC_MODEL,
                "seat_a",
                "anthropic",
                "a",
                header_reset_secs,
            ),
        ),
        (
            CODEX_MODEL,
            model(CODEX_MODEL, "codex_c", "codex", "c", header_reset_secs),
        ),
    ]
    .into_iter()
    .map(|(name, model)| (name.to_string(), Arc::new(model)))
    .collect();
    router.install_resolved_models(models);
    router
}

/// Write one record per `(seat, access token, expires_at_unix, account id)`.
pub(super) fn write_credentials(path: &Path, seats: &[(&str, &str, u64, Option<&str>)]) {
    let mut providers = serde_json::Map::new();
    for (key, token, expires_at, account_id) in seats {
        providers.insert(
            (*key).to_string(),
            serde_json::json!({
                "access_token": token,
                "refresh_token": "seeded-refresh-token",
                "token_type": "Bearer",
                "expires_at_unix": expires_at,
                "scopes": ["user:inference"],
                "account": { "email": null, "account_id": account_id },
                "obtained_at_unix": 0
            }),
        );
    }
    let doc = serde_json::json!({ "schema_version": 1, "providers": providers });
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
    std::fs::write(path, serde_json::to_vec_pretty(&doc).expect("json")).expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
}

pub(super) struct Rig {
    pub(super) creds: PathBuf,
    pub(super) store: Arc<OAuthStore>,
    pub(super) swap: Arc<ArcSwap<Router>>,
    pub(super) anthropic: MockServer,
    pub(super) codex: MockServer,
    pub(super) clock: Arc<ManualClock>,
    pub(super) counters: Arc<UsageCounters>,
    pub(super) writer_rx: mpsc::Receiver<WriterMessage>,
    usage: UsageHandle,
    client: UsagePollClient,
    _fronts: (TlsFront, TlsFront),
    _dir: tempfile::TempDir,
}

impl Rig {
    /// Both seats logged in with tokens far from expiry.
    pub(super) async fn new() -> Self {
        Self::with_seats(&[
            (ANTHROPIC_SEAT, TOKEN_MARKER, unix_now() + 3600, None),
            (
                CODEX_SEAT,
                TOKEN_MARKER,
                unix_now() + 3600,
                Some(ACCOUNT_ID_MARKER),
            ),
        ])
        .await
    }

    pub(super) async fn with_seats(seats: &[(&str, &str, u64, Option<&str>)]) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let creds = dir.path().join("credentials.json");
        write_credentials(&creds, seats);
        let store = Arc::new(OAuthStore::open(&creds).await.expect("open the store"));
        let header_reset_secs = unix_now() + 3600;
        let anthropic = MockServer::start().await;
        let codex = MockServer::start().await;
        let anthropic_front = TlsFront::start(ANTHROPIC_HOST, *anthropic.address()).await;
        let codex_front = TlsFront::start(CODEX_HOST, *codex.address()).await;
        let client = UsagePollClient::with_fronts_for_tests(&[
            (ANTHROPIC_HOST, &anthropic_front),
            (CODEX_HOST, &codex_front),
        ]);
        let (tx, writer_rx) = mpsc::channel(WRITER_CAPACITY);
        let counters = Arc::new(UsageCounters::default());
        let usage = routectl_usage::handle_over_channel_with(tx, Arc::clone(&counters));
        Self {
            creds,
            store,
            swap: Arc::new(ArcSwap::from_pointee(build_router(header_reset_secs))),
            anthropic,
            codex,
            clock: Arc::new(ManualClock(AtomicI64::new(now_ms()))),
            counters,
            writer_rx,
            usage,
            client,
            _fronts: (anthropic_front, codex_front),
            _dir: dir,
        }
    }

    pub(super) fn router(&self) -> Arc<Router> {
        self.swap.load_full()
    }

    /// A job over the rig's store, stubs, writer channel and clock, with
    /// seeded jitter.
    pub(super) fn job(&self) -> UsagePollJob {
        self.job_with_client(self.client.clone())
    }

    pub(super) fn job_with_client(&self, client: UsagePollClient) -> UsagePollJob {
        let clock: Arc<dyn WallClock> = self.clock.clone();
        UsagePollJob::with_parts(
            Arc::clone(&self.store),
            client,
            self.usage.clone(),
            UsagePollState::new(RandomJitter::seeded(7), clock),
            Arc::clone(&self.swap),
        )
    }

    pub(super) fn usage_handle(&self) -> UsageHandle {
        self.usage.clone()
    }

    /// Empty the writer channel, returning how many messages it held.
    pub(super) fn drain_writer(&mut self) -> usize {
        let mut count = 0;
        while self.writer_rx.try_recv().is_ok() {
            count += 1;
        }
        count
    }

    pub(super) fn clock_ms(&self) -> i64 {
        self.clock.now_ms()
    }

    /// Let `job` see every seat once at the current clock, then move the
    /// clock past the boot spread so every seat is due.
    pub(super) fn sight_then_pass_boot_spread(&self, job: &UsagePollJob) {
        let _ = super::super::seat_driver::SeatJob::due(job, &self.router());
        self.clock.set(self.clock_ms() + BOOT_SPREAD_MS);
    }

    /// Dispatch one request to the Anthropic seat, which feeds the meter a
    /// response header reading (5-hour window at 60%).
    pub(super) async fn feed_header_reading(&self) {
        let request = ChatRequest {
            model: ANTHROPIC_MODEL.into(),
            messages: Arc::from(vec![test_utils::user_msg("hi")]),
            ..Default::default()
        };
        self.router()
            .complete(request)
            .await
            .expect("the stub answers");
    }

    pub(super) async fn answer_anthropic(&self, response: impl Respond + 'static) {
        self.anthropic.reset().await;
        Mock::given(any())
            .respond_with(response)
            .mount(&self.anthropic)
            .await;
    }

    pub(super) async fn answer_codex(&self, response: impl Respond + 'static) {
        self.codex.reset().await;
        Mock::given(any())
            .respond_with(response)
            .mount(&self.codex)
            .await;
    }

    pub(super) async fn anthropic_hits(&self) -> usize {
        self.anthropic
            .received_requests()
            .await
            .expect("recording")
            .len()
    }

    pub(super) async fn codex_hits(&self) -> usize {
        self.codex
            .received_requests()
            .await
            .expect("recording")
            .len()
    }

    pub(super) fn reading(&self, account: &str) -> AccountReading {
        self.router()
            .account_readings(&ObservationStamp::now())
            .accounts
            .into_iter()
            .find(|reading| reading.account == account)
            .expect("the configured account is listed")
    }
}

/// A seat target's gate and pool counts with the elapsed-time fields
/// dropped, so two reads taken apart compare equal unless state changed.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct TargetView {
    pub(super) state_key: String,
    pub(super) account: Option<String>,
    pub(super) reason: Option<routectl_core::EligibilityReason>,
    pub(super) circuit: routectl_router::runtime_state::CircuitPhase,
    pub(super) half_open_probe_in_flight: bool,
    pub(super) last_outcome: Option<routectl_core::failure_class::LastOutcome>,
    pub(super) parked: Option<routectl_core::UpstreamReason>,
    pub(super) unusable: Option<routectl_router::runtime_state::UnusableCause>,
}

impl TargetView {
    pub(super) fn of(target: RouteTargetStatus) -> Self {
        Self {
            state_key: target.state_key,
            account: target.account,
            reason: target.gate.reason,
            circuit: target.gate.circuit,
            half_open_probe_in_flight: target.gate.half_open_probe_in_flight,
            last_outcome: target.gate.last_outcome,
            parked: target.gate.credential_park.map(|park| park.reason),
            unusable: target.gate.credential_unusable.map(|mark| mark.cause),
        }
    }
}

/// Everything a poll must never change, read at one instant.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Containment {
    pub(super) credentials_file: Vec<u8>,
    pub(super) targets: Vec<TargetView>,
    /// Each target's state key and its account's credential generation.
    pub(super) generations: Vec<(String, u64)>,
    pub(super) pools: Vec<PoolEligibility>,
}

impl Containment {
    pub(super) fn read(rig: &Rig) -> Self {
        let router = rig.router();
        let targets: Vec<TargetView> = router
            .status_targets(Instant::now())
            .into_iter()
            .map(TargetView::of)
            .collect();
        let generations = targets
            .iter()
            .map(|target| {
                let generation = router.credential_generation_for_tests(&target.state_key);
                (target.state_key.clone(), generation)
            })
            .collect();
        Self {
            credentials_file: std::fs::read(&rig.creds).expect("read the credentials file"),
            targets,
            generations,
            pools: router.pool_eligibility(Instant::now()),
        }
    }
}

/// The state key of the router target on `account`.
pub(super) fn state_key_of(router: &Router, account: &str) -> String {
    router
        .status_targets(Instant::now())
        .into_iter()
        .find(|target| target.account.as_deref() == Some(account))
        .expect("the account's target")
        .state_key
}

/// One account's accepted reading and placement-safe view, with the age
/// (which moves with the clock) dropped.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ReadingView {
    pub(super) report: routectl_router::ReportState,
    pub(super) last_accepted: Option<routectl_router::LastReading>,
    pub(super) windows: Vec<(
        routectl_router::ReadingWindow,
        SystemTime,
        bool,
        Option<f64>,
    )>,
}

impl ReadingView {
    pub(super) fn of(reading: AccountReading) -> Self {
        Self {
            report: reading.report,
            last_accepted: reading.last_accepted,
            windows: reading
                .windows
                .into_iter()
                .map(|window| {
                    let remaining = window.remaining();
                    (window.window, window.observed_at, window.stale, remaining)
                })
                .collect(),
        }
    }
}

/// Every message or field of `events` that carries a token, account id,
/// body or header marker.
pub(super) fn leaked(events: &[routectl_testkit::CapturedEvent]) -> Vec<String> {
    events
        .iter()
        .flat_map(|event| {
            std::iter::once(event.message.clone()).chain(
                event
                    .fields
                    .iter()
                    .map(|(name, value)| format!("{name}={value}")),
            )
        })
        .filter(|text| {
            [TOKEN_MARKER, ACCOUNT_ID_MARKER, BODY_MARKER, HEADER_MARKER]
                .iter()
                .any(|marker| text.contains(marker))
        })
        .collect()
}
