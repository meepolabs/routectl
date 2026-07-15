//! Usage-capture RAII guard and its draft-construction / outcome-mapping
//! helpers.
//!
//! `UsageCapture` records EXACTLY ONE `UsageRecord` per request on every
//! visible exit path. A draft is seeded from the request shape + identity
//! at the handler boundary (`build_usage_draft`), the dispatch / token /
//! quota / outcome columns are stamped via `observe_*`, and `finalize`
//! emits the row once (idempotent). The Drop fallback stamps the
//! `client_disconnect` outcome for a cancelled / disconnected request.
//!
//! Extracted from `ingress_handle.rs` to keep both files under the
//! project's 800-line ceiling. The two handler functions
//! (`complete_response`, `stream_response`) and `render_stream_task`
//! stay in `ingress_handle.rs` and call into this module.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use routectl_core::Error;
use routectl_router::DispatchMeta;
use routectl_usage::{CapabilityLearnEvent, Outcome, UsageHandle, UsageRecord};
use serde_json::Value;

/// Seed a `UsageRecord` draft from the request shape + identity, before
/// dispatch. The dispatch / token / outcome / timing columns are stamped
/// later by `UsageCapture`. `ts_start` is the wall-clock epoch-ms when the
/// row is born; identity + shape columns come straight off `req`.
///
/// `session_id` reads `req.routectl_internal.inbound_session_key` --
/// the SAME canonical value the K-estimator keys on (see
/// `complete_response` / `stream_response`'s `session_key` extraction) --
/// rather than a separately-derived header-only value. A single source
/// means a metadata-derived (header-absent) session id persists on the
/// ledger row instead of silently recording `NULL`.
pub(crate) fn build_usage_draft(
    ingress_dialect: &str,
    req: &routectl_core::ChatRequest,
    request_id: String,
) -> UsageRecord {
    let session_id = req.routectl_internal.inbound_session_key.clone();
    let (thinking_req, thinking_req_kind) = thinking_of(req);
    UsageRecord {
        ts_start: epoch_ms_now(),
        ts_end: 0,
        request_id,
        ingress_dialect: ingress_dialect.to_string(),
        requested_model: req.model.clone(),
        // `alias` is overwritten from `meta.resolved_alias` on dispatch;
        // seed it with the raw wire model so a pre-dispatch row still
        // names the route the caller asked for.
        alias: req.model.clone(),
        model: None,
        upstream: None,
        provider: None,
        provider_kind: None,
        seat: None,
        session_id,
        stream: req.stream == Some(true),
        max_tokens_req: req.max_tokens,
        tool_count: req
            .tools
            .as_ref()
            .map_or(0, |t| u32::try_from(t.len()).unwrap_or(u32::MAX)),
        thinking_req,
        thinking_req_kind,
        msg_count: u32::try_from(req.messages.len()).unwrap_or(u32::MAX),
        service_tier: None,
        outcome: Outcome::ClientDisconnect,
        http_status: None,
        error_class: None,
        finish_reason: None,
        attempt_count: 0,
        fallback_count: 0,
        strategy: None,
        reduction_strategy: None,
        selection_decision: None,
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
    }
}

/// Derive the `(thinking_req, thinking_req_kind)` columns from the
/// request's reasoning config. A budget request (`max_tokens` set) records
/// the budget value under `"budget_tokens"`; an effort request records
/// `"effort"` with no numeric budget. Both an absent reasoning block AND a
/// `Some(reasoning)` carrying neither `max_tokens` nor `effort` yield
/// `(None, None)`.
fn thinking_of(req: &routectl_core::ChatRequest) -> (Option<u32>, Option<String>) {
    match &req.reasoning {
        Some(r) if r.max_tokens.is_some() => (r.max_tokens, Some("budget_tokens".to_string())),
        Some(r) if r.effort.is_some() => (None, Some("effort".to_string())),
        _ => (None, None),
    }
}

/// Current wall-clock time as epoch milliseconds. Uses `i64::try_from`
/// (never a lossy `as`); a pre-epoch / overflowing clock yields 0 rather
/// than a wrapped value.
pub(crate) fn epoch_ms_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// Ceiling on persisted request-feature entries per learn event. The set
/// derives from arbitrary client tool-type strings, so bound the count
/// before it reaches the ledger row -- a client must not be able to bloat
/// a persisted row with an unbounded feature list.
const MAX_REQUEST_FEATURES: usize = 16;

/// Ceiling on each persisted request-feature string, in chars. Overlong
/// entries are truncated (on a char boundary) before enqueue, for the same
/// untrusted-input reason as the count cap.
const MAX_REQUEST_FEATURE_LEN: usize = 128;

/// Bound an untrusted request-feature set before it is persisted: cap the
/// number of entries and truncate each overlong string on a char boundary.
fn bound_request_features(features: &[String]) -> Vec<String> {
    features
        .iter()
        .take(MAX_REQUEST_FEATURES)
        .map(|f| f.chars().take(MAX_REQUEST_FEATURE_LEN).collect())
        .collect()
}

/// Convert an epoch-millis timestamp (as stored on `UsageRecord::ts_start`)
/// back into a `SystemTime`, clamping a theoretical negative value to the
/// epoch. The inverse of `epoch_ms_now`; used to hand the request start
/// time to the K-estimator store so live samples age on the same clock the
/// ledger rebuild reads.
fn ms_to_system_time(ms: i64) -> SystemTime {
    UNIX_EPOCH + std::time::Duration::from_millis(ms.max(0) as u64)
}

/// Map a dispatch `Err` to its terminal `Outcome` using the documented
/// `DispatchMeta` contract: `attempt_count == 0` means the gate / breaker
/// refused before any upstream contact (`gate_blocked`); any attempt that
/// reached an upstream and failed is `upstream_error`. Timeouts surface as
/// a status-0 upstream error with `attempt_count > 0`, so they fold into
/// `upstream_error` here -- inc2 does NOT separately classify `timeout`
/// (the router error carries no distinguishable timeout marker).
pub(crate) const fn outcome_for_dispatch_err(meta: &DispatchMeta) -> Outcome {
    if meta.attempt_count == 0 {
        Outcome::GateBlocked
    } else {
        Outcome::UpstreamError
    }
}

/// True when an auto-emitted breakpoint created a cache entry that got no
/// read this request -- the thrash signature. The router auto-emitted
/// (`strategy == "auto_emitted"`), the upstream reported cache CREATION
/// (`cache_creation > 0`), and NO cache READ (`cache_read == 0`). A
/// caller-supplied or skipped strategy is never thrash here (routectl did
/// not make the decision); a created-and-read entry is the healthy case.
///
/// Pure + total so it can be unit-tested without a live capture.
pub(crate) fn is_cache_thrash(
    strategy: Option<&str>,
    cache_creation: u64,
    cache_read: u64,
) -> bool {
    strategy == Some("auto_emitted") && cache_creation > 0 && cache_read == 0
}

/// Cache-EXCLUSIVE new input for the DB `input_tokens` column. This is
/// the exact inverse of `anthropic_api::response::sum_prompt_tokens`,
/// which folds new + cache-read + cache-creation into the cache-INCLUSIVE
/// canonical `prompt_tokens`. cost.rs prices `input_tokens`, `cache_read`,
/// and `cache_write_*` as DISJOINT dimensions, so the row must store only
/// the NEW input here -- subtracting the AGGREGATE cache-creation total
/// (`cache_creation_input_tokens`), never the per-TTL breakdown (which is
/// often absent and would under-subtract). Saturating so a malformed
/// upstream tally can never wrap.
pub(crate) const fn cache_exclusive_input(
    prompt: u32,
    cache_read: u32,
    cache_creation: u32,
) -> u32 {
    prompt
        .saturating_sub(cache_read)
        .saturating_sub(cache_creation)
}

/// Integer cache-hit percentage: `read * 100 / prompt`, where `prompt` is
/// the cache-INCLUSIVE prompt total. Guards `prompt == 0` -> 0 (no
/// divide-by-zero) and saturates the multiply so a malformed tally cannot
/// wrap. Pure + total so it can be unit-tested without a live capture.
pub(crate) const fn cache_hit_pct(read: u64, prompt: u64) -> u64 {
    if prompt == 0 {
        return 0;
    }
    read.saturating_mul(100) / prompt
}

/// Short, stable error-class token for the `error_class` column. Never
/// the Display string (which can embed provider names / upstream bodies);
/// just the routectl error variant family.
pub(crate) const fn error_class_of(e: &Error) -> &'static str {
    match e {
        Error::Upstream { .. } => "upstream",
        Error::Streaming(_) => "streaming",
        Error::Validation(_) => "validation",
        Error::Auth(_) => "auth",
        Error::Config(_) => "config",
        Error::Internal(_) => "internal",
        Error::NotImplemented(_, _) => "not_implemented",
        Error::UnknownAlias(_) => "unknown_alias",
        Error::UnknownProvider(_) => "unknown_provider",
        Error::NormalizeRequest(_, _) => "normalize_request",
        Error::NormalizeResponse(_, _) => "normalize_response",
        _ => "other",
    }
}

/// Which stage of a COMMITTED stream a terminal upstream failure happened
/// at, recorded under `extra.stream_stage` so the ledger can distinguish a
/// warm-hold pre-content dispatch failure (the early-flush grace expired,
/// the SSE head was committed, THEN the dispatch resolved `Err` before any
/// content flowed) from a genuine mid-stream cut (content was delivered,
/// then the upstream died). Both finalize as `Outcome::UpstreamError`, so
/// without this marker they would collapse into one ledger bucket. It does
/// NOT apply to a fast pre-stream dispatch `Err` -- that returns a real HTTP
/// status via `map_error` and never becomes an in-stream failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamStage {
    /// Grace expired, SSE head committed, dispatch then resolved `Err`
    /// before any content -- surfaced as a single terminal in-stream error.
    PreContentDispatch,
    /// Content was delivered, then the upstream stream errored mid-flight.
    MidStream,
}

impl StreamStage {
    /// Stable lowercase token stored under `extra.stream_stage`.
    const fn as_str(self) -> &'static str {
        match self {
            Self::PreContentDispatch => "pre_content_dispatch",
            Self::MidStream => "mid_stream",
        }
    }
}

/// Unified RAII capture guard: holds the in-progress `UsageRecord` draft
/// and emits EXACTLY ONE row per request on every visible exit path.
///
/// Lifecycle:
///   - constructed at the handler boundary with the pre-seeded draft,
///   - `observe_*` stamps dispatch / token / quota / outcome columns,
///   - `finalize(outcome)` stamps timing + outcome and `try_send`s the
///     row ONCE (idempotent: a second call is a no-op),
///   - `Drop`, if not yet finalized, finalizes with the
///     `client_disconnect` fallback so a cancelled / disconnected request
///     still emits a row.
///
/// The `finalized` field is the inverse-flag: a normal exit sets it via
/// `finalize`; an abnormal exit (client hangup, render-send failure, task
/// cancellation -- where Drop runs without our code path) leaves it false
/// and Drop stamps the fallback outcome. `cancelled` is folded into
/// `client_disconnect`: inc2 cannot reliably distinguish a task
/// cancellation from a client hangup inside Drop, so it does not separate
/// them.
///
/// `try_send` never blocks / awaits / panics, so calling `finalize` from
/// Drop is safe. The guard also subsumes the old `EgressStreamSummary`
/// egress trace-summary line (emitted from `finalize`) so operators keep
/// the matching `direction=egress` summary for every stream.
pub(crate) struct UsageCapture {
    record: UsageRecord,
    usage: UsageHandle,
    ingress_id: String,
    start: Instant,
    first_byte: Option<Instant>,
    finalized: bool,
    // Stream-summary observation state (mirrors the old
    // EgressStreamSummary): chunk count + last finish_reason for the
    // egress trace line.
    chunks: u64,
    last_finish: Option<String>,
    last_prompt: u32,
    last_completion: u32,
    last_total: u32,
}

impl UsageCapture {
    pub(crate) fn new(record: UsageRecord, usage: UsageHandle, ingress_id: String) -> Self {
        Self {
            record,
            usage,
            ingress_id,
            start: Instant::now(),
            first_byte: None,
            finalized: false,
            chunks: 0,
            last_finish: None,
            last_prompt: 0,
            last_completion: 0,
            last_total: 0,
        }
    }

    /// Record the first-byte marker (first stream chunk, or the
    /// non-streaming response becoming ready). Idempotent: only the first
    /// call sticks, so ttfb measures time-to-first-byte.
    pub(crate) fn mark_first_byte(&mut self) {
        if self.first_byte.is_none() {
            self.first_byte = Some(Instant::now());
        }
    }

    /// Stamp the dispatch-derived columns from `DispatchMeta`. Valid on
    /// both the success and the all-failed paths.
    ///
    /// `meta.served_upstream` already carries the client's requested model
    /// (not `target.upstream`) when `served_forwarded_credential` is set --
    /// `DispatchMeta::mark_target` resolves that; this method just copies
    /// the column. `served_model` (the K-triple nickname dimension) is
    /// untouched on both lanes. A forwarded row additionally gets the
    /// `credential_source=forwarded` disambiguation: reused into the
    /// existing `extra` JSON column (no schema migration) and echoed as a
    /// tracing field for live-log grep, so a K/pricing query keyed on
    /// `served_model` is never perturbed by forwarded rows while both a
    /// DB consumer and an operator tailing logs can still tell them apart
    /// from own-credential rows. An own-credential row leaves `extra`
    /// untouched (byte-for-byte unchanged).
    pub(crate) fn observe_meta(&mut self, meta: &DispatchMeta) {
        self.record.alias = meta.resolved_alias.clone();
        self.record.attempt_count = meta.attempt_count;
        self.record.fallback_count = meta.fallback_count;
        self.record.provider = meta.served_provider.clone();
        self.record.provider_kind = meta.served_provider_kind.clone();
        self.record.model = meta.served_model.clone();
        self.record.upstream = meta.served_upstream.clone();
        if meta.served_forwarded_credential {
            self.stamp_extra("credential_source", "forwarded");
            tracing::debug!(
                request_id = %self.record.request_id,
                provider = self.record.provider.as_deref().unwrap_or(""),
                served_model = self.record.model.as_deref().unwrap_or(""),
                served_upstream = self.record.upstream.as_deref().unwrap_or(""),
                credential_source = "forwarded",
                "usage row disambiguated as forwarded-credential dispatch",
            );
        }
        self.record.strategy = meta.cache_strategy.map(std::string::ToString::to_string);
        self.record.reduction_strategy = meta
            .reduction_strategy
            .map(std::string::ToString::to_string);
        self.record.selection_decision = meta
            .selection_decision
            .map(std::string::ToString::to_string);
        self.record.would_trim_tokens = meta.would_trim_tokens;
        self.record.would_trim_break_even_k = meta.would_trim_break_even_k;
        self.record.would_trim_k_floor = meta.would_trim_k_floor;
        self.record.would_trim_shadow_misfire = meta.would_trim_shadow_misfire;
        self.record.would_trim_dedup_tokens = meta.would_trim_dedup_tokens;
        self.record.would_trim_supersession_tokens = meta.would_trim_supersession_tokens;
        self.record.would_trim_path_units = meta.would_trim_path_units;
        self.record.would_trim_path_extractable = meta.would_trim_path_extractable;
        self.record.would_trim_recorder_version = meta.would_trim_recorder_version;
        self.record.would_trim_raw_marks = meta.would_trim_raw_marks.clone();
        self.record.would_trim_context_fraction = meta.would_trim_context_fraction;
        self.drain_learn_events(meta);
    }

    /// Drain the dispatch's captured capability learn events into the usage
    /// writer. Empty on the common (non-capability) path; each event maps
    /// one-to-one to a `capability_learn_events` row. The tier is
    /// stringified through the persisted `SignalTier::as_str` contract and
    /// the derived feature set is carried through as the replay-verification
    /// array (the writer serializes it to the JSON TEXT column). Best-effort
    /// like every usage write: `try_send_learn_event` never blocks / awaits /
    /// panics, drops on a full channel with its own counter, and applies the
    /// same usage-enabled gate as request rows.
    fn drain_learn_events(&self, meta: &DispatchMeta) {
        for ev in &meta.learned_capabilities {
            self.usage.try_send_learn_event(CapabilityLearnEvent {
                ts: epoch_ms_now(),
                state_key: ev.state_key.clone(),
                capability_key: ev.capability_key.clone(),
                provider_kind: ev.provider_kind.clone(),
                signal_tier: ev.signal_tier.as_str().to_string(),
                observations: ev.observations,
                upstream_status: ev.upstream_status,
                remapped: ev.remapped,
                request_features: bound_request_features(&ev.request_features),
            });
        }
    }

    /// Stamp the token / quota / finish columns from a non-streaming
    /// `ChatResponse`. HTTP status is fixed at 200 for a delivered body.
    pub(crate) fn observe_response(&mut self, resp: &routectl_core::ChatResponse) {
        self.record.http_status = Some(200);
        self.record.finish_reason = resp
            .choices
            .iter()
            .rev()
            .find_map(|c| c.finish_reason.clone());
        if let Some(u) = &resp.usage {
            // Store cache-EXCLUSIVE new input: the canonical `prompt_tokens`
            // is cache-inclusive, but cost.rs prices input / cache_read /
            // cache_write_* as disjoint dimensions. Subtract the aggregate
            // cache-read + cache-creation totals so cached tokens are not
            // billed twice. A fully-cached prompt yields a real Some(0).
            self.record.input_tokens = Some(cache_exclusive_input(
                u.prompt_tokens,
                u.cache_read_input_tokens.unwrap_or(0),
                u.cache_creation_input_tokens.unwrap_or(0),
            ) as u64);
            self.record.output_tokens = Some(u.completion_tokens as u64);
            self.record.reasoning_tokens = u.reasoning_tokens.map(|v| v as u64);
            self.record.cache_read = u.cache_read_input_tokens.map(|v| v as u64);
            if let Some(cc) = &u.cache_creation {
                self.record.cache_write_5m = cc.ephemeral_5m_input_tokens.map(|v| v as u64);
                self.record.cache_write_1h = cc.ephemeral_1h_input_tokens.map(|v| v as u64);
            }
            self.record.server_tool_use = u.server_tool_use.clone();
            // Anthropic returns `service_tier` in the usage extras
            // forward-compat sweep; lift it when present.
            self.record.service_tier = u
                .extras
                .get("service_tier")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        self.observe_quota(resp.upstream_meta.as_ref());
    }

    /// Stamp token / quota / finish columns from one stream chunk, and
    /// advance the egress trace-summary tallies. Quota rides only on the
    /// first chunk's `upstream_meta`; tokens accumulate (last-writer-wins
    /// on the cumulative counters Anthropic emits at end-of-stream).
    pub(crate) fn observe_chunk(&mut self, chunk: &routectl_core::ChatChunk) {
        self.chunks += 1;
        for choice in chunk.choices.iter().rev() {
            if let Some(fr) = &choice.finish_reason {
                self.last_finish = Some(fr.clone());
                self.record.finish_reason = Some(fr.clone());
                break;
            }
        }
        if let Some(u) = &chunk.usage {
            if let Some(p) = u.prompt_tokens {
                self.last_prompt = p;
                // Cache-exclusive new input, derived from THIS delta's own
                // cache fields (Anthropic sends prompt + cache together on
                // the terminal message_delta), so the per-chunk derivation
                // stays atomic. `last_prompt` keeps the raw cache-inclusive
                // value for the egress trace summary.
                self.record.input_tokens = Some(cache_exclusive_input(
                    p,
                    u.cache_read_input_tokens.unwrap_or(0),
                    u.cache_creation_input_tokens.unwrap_or(0),
                ) as u64);
            }
            if let Some(c) = u.completion_tokens {
                self.last_completion = c;
                self.record.output_tokens = Some(c as u64);
            }
            if let Some(t) = u.total_tokens {
                self.last_total = t;
            }
            if let Some(r) = u.reasoning_tokens {
                self.record.reasoning_tokens = Some(r as u64);
            }
            if let Some(cr) = u.cache_read_input_tokens {
                self.record.cache_read = Some(cr as u64);
            }
            if let Some(cc) = &u.cache_creation {
                if let Some(v) = cc.ephemeral_5m_input_tokens {
                    self.record.cache_write_5m = Some(v as u64);
                }
                if let Some(v) = cc.ephemeral_1h_input_tokens {
                    self.record.cache_write_1h = Some(v as u64);
                }
            }
            if u.server_tool_use.is_some() {
                self.record.server_tool_use = u.server_tool_use.clone();
            }
        }
        self.observe_quota(chunk.upstream_meta.as_ref());
    }

    /// Lift the Anthropic unified quota snapshot into the QUOTA columns.
    /// No-op for non-Anthropic upstreams / when absent. Numeric utilization
    /// fields parse from their raw header strings; an unparseable value
    /// stays `None` rather than failing the row.
    fn observe_quota(&mut self, meta: Option<&routectl_core::upstream_meta::UpstreamMeta>) {
        let Some(q) = meta.and_then(|m| m.anthropic_unified.as_ref()) else {
            return;
        };
        self.record.quota_claim = q.representative_claim.clone();
        self.record.quota_status = q.status.clone();
        self.record.quota_overage_status = q.overage_status.clone();
        self.record.quota_utilization = q.utilization.as_deref().and_then(|s| s.parse().ok());
        self.record.quota_overage_utilization = q
            .overage_utilization
            .as_deref()
            .and_then(|s| s.parse().ok());
        self.record.quota_reset = q.reset.as_deref().and_then(|s| s.parse().ok());
        if !q.extras.is_empty() {
            self.record.quota_extras = Some(Value::Object(
                q.extras
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                    .collect(),
            ));
        }
    }

    /// Stamp the outcome-detail columns from a dispatch / stream error:
    /// the upstream HTTP status (when the error carries one) and the short
    /// error-class token. Never the Display string.
    pub(crate) fn observe_error(&mut self, e: &Error) {
        if let Error::Upstream { status, .. } = e {
            // A status-0 upstream error is a local gate / timeout sentinel,
            // not a real HTTP code; leave http_status None in that case.
            if *status != 0 {
                self.record.http_status = Some(*status);
            }
        }
        self.record.error_class = Some(error_class_of(e).to_string());
    }

    /// Merge one string key into the record's `extra` JSON object,
    /// preserving any existing keys. The shared write path for additive
    /// disambiguation markers that reuse the existing `extra` column
    /// instead of a new schema column (`mark_stream_stage`'s
    /// `stream_stage`, `observe_meta`'s forwarded-credential
    /// `credential_source`).
    fn stamp_extra(&mut self, key: &str, value: &str) {
        let obj = self
            .record
            .extra
            .get_or_insert_with(|| Value::Object(serde_json::Map::new()));
        if !obj.is_object() {
            *obj = Value::Object(serde_json::Map::new());
        }
        if let Value::Object(map) = obj {
            map.insert(key.to_string(), Value::String(value.to_string()));
        }
    }

    /// Stamp a stream-lifecycle stage marker into the record's `extra` JSON
    /// (`extra.stream_stage`). Lets the ledger tell a warm-hold pre-content
    /// dispatch failure apart from a mid-stream cut -- both are
    /// `Outcome::UpstreamError`, so the outcome column alone collapses them.
    /// Additive: preserves any existing `extra` object keys.
    pub(crate) fn mark_stream_stage(&mut self, stage: StreamStage) {
        self.stamp_extra("stream_stage", stage.as_str());
    }

    /// Record one live cache-reuse observation into the router's per-session
    /// K-estimator store, from the columns already stamped on this capture's
    /// draft (`provider_kind` / `model` via `observe_meta`, `cache_read` via
    /// `observe_response` / `observe_chunk`).
    ///
    /// Best-effort and POST-response: call it after the served target and
    /// the response usage are observed, around `finalize`. It must never
    /// change the response or fail the request.
    ///
    /// `session_key` is the request's `inbound_session_key`, extracted
    /// BEFORE dispatch (the request is moved into the router) and threaded
    /// in here. A keyless request, or one with no served target (a
    /// pre-dispatch failure leaving `provider_kind` / `model` unset), is
    /// skipped: there is no triple to accumulate against. The served
    /// `provider_kind` / `model` are the SAME values a later K query will
    /// key on, so they must come from the dispatch meta, not the request.
    pub(crate) fn record_k_sample(
        &self,
        router: &routectl_router::Router,
        session_key: Option<&str>,
    ) {
        let (Some(provider_kind), Some(model)) = (
            self.record.provider_kind.as_deref(),
            self.record.model.as_deref(),
        ) else {
            return;
        };
        let cache_read = self.record.cache_read.unwrap_or(0);
        let ts = ms_to_system_time(self.record.ts_start);
        router.record_k_sample(session_key, provider_kind, model, cache_read, ts);
    }

    /// Stamp timing + outcome and emit the row exactly once. Idempotent:
    /// Never blocks / awaits / panics -- safe from Drop.
    pub(crate) fn finalize(&mut self, outcome: Outcome) {
        if self.finalized {
            return;
        }
        self.finalized = true;
        self.record.outcome = outcome;
        self.record.ts_end = epoch_ms_now();
        self.record.latency_ms = i64::try_from(self.start.elapsed().as_millis()).unwrap_or(0);
        self.record.ttfb_ms = self
            .first_byte
            .map(|fb| i64::try_from(fb.duration_since(self.start).as_millis()).unwrap_or(0));
        self.emit_egress_summary();
        self.emit_cache_outcome();
        self.emit_cache_summary();
        // Move the owned record into the channel rather than cloning a
        // wide ~40-column struct (some columns carry JSON Value trees).
        // `finalized` is already set above, so the only later access --
        // Drop -- short-circuits and never touches the taken record.
        self.usage.try_send(std::mem::take(&mut self.record));
    }

    /// Emit the auto-cache outcome signal for an auto-emitted breakpoint:
    /// WARN on thrash (created an entry, got no read this request), debug
    /// on the healthy created-and-read case. No-op for caller-supplied /
    /// skipped strategies and when nothing was created. Counts only --
    /// never message content / secrets. `cache_creation` is the aggregate
    /// 5m + 1h write the upstream reported (the per-TTL columns already on
    /// the record); `cache_read` is the read count.
    fn emit_cache_outcome(&self) {
        let strategy = self.record.strategy.as_deref();
        if strategy != Some("auto_emitted") {
            return;
        }
        let cache_creation =
            self.record.cache_write_5m.unwrap_or(0) + self.record.cache_write_1h.unwrap_or(0);
        let cache_read = self.record.cache_read.unwrap_or(0);
        if cache_creation == 0 {
            // Auto-emitted but the upstream reported no creation: not a
            // thrash, not a confirmed-working cache. Nothing actionable.
            return;
        }
        let provider = self.record.provider.as_deref().unwrap_or("");
        let model = self.record.upstream.as_deref().unwrap_or("");
        if is_cache_thrash(strategy, cache_creation, cache_read) {
            tracing::warn!(
                provider = %provider,
                model = %model,
                strategy = "auto_emitted",
                cache_creation = cache_creation,
                cache_read = cache_read,
                "cache_auto_outcome",
            );
        } else {
            tracing::debug!(
                provider = %provider,
                model = %model,
                strategy = "auto_emitted",
                cache_creation = cache_creation,
                cache_read = cache_read,
                "cache_auto_outcome",
            );
        }
    }

    /// Emit the per-request cache breadcrumb: `cache=READ/PROMPT (PCT%)`.
    /// READ is the cache-read token count; PROMPT is the cache-INCLUSIVE
    /// prompt total, reconstructed from the cache-EXCLUSIVE `input_tokens`
    /// the record stores plus the disjoint cache columns
    /// (`input_tokens + cache_read + cache_write_5m + cache_write_1h`).
    ///
    /// Surfaced at INFO only when there was cache activity (a read, a
    /// write, or an auto-emitted decision), so cached / auto-emitted
    /// requests get an INFO breadcrumb while uncached requests stay at
    /// DEBUG (no `cache=0/0` flood). Counts / ids / strategy only -- never
    /// message content / secrets / bodies.
    fn emit_cache_summary(&self) {
        let cache_read = self.record.cache_read.unwrap_or(0);
        let cache_write_5m = self.record.cache_write_5m.unwrap_or(0);
        let cache_write_1h = self.record.cache_write_1h.unwrap_or(0);
        let cache_creation = cache_write_5m + cache_write_1h;
        let prompt = self
            .record
            .input_tokens
            .unwrap_or(0)
            .saturating_add(cache_read)
            .saturating_add(cache_creation);
        let pct = cache_hit_pct(cache_read, prompt);
        let strategy = self.record.strategy.as_deref().unwrap_or("");
        let provider = self.record.provider.as_deref().unwrap_or("");
        let model = self.record.model.as_deref().unwrap_or("");
        let request_id = self.record.request_id.as_str();
        let cache_active = cache_read > 0 || cache_creation > 0 || strategy == "auto_emitted";
        if cache_active {
            tracing::info!(
                request_id = %request_id,
                provider = %provider,
                model = %model,
                strategy = %strategy,
                cache_read = cache_read,
                prompt = prompt,
                cache_hit_pct = pct,
                "cache={cache_read}/{prompt} ({pct}%)",
            );
        } else {
            tracing::debug!(
                request_id = %request_id,
                provider = %provider,
                model = %model,
                strategy = %strategy,
                cache_read = cache_read,
                prompt = prompt,
                cache_hit_pct = pct,
                "cache={cache_read}/{prompt} ({pct}%)",
            );
        }
    }

    /// Emit the single `direction=egress` stream trace-summary line that
    /// the old `EgressStreamSummary` produced. Counts/ids/finish only --
    /// never message content. A non-clean exit (no observed finish) reports
    /// `"truncated"` so operators can still enumerate cuts.
    fn emit_egress_summary(&self) {
        if self.chunks == 0 && self.last_finish.is_none() {
            // Non-streaming or pre-upstream exit: no stream to summarize.
            return;
        }
        let usage = (self.last_prompt != 0 || self.last_completion != 0 || self.last_total != 0)
            .then_some(routectl_core::Usage {
                prompt_tokens: self.last_prompt,
                completion_tokens: self.last_completion,
                total_tokens: self.last_total,
                ..Default::default()
            });
        let finish_reason = if matches!(self.record.outcome, Outcome::Ok) {
            self.last_finish.as_deref()
        } else {
            Some("truncated")
        };
        routectl_core::trace_stream_summary(
            "egress",
            "ingress",
            &self.ingress_id,
            self.chunks,
            finish_reason,
            usage.as_ref(),
        );
    }
}

impl Drop for UsageCapture {
    fn drop(&mut self) {
        // Inverse-flag fallback: any drop without a prior `finalize` is an
        // abnormal exit (client hangup, render-send failure, task
        // cancellation). Stamp `client_disconnect` and emit the one row.
        // `cancelled` is folded in here -- Drop cannot reliably tell a
        // cancellation from a hangup.
        if !self.finalized {
            self.finalize(Outcome::ClientDisconnect);
        }
    }
}

#[cfg(test)]
#[path = "usage_capture_tests.rs"]
mod tests;
