//! Fallback-chain router. Given an incoming request, walks the configured
//! alias chain attempting each provider until one succeeds or all are
//! exhausted. Retries within a single provider per `RetryPolicy.max_attempts`
//! with exponential backoff. Per-provider runtime gates (RPM bucket,
//! circuit breaker) skip unhealthy providers in the chain.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::stream::{BoxStream, StreamExt};
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result};

use crate::config::{Config, RetryPolicy};
use crate::runtime_state::{GateDecision, ProviderState};

pub struct Router {
    pub config: Arc<Config>,
    pub providers: BTreeMap<String, Arc<dyn Provider>>,
    /// Per-provider runtime gates. Built lazily from
    /// `config.providers[name].runtime()` on the first dispatch and
    /// kept under a mutex for the lifetime of the router.
    state: BTreeMap<String, Arc<Mutex<ProviderState>>>,
}

/// Per-request switches that the HTTP handler can flip via header
/// without polluting the wire schema. Defaults preserve current behavior.
///
/// Marked `#[non_exhaustive]` so adding new options later is a
/// non-breaking change for downstream Rust callers. Construct via
/// [`RouterOptions::new`] (alias for `default()`) and mutate fields:
///
/// ```ignore
/// let mut opts = RouterOptions::new();
/// opts.disable_fallbacks = true;
/// router.complete_with_options(req, opts).await
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, Default)]
pub struct RouterOptions {
    /// When true, do NOT walk past the first provider in the chain.
    /// The first failure (after retries) propagates verbatim.
    /// Wired to header `x-routectl-disable-fallbacks: 1`.
    pub disable_fallbacks: bool,
}

impl RouterOptions {
    /// Construct a `RouterOptions` with all-default values. Use this
    /// instead of `RouterOptions { disable_fallbacks: ... }` literals
    /// so future field additions don't break your code.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Router {
    pub fn new(config: Arc<Config>) -> Self {
        let mut state = BTreeMap::new();
        for (name, entry) in &config.providers {
            state.insert(
                name.clone(),
                Arc::new(Mutex::new(ProviderState::new(entry.runtime()))),
            );
        }
        Self {
            config,
            providers: Default::default(),
            state,
        }
    }

    pub fn register(&mut self, name: impl Into<String>, provider: Arc<dyn Provider>) {
        let name = name.into();
        // Ensure a gate exists even for providers registered without a
        // matching config entry (test harnesses rely on this).
        self.state
            .entry(name.clone())
            .or_insert_with(|| Arc::new(Mutex::new(ProviderState::new(&Default::default()))));
        self.providers.insert(name, provider);
    }

    pub async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.complete_with_options(req, RouterOptions::default()).await
    }

    pub async fn complete_with_options(
        &self,
        req: ChatRequest,
        opts: RouterOptions,
    ) -> Result<ChatResponse> {
        let chain = self.resolve_chain(&req.model)?;
        let policy = self.policy_for(&req.model);
        let hard_cap = policy.hard_retry_cap();
        let mut last_err: Option<Error> = None;

        'chain: for target in chain.iter() {
            let (provider_name, model) = parse_target(target);
            let Some(provider) = self.providers.get(provider_name).cloned() else {
                last_err = Some(Error::UnknownProvider(provider_name.to_string()));
                if opts.disable_fallbacks {
                    break 'chain;
                }
                continue;
            };

            let mut attempt_req = req.clone();
            attempt_req.model = model.to_string();

            let mut backoff = Duration::from_millis(policy.initial_backoff_ms);
            let mut attempts_made: u32 = 0;

            loop {
                // Per-attempt gate: rate limit + circuit breaker.
                // Charges one RPM token and (when half-open) claims the
                // probe slot. If the gate refuses, treat as a fallback
                // event for THIS provider and move to the next chain
                // entry -- retrying the same provider would just hit
                // the gate again.
                if let Some(gate_err) = self.gate_check(provider_name) {
                    tracing::warn!(provider = provider_name, ?gate_err, "gate blocked");
                    last_err = Some(gate_err);
                    if opts.disable_fallbacks {
                        break 'chain;
                    }
                    continue 'chain;
                }

                if attempts_made > 0 {
                    let jittered = add_jitter(backoff, policy.jitter_ms);
                    tokio::time::sleep(jittered).await;
                    backoff = mul_duration(backoff, policy.backoff_multiplier);
                }

                let result = run_with_timeout(provider.as_ref(), &attempt_req, &policy).await;
                attempts_made += 1;

                match result {
                    Ok(mut resp) => {
                        self.record_success(provider_name);
                        resp.routectl_provider = Some(provider_name.to_string());
                        return Ok(resp);
                    }
                    Err(e) => {
                        self.record_failure(provider_name);
                        if opts.disable_fallbacks {
                            return Err(e);
                        }
                        let can_retry_here = attempts_made < hard_cap
                            && should_retry_same_provider(&e, &policy, attempts_made);
                        if can_retry_here {
                            tracing::debug!(attempt = attempts_made, error = ?e, "retrying same provider");
                            // last_err is overwritten on the next failure
                            // or in the fallback branch; no need to store
                            // intermediate retry errors.
                            let _ = e;
                            continue;
                        }
                        // Done with this provider. Decide fallback vs propagate.
                        if should_fallback(&e, &policy) {
                            tracing::warn!(provider = provider_name, error = ?e, "fallback to next");
                            last_err = Some(e);
                            continue 'chain;
                        }
                        return Err(e);
                    }
                }
            }
        }

        Err(last_err.unwrap_or_else(|| Error::UnknownAlias(req.model.clone())))
    }

    pub async fn stream(
        &self,
        req: ChatRequest,
    ) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.stream_with_options(req, RouterOptions::default()).await
    }

    /// Streaming counterpart. Fallback only happens BEFORE the first
    /// chunk reaches us; once the upstream has emitted a chunk,
    /// mid-stream errors propagate. Gate checks (rate limit / breaker)
    /// run before the upstream is touched.
    pub async fn stream_with_options(
        &self,
        req: ChatRequest,
        opts: RouterOptions,
    ) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let chain = self.resolve_chain(&req.model)?;
        let policy = self.policy_for(&req.model);
        let mut last_err: Option<Error> = None;

        'chain: for target in chain.iter() {
            let (provider_name, model) = parse_target(target);
            let Some(provider) = self.providers.get(provider_name).cloned() else {
                last_err = Some(Error::UnknownProvider(provider_name.to_string()));
                if opts.disable_fallbacks {
                    break 'chain;
                }
                continue;
            };

            // Per-attempt gate: streams don't retry against the same
            // provider, so this is also the "single attempt" gate.
            if let Some(gate_err) = self.gate_check(provider_name) {
                tracing::warn!(provider = provider_name, ?gate_err, "stream gate blocked");
                last_err = Some(gate_err);
                if opts.disable_fallbacks {
                    break 'chain;
                }
                continue 'chain;
            }

            let mut attempt_req = req.clone();
            attempt_req.model = model.to_string();

            match try_stream_with_first_chunk(provider, attempt_req, &policy).await {
                Ok(stream) => {
                    // DON'T record_success here. A first chunk arriving
                    // is not the same as a healthy upstream -- a provider
                    // that emits one token then dies should still count
                    // toward the breaker. Wrap the stream so success
                    // records on clean EOS and the first error inside
                    // the stream records a failure.
                    let state = self.state.get(provider_name).cloned();
                    return Ok(wrap_with_breaker_accounting(stream, state));
                }
                Err(e) => {
                    self.record_failure(provider_name);
                    if opts.disable_fallbacks {
                        return Err(e);
                    }
                    if should_fallback(&e, &policy) {
                        tracing::warn!(provider = provider_name, error = ?e, "stream fallback to next");
                        last_err = Some(e);
                        continue 'chain;
                    }
                    return Err(e);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| Error::UnknownAlias(req.model.clone())))
    }

    /// Run RPM bucket + circuit breaker. Returns `Some(err)` if the
    /// gate refuses this dispatch (pretreated as a fallbackable
    /// status-0 upstream error).
    fn gate_check(&self, provider_name: &str) -> Option<Error> {
        let state = self.state.get(provider_name)?.clone();
        let mut s = state.lock().expect("poisoned");
        match s.try_dispatch(Instant::now()) {
            GateDecision::Allow => None,
            GateDecision::RateLimited => Some(Error::upstream(
                provider_name,
                0,
                "local rpm_limit exceeded",
            )),
            GateDecision::CircuitOpen => Some(Error::upstream(
                provider_name,
                0,
                "circuit breaker open",
            )),
        }
    }

    fn record_success(&self, provider_name: &str) {
        if let Some(state) = self.state.get(provider_name) {
            state.lock().expect("poisoned").record_success();
        }
    }

    fn record_failure(&self, provider_name: &str) {
        if let Some(state) = self.state.get(provider_name) {
            state.lock().expect("poisoned").record_failure(Instant::now());
        }
    }

    fn resolve_chain(&self, model: &str) -> Result<Vec<String>> {
        if let Some(alias) = self.config.aliases.get(model) {
            return Ok(alias.chain.clone());
        }
        if model.contains(':') {
            return Ok(vec![model.to_string()]);
        }
        Err(Error::UnknownAlias(model.to_string()))
    }

    fn policy_for(&self, model: &str) -> RetryPolicy {
        self.config
            .aliases
            .get(model)
            .and_then(|a| a.retry.clone())
            .unwrap_or_else(|| self.config.retry.clone())
    }
}

/// Execute one upstream `complete()` call with the policy's per-request
/// timeout (if configured). Timeout expiry surfaces as a status-0
/// upstream error, treated as a network class for retry/fallback.
async fn run_with_timeout(
    provider: &dyn Provider,
    req: &ChatRequest,
    policy: &RetryPolicy,
) -> Result<ChatResponse> {
    match policy.request_timeout_ms {
        Some(ms) => match tokio::time::timeout(
            Duration::from_millis(ms),
            provider.complete(req.clone()),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => Err(Error::upstream(
                provider.id(),
                0,
                format!("request timed out after {ms}ms"),
            )),
        },
        None => provider.complete(req.clone()).await,
    }
}

/// Wrap an upstream stream so the breaker records success on clean
/// completion (None / EOS) and records ONE failure on the first error
/// that bubbles out of the stream. Subsequent errors do not double-count.
fn wrap_with_breaker_accounting(
    inner: BoxStream<'static, Result<ChatChunk>>,
    state: Option<Arc<Mutex<crate::runtime_state::ProviderState>>>,
) -> BoxStream<'static, Result<ChatChunk>> {
    use futures::stream::StreamExt as _;
    let s = async_stream::stream! {
        let mut errored = false;
        let mut inner = inner;
        while let Some(item) = inner.next().await {
            let is_err = item.is_err();
            if is_err && !errored {
                errored = true;
                if let Some(st) = &state {
                    st.lock().expect("poisoned").record_failure(Instant::now());
                }
            }
            yield item;
        }
        if !errored {
            if let Some(st) = &state {
                st.lock().expect("poisoned").record_success();
            }
        }
    };
    Box::pin(s)
}

/// Open the upstream stream and pull the first chunk. If that initial step
/// fails with a fallbackable error, return it so the caller can try the next
/// provider. If the first chunk arrives, return a `BoxStream` that yields it
/// followed by the rest of the upstream stream -- mid-stream errors propagate.
///
/// `policy.stream_first_byte_timeout_ms` (when set) caps the wait for the
/// stream-open + first-chunk arrival; expiry surfaces as a status-0
/// upstream error which is fallbackable per `should_fallback`.
async fn try_stream_with_first_chunk(
    provider: Arc<dyn Provider>,
    req: ChatRequest,
    policy: &RetryPolicy,
) -> Result<BoxStream<'static, Result<ChatChunk>>> {
    let provider_id = provider.id().to_string();
    let open_and_first = async {
        let mut upstream = provider.stream(req).await?;
        match upstream.next().await {
            Some(Ok(first)) => {
                let merged = futures::stream::once(async move { Ok(first) }).chain(upstream);
                Ok(merged.boxed())
            }
            Some(Err(e)) => Err(e),
            None => Ok(futures::stream::empty().boxed()),
        }
    };

    match policy.stream_first_byte_timeout_ms {
        Some(ms) => match tokio::time::timeout(Duration::from_millis(ms), open_and_first).await {
            Ok(r) => r,
            Err(_) => Err(Error::upstream(
                &provider_id,
                0,
                format!("stream first-byte timeout after {ms}ms"),
            )),
        },
        None => open_and_first.await,
    }
}

fn parse_target(target: &str) -> (&str, &str) {
    target.split_once(':').unwrap_or((target, ""))
}

fn should_fallback(err: &Error, policy: &RetryPolicy) -> bool {
    match err {
        // status 0 means we never reached the upstream HTTP layer
        // (DNS, TCP connect, TLS handshake, request body, timeout). Always
        // fallbackable -- nothing upstream-specific has happened yet.
        Error::Upstream { status: 0, .. } => true,
        Error::Upstream { status, .. } => policy.fallback_on_status.contains(status),
        Error::Streaming(_) => true,
        Error::UnknownProvider(_) => true,
        _ => false,
    }
}

fn should_retry_same_provider(err: &Error, policy: &RetryPolicy, attempts_made: u32) -> bool {
    let cap = match err {
        Error::Upstream { status, .. } => policy.retries_for_status(*status),
        Error::Streaming(_) => policy.retry_on_5xx.unwrap_or(policy.max_attempts),
        _ => 0,
    };
    attempts_made < cap
}

fn add_jitter(base: Duration, jitter_ms: u64) -> Duration {
    if jitter_ms == 0 {
        return base;
    }
    // Non-cryptographic jitter from the wall clock's sub-millisecond
    // bits. Suitable for retry-spreading; not for anything else.
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    base + Duration::from_millis(nanos % jitter_ms)
}

fn mul_duration(d: Duration, factor: f64) -> Duration {
    let nanos = d.as_nanos() as f64 * factor;
    Duration::from_nanos(nanos.min(u64::MAX as f64) as u64)
}
