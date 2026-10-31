//! Breaker accounting for a committed upstream stream, and the pre-content
//! guard that buffers content-free leading chunks until the first
//! content-bearing one commits the provider.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::{BoxStream, StreamExt};
use parking_lot::Mutex;
use routectl_core::{
    ChatChunk, ChatRequest, Error, Provider, Result,
    failure_class::{FailureClass, LastOutcome, classify},
};

use super::breaker_transition;
use super::class_observe::{apply_remap, upstream_status_for_remap};
use super::outcome_rule::{BreakerOutcome, breaker_outcome, reset_hint_clamped_to};
use super::precontent::{is_empty_reasoning_only, precontent_summary};

/// Wrap an upstream stream so the breaker settles exactly once, on whichever
/// of three exits the stream takes first:
///
/// - natural end of stream (the inner stream yields `None`) records success;
/// - the FIRST error frame settles that error; later frames, and the end of
///   stream that may follow, record nothing more;
/// - a drop before either (the client cancelled after content) records
///   nothing: neither a success that would reset the failure count nor a
///   failure.
///
/// An error frame settles by the same rule as a failure before content
/// (`breaker_outcome`), on the class after the target's `class_overrides`
/// remap: a debiting (health) class takes the threshold-gated debit, a
/// non-debiting class (auth, bad request, content policy, context window,
/// unsupported feature, ...) records nothing, and a non-probe frame whose
/// Retry-After, clamped to `max_honored_retry_after`, exceeds the in-loop
/// cap parks the breaker for that long. A probe stream (`is_probe`) keeps
/// the probe exceptions: its 429/529 leaves the breaker untouched and it
/// never parks.
///
/// For a half-open PROBE the call site already closed the breaker and
/// released the probe slot on first content, so a mid-stream failure here
/// debits the just-closed breaker. For a HEALTHY (closed) breaker the call
/// site did NOT touch the breaker, so this wrap is where mid-stream failures
/// accumulate toward the threshold and a natural end resets the counter.
///
/// A settle that moves the circuit (a debit that opens it, a park, a natural
/// end that closes an open circuit) logs the same transition line the
/// dispatch walk does, keyed on the slot's `state_key`.
///
/// Also the streaming quota seam: `quota_feed` is offered every Ok chunk on
/// its way through and takes the reading off the FIRST one carrying it, which
/// is the only chunk that carries it. This wrap is where that happens because
/// it is the last router-owned point before the caller consumes the stream:
/// the reading is gone by end-of-stream.
pub(super) fn wrap_with_breaker_accounting(
    inner: BoxStream<'static, Result<ChatChunk>>,
    slot: Option<BreakerSlot>,
    provider_kind: Option<&'static str>,
    quota_feed: crate::quota::feed::FirstChunkFeed,
    is_probe: bool,
    class_overrides: BTreeMap<u16, FailureClass>,
    max_honored_retry_after: Duration,
) -> BoxStream<'static, Result<ChatChunk>> {
    use futures::stream::StreamExt as _;

    let mut accounting = BreakerAccounting {
        slot,
        ctx: FrameErrorContext {
            provider_kind,
            is_probe,
            class_overrides,
            max_honored_retry_after,
        },
        settled: false,
    };
    let mut quota_feed = quota_feed;
    let s = async_stream::stream! {
        let mut inner = inner;
        while let Some(item) = inner.next().await {
            match &item {
                Ok(chunk) => quota_feed.offer(chunk),
                Err(e) => accounting.settle(TailExit::FrameError(e)),
            }
            yield item;
        }
        accounting.settle(TailExit::EndOfStream);
    };
    Box::pin(s)
}

/// The breaker slot a committed stream settles, and the key that names it.
pub(super) struct BreakerSlot {
    pub(super) state: Arc<Mutex<crate::runtime_state::ProviderState>>,
    pub(super) state_key: String,
}

/// How a committed stream's tail ended, as far as the breaker is concerned.
enum TailExit<'a> {
    EndOfStream,
    FrameError(&'a Error),
    Cancelled,
}

struct FrameErrorContext {
    provider_kind: Option<&'static str>,
    is_probe: bool,
    class_overrides: BTreeMap<u16, FailureClass>,
    max_honored_retry_after: Duration,
}

struct BreakerAccounting {
    slot: Option<BreakerSlot>,
    ctx: FrameErrorContext,
    settled: bool,
}

impl BreakerAccounting {
    /// Run `f` on the slot under its lock, then hand `f`'s verdict on
    /// whether the circuit transitioned to `log` with the lock released.
    fn with_state(
        &self,
        f: impl FnOnce(&mut crate::runtime_state::ProviderState) -> bool,
        log: impl FnOnce(&str),
    ) {
        let Some(slot) = &self.slot else {
            return;
        };
        let transitioned = f(&mut slot.state.lock());
        if transitioned {
            log(&slot.state_key);
        }
    }

    fn settle(&mut self, exit: TailExit<'_>) {
        if std::mem::replace(&mut self.settled, true) {
            return;
        }
        match exit {
            TailExit::EndOfStream => self.with_state(
                |state| state.record_success_closing(Instant::now()),
                breaker_transition::log_closed,
            ),
            TailExit::FrameError(e) => self.settle_frame_error(e),
            TailExit::Cancelled => {}
        }
    }

    fn settle_frame_error(&self, e: &Error) {
        let ctx = &self.ctx;
        let native = classify(e, ctx.provider_kind);
        let (cf, _) = apply_remap(native, upstream_status_for_remap(e), &ctx.class_overrides);
        let hint = reset_hint_clamped_to(e, ctx.max_honored_retry_after);
        match breaker_outcome(e, &cf.class, hint, ctx.is_probe) {
            BreakerOutcome::None => {}
            BreakerOutcome::Debit => self.with_state(
                |state| {
                    let outcome = LastOutcome::from_failure_class(&cf.class);
                    state.record_failure_debit(Instant::now(), outcome).opened
                },
                breaker_transition::log_opened,
            ),
            BreakerOutcome::Park(cooldown) => self.with_state(
                |state| {
                    state.force_open(Instant::now(), cooldown);
                    true
                },
                |state_key| breaker_transition::log_parked(state_key, cooldown),
            ),
        }
    }
}

impl Drop for BreakerAccounting {
    fn drop(&mut self) {
        // A no-op after either other exit; otherwise the consumer dropped the
        // stream mid-tail, which says nothing about the provider's health.
        self.settle(TailExit::Cancelled);
    }
}

/// Upper bound on the content-free leading chunks that are NOT
/// empty-reasoning-only (see `is_empty_reasoning_only`), counted while
/// waiting for the first content-bearing one. A healthy stream opens with
/// at most a handful of metadata chunks (a role delta, an id/model stamp);
/// a stream that exceeds this before emitting content is treated as a
/// pre-content failure rather than buffering unboundedly. Empty-reasoning
/// chunks are not counted, and each consecutive run of them is buffered as
/// its first chunk only, so the buffer holds at most twice this many
/// entries; `stream_first_byte_timeout_ms` bounds a stream that never
/// sends anything else.
pub(super) const MAX_PRECONTENT_CHUNKS: usize = 8;

/// True when `chunk` carries client-visible generated content: non-empty
/// text or reasoning text, typed reasoning blocks, tool-call requests, or
/// opaque SSE blocks. Content-free metadata -- a leading role delta,
/// id/model/upstream_meta stamps, a usage-only tail, a bare finish_reason,
/// or empty choices -- returns false: those are not the content-commit
/// boundary and may still be followed by a fallback to a sibling provider.
///
/// Opaque carriers count as content: they are client-visible unknown block
/// data that cannot be mixed with a different provider's output, so once one
/// arrives the provider is committed exactly as a text chunk commits it.
pub(super) fn is_content_bearing(chunk: &ChatChunk) -> bool {
    if !chunk.opaque_events.is_empty() {
        return true;
    }
    chunk.choices.iter().any(|choice| {
        let delta = &choice.delta;
        delta.content.as_deref().is_some_and(|t| !t.is_empty())
            || delta.reasoning.as_deref().is_some_and(|t| !t.is_empty())
            || !delta.reasoning_details.is_empty()
            || delta
                .tool_calls
                .as_ref()
                .is_some_and(|calls| !calls.is_empty())
    })
}

/// Open the upstream stream and pull chunks until the first CONTENT-bearing
/// one (see `is_content_bearing`). The hard non-retry boundary is first
/// content, not stream-open: leading content-free chunks (a `delta.role`
/// opener, id/model metadata) are buffered, so a fallbackable error, EOS, or
/// buffer overflow in the [stream-open, first-content] window still walks to
/// the next provider. If a content chunk arrives, return a `BoxStream` that
/// yields the buffered metadata (in order, upstream_meta preserved), then the
/// first content chunk, then the rest of the upstream stream -- mid-stream
/// errors propagate.
///
/// The buffer is never exposed to the client before content commits: the API
/// cannot both surface those chunks and re-enter the outer fallback loop.
///
/// The returned future is the whole pre-content window: stream-open PLUS the
/// pre-content pull loop. The dispatch walk's attempt deadline
/// (`stream_first_byte_timeout_ms`) wraps it whole, so leading content-free
/// chunks neither reset nor satisfy that deadline -- a role-then-hang still
/// trips it and falls over.
///
/// Also emits a debug-level first-activity log the moment the upstream
/// response headers arrive, ahead of the pre-content pull below (see the
/// `attempt_start` comment inside).
pub(super) async fn try_stream_with_first_content(
    provider_name: &str,
    upstream_model: &str,
    provider: Arc<dyn Provider>,
    req: ChatRequest,
) -> Result<BoxStream<'static, Result<ChatChunk>>> {
    // Per-attempt clock for the first-activity mark below -- NOT the
    // request-level clock `UsageCapture::start` uses for `ttfb_ms`
    // (that lives in a higher crate this one cannot see). For the
    // common case (no prior fallback hop on this request) the two are
    // within noise of each other, so `elapsed_ms` here is directly
    // comparable to the ledger's `ttfb_ms` to derive the first-activity
    // -> first-content gap. A request that fell back through one
    // or more dead chain entries first will show extra unaccounted time
    // on the ttfb side that this attempt's clock does not carry -- that
    // is chain-walk overhead, a separate concern from the prefill gap
    // this instrumentation targets.
    let attempt_start = Instant::now();
    let mut upstream = provider.stream(req).await?;
    // First sign of upstream life: response headers arrived (every
    // egress's `stream()` awaits `client.execute()` -- which
    // resolves once the status line + headers are in -- BEFORE
    // constructing the lazy body-byte stream below). Distinct from
    // the first-CONTENT mark (`mark_first_byte` in
    // `ingress_handle.rs`), which additionally waits out any
    // upstream `message_start`/`ping` events the SSE parser
    // swallows. One site here covers every provider (Anthropic,
    // Bedrock, gemini, openai-compat, openai-responses) since they
    // all share this `stream()` -> execute -> byte-stream shape.
    // Debug-only; no bodies/prompts/PII, structured fields only.
    // Manual capture recipe: docs/LOGGING.md, "Stream first-activity
    // mark" -- run with ROUTECTL_LOG=routectl_router=debug and issue
    // a streaming request; this line's `elapsed_ms` is the gap
    // between upstream headers and the existing first-content
    // ttfb_ms mark.
    tracing::debug!(
        provider = provider_name,
        upstream = upstream_model,
        elapsed_ms = attempt_start.elapsed().as_millis() as u64,
        "stream first-activity: upstream response headers received",
    );
    // Buffer content-free leading chunks (role opener, id/model
    // metadata) until the first content-bearing one. Bounded: a
    // stream that never produces content must not buffer forever.
    let mut buffered: Vec<ChatChunk> = Vec::new();
    let mut counted = 0usize;
    loop {
        match upstream.next().await {
            Some(Ok(chunk)) if is_content_bearing(&chunk) => {
                // Commit: buffered metadata (order + upstream_meta
                // preserved) -> first content -> upstream tail.
                let head = std::mem::take(&mut buffered);
                let merged = futures::stream::iter(head.into_iter().map(Ok))
                    .chain(futures::stream::once(async move { Ok(chunk) }))
                    .chain(upstream);
                return Ok(merged.boxed());
            }
            Some(Ok(chunk)) if is_empty_reasoning_only(&chunk) => {
                if !buffered.last().is_some_and(is_empty_reasoning_only) {
                    buffered.push(chunk);
                }
                // Uncounted chunks leave the loop unbounded: an upstream
                // that is always ready would never return Pending, and the
                // first-content timeout around this future could not fire.
                tokio::task::consume_budget().await;
            }
            Some(Ok(chunk)) => {
                if counted >= MAX_PRECONTENT_CHUNKS {
                    // Buffer overflow before any content: a
                    // pre-content failure. Discard the buffer (nothing
                    // reached the client) and fall over. Fallbackable
                    // per `should_fallback`; the breaker records it.
                    let summary = precontent_summary(buffered.iter().chain([&chunk]));
                    return Err(Error::Streaming(format!(
                        "{provider_name} emitted more than {MAX_PRECONTENT_CHUNKS} \
                         content-free chunks before any content (buffered: {summary})",
                    )));
                }
                counted += 1;
                buffered.push(chunk);
            }
            Some(Err(e)) => return Err(e),
            // Upstream ended before any content (empty stream, or only
            // content-free chunks). This is NOT a successful empty
            // completion -- a healthy provider always emits content
            // (even a role opener is followed by content or an error).
            // Discard the buffer and treat as a fallbackable streaming
            // error so the chain walks to the next provider AND the
            // breaker records a failed probe. Without this, an upstream
            // that closes before producing content would be reported as
            // a successful completion to both the client and the
            // router's health accounting.
            None => {
                let summary = precontent_summary(&buffered);
                return Err(Error::Streaming(format!(
                    "{provider_name} stream closed before any content arrived \
                     (buffered: {summary})",
                )));
            }
        }
    }
}
