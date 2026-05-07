//! Fallback-chain router. Given an incoming request, walks the configured
//! alias chain attempting each provider until one succeeds or all are
//! exhausted. Retries within a single provider per `RetryPolicy.max_attempts`
//! with exponential backoff.

use std::sync::Arc;
use std::time::Duration;

use futures::stream::{BoxStream, StreamExt};
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result};

use crate::config::{Config, RetryPolicy};

pub struct Router {
    pub config: Arc<Config>,
    pub providers: std::collections::BTreeMap<String, Arc<dyn Provider>>,
}

impl Router {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            config,
            providers: Default::default(),
        }
    }

    pub fn register(&mut self, name: impl Into<String>, provider: Arc<dyn Provider>) {
        self.providers.insert(name.into(), provider);
    }

    pub async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        let chain = self.resolve_chain(&req.model)?;
        let policy = self.policy_for(&req.model);
        let mut last_err: Option<Error> = None;

        for target in chain {
            let (provider_name, model) = parse_target(&target);
            let Some(provider) = self.providers.get(provider_name).cloned() else {
                last_err = Some(Error::UnknownProvider(provider_name.to_string()));
                continue;
            };

            let mut attempt_req = req.clone();
            attempt_req.model = model.to_string();

            match attempt_with_retries(provider.as_ref(), attempt_req, &policy).await {
                Ok(mut resp) => {
                    resp.routectl_provider = Some(provider_name.to_string());
                    return Ok(resp);
                }
                Err(e) if should_fallback(&e, &policy) => {
                    tracing::warn!(provider = provider_name, error = ?e, "fallback to next");
                    last_err = Some(e);
                    continue;
                }
                Err(e) => return Err(e),
            }
        }

        Err(last_err.unwrap_or_else(|| Error::UnknownAlias(req.model.clone())))
    }

    /// Streaming counterpart to `complete`. Fallback only happens on errors
    /// that surface BEFORE the first chunk reaches us; once the upstream has
    /// emitted any chunk, mid-stream errors propagate to the caller.
    pub async fn stream(
        &self,
        req: ChatRequest,
    ) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let chain = self.resolve_chain(&req.model)?;
        let policy = self.policy_for(&req.model);
        let mut last_err: Option<Error> = None;

        for target in chain {
            let (provider_name, model) = parse_target(&target);
            let Some(provider) = self.providers.get(provider_name).cloned() else {
                last_err = Some(Error::UnknownProvider(provider_name.to_string()));
                continue;
            };

            let mut attempt_req = req.clone();
            attempt_req.model = model.to_string();

            match try_stream_with_first_chunk(provider, attempt_req, &policy).await {
                Ok(stream) => return Ok(stream),
                Err(e) if should_fallback(&e, &policy) => {
                    tracing::warn!(provider = provider_name, error = ?e, "stream fallback to next");
                    last_err = Some(e);
                    continue;
                }
                Err(e) => return Err(e),
            }
        }

        Err(last_err.unwrap_or_else(|| Error::UnknownAlias(req.model.clone())))
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

async fn attempt_with_retries(
    provider: &dyn Provider,
    req: ChatRequest,
    policy: &RetryPolicy,
) -> Result<ChatResponse> {
    let mut backoff = Duration::from_millis(policy.initial_backoff_ms);
    let mut last_err: Option<Error> = None;

    for attempt in 0..policy.max_attempts.max(1) {
        if attempt > 0 {
            tokio::time::sleep(backoff).await;
            backoff = mul_duration(backoff, policy.backoff_multiplier);
        }
        match provider.complete(req.clone()).await {
            Ok(resp) => return Ok(resp),
            Err(e) if should_retry_same_provider(&e, policy) => {
                tracing::debug!(attempt, error = ?e, "retrying same provider");
                last_err = Some(e);
                continue;
            }
            Err(e) => return Err(e),
        }
    }

    Err(last_err.unwrap_or_else(|| Error::Streaming("no attempts ran".into())))
}

/// Open the upstream stream and pull the first chunk. If that initial step
/// fails with a fallbackable error, return it so the caller can try the next
/// provider. If the first chunk arrives, return a `BoxStream` that yields it
/// followed by the rest of the upstream stream -- mid-stream errors propagate.
async fn try_stream_with_first_chunk(
    provider: Arc<dyn Provider>,
    req: ChatRequest,
    _policy: &RetryPolicy,
) -> Result<BoxStream<'static, Result<ChatChunk>>> {
    let mut upstream = provider.stream(req).await?;
    match upstream.next().await {
        Some(Ok(first)) => {
            let merged = futures::stream::once(async move { Ok(first) }).chain(upstream);
            Ok(merged.boxed())
        }
        Some(Err(e)) => Err(e),
        None => Ok(futures::stream::empty().boxed()),
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

fn should_retry_same_provider(err: &Error, policy: &RetryPolicy) -> bool {
    match err {
        Error::Upstream { status: 0, .. } => true,
        Error::Upstream { status, .. } => {
            *status == 429 || (500..600).contains(status) && policy.fallback_on_status.contains(status)
        }
        Error::Streaming(_) => true,
        _ => false,
    }
}

fn mul_duration(d: Duration, factor: f64) -> Duration {
    let nanos = d.as_nanos() as f64 * factor;
    Duration::from_nanos(nanos.min(u64::MAX as f64) as u64)
}
