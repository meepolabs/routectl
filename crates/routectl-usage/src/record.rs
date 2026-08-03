//! Usage-accounting record types.
//!
//! `UsageRecord` mirrors the capture-column set persisted by the usage
//! writer. It is shaped for the DB writer, not the public wire -- one
//! field per column, nullable columns as `Option<T>`, timestamps as
//! epoch-millis UTC `i64`, JSON-text columns as `serde_json::Value`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Terminal outcome of a routed request.
///
/// CLOSED set: exactly these six variants. This enum is the single
/// source of truth for the DB `outcome` CHECK constraint -- a later
/// schema task mirrors the lowercase tokens returned by `as_str`. Add
/// a variant here only alongside the matching schema migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The request completed successfully.
    Ok,
    /// The upstream provider returned an error after a dispatch attempt.
    UpstreamError,
    /// The client hung up before the request finalized. Also the enum's
    /// `Default`, so it is the abnormal-exit sentinel stamped on a
    /// finalize-less exit.
    //
    // `#[default]` resolves to the real, persisted `ClientDisconnect`
    // outcome -- chosen because it is the abnormal-exit sentinel the
    // finalize/Drop path already stamps via `mem::take` for a
    // finalize-less exit. CAUTION: this default is NOT an inert
    // placeholder. A default-constructed `Outcome`/`UsageRecord` that
    // reaches the persistence path is silently written as
    // `ClientDisconnect`, so default-construction must never feed the
    // DB writer except on that abnormal-exit path.
    #[default]
    ClientDisconnect,
    /// The request exceeded its deadline before completing.
    Timeout,
    /// The request was cancelled before completing.
    Cancelled,
    /// A local gate refused the request before any dispatch attempt.
    GateBlocked,
}

impl Outcome {
    /// The exact lowercase wire token for this outcome. These tokens are
    /// the DB `outcome` CHECK-constraint values; keep them in sync.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::UpstreamError => "upstream_error",
            Self::ClientDisconnect => "client_disconnect",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::GateBlocked => "gate_blocked",
        }
    }
}

/// Error returned when a wire token does not map to a known `Outcome`.
#[derive(Debug, thiserror::Error)]
#[error("unknown outcome token: {0}")]
pub struct ParseOutcomeError(pub String);

impl std::str::FromStr for Outcome {
    type Err = ParseOutcomeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ok" => Ok(Self::Ok),
            "upstream_error" => Ok(Self::UpstreamError),
            "client_disconnect" => Ok(Self::ClientDisconnect),
            "timeout" => Ok(Self::Timeout),
            "cancelled" => Ok(Self::Cancelled),
            "gate_blocked" => Ok(Self::GateBlocked),
            other => Err(ParseOutcomeError(other.to_string())),
        }
    }
}

impl TryFrom<&str> for Outcome {
    type Error = ParseOutcomeError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        s.parse()
    }
}

/// One persisted usage-accounting row. One field per capture column.
///
/// Timestamps are epoch-millis UTC (`i64`). JSON-text columns are
/// `serde_json::Value`. Token counts are unsigned (`u64`) and nullable
/// because not every upstream reports every counter.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageRecord {
    // IDENTITY
    /// Request start time, epoch-millis UTC.
    pub ts_start: i64,
    /// Request completion time, epoch-millis UTC.
    pub ts_end: i64,
    /// Unique request identifier; the idempotency key for this row.
    pub request_id: String,
    /// Wire dialect the client spoke (e.g. `anthropic`, `openai`).
    pub ingress_dialect: String,
    /// Model name the client asked for, before alias resolution.
    pub requested_model: String,
    /// Resolved routing alias the request mapped to.
    pub alias: String,
    /// Served model nickname. `None` when no target was dispatched.
    pub model: Option<String>,
    /// Served upstream target id. `None` when no target was dispatched.
    pub upstream: Option<String>,
    /// Served provider name. `None` when no target was dispatched.
    pub provider: Option<String>,
    /// Stable provider-kind token of the served target. `None` when no
    /// target was dispatched.
    pub provider_kind: Option<String>,
    /// Seat the served target drew from. `None` for keyless / non-pooled
    /// targets.
    pub seat: Option<String>,
    /// Populated from the Anthropic ingress's `inbound_session_key`
    /// (header `x-claude-code-session-id`, falling back to body
    /// `metadata.session_id`) -- see
    /// `routectl_core::ChatRequest::routectl_internal.inbound_session_key`.
    /// `None` for the OpenAI chat-completions and Responses dialects,
    /// which do not set `inbound_session_key` (no session-identity
    /// concept on those wire protocols).
    pub session_id: Option<String>,

    // SHAPE
    /// Whether the client requested a streaming response.
    pub stream: bool,
    /// Client-requested max output tokens. `None` when unset.
    pub max_tokens_req: Option<u32>,
    /// Number of tools declared in the request.
    pub tool_count: u32,
    /// Client-requested thinking/reasoning budget. `None` when unset.
    pub thinking_req: Option<u32>,
    /// Kind of thinking budget requested (e.g. `budget_tokens`). `None`
    /// when unset.
    pub thinking_req_kind: Option<String>,
    /// Number of messages in the request.
    pub msg_count: u32,
    /// Requested service tier. `None` when unset.
    pub service_tier: Option<String>,

    // OUTCOME
    /// Terminal outcome of the request.
    pub outcome: Outcome,
    /// Client-transport HTTP status. `None` for streaming rows written
    /// before the commit-point fix.
    pub http_status: Option<u16>,
    /// Legacy free-form error-class label. `None` on success.
    pub error_class: Option<String>,
    /// Canonical kebab failure-class token (`FailureClass::class_token`) for a
    /// request that reached a dispatch attempt and failed. `None` for a
    /// success, for any pre-dispatch / validation / local-gate failure that
    /// never reached an upstream, and when the class carries no token
    /// (`Unknown`). A `None` reads back as "unclassified"; there is no
    /// backfill of rows written before this column existed.
    pub resolved_class: Option<String>,
    /// Upstream finish reason (e.g. `stop`, `length`). `None` when the
    /// request did not complete with a reported reason.
    pub finish_reason: Option<String>,
    /// Number of dispatch attempts made for this request.
    pub attempt_count: u32,
    /// Number of fallbacks past the first target.
    pub fallback_count: u32,
    /// Non-mutating steady-state would-trim advisory: the freed-token count
    /// `d` of the trimmer's would-cut candidate for this request. `None` when
    /// the steady-state trimmer proposed no cut. The live request is NEVER
    /// mutated -- this is recording only.
    pub would_trim_tokens: Option<u64>,
    /// Non-mutating steady-state would-trim advisory: the break-even reuse
    /// count K* the cost gate priced for the would-cut candidate. `None` when
    /// the trimmer proposed no cut, when the pricing cell is unverified /
    /// sentinel (an unknown / unverified provider records the freed-token
    /// count but no K* -- no trusted pricing), or when a verified row carried
    /// no finite break-even. Recording only.
    pub would_trim_break_even_k: Option<f64>,
    /// Non-mutating steady-state would-trim advisory: the per-session K
    /// estimator's lower confidence bound `k_floor`, recorded ONLY when the
    /// estimator returned a `Calibrated` confidence for the request's
    /// (session, provider_kind, model) triple. `None` for a `Cold` / `Low`
    /// estimate (insufficient session history), when the pricing cell was
    /// unverified (no K* to compare against), and when no would-cut candidate
    /// was proposed. Recording only.
    pub would_trim_k_floor: Option<f64>,
    /// Non-mutating shadow misfire monitor advisory: `Some(0)` when the
    /// trimmed cacheable prefix fingerprint matched the stored value for this
    /// (session, provider_kind, model) triple (Stable), `Some(1)` when it
    /// differed (Misfire), `None` when no session key was present or this was
    /// the first observation for the triple (FirstSeen). Recording only.
    pub would_trim_shadow_misfire: Option<i64>,
    /// Non-mutating near-lossless attribution: freed tokens attributed to
    /// the dedup heuristic for this request's would-cut candidate. Plumbing
    /// only -- this field is always `None` until the near-lossless recorder
    /// pass computes it. Recording only.
    pub would_trim_dedup_tokens: Option<u64>,
    /// Non-mutating near-lossless attribution: freed tokens attributed to
    /// the supersession heuristic for this request's would-cut candidate.
    /// Plumbing only -- this field is always `None` until the near-lossless
    /// recorder pass computes it. Recording only.
    pub would_trim_supersession_tokens: Option<u64>,
    /// Non-mutating path-extractability count-pair: the denominator (total
    /// path units considered). Paired with `would_trim_path_extractable` so
    /// the extractability rate is reconstructable offline via SUM/SUM
    /// rather than pre-averaged per row. Plumbing only -- always `None`
    /// until the near-lossless recorder pass computes it. Recording only.
    pub would_trim_path_units: Option<u64>,
    /// Non-mutating path-extractability count-pair: the numerator (path
    /// units that were extractable). See `would_trim_path_units`. Plumbing
    /// only -- always `None` until the near-lossless recorder pass computes
    /// it. Recording only.
    pub would_trim_path_extractable: Option<u64>,
    /// Recorder-version marker: `None` on baseline rows and on rows where
    /// the near-lossless pass did not run; stamped by the near-lossless
    /// recorder on every trigger-clearing row. Lets reporting filter to
    /// non-NULL rows so aggregates never mix baseline vs near-lossless
    /// semantics. Plumbing only -- always `None` until that pass stamps it.
    pub would_trim_recorder_version: Option<i64>,
    /// Capped raw-marks JSON blob: per-mark ordering captured for a future
    /// raw-marks sweep, bounded to a byte cap (see
    /// `writer::capped_raw_marks_text`) so the stored JSON is always valid.
    /// Plumbing only -- always `None` until the near-lossless recorder pass
    /// computes it. Recording only.
    pub would_trim_raw_marks: Option<Value>,
    /// Non-mutating context-fraction advisory: `estimate_total_tokens /
    /// max_context_tokens` from the resolved pricing row. `None` when the
    /// context window is unknown (fail-closed). Plumbing only -- always
    /// `None` until the near-lossless recorder pass computes it. Recording
    /// only.
    pub would_trim_context_fraction: Option<f64>,

    // TIMING
    /// End-to-end request latency, milliseconds.
    pub latency_ms: i64,
    /// Time to first byte, milliseconds. `None` for non-streaming rows
    /// and when no first byte was observed.
    pub ttfb_ms: Option<i64>,

    // TOKENS
    /// Cache-exclusive new input tokens. `None` when not reported.
    pub input_tokens: Option<u64>,
    /// Output tokens generated. `None` when not reported.
    pub output_tokens: Option<u64>,
    /// Reasoning tokens (billed as output upstream). `None` when not
    /// reported.
    pub reasoning_tokens: Option<u64>,
    /// Cached prefix tokens re-read this turn (a per-turn snapshot, not a
    /// flow). `None` when not reported.
    pub cache_read: Option<u64>,
    /// Tokens written to the 5-minute cache. `None` when not reported.
    pub cache_write_5m: Option<u64>,
    /// Tokens written to the 1-hour cache. `None` when not reported.
    pub cache_write_1h: Option<u64>,
    /// Server-tool invocation counts as a JSON map. `None` when no server
    /// tools were used.
    pub server_tool_use: Option<Value>,

    // QUOTA snapshot
    /// Quota claim token reported by the upstream. `None` when absent.
    pub quota_claim: Option<String>,
    /// Quota status reported by the upstream. `None` when absent.
    pub quota_status: Option<String>,
    /// Overage-quota status reported by the upstream. `None` when absent.
    pub quota_overage_status: Option<String>,
    /// Primary-quota utilization ratio. `None` when absent.
    pub quota_utilization: Option<f64>,
    /// Overage-quota utilization ratio. `None` when absent.
    pub quota_overage_utilization: Option<f64>,
    /// Quota reset time, epoch-millis UTC. `None` when absent.
    pub quota_reset: Option<i64>,
    /// Remaining quota fields as a JSON map. `None` when absent.
    pub quota_extras: Option<Value>,

    // EXTENSIBILITY
    /// Forward-compatible JSON side-channel for fields without a dedicated
    /// column. `None` when unused.
    pub extra: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn outcome_as_str_returns_exact_lowercase_tokens() {
        // Arrange
        let cases = [
            (Outcome::Ok, "ok"),
            (Outcome::UpstreamError, "upstream_error"),
            (Outcome::ClientDisconnect, "client_disconnect"),
            (Outcome::Timeout, "timeout"),
            (Outcome::Cancelled, "cancelled"),
            (Outcome::GateBlocked, "gate_blocked"),
        ];

        // Act + Assert
        for (variant, token) in cases {
            assert_eq!(variant.as_str(), token);
        }
    }

    #[test]
    fn outcome_serializes_to_as_str_token() {
        // Arrange
        let variants = [
            Outcome::Ok,
            Outcome::UpstreamError,
            Outcome::ClientDisconnect,
            Outcome::Timeout,
            Outcome::Cancelled,
            Outcome::GateBlocked,
        ];

        // Act + Assert
        for variant in variants {
            let serialized = serde_json::to_value(variant).expect("variant serializes");
            assert_eq!(
                serialized,
                serde_json::Value::String(variant.as_str().to_string()),
            );
        }
    }

    #[test]
    fn outcome_round_trips_through_str() {
        // Arrange
        let variants = [
            Outcome::Ok,
            Outcome::UpstreamError,
            Outcome::ClientDisconnect,
            Outcome::Timeout,
            Outcome::Cancelled,
            Outcome::GateBlocked,
        ];

        // Act + Assert
        for variant in variants {
            let parsed: Outcome = variant.as_str().parse().expect("token parses");
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn outcome_from_str_rejects_unknown_token() {
        // Act
        let parsed = "definitely_not_an_outcome".parse::<Outcome>();

        // Assert
        assert!(parsed.is_err());
    }

    #[test]
    fn fully_populated_record_constructs() {
        // Arrange + Act
        let record = UsageRecord {
            ts_start: 1_700_000_000_000,
            ts_end: 1_700_000_001_000,
            request_id: "req-1".to_string(),
            ingress_dialect: "anthropic".to_string(),
            requested_model: "claude-x".to_string(),
            alias: "fast".to_string(),
            model: Some("served-nick".to_string()),
            upstream: Some("vendor/model-id".to_string()),
            provider: Some("vendor".to_string()),
            provider_kind: Some("anthropic-api".to_string()),
            seat: Some("seat-1".to_string()),
            session_id: Some("sess-1".to_string()),
            stream: true,
            max_tokens_req: Some(4096),
            tool_count: 3,
            thinking_req: Some(2048),
            thinking_req_kind: Some("budget_tokens".to_string()),
            msg_count: 7,
            service_tier: Some("default".to_string()),
            outcome: Outcome::Ok,
            http_status: Some(200),
            error_class: None,
            resolved_class: None,
            finish_reason: Some("stop".to_string()),
            attempt_count: 1,
            fallback_count: 0,
            would_trim_tokens: Some(40_000),
            would_trim_break_even_k: Some(50.0),
            would_trim_k_floor: Some(60.0),
            would_trim_shadow_misfire: None,
            would_trim_dedup_tokens: Some(1_200),
            would_trim_supersession_tokens: Some(800),
            would_trim_path_units: Some(10),
            would_trim_path_extractable: Some(7),
            would_trim_recorder_version: Some(1),
            would_trim_raw_marks: Some(json!([{"kind": "dedup", "index": 0}])),
            would_trim_context_fraction: Some(0.25),
            latency_ms: 1000,
            ttfb_ms: Some(120),
            input_tokens: Some(100),
            output_tokens: Some(200),
            reasoning_tokens: Some(50),
            cache_read: Some(10),
            cache_write_5m: Some(5),
            cache_write_1h: Some(1),
            server_tool_use: Some(json!({"web_search": 2})),
            quota_claim: Some("claim-1".to_string()),
            quota_status: Some("active".to_string()),
            quota_overage_status: Some("none".to_string()),
            quota_utilization: Some(0.42),
            quota_overage_utilization: Some(0.0),
            quota_reset: Some(1_700_000_100_000),
            quota_extras: Some(json!({"plan": "pro"})),
            extra: Some(json!({"note": "test"})),
        };

        // Assert
        assert_eq!(record.outcome.as_str(), "ok");
        assert_eq!(record.input_tokens, Some(100));
    }

    #[test]
    fn all_nullable_fields_none_record_constructs() {
        // Arrange + Act
        let record = UsageRecord {
            ts_start: 0,
            ts_end: 0,
            request_id: "req-2".to_string(),
            ingress_dialect: "openai".to_string(),
            requested_model: "gpt-x".to_string(),
            alias: "default".to_string(),
            model: None,
            upstream: None,
            provider: None,
            provider_kind: None,
            seat: None,
            session_id: None,
            stream: false,
            max_tokens_req: None,
            tool_count: 0,
            thinking_req: None,
            thinking_req_kind: None,
            msg_count: 1,
            service_tier: None,
            outcome: Outcome::UpstreamError,
            http_status: None,
            error_class: None,
            resolved_class: None,
            finish_reason: None,
            attempt_count: 1,
            fallback_count: 0,
            would_trim_tokens: None,
            would_trim_break_even_k: None,
            would_trim_k_floor: None,
            would_trim_shadow_misfire: None,
            would_trim_dedup_tokens: None,
            would_trim_supersession_tokens: None,
            would_trim_path_units: None,
            would_trim_path_extractable: None,
            would_trim_recorder_version: None,
            would_trim_raw_marks: None,
            would_trim_context_fraction: None,
            latency_ms: 0,
            ttfb_ms: None,
            input_tokens: None,
            output_tokens: None,
            reasoning_tokens: None,
            cache_read: None,
            cache_write_5m: None,
            cache_write_1h: None,
            server_tool_use: None,
            quota_claim: None,
            quota_status: None,
            quota_overage_status: None,
            quota_utilization: None,
            quota_overage_utilization: None,
            quota_reset: None,
            quota_extras: None,
            extra: None,
        };

        // Assert
        assert_eq!(record.outcome.as_str(), "upstream_error");
        assert!(record.input_tokens.is_none());
    }
}
