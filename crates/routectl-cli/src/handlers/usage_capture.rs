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
use routectl_core::capability::{EvidenceSource, FailurePhase, Verdict};
use routectl_core::failure_class::classify;
use routectl_router::{DispatchMeta, ObservationDirection};
use routectl_usage::{CapabilityEvent, Outcome, UsageHandle, UsageRecord};
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
        resolved_class: None,
        finish_reason: None,
        attempt_count: 0,
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
        calib_estimated_tokens: None,
        calib_prompt_tokens: None,
        reduction_decision: None,
        reduction_strings_compressed: None,
        reduction_strings_skipped: None,
        reduction_strings_rejected: None,
        reduction_bytes_saved: None,
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

/// Most forward-compat quota entries captured per row. An upstream controls
/// how many quota headers it sends, and every captured entry is persisted for
/// every request, so the capture is bounded rather than trusting the peer.
const MAX_QUOTA_EXTRAS_ENTRIES: usize = 64;

/// Most bytes kept per captured quota-extra value, same rationale.
const MAX_QUOTA_EXTRA_VALUE_BYTES: usize = 1024;

/// Lift a vendor quota family's forward-compat `extras` pairs into the
/// shared `quota_extras` JSON column: a flat object of suffix -> raw
/// string value. `None` for an empty list so the column reads NULL rather
/// than an empty object. Bounded on both axes (entry count and per-value
/// length) so a header flood cannot blow up the ledger row.
fn stamp_quota_extras(extras: &[(String, String)]) -> Option<Value> {
    if extras.is_empty() {
        return None;
    }
    Some(Value::Object(
        extras
            .iter()
            .take(MAX_QUOTA_EXTRAS_ENTRIES)
            .map(|(k, v)| {
                let value = truncate_on_char_boundary(v, MAX_QUOTA_EXTRA_VALUE_BYTES);
                (k.clone(), Value::String(value.to_string()))
            })
            .collect(),
    ))
}

/// Whether a parsed utilization FRACTION is storable: finite and within a
/// generous 0-1000% band. A non-finite value must never reach the REAL
/// `quota_utilization` column -- SQLite cannot represent NaN, and readers
/// treat the column as a fraction.
fn sane_fraction(p: &f64) -> bool {
    p.is_finite() && (0.0..=10.0).contains(p)
}

/// Same guard for a value still on the 0-100 PERCENT scale.
fn sane_percent(p: &f64) -> bool {
    p.is_finite() && (0.0..=1000.0).contains(p)
}

/// Longest prefix of `s` that fits in `max_bytes` without splitting a UTF-8
/// codepoint.
fn truncate_on_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
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
    /// Cache-INCLUSIVE prompt total most recently reported by the upstream
    /// (the canonical `prompt_tokens`, unlike the cache-EXCLUSIVE residual
    /// the `input_tokens` column stores). Held here rather than stamped
    /// straight onto the record because it is admitted as calibration
    /// evidence only on the success finalize -- see `finalize`.
    observed_prompt_total: u32,
    /// The dispatch's auto-cache decision token, read PRE-persistence by
    /// the cache-outcome / cache-summary log emitters. Log-only: the ledger
    /// no longer persists it.
    cache_strategy: Option<&'static str>,
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
            observed_prompt_total: 0,
            cache_strategy: None,
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
    ///
    /// `catalog_version` / `overlay_revision` are read from the `Router`
    /// getters at the ingress boundary and stamped onto every drained
    /// capability event so a warm rebuild can filter by the boundary revision;
    /// the router's ride-along structs are NOT widened to carry them.
    pub(crate) fn observe_meta(
        &mut self,
        meta: &DispatchMeta,
        catalog_version: u32,
        overlay_revision: u64,
    ) {
        self.record.alias = meta.resolved_alias.clone();
        self.record.attempt_count = meta.attempt_count;
        self.record.fallback_count = meta.fallback_count;
        self.record.provider = meta.served_provider.clone();
        self.record.provider_kind = meta.served_provider_kind.clone();
        self.record.seat = meta.served_seat.clone();
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
        self.cache_strategy = meta.cache_strategy;
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
        self.record.calib_estimated_tokens = meta.calib_estimated_tokens;
        // Counts and the decision token only -- the counters carry no
        // payload bytes, so nothing from a request body reaches the ledger.
        self.record.reduction_decision = meta.reduction_strategy.map(str::to_string);
        self.record.reduction_strings_compressed = meta.reduction_strings_compressed;
        self.record.reduction_strings_skipped = meta.reduction_strings_skipped;
        self.record.reduction_strings_rejected = meta.reduction_strings_rejected;
        self.record.reduction_bytes_saved = meta.reduction_bytes_saved;
        self.drain_capability_events(meta, catalog_version, overlay_revision);
    }

    /// Drain the dispatch's captured capability events into the usage writer
    /// as unified `capability_events` rows -- the append-only ledger the
    /// warm-rebuild replayer reads on boot. Empty on the common
    /// (non-capability) path. The mapping, one enqueue per event:
    ///
    /// - each learned negative (`meta.learned_capabilities`) -> a `broken` row,
    ///   phase (`f1`/`f2`) and tier from the event, no evidence class, no
    ///   upstream token;
    /// - each response-evidence observation (`meta.capability_observations`) ->
    ///   a `verified` (positive) or `suspect` (F3 suspected-absence) row, phase
    ///   `f3`, tier from the event, the pinned evidence-class token;
    /// - each probe-settled clear (`meta.cleared_capabilities`) -> a `cleared`
    ///   row, `live` source, no phase / tier / evidence.
    ///
    /// Every row rides the `live` source and is stamped with the boundary
    /// `(catalog_version, overlay_revision)` read from the router getters at
    /// the ingress boundary. Best-effort like every usage write:
    /// `try_send_capability_event` never blocks / awaits / panics and drops on
    /// a full channel with its own counter. NEVER carries a request body /
    /// prompt / upstream text (log hygiene): only the normalized keys and the
    /// closed-set tokens reach the row.
    fn drain_capability_events(
        &self,
        meta: &DispatchMeta,
        catalog_version: u32,
        overlay_revision: u64,
    ) {
        let catalog_version = i64::from(catalog_version);
        let overlay_revision = i64::try_from(overlay_revision).unwrap_or(i64::MAX);
        let ts = epoch_ms_now();
        for ev in &meta.learned_capabilities {
            self.usage.try_send_capability_event(CapabilityEvent {
                ts,
                lane_key: ev.state_key.clone(),
                capability: ev.capability_key.clone(),
                verdict: Verdict::LearnedBroken(ev.phase).as_str().to_string(),
                phase: ev.phase.as_str().to_string(),
                source: ev.source.as_str().to_string(),
                tier: ev.signal_tier.as_str().to_string(),
                evidence_class: None,
                upstream_token: None,
                catalog_version,
                overlay_revision,
            });
        }
        for ev in &meta.capability_observations {
            let verdict = match ev.direction {
                ObservationDirection::Verified => Verdict::VerifiedWorking,
                ObservationDirection::SuspectAbsence => Verdict::SuspectIgnored,
            };
            self.usage.try_send_capability_event(CapabilityEvent {
                ts,
                lane_key: ev.state_key.clone(),
                capability: ev.capability_key.clone(),
                verdict: verdict.as_str().to_string(),
                phase: FailurePhase::F3.as_str().to_string(),
                source: ev.source.as_str().to_string(),
                tier: ev.signal_tier.as_str().to_string(),
                evidence_class: Some(ev.evidence_class.clone()),
                upstream_token: None,
                catalog_version,
                overlay_revision,
            });
        }
        for ev in &meta.cleared_capabilities {
            self.usage.try_send_capability_event(CapabilityEvent {
                ts,
                lane_key: ev.state_key.clone(),
                capability: ev.capability_key.clone(),
                verdict: Verdict::Cleared.as_str().to_string(),
                phase: String::new(),
                source: EvidenceSource::Live.as_str().to_string(),
                tier: String::new(),
                evidence_class: None,
                upstream_token: None,
                catalog_version,
                overlay_revision,
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
            // Calibration evidence: the canonical `prompt_tokens` verbatim,
            // cache-INCLUSIVE, kept BEFORE the cache-exclusive subtraction
            // below discards it. Uniform across every egress -- each
            // translator produces this field as new + cache-creation +
            // cache-read (or is natively inclusive) -- so no per-provider
            // branching is needed here.
            self.observed_prompt_total = u.prompt_tokens;
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
                // Same cache-INCLUSIVE calibration evidence as the
                // non-streaming path takes; the cumulative counters
                // Anthropic emits on the terminal delta make
                // last-writer-wins the whole-prompt total.
                self.observed_prompt_total = p;
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

    /// Lift whichever vendor quota snapshot the upstream reported into the
    /// shared QUOTA columns. The arms are INDEPENDENT (only one family is
    /// ever present on a response) and no-ops when absent. Numeric fields
    /// parse from their raw header strings; an unparseable value stays
    /// `None` rather than failing the row.
    fn observe_quota(&mut self, meta: Option<&routectl_core::upstream_meta::UpstreamMeta>) {
        let Some(meta) = meta else {
            return;
        };
        if let Some(q) = meta.anthropic_unified.as_ref() {
            self.observe_anthropic_quota(q);
        }
        if let Some(q) = meta.codex.as_ref() {
            self.observe_codex_quota(q);
        }
    }

    /// Map the Anthropic unified quota family into the shared QUOTA
    /// columns. `reset` and the utilization fields arrive as raw strings.
    /// The utilization values are already 0-1 fractions; a non-finite or
    /// wildly out-of-range parse is dropped rather than stored, since the
    /// ledger column is a REAL shared with the Codex arm.
    fn observe_anthropic_quota(&mut self, q: &routectl_core::AnthropicUnifiedQuota) {
        self.record.quota_claim = q.representative_claim.clone();
        self.record.quota_status = q.status.clone();
        self.record.quota_overage_status = q.overage_status.clone();
        self.record.quota_utilization = q
            .utilization
            .as_deref()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(sane_fraction);
        self.record.quota_overage_utilization = q
            .overage_utilization
            .as_deref()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(sane_fraction);
        self.record.quota_reset = q.reset.as_deref().and_then(|s| s.parse().ok());
        self.record.quota_extras = stamp_quota_extras(&q.extras);
    }

    /// Map the Codex quota family into the shared QUOTA columns. Codex
    /// reports used-percent on a 0-100 integer scale, so it normalizes to
    /// the column's 0-1 fraction; `primary_reset_at` is already epoch
    /// SECONDS -- the scale `quota_reset` stores -- so it lands verbatim.
    /// The Anthropic-only columns (status / overage) have no Codex
    /// counterpart and stay NULL.
    ///
    /// The router's `quota::reduce` module converts the same percent for a
    /// SEAT-PLACEMENT signal, and the two deliberately keep different bounds
    /// rather than sharing one function. This is an observability WRITE path,
    /// so its bounds are loose on purpose: a percent far above 100 is still
    /// RECORDED verbatim, because a weird upstream value is exactly what an
    /// operator needs to see. The routing reducer is bounded instead -- a value
    /// it cannot interpret at all becomes cap-dormant, and a finite percent
    /// over 100 saturates to an exhausted window rather than being stored raw.
    /// Two sites converting one percent can drift, so a test there pins both
    /// against the same captured input and the same expected fraction.
    fn observe_codex_quota(&mut self, q: &routectl_core::CodexQuota) {
        self.record.quota_claim = q.active_limit.clone();
        self.record.quota_utilization = q
            .primary_used_percent
            .as_deref()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(sane_percent)
            .map(|p| p / 100.0);
        self.record.quota_reset = q
            .primary_reset_at
            .as_deref()
            .and_then(|s| s.parse::<i64>().ok());
        self.record.quota_extras = stamp_quota_extras(&q.extras);
    }

    /// Stamp the outcome-detail columns from a dispatch / stream error:
    /// the upstream HTTP status (when the error carries one), the short
    /// error-class token, and -- gated on the request having reached a
    /// dispatch attempt -- the canonical `resolved_class` failure token.
    /// Never the Display string.
    ///
    /// `http_status` is the transport status the CLIENT received. Once the
    /// SSE head has committed (`mark_stream_http_committed` stamped 200), a
    /// later mid-stream upstream failure must not overwrite it: the client
    /// already saw a 200 status line, and the provider failure is carried by
    /// outcome / error_class / stream_stage instead. So the upstream status
    /// is recorded only while `http_status` is still unset -- this is the
    /// pre-head status recorder.
    ///
    /// `resolved_class` is stamped ONLY when `provider_kind` is already set
    /// (i.e. `observe_meta` ran, so the request reached a dispatch attempt).
    /// A pre-dispatch / validation / local-gate failure leaves it NULL so it
    /// reads back "unclassified" rather than getting a fake network-ish class.
    /// A class with no token (`Unknown`) also stores NULL.
    pub(crate) fn observe_error(&mut self, e: &Error) {
        // Record the upstream status only while http_status is still unset
        // (pre-head), and only for a real HTTP code -- a status-0 upstream
        // error is a local gate / timeout sentinel, not a transport status.
        if self.record.http_status.is_none()
            && let Error::Upstream { status, .. } = e
            && *status != 0
        {
            self.record.http_status = Some(*status);
        }
        self.record.error_class = Some(error_class_of(e).to_string());
        if let Some(provider_kind) = self.record.provider_kind.as_deref() {
            self.record.resolved_class = classify(e, Some(provider_kind))
                .class
                .class_token()
                .map(str::to_string);
        }
    }

    /// Stamp `http_status = 200` at the point the SSE head becomes
    /// client-visible (the first successful event send). Idempotent: only
    /// writes when `http_status` is still unset, so a repeat call over the
    /// stream lifetime is a no-op and a status recorded earlier is preserved.
    ///
    /// This is the streaming counterpart of `observe_response`'s fixed 200:
    /// http_status reflects the transport status the client received, which
    /// is 200 the moment the head commits. Call it only from a first-
    /// successful-send site, never at spawn: a disconnect before any byte
    /// flushed leaves the head uncommitted and http_status stays NULL.
    pub(crate) const fn mark_stream_http_committed(&mut self) {
        if self.record.http_status.is_none() {
            self.record.http_status = Some(200);
        }
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

    /// Record one live token-estimate observation into the router's per-lane
    /// calibration store, from the same values the ledger's own calibration
    /// pair is built from: the estimate stamped at the attempt site
    /// (`observe_meta`) and the upstream's cache-inclusive prompt total
    /// (`observe_response` / `observe_chunk`).
    ///
    /// Best-effort and POST-response, same placement as
    /// [`UsageCapture::record_k_sample`]: call it once the served target and
    /// the response usage are observed. It must never change the response or
    /// fail the request.
    ///
    /// `self.record.model` is the served NICKNAME (`observe_meta` copies
    /// `DispatchMeta::served_model`, which is the target's nickname and never
    /// the upstream wire id) -- the same label the gate's lane lookup keys on.
    /// A dispatch missing either half of that key forms no lane and is
    /// skipped inside the router.
    ///
    /// Admission mirrors the persisted pair exactly: only a success, and only
    /// a nonzero prompt total. The canonical upstream field is not optional,
    /// so an upstream reporting nothing arrives as a real zero, and admitting
    /// it would drag the lane's correction toward zero -- the direction that
    /// makes the context-window gate admit oversized requests.
    pub(crate) fn record_calibration_sample(
        &self,
        router: &routectl_router::Router,
        session_key: Option<&str>,
    ) {
        let Some(estimated_tokens) = self.record.calib_estimated_tokens else {
            return;
        };
        let Some(prompt_tokens) = self.admissible_prompt_total(Outcome::Ok) else {
            return;
        };
        router.record_calibration_sample(
            self.record.provider_kind.as_deref(),
            self.record.model.as_deref(),
            session_key,
            estimated_tokens,
            prompt_tokens,
            ms_to_system_time(self.record.ts_start),
        );
    }

    /// Stamp timing + outcome and emit the row exactly once. Idempotent:
    /// Never blocks / awaits / panics -- safe from Drop.
    pub(crate) fn finalize(&mut self, outcome: Outcome) {
        if self.finalized {
            return;
        }
        self.finalized = true;
        self.record.outcome = outcome;
        self.admit_evidence_pair(outcome);
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

    /// Admit or refuse the calibration evidence PAIR as a unit.
    ///
    /// The estimate column was copied from the dispatch meta long before the
    /// outcome was known (`observe_meta`), so a refusal decided here has to
    /// clear it as well: a row carrying an estimate with no paired actual
    /// would put the two columns in different populations, and an
    /// estimate-side aggregate would then count samples the actual-side
    /// aggregate skips.
    const fn admit_evidence_pair(&mut self, outcome: Outcome) {
        match self.admissible_prompt_total(outcome) {
            Some(total) => self.record.calib_prompt_tokens = Some(total),
            None => {
                self.record.calib_prompt_tokens = None;
                self.record.calib_estimated_tokens = None;
            }
        }
    }

    /// The observed cache-INCLUSIVE prompt total, if it is admissible as
    /// calibration evidence for a row finishing with `outcome`.
    ///
    /// Two refusals, both fail-closed:
    ///
    /// - a non-success row is refused outright -- a partial or failed request
    ///   has no trustworthy pairing between what was estimated and what the
    ///   upstream actually charged for;
    /// - a success reporting a ZERO total is refused. The canonical prompt
    ///   field is not optional, so an upstream that reports nothing arrives
    ///   here as a real 0 rather than an absence, and a stored 0 would drag a
    ///   later correction factor toward zero on what is purely a data bug.
    const fn admissible_prompt_total(&self, outcome: Outcome) -> Option<u64> {
        if !matches!(outcome, Outcome::Ok) || self.observed_prompt_total == 0 {
            return None;
        }
        Some(self.observed_prompt_total as u64)
    }

    /// Emit the auto-cache outcome signal for an auto-emitted breakpoint:
    /// WARN on thrash (created an entry, got no read this request), debug
    /// on the healthy created-and-read case. No-op for caller-supplied /
    /// skipped strategies and when nothing was created. Counts only --
    /// never message content / secrets. `cache_creation` is the aggregate
    /// 5m + 1h write the upstream reported (the per-TTL columns already on
    /// the record); `cache_read` is the read count.
    fn emit_cache_outcome(&self) {
        let strategy = self.cache_strategy;
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
        let strategy = self.cache_strategy.unwrap_or("");
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
