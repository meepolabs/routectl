// Shared fixture of the repair-chaining tests: the chain seat that runs the
// real egress body assembly, the routers over it, the captured and
// synthesized rejections, the client requests, and the settle readers.
// An `include!`d FRAGMENT of `field_repair_chain_tests.rs`, inside its
// `repair_chain` module.

use crate::VerdictValue;
use routectl_providers::openai_compat::request::normalize;
use routectl_providers::openai_compat::{HistoryReasoning, ReasoningDialect};

const MAX_TOKENS_PATH: &str = "max_tokens";
/// The committed anthropic-api capture of `max_tokens` above the model's
/// output limit.
const MAX_TOKENS_FIXTURE: &str = include_str!(
    "../../tests/fixtures/rejections/anthropic-api/max-tokens-above-model-limit-messages.json"
);
/// The value that capture says was sent, and the limit it names.
const SENT_MAX_TOKENS: u32 = 1_000_000;
const MODEL_LIMIT: u32 = 128_000;
/// An upstream model name no openai-compat reasoning profile matches, so
/// the egress forwards every sampling parameter the client sent.
const UNPROFILED_MODEL: &str = "acme-chat-1";
const OPENAI_COMPAT_TOML: &str = "kind = \"openai-compat\"\n\
    base_url = \"https://example.test/v1\"\napi_key_ref = \"literal:k\"\n";
const ONE_ATTEMPT: &str = "[retry]\nmax_attempts = 1\ninitial_backoff_ms = 0\njitter_ms = 0\n";
/// Room for one same-target retry after a repaired attempt.
const THREE_ATTEMPTS: &str = "[retry]\nmax_attempts = 3\ninitial_backoff_ms = 0\njitter_ms = 0\n";
/// The `stop` the requests that need a carried, row-less field send.
const STOP: &str = "zz-stop";

/// How a seat answers one attempt, given the body it assembled and the
/// attempt's zero-based position; `None` serves.
type Respond = fn(&serde_json::Value, usize) -> Option<Error>;

#[derive(Clone, Copy)]
enum Egress {
    Anthropic,
    OpenAiCompat,
}

/// A seat that assembles each attempt's body with the real egress,
/// deposits it into the attempt's wire tap for the first `deposits`
/// attempts, and answers per `respond`.
struct ChainSeat {
    egress: Egress,
    respond: Respond,
    deposits: usize,
    bodies: Mutex<Vec<serde_json::Value>>,
}

impl ChainSeat {
    fn new(egress: Egress, respond: Respond) -> Arc<Self> {
        Self::depositing(egress, respond, usize::MAX)
    }

    /// A seat whose attempts from position `deposits` on leave the wire
    /// tap empty, as an egress that never recorded its body would.
    fn depositing(egress: Egress, respond: Respond, deposits: usize) -> Arc<Self> {
        Arc::new(Self {
            egress,
            respond,
            deposits,
            bodies: Mutex::new(Vec::new()),
        })
    }

    fn bodies(&self) -> Vec<serde_json::Value> {
        self.bodies.lock().clone()
    }

    fn calls(&self) -> usize {
        self.bodies.lock().len()
    }

    fn body_of(&self, req: &ChatRequest) -> Result<serde_json::Value> {
        match self.egress {
            Egress::Anthropic => {
                let egress = routectl_providers::anthropic_api::AnthropicApiProvider::new(
                    routectl_providers::anthropic_api::AnthropicApiConfig::new("chain-seat", "k"),
                );
                let mut body = egress.normalize_request(req)?;
                if let Some(object) = body.as_object_mut() {
                    object.remove("stream");
                }
                Ok(body)
            }
            Egress::OpenAiCompat => normalize(
                "chain-seat",
                req,
                ReasoningDialect::OpenAi,
                HistoryReasoning::default(),
                None,
                false,
            ),
        }
    }

    fn answer(&self, req: &ChatRequest) -> Result<()> {
        let body = self.body_of(req)?;
        let position = {
            let mut bodies = self.bodies.lock();
            bodies.push(body.clone());
            bodies.len() - 1
        };
        if let Some(tap) = req.routectl_internal.wire_tap.as_ref()
            && position < self.deposits
        {
            tap.deposit(serde_json::to_vec(&body).expect("serialize").into());
        }
        (self.respond)(&body, position).map_or(Ok(()), Err)
    }
}

#[async_trait::async_trait]
impl Provider for ChainSeat {
    fn id(&self) -> &'static str {
        "chain-seat"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("chain-seat", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.answer(&req).map(|()| success_response())
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.answer(&req).map(|()| {
            stream::iter(vec![Ok(content_chunk())]).boxed() as BoxStream<'static, Result<ChatChunk>>
        })
    }
    async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        self.answer(&req).map(|()| TokenCount {
            input_tokens: 7,
            ..Default::default()
        })
    }
}

/// A router over one seat per `(provider_toml, seat)`, in chain order,
/// with `extra_toml` prepended and durable capability writes assumed.
fn chain_router(seats: &[(&str, &Arc<ChainSeat>)], extra_toml: &str) -> Router {
    let mut toml_text = String::from(extra_toml);
    for (idx, (provider_toml, _)) in seats.iter().enumerate() {
        toml_text.push_str(&format!("\n[providers.p{idx}]\n{provider_toml}"));
    }
    let mut config: Config = toml::from_str(&toml_text).expect("valid test toml");
    config.aliases.insert(
        ALIAS.to_string(),
        AliasValue::Chain((0..seats.len()).map(|idx| format!("m{idx}")).collect()),
    );
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(
        seats
            .iter()
            .enumerate()
            .map(|(idx, (_, seat))| {
                let upstream = match seat.egress {
                    Egress::Anthropic => "upstream",
                    Egress::OpenAiCompat => UNPROFILED_MODEL,
                };
                (
                    format!("m{idx}"),
                    Arc::new(ResolvedModel::new(
                        format!("m{idx}"),
                        format!("p{idx}"),
                        Arc::clone(*seat) as Arc<dyn Provider>,
                        upstream,
                    )),
                )
            })
            .collect(),
    );
    router.with_capability_writes_assumed_durable_for_tests()
}

fn anthropic_router(seat: &Arc<ChainSeat>, extra_toml: &str) -> Router {
    chain_router(&[(ANTHROPIC_API_TOML, seat)], extra_toml)
}

fn wire_checked(retry: &str) -> String {
    format!("{WIRE_CHECK_EVERY}{retry}")
}

// -----------------------------------------------------------------------
// Rejections
// -----------------------------------------------------------------------

/// The captured max_tokens rejection exactly as the upstream returned it.
fn max_tokens_rejection() -> Error {
    Error::upstream("chain-seat", 400, disabled_fixture_body(MAX_TOKENS_FIXTURE))
}

/// An anthropic-api `invalid_request_error` 400 carrying `message`.
fn anthropic_rejection(message: &str) -> Error {
    Error::upstream(
        "chain-seat",
        400,
        json!({"type": "error", "error": {"type": "invalid_request_error", "message": message}})
            .to_string(),
    )
}

/// A 400 naming nothing.
fn unlocalized_rejection() -> Error {
    anthropic_rejection("the request is malformed")
}

fn unavailable() -> Error {
    Error::upstream(
        "chain-seat",
        503,
        r#"{"error":{"type":"api_error","message":"unavailable"}}"#,
    )
}

/// The captured disabled refusal with its two thinking types swapped: a
/// refusal of the adaptive value the rewrite sent. Synthesized from the
/// committed template, so it parses exactly as a real refusal of that
/// value would; no capture of it exists.
fn adaptive_refused() -> Error {
    anthropic_rejection(
        "\"thinking.type.adaptive\" is not supported for this model. Use \
         \"thinking.type.disabled\" and \"output_config.effort\" to control thinking behavior.",
    )
}

/// The grounded request-validation refusal of a field the body carried
/// and the closed table has no row for.
fn stop_refused() -> Error {
    anthropic_rejection("stop_sequences: Extra inputs are not permitted")
}

/// The same field named by the ungrounded quoted template, which earns
/// only an inferred attribution.
fn stop_refused_inferred() -> Error {
    anthropic_rejection("\"stop_sequences\" is not supported on this model.")
}

/// A grounded refusal of a path the body did not carry.
fn absent_top_k_refused() -> Error {
    anthropic_rejection("top_k: Extra inputs are not permitted")
}

fn thinking_type(body: &serde_json::Value) -> Option<&str> {
    body["thinking"]["type"].as_str()
}

fn sent_max_tokens(body: &serde_json::Value) -> Option<u64> {
    body["max_tokens"].as_u64()
}

/// Refuse disabled thinking, then a max_tokens above the limit, else
/// serve.
fn two_key(body: &serde_json::Value, _: usize) -> Option<Error> {
    if thinking_type(body) == Some("disabled") {
        return Some(anthropic_disabled_rejection());
    }
    sent_max_tokens(body)
        .is_some_and(|sent| sent > u64::from(MODEL_LIMIT))
        .then(max_tokens_rejection)
}

/// Refuse disabled thinking, then answer the rewrite with `then`.
fn disabled_then(body: &serde_json::Value, then: fn() -> Error) -> Option<Error> {
    Some(if thinking_type(body) == Some("disabled") {
        anthropic_disabled_rejection()
    } else {
        then()
    })
}

// -----------------------------------------------------------------------
// Requests
// -----------------------------------------------------------------------

/// An Anthropic client's disabled thinking with an explicit max_tokens of
/// `max_tokens` and, when `stop`, a stop sequence.
fn anthropic_req(max_tokens: u32, stop: bool) -> ChatRequest {
    let mut req = disabled_req(None);
    req.max_tokens = Some(max_tokens);
    let mut client = disabled_client_body(None);
    client["max_tokens"] = json!(max_tokens);
    if stop {
        req.stop = Some(vec![STOP.to_string()]);
        client["stop_sequences"] = json!([STOP]);
    }
    if let Some(wire) = req.routectl_internal.ingress_wire.as_mut() {
        wire.bytes = serde_json::to_vec(&client).expect("serialize").into();
    }
    req
}

fn two_key_req() -> ChatRequest {
    anthropic_req(SENT_MAX_TOKENS, false)
}

fn verdicts(meta: &crate::router::DispatchMeta) -> Vec<(String, Option<VerdictValue>)> {
    meta.learned_capabilities
        .iter()
        .map(|event| (event.capability_key.clone(), event.value))
        .collect()
}

fn key(path: &str) -> String {
    crate::field_capability::field_capability_key(path).expect("a grounded path mints a key")
}

fn two_key_verdicts() -> Vec<(String, Option<VerdictValue>)> {
    vec![
        (
            thinking_disabled_key(),
            Some(VerdictValue::Shape("thinking.type.adaptive")),
        ),
        (
            key(MAX_TOKENS_PATH),
            Some(VerdictValue::Ceiling(MODEL_LIMIT)),
        ),
    ]
}

fn succeeded(meta: &crate::router::DispatchMeta) -> Vec<(&'static str, bool, bool)> {
    meta.field_repairs
        .iter()
        .map(|record| (record.field_path, record.repair_succeeded, record.learned))
        .collect()
}

fn assert_two_key_bodies(seat: &ChainSeat, label: &str) {
    let bodies = seat.bodies();
    assert_eq!(
        bodies.len(),
        3,
        "{label}: two refusals and one served retry"
    );
    assert_eq!(thinking_type(&bodies[0]), Some("disabled"), "{label}");
    assert_eq!(
        sent_max_tokens(&bodies[0]),
        Some(u64::from(SENT_MAX_TOKENS)),
        "{label}"
    );
    assert_eq!(thinking_type(&bodies[1]), Some("adaptive"), "{label}");
    assert_eq!(
        sent_max_tokens(&bodies[1]),
        Some(u64::from(SENT_MAX_TOKENS)),
        "{label}: the second refusal names the field the first left alone",
    );
    assert_eq!(thinking_type(&bodies[2]), Some("adaptive"), "{label}");
    assert_eq!(
        sent_max_tokens(&bodies[2]),
        Some(u64::from(MODEL_LIMIT)),
        "{label}: both edits accumulate on the retried body",
    );
}

/// Each verdict carries the features of the request the CLIENT sent: the
/// disabled thinking the first repair rewrote is still among them, on
/// the moved-on commit as on the success commit.
fn assert_two_key_settled(meta: &crate::router::DispatchMeta, label: &str) {
    assert_eq!(verdicts(meta), two_key_verdicts(), "{label}");
    for event in &meta.learned_capabilities {
        assert!(
            event.request_features.contains(&thinking_disabled_key()),
            "{label}: {} features {:?}",
            event.capability_key,
            event.request_features,
        );
    }
    assert_eq!(
        succeeded(meta),
        vec![
            (THINKING_DISABLED_PATH, true, true),
            (MAX_TOKENS_PATH, true, true),
        ],
        "{label}",
    );
}

// ---------------------------------------------------------------------------
// openai-compat refusals and requests
// ---------------------------------------------------------------------------

fn compat_fixture(key: &str) -> &'static str {
    match key {
        "temperature" => include_str!(
            "../../tests/fixtures/rejections/openai-compat/unsupported-parameter-temperature.json"
        ),
        "top_p" => include_str!(
            "../../tests/fixtures/rejections/openai-compat/unsupported-parameter-top-p.json"
        ),
        "presence_penalty" => include_str!(
            "../../tests/fixtures/rejections/openai-compat/unsupported-parameter-presence-penalty.json"
        ),
        "frequency_penalty" => include_str!(
            "../../tests/fixtures/rejections/openai-compat/unsupported-parameter-frequency-penalty.json"
        ),
        other => panic!("no committed refusal of {other}"),
    }
}

/// The committed refusal of `key` as the openai-compat provider surfaces
/// it: the body with its lifted `error.type` and `error.code`.
fn compat_rejection(key: &str) -> Error {
    let fixture: serde_json::Value =
        serde_json::from_str(compat_fixture(key)).expect("the fixture is JSON");
    let status = u16::try_from(fixture["status"].as_u64().expect("a status")).expect("u16");
    let body = fixture["body"].as_str().expect("a body");
    let parsed: serde_json::Value = serde_json::from_str(body).expect("the body is JSON");
    let token = |pointer: &str| {
        parsed
            .pointer(pointer)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    Error::upstream_with_body(
        "chain-seat",
        status,
        routectl_core::UpstreamBody::new(body),
        None,
        token("/error/type"),
        token("/error/code"),
    )
}

const THREE_KEYS: [&str; 3] = ["temperature", "top_p", "presence_penalty"];
const FOUR_KEYS: [&str; 4] = [
    "temperature",
    "top_p",
    "presence_penalty",
    "frequency_penalty",
];

/// Refuse the first of `keys` the body carries, in order.
fn refuse_first_of(keys: &[&str], body: &serde_json::Value) -> Option<Error> {
    keys.iter()
        .find(|key| body.get(**key).is_some())
        .map(|key| compat_rejection(key))
}

/// A Chat Completions client's request setting every one of `keys`.
fn compat_req(keys: &[&str]) -> ChatRequest {
    let mut req = req_on(ALIAS);
    req.max_tokens = Some(1024);
    req.reasoning = None;
    req.routectl_internal.anthropic_thinking_display = None;
    req.routectl_internal.anthropic_thinking = routectl_core::AnthropicThinking::SentNone;
    let mut client = json!({
        "model": ALIAS,
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "hello"}],
    });
    for key in keys {
        match *key {
            "temperature" => req.temperature = Some(0.5),
            "top_p" => req.top_p = Some(0.5),
            "presence_penalty" => req.presence_penalty = Some(0.5),
            "frequency_penalty" => req.frequency_penalty = Some(0.5),
            other => panic!("no sampling row {other}"),
        }
        client[*key] = json!(0.5);
    }
    req.routectl_internal.ingress_wire = Some(routectl_core::wire_check::IngressWire {
        dialect: routectl_core::wire_check::IngressDialect::Chat,
        client_version: None,
        bytes: serde_json::to_vec(&client).expect("serialize").into(),
    });
    req
}
