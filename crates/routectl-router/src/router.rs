//! Fallback-chain router. Given an incoming request, walks the configured
//! alias chain attempting each provider until one succeeds or all are
//! exhausted. Retries within a single provider per `RetryPolicy.max_attempts`
//! with exponential backoff. Per-provider runtime gates (RPM bucket,
//! circuit breaker) skip unhealthy providers in the chain.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::{BoxStream, StreamExt};
use parking_lot::Mutex;
use routectl_core::{
    sanitize_for_log, ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result,
};

use crate::config::{Config, RetryPolicy};
use crate::runtime_state::{GateDecision, ProviderState};

pub struct Router {
    pub config: Arc<Config>,
    /// Provider implementations keyed by user-facing name. Private so
    /// every insertion goes through [`Router::register`], which keeps
    /// the parallel `state` map (RPM bucket, circuit breaker) in sync.
    /// A direct insert here would silently disable runtime gating for
    /// that provider -- see `gate_check`.
    providers: BTreeMap<String, Arc<dyn Provider>>,
    /// Per-provider runtime gates. Eagerly populated from
    /// `config.providers[name].runtime()` in `Router::new` for every
    /// configured provider, plus an on-demand zero-policy entry
    /// inserted on first dispatch for any provider registered after
    /// construction. Kept under a parking_lot Mutex (no poisoning) for
    /// the lifetime of the router.
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

        // Heuristic startup warning: if default_model is a
        // `provider:model` literal AND the operator has [aliases.*]
        // entries with their own [aliases.<name>.retry] tables, the
        // literal-form default bypasses alias-attached retry. Operators
        // who care about retry-per-alias should use the alias name as
        // the default. The warning fires once at startup; it doesn't
        // change behavior.
        if let Some(default) = config.default_model.as_deref() {
            let is_literal = default.contains(':') && !config.aliases.contains_key(default);
            let any_alias_has_retry = config.aliases.values().any(|a| a.retry.is_some());
            if is_literal && any_alias_has_retry {
                tracing::warn!(
                    default_model = %default,
                    "default_model is a `provider:model` literal but the [aliases] table has per-alias [retry] overrides; \
                     literal defaults inherit the top-level [retry] only. \
                     Set default_model to an alias name to attach a per-alias retry policy.",
                );
            }
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
        self.complete_with_options(req, RouterOptions::default())
            .await
    }

    #[tracing::instrument(skip_all, fields(alias = %sanitize_for_log(&req.model)))]
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
                if let Some((gate_kind, gate_err)) = self.gate_check(provider_name) {
                    tracing::warn!(
                        provider = provider_name,
                        gate_kind,
                        error = ?gate_err,
                        "gate blocked",
                    );
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

                let attempt_policy = self.compose_attempt_policy(&policy, provider_name);
                let result = run_with_timeout(
                    provider_name,
                    provider.as_ref(),
                    &attempt_req,
                    &attempt_policy,
                )
                .await;
                attempts_made += 1;

                match result {
                    Ok(mut resp) => {
                        self.record_success(provider_name);
                        resp.routectl_provider = Some(provider_name.to_string());
                        return Ok(resp);
                    }
                    Err(e) => {
                        // Only charge the circuit breaker for
                        // health-indicative failures. A 400 (bad
                        // request shape), 401 (auth), 404 (model not
                        // found), etc. is the caller's mistake -- it
                        // says nothing about whether the provider is
                        // healthy, so quarantining the provider on
                        // repeated client errors would be wrong. We
                        // piggyback on `should_fallback` because it
                        // already encodes "is this error provider-side
                        // and worth working around?" (network/status=0,
                        // configured 5xx, 429, streaming).
                        // Capture once: should_fallback is a pure
                        // predicate but evaluating it twice on the
                        // same value is a maintenance hazard
                        // (asymmetric edits would silently break the
                        // breaker-vs-fallback symmetry).
                        let do_fallback = should_fallback(&e, &policy);
                        if do_fallback {
                            self.record_failure(provider_name);
                        }
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
                        if do_fallback {
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

    pub async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.stream_with_options(req, RouterOptions::default())
            .await
    }

    /// Streaming counterpart. Fallback only happens BEFORE the first
    /// chunk reaches us; once the upstream has emitted a chunk,
    /// mid-stream errors propagate. Gate checks (rate limit / breaker)
    /// run before the upstream is touched.
    #[tracing::instrument(skip_all, fields(alias = %sanitize_for_log(&req.model)))]
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
            if let Some((gate_kind, gate_err)) = self.gate_check(provider_name) {
                tracing::warn!(
                    provider = provider_name,
                    gate_kind,
                    error = ?gate_err,
                    "stream gate blocked",
                );
                last_err = Some(gate_err);
                if opts.disable_fallbacks {
                    break 'chain;
                }
                continue 'chain;
            }

            let mut attempt_req = req.clone();
            attempt_req.model = model.to_string();

            let attempt_policy = self.compose_attempt_policy(&policy, provider_name);
            match try_stream_with_first_chunk(provider_name, provider, attempt_req, &attempt_policy)
                .await
            {
                Ok(stream) => {
                    // DON'T record_success here. A first chunk arriving
                    // is not the same as a healthy upstream -- a provider
                    // that emits one token then dies should still count
                    // toward the breaker. Wrap the stream so success
                    // records on clean EOS and the first error inside
                    // the stream records a failure.
                    let state = self.state.get(provider_name).cloned();
                    let cancel_is_failure = state
                        .as_ref()
                        .is_some_and(|st| st.lock().half_open_probe_in_flight());
                    return Ok(wrap_with_breaker_accounting(
                        stream,
                        state,
                        cancel_is_failure,
                    ));
                }
                Err(e) => {
                    // See parallel comment in `complete_with_options`:
                    // only charge the breaker for health-indicative
                    // failures. Stream-path errors *before* the first
                    // chunk arrives go through here; mid-stream errors
                    // are handled by `BreakerAccounting` in the wrapped
                    // stream and apply the same gating implicitly
                    // (`Error::Streaming(_)` is fall-back-able).
                    // Capture once -- see parallel comment in
                    // complete_with_options.
                    let do_fallback = should_fallback(&e, &policy);
                    if do_fallback {
                        self.record_failure(provider_name);
                    }
                    if opts.disable_fallbacks {
                        return Err(e);
                    }
                    if do_fallback {
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

    /// Run RPM bucket + circuit breaker. Returns `Some((kind, err))` if
    /// the gate refuses this dispatch (pretreated as a fallbackable
    /// status-0 upstream error). The `kind` tag is a stable string
    /// (`"rate_limit"` or `"circuit_breaker"`) used as a `gate_kind`
    /// field on the gate-blocked log so operators can filter by reason.
    fn gate_check(&self, provider_name: &str) -> Option<(&'static str, Error)> {
        let state = self.state.get(provider_name)?.clone();
        let mut s = state.lock();
        match s.try_dispatch(Instant::now()) {
            GateDecision::Allow => None,
            GateDecision::RateLimited => Some((
                "rate_limit",
                Error::upstream(provider_name, 0, "local rpm_limit exceeded"),
            )),
            GateDecision::CircuitOpen => Some((
                "circuit_breaker",
                Error::upstream(provider_name, 0, "circuit breaker open"),
            )),
        }
    }

    fn record_success(&self, provider_name: &str) {
        if let Some(state) = self.state.get(provider_name) {
            state.lock().record_success();
        }
    }

    fn record_failure(&self, provider_name: &str) {
        if let Some(state) = self.state.get(provider_name) {
            state.lock().record_failure(Instant::now());
        }
    }

    fn resolve_chain(&self, model: &str) -> Result<Vec<String>> {
        if let Some(alias) = self.config.aliases.get(model) {
            return Ok(alias.chain.clone());
        }
        if model.contains(':') {
            return Ok(vec![model.to_string()]);
        }
        // Fallback to the configured default model when the request's
        // `model` field doesn't match any configured alias and isn't a
        // `provider:model` literal. Lets new client-side model names
        // route to a sensible destination without requiring an operator
        // to update [ingress.<dialect>.aliases] for every release. The
        // default value can be either an alias key from [aliases] OR a
        // `provider:model` literal -- the same shapes the wire `model`
        // field accepts. If the value is neither, log a WARN and fall
        // through to UnknownAlias for the ORIGINAL request model so
        // the offending name still appears in the error.
        if let Some(default) = self.config.default_model.as_deref() {
            if let Some(alias) = self.config.aliases.get(default) {
                // DEBUG (not INFO): when default_model is configured
                // wide-open as a catch-all, this fires on every request
                // whose model didn't otherwise match. INFO would bury
                // unrelated lines in production.
                tracing::debug!(
                    requested_model = %sanitize_for_log(model),
                    default_model = %default,
                    "resolved unknown model to default_model (alias)",
                );
                return Ok(alias.chain.clone());
            }
            if default.contains(':') {
                tracing::debug!(
                    requested_model = %sanitize_for_log(model),
                    default_model = %default,
                    "resolved unknown model to default_model (provider:model literal)",
                );
                return Ok(vec![default.to_string()]);
            }
            tracing::warn!(
                requested_model = %sanitize_for_log(model),
                default_model = %default,
                "default_model is configured but is neither an alias key nor a provider:model literal; falling through to UnknownAlias",
            );
        }
        Err(Error::UnknownAlias(model.to_string()))
    }

    fn policy_for(&self, model: &str) -> RetryPolicy {
        // Branches mirror `resolve_chain` exactly to keep
        // policy-resolution and chain-resolution in lockstep:
        //
        //   1. Known alias: use its [aliases.<name>.retry] override if
        //      set, else fall through to top-level [retry]. The
        //      default_model retry is NOT consulted -- a known alias
        //      whose retry is None means "no override, use the
        //      global", not "borrow from default_model". This matches
        //      `resolve_chain`'s shape: a known alias never falls
        //      through to default_model.
        //
        //   2. `provider:model` literal: no per-alias retry to
        //      inherit; use top-level [retry].
        //
        //   3. Unknown model with default_model configured: inherit
        //      the default's per-alias retry override if it has one,
        //      else top-level [retry]. This is the path
        //      `resolve_chain` takes when defaulting.
        //
        // Invariant: callers only invoke `policy_for` AFTER
        // `resolve_chain` has succeeded, so the "unknown model with
        // a misconfigured default_model" path is unreachable in
        // practice -- `resolve_chain` would have errored. The
        // `debug_assert!` pins this for debug builds; in release we
        // fall through to the global retry rather than panicking.
        debug_assert!(
            self.resolve_chain(model).is_ok(),
            "policy_for invoked for `{model}` whose chain doesn't resolve; call resolve_chain first",
        );
        if let Some(alias) = self.config.aliases.get(model) {
            return alias
                .retry
                .clone()
                .unwrap_or_else(|| self.config.retry.clone());
        }
        if model.contains(':') {
            return self.config.retry.clone();
        }
        if let Some(default) = self.config.default_model.as_deref() {
            if let Some(retry) = self
                .config
                .aliases
                .get(default)
                .and_then(|a| a.retry.clone())
            {
                return retry;
            }
        }
        self.config.retry.clone()
    }

    /// Overlay the target provider's timeout config onto the alias-
    /// resolved `RetryPolicy`. Alias-level fields in `base` always
    /// win; provider-level fills in only when the alias left the
    /// field None. Both None falls through to reqwest's default.
    fn compose_attempt_policy(&self, base: &RetryPolicy, provider_name: &str) -> RetryPolicy {
        let provider_runtime = self
            .config
            .providers
            .get(provider_name)
            .map(|e| e.runtime());
        let mut out = base.clone();
        if out.request_timeout_ms.is_none() {
            out.request_timeout_ms = provider_runtime.and_then(|p| p.request_timeout_ms);
        }
        if out.stream_first_byte_timeout_ms.is_none() {
            out.stream_first_byte_timeout_ms =
                provider_runtime.and_then(|p| p.stream_first_byte_timeout_ms);
        }
        out
    }
}

/// Execute one upstream `complete()` call with the policy's per-request
/// timeout (if configured). Timeout expiry surfaces as a status-0
/// upstream error, treated as a network class for retry/fallback. The
/// error reports `provider_name` (the config-key the chain walks) so
/// it lines up with `routectl_provider` in responses and with
/// gate-error sources -- never the kind-prefixed `provider.id()`.
async fn run_with_timeout(
    provider_name: &str,
    provider: &dyn Provider,
    req: &ChatRequest,
    policy: &RetryPolicy,
) -> Result<ChatResponse> {
    match policy.request_timeout_ms {
        Some(ms) => {
            match tokio::time::timeout(Duration::from_millis(ms), provider.complete(req.clone()))
                .await
            {
                Ok(r) => r,
                Err(_) => Err(Error::upstream(
                    provider_name,
                    0,
                    format!("request timed out after {ms}ms"),
                )),
            }
        }
        None => provider.complete(req.clone()).await,
    }
}

/// Wrap an upstream stream so the breaker records success on clean
/// completion (None / EOS) and records ONE failure on the first error
/// that bubbles out of the stream. Subsequent errors do not double-count.
///
/// Consumer cancellation after the first upstream chunk is treated as a
/// success for steady-state traffic, but still counts as a failure for a
/// half-open probe because the breaker has not observed a full recovery yet.
fn wrap_with_breaker_accounting(
    inner: BoxStream<'static, Result<ChatChunk>>,
    state: Option<Arc<Mutex<crate::runtime_state::ProviderState>>>,
    cancel_is_failure: bool,
) -> BoxStream<'static, Result<ChatChunk>> {
    use futures::stream::StreamExt as _;
    struct BreakerAccounting {
        state: Option<Arc<Mutex<crate::runtime_state::ProviderState>>>,
        cancel_is_failure: bool,
        settled: bool,
    }

    impl BreakerAccounting {
        fn new(
            state: Option<Arc<Mutex<crate::runtime_state::ProviderState>>>,
            cancel_is_failure: bool,
        ) -> Self {
            Self {
                state,
                cancel_is_failure,
                settled: false,
            }
        }

        fn with_state(&self, f: impl FnOnce(&mut crate::runtime_state::ProviderState)) {
            let Some(st) = &self.state else {
                return;
            };
            f(&mut st.lock());
        }

        fn record_success(&mut self) {
            if self.settled {
                return;
            }
            self.settled = true;
            self.with_state(|state| state.record_success());
        }

        fn record_failure(&mut self) {
            if self.settled {
                return;
            }
            self.settled = true;
            self.with_state(|state| state.record_failure(Instant::now()));
        }
    }

    impl Drop for BreakerAccounting {
        fn drop(&mut self) {
            if !self.settled {
                if self.cancel_is_failure {
                    self.record_failure();
                } else {
                    self.record_success();
                }
            }
        }
    }

    let mut accounting = BreakerAccounting::new(state, cancel_is_failure);
    let s = async_stream::stream! {
        let mut inner = inner;
        while let Some(item) = inner.next().await {
            if item.is_err() {
                accounting.record_failure();
            }
            yield item;
        }
        accounting.record_success();
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
    provider_name: &str,
    provider: Arc<dyn Provider>,
    req: ChatRequest,
    policy: &RetryPolicy,
) -> Result<BoxStream<'static, Result<ChatChunk>>> {
    let open_and_first = async {
        let mut upstream = provider.stream(req).await?;
        match upstream.next().await {
            Some(Ok(first)) => {
                let merged = futures::stream::once(async move { Ok(first) }).chain(upstream);
                Ok(merged.boxed())
            }
            Some(Err(e)) => Err(e),
            // Upstream returned an empty stream (stream() returned Ok
            // but no chunk ever arrived). This is NOT a successful
            // empty completion -- a healthy provider always emits at
            // least one chunk (even just a usage tail). Treat as a
            // fallbackable streaming error so the chain walks to the
            // next provider AND the breaker records a failed probe.
            // Without this, an upstream that closes the connection
            // before producing any data would be reported as a
            // successful completion to both the client and the
            // router's health accounting.
            None => Err(Error::Streaming(format!(
                "{provider_name} stream closed before any chunk arrived",
            ))),
        }
    };

    match policy.stream_first_byte_timeout_ms {
        Some(ms) => match tokio::time::timeout(Duration::from_millis(ms), open_and_first).await {
            Ok(r) => r,
            Err(_) => Err(Error::upstream(
                provider_name,
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
        // Streaming errors are transport-level (broken connection
        // mid-stream, partial frame, decode failure on the wire) --
        // semantically network-class, not 5xx-class. Bucket them under
        // `retry_on_network` so configuration matches the error class.
        Error::Streaming(_) => policy.retry_on_network.unwrap_or(policy.max_attempts),
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
    base.saturating_add(Duration::from_millis(nanos % jitter_ms))
}

fn mul_duration(d: Duration, factor: f64) -> Duration {
    let nanos = d.as_nanos() as f64 * factor;
    Duration::from_nanos(nanos.min(u64::MAX as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AliasEntry, ProviderEntry, RetryPolicy};
    use std::collections::BTreeMap;

    /// Build a Router with one openai-compat provider that has the given
    /// runtime-policy timeouts, and an alias chain of length 1 pointing
    /// at it. The base RetryPolicy passed to compose_attempt_policy
    /// represents what `policy_for(alias)` resolved to.
    fn build_router_with_provider_timeouts(
        provider_request_timeout: Option<u64>,
        provider_first_byte_timeout: Option<u64>,
    ) -> Router {
        let mut providers = BTreeMap::new();
        let mut entry = ProviderEntry::openai_compat("https://example.test/v1", "literal:k");
        if let ProviderEntry::OpenaiCompat { runtime, .. } = &mut entry {
            runtime.request_timeout_ms = provider_request_timeout;
            runtime.stream_first_byte_timeout_ms = provider_first_byte_timeout;
        }
        providers.insert("p1".to_string(), entry);

        let mut aliases = BTreeMap::new();
        aliases.insert("a".to_string(), AliasEntry::new(vec!["p1:m".to_string()]));

        let cfg = Config {
            server: Default::default(),
            providers,
            aliases,
            default_model: None,
            retry: RetryPolicy::default(),
            legacy_compat: Default::default(),
            ingress: Default::default(),
        };
        Router::new(Arc::new(cfg))
    }

    #[test]
    fn compose_inherits_timeout_from_provider_when_alias_left_none() {
        // Alias-resolved policy has no timeout overrides.
        // Provider config sets both timeouts.
        // Expected: provider's values land in the per-attempt policy.
        let router = build_router_with_provider_timeouts(Some(180_000), Some(60_000));
        let base = RetryPolicy::default(); // both timeout fields None
        let composed = router.compose_attempt_policy(&base, "p1");
        assert_eq!(composed.request_timeout_ms, Some(180_000));
        assert_eq!(composed.stream_first_byte_timeout_ms, Some(60_000));
    }

    #[test]
    fn compose_alias_override_wins_over_provider() {
        // Alias-resolved policy has BOTH timeouts set explicitly.
        // Provider config also sets values. Alias wins.
        let router = build_router_with_provider_timeouts(Some(180_000), Some(60_000));
        let base = RetryPolicy {
            request_timeout_ms: Some(30_000),
            stream_first_byte_timeout_ms: Some(5_000),
            ..RetryPolicy::default()
        };
        let composed = router.compose_attempt_policy(&base, "p1");
        assert_eq!(composed.request_timeout_ms, Some(30_000));
        assert_eq!(composed.stream_first_byte_timeout_ms, Some(5_000));
    }

    #[test]
    fn compose_independent_per_field_resolution() {
        // Alias sets ONLY request_timeout_ms; provider sets ONLY
        // stream_first_byte_timeout_ms. Expected: each field falls
        // through independently.
        let router = build_router_with_provider_timeouts(None, Some(120_000));
        let base = RetryPolicy {
            request_timeout_ms: Some(45_000),
            ..RetryPolicy::default()
        };
        let composed = router.compose_attempt_policy(&base, "p1");
        assert_eq!(composed.request_timeout_ms, Some(45_000));
        assert_eq!(composed.stream_first_byte_timeout_ms, Some(120_000));
    }

    #[test]
    fn compose_no_provider_entry_passes_base_through_unchanged() {
        // If the chain entry's provider isn't in config (e.g. test
        // harness that registered a Provider without adding a config
        // ProviderEntry), provider-level lookup returns None and the
        // base policy survives unchanged.
        let router = build_router_with_provider_timeouts(Some(99_999), Some(99_999));
        let base = RetryPolicy {
            request_timeout_ms: Some(7_000),
            ..RetryPolicy::default()
        };
        let composed = router.compose_attempt_policy(&base, "missing-provider");
        assert_eq!(composed.request_timeout_ms, Some(7_000));
        assert!(composed.stream_first_byte_timeout_ms.is_none());
    }

    #[test]
    fn compose_no_overrides_anywhere_yields_none() {
        // Belt-and-braces: alias = None, provider = None, default
        // policy = None. composed.request_timeout_ms stays None
        // (router falls through to reqwest's default).
        let router = build_router_with_provider_timeouts(None, None);
        let base = RetryPolicy::default();
        let composed = router.compose_attempt_policy(&base, "p1");
        assert!(composed.request_timeout_ms.is_none());
        assert!(composed.stream_first_byte_timeout_ms.is_none());
    }
}
