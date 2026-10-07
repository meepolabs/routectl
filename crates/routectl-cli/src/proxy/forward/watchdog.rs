//! Forward-body cancellation independent of downstream polling.

use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use bytes::Bytes;
use futures::Stream;
use hyper::body::Frame;
use tokio::sync::Notify;
use tokio::time::Instant;

use super::{ForwardBodyError, ProxyMetrics, StreamGuard};

struct State<S> {
    inner: Option<S>,
    guard: Option<StreamGuard>,
    deadline: Instant,
    expired: bool,
    waker: Option<Waker>,
}

struct WatchdogStream<S> {
    state: Arc<Mutex<State<S>>>,
    changed: Arc<Notify>,
    idle_window: Duration,
    timer: tokio::task::JoinHandle<()>,
}

impl<S> Drop for WatchdogStream<S> {
    fn drop(&mut self) {
        self.timer.abort();
        let cleanup = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            (state.inner.take(), state.guard.take())
        };
        drop(cleanup);
    }
}

impl<S> Stream for WatchdogStream<S>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    type Item = Result<Frame<Bytes>, ForwardBodyError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.expired {
            state.expired = false;
            return Poll::Ready(Some(Err(ForwardBodyError::IdleTimeout)));
        }
        let Some(inner) = state.inner.as_mut() else {
            return Poll::Ready(None);
        };
        match Pin::new(inner).poll_next(cx) {
            Poll::Pending => {
                state.waker = Some(cx.waker().clone());
                Poll::Pending
            }
            Poll::Ready(Some(Ok(bytes))) => {
                state.deadline = Instant::now() + self.idle_window;
                state.waker = None;
                drop(state);
                self.changed.notify_one();
                Poll::Ready(Some(Ok(Frame::data(bytes))))
            }
            Poll::Ready(terminal) => {
                let cleanup = (state.inner.take(), state.guard.take());
                state.waker = None;
                drop(state);
                drop(cleanup);
                self.changed.notify_one();
                Poll::Ready(
                    terminal
                        .map(|result| result.map(Frame::data).map_err(ForwardBodyError::Upstream)),
                )
            }
        }
    }
}

/// The timer owns only a weak state reference while sleeping. The body never
/// buffers ahead, and dropping it synchronously drops the upstream and permit.
/// Idle expiry does the same even when downstream backpressure prevents polls.
pub(super) fn watchdog_stream<S>(
    inner: S,
    idle_window: Duration,
    metrics: Arc<ProxyMetrics>,
    guard: StreamGuard,
) -> impl Stream<Item = Result<Frame<Bytes>, ForwardBodyError>>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin + Send + 'static,
{
    let state = Arc::new(Mutex::new(State {
        inner: Some(inner),
        guard: Some(guard),
        deadline: Instant::now() + idle_window,
        expired: false,
        waker: None,
    }));
    let changed = Arc::new(Notify::new());
    let timer = tokio::spawn(watch_idle(
        Arc::downgrade(&state),
        Arc::clone(&changed),
        metrics,
        idle_window,
    ));
    WatchdogStream {
        state,
        changed,
        idle_window,
        timer,
    }
}

async fn watch_idle<S>(
    weak: Weak<Mutex<State<S>>>,
    changed: Arc<Notify>,
    metrics: Arc<ProxyMetrics>,
    idle_window: Duration,
) where
    S: Send + 'static,
{
    loop {
        let deadline = {
            let Some(shared) = weak.upgrade() else {
                return;
            };
            let state = shared.lock().unwrap_or_else(|p| p.into_inner());
            if state.guard.is_none() {
                return;
            }
            state.deadline
        };
        tokio::select! {
            () = changed.notified() => continue,
            () = tokio::time::sleep_until(deadline) => {}
        }
        let Some(shared) = weak.upgrade() else {
            return;
        };
        let expired = {
            let mut state = shared.lock().unwrap_or_else(|p| p.into_inner());
            if state.guard.is_none() {
                return;
            }
            // A progress poll can race the old timer's wakeup.
            if Instant::now() < state.deadline {
                continue;
            }
            state.expired = true;
            (state.inner.take(), state.guard.take(), state.waker.take())
        };
        drop(expired.0);
        drop(expired.1);
        metrics.incr_stream_idle_aborts();
        tracing::warn!(
            target: "routectl_cli::proxy::forward", idle_window_secs = idle_window.as_secs(),
            "forwarded stream idle watchdog aborted stream"
        );
        if let Some(waker) = expired.2 {
            waker.wake();
        }
        return;
    }
}

#[cfg(test)]
#[path = "watchdog_tests.rs"]
mod tests;
