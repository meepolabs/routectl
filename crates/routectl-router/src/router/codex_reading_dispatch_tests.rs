//! Codex response headers reach the account meter through a real dispatch:
//! every window the family reports, keyed and classed by the length it
//! reports, readable through `Router::account_readings`, and ranked by the
//! pace rule like any other account.

use super::*;

use async_trait::async_trait;
use routectl_core::upstream_meta::{CodexQuota, UpstreamMeta};
use routectl_core::{Choice, Message};

use crate::config::ProviderEntry;
use crate::quota::freshness::ObservationStamp;
use crate::quota::placement::{Reason, Tier, standing};
use crate::quota::reading::{LengthClass, ReadingSource, Started};

const SEAT_PROVIDER: &str = "codex";
const SEAT_LABEL: &str = "c";
const ACCOUNT: &str = "codex#c";

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .expect("a post-epoch clock")
        .as_secs()
}

/// A plan reporting a five-hour primary at 25% (two hours left), a weekly
/// secondary at 40% (three days left) and a weekly `bengalfox` extra limit
/// at 10% (four days left), as the header parser places them.
fn codex_family() -> CodexQuota {
    let now = unix_now();
    let mut quota = CodexQuota::default();
    quota.primary_used_percent = Some("25".into());
    quota.primary_reset_at = Some((now + 7_200).to_string());
    quota.extras = [
        ("primary-window-minutes", "300".to_string()),
        ("primary-reset-after-seconds", "7200".to_string()),
        ("secondary-used-percent", "40".to_string()),
        ("secondary-window-minutes", "10080".to_string()),
        ("secondary-reset-after-seconds", "259200".to_string()),
        ("secondary-reset-at", (now + 259_200).to_string()),
        ("bengalfox-primary-used-percent", "10".to_string()),
        ("bengalfox-primary-window-minutes", "10080".to_string()),
        (
            "bengalfox-primary-reset-after-seconds",
            "345600".to_string(),
        ),
        ("bengalfox-primary-reset-at", (now + 345_600).to_string()),
        ("bengalfox-limit-name", "GPT-5.3-Codex-Spark".to_string()),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value))
    .collect();
    quota
}

struct CodexProvider;

#[async_trait]
impl Provider for CodexProvider {
    fn id(&self) -> &'static str {
        "codex-quota"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("codex-quota", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        Ok(ChatResponse {
            id: "resp".into(),
            model: req.model,
            created: 0,
            choices: vec![Choice {
                logprobs: None,
                index: 0,
                message: Message {
                    refusal: None,
                    role: routectl_core::Role::Assistant,
                    content: routectl_core::MessageContent::Text("ok".into()),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    extras: serde_json::Map::new(),
                },
                finish_reason: Some("stop".into()),
                matched_stop_sequence: None,
            }],
            usage: Some(routectl_core::Usage::default()),
            routectl_provider: None,
            extras: Default::default(),
            upstream_meta: Some(UpstreamMeta::from_codex(codex_family())),
        })
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        unreachable!("these tests dispatch non-streaming only")
    }
}

fn secret_ref() -> routectl_auth::SecretRef {
    routectl_auth::SecretRef::OAuth {
        provider: SEAT_PROVIDER.to_string(),
        label: Some(SEAT_LABEL.to_string()),
    }
}

/// One model on a managed Codex seat whose provider answers with the family.
fn router() -> Router {
    let mut providers = BTreeMap::new();
    providers.insert(
        SEAT_PROVIDER.to_string(),
        ProviderEntry::openai_responses(secret_ref().to_string()),
    );
    let mut router = Router::new(Arc::new(Config {
        providers,
        ..Config::default()
    }));
    let provider: Arc<dyn Provider> = Arc::new(CodexProvider);
    let model = ResolvedModel::new("gpt", SEAT_PROVIDER, provider, "gpt-5-codex")
        .with_auth_secret_ref(secret_ref());
    router.install_resolved_models(std::iter::once(("gpt".to_string(), Arc::new(model))).collect());
    router
}

fn req() -> ChatRequest {
    ChatRequest {
        model: "gpt".into(),
        messages: Arc::from(vec![routectl_core::test_utils::user_msg("hi")]),
        ..Default::default()
    }
}

#[tokio::test]
async fn every_codex_header_window_reaches_the_account_readings_at_its_reported_length() {
    // Arrange
    let router = router();

    // Act
    router.complete(req()).await.expect("the dispatch succeeds");

    // Assert
    let reading = router
        .account_readings(&ObservationStamp::now())
        .accounts
        .into_iter()
        .find(|reading| reading.account == ACCOUNT)
        .expect("the configured account is listed");
    let windows: Vec<(
        u64,
        LengthClass,
        Option<String>,
        f64,
        Started,
        ReadingSource,
    )> = reading
        .windows
        .iter()
        .map(|window| {
            (
                window.window.length_secs,
                window.window.class,
                window.window.scope.clone(),
                window.window.used,
                window.window.started,
                window.source,
            )
        })
        .collect();
    assert_eq!(
        windows,
        vec![
            (
                18_000,
                LengthClass::Short,
                None,
                0.25,
                Started::Yes,
                ReadingSource::Response
            ),
            (
                604_800,
                LengthClass::Long,
                None,
                0.40,
                Started::Yes,
                ReadingSource::Response
            ),
            (
                604_800,
                LengthClass::Long,
                Some("codex_bengalfox".into()),
                0.10,
                Started::Yes,
                ReadingSource::Response
            ),
        ]
    );
}

/// The readings a dispatch fed rank the account on pace, with no Codex special
/// case: the weekly secondary (40% used, three of seven days left) is slower
/// than the five-hour primary and decides; the `codex_bengalfox` extra limit
/// binds no model and is ignored.
#[tokio::test]
async fn codex_header_readings_rank_the_account_by_its_slowest_window() {
    // Arrange
    let router = router();
    router.complete(req()).await.expect("the dispatch succeeds");
    let now = ObservationStamp::now();
    let readings = router.account_readings(&now);
    let reading = readings
        .accounts
        .iter()
        .find(|reading| reading.account == ACCOUNT);

    // Act
    let standing = standing(reading, "gpt-5-codex", &now);

    // Assert
    assert_eq!(
        (standing.tier, standing.reason),
        (Tier::Roomy, Reason::Pace)
    );
    let score = standing.score.expect("a read seat is scored");
    assert!(
        (score - 0.6 / (3.0 / 7.0)).abs() < 0.01,
        "the weekly window decides: {score}"
    );
}
