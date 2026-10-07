use std::sync::atomic::{AtomicBool, Ordering};

use futures::StreamExt;
use tokio::sync::Semaphore;

use super::*;

struct DropFlag(Arc<AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test(start_paused = true)]
async fn a_never_polled_body_drops_upstream_and_releases_its_permit_at_the_deadline() {
    let metrics = Arc::new(ProxyMetrics::new());
    let semaphore = Arc::new(Semaphore::new(1));
    let guard = StreamGuard::new(
        Arc::clone(&metrics),
        Arc::clone(&semaphore).acquire_owned().await.unwrap(),
    );
    let dropped = Arc::new(AtomicBool::new(false));
    let inner = futures::stream::unfold(DropFlag(Arc::clone(&dropped)), |flag| async move {
        futures::future::pending::<()>().await;
        Some((Ok(Bytes::new()), flag))
    });
    let mut body = watchdog_stream(
        Box::pin(inner),
        Duration::from_millis(50),
        Arc::clone(&metrics),
        guard,
    );
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(60)).await;
    tokio::task::yield_now().await;
    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(semaphore.available_permits(), 1);
    assert_eq!(metrics.streams_open(), 0);
    assert_eq!(metrics.stream_idle_aborts_total(), 1);
    assert!(matches!(
        body.next().await,
        Some(Err(ForwardBodyError::IdleTimeout))
    ));
    assert!(body.next().await.is_none());
    assert_eq!(metrics.stream_idle_aborts_total(), 1);
}

#[tokio::test(start_paused = true)]
async fn progress_resets_the_timer_but_stopped_downstream_polling_does_not() {
    let metrics = Arc::new(ProxyMetrics::new());
    let semaphore = Arc::new(Semaphore::new(1));
    let guard = StreamGuard::new(
        Arc::clone(&metrics),
        Arc::clone(&semaphore).acquire_owned().await.unwrap(),
    );
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let inner = tokio_stream::wrappers::UnboundedReceiverStream::new(rx);
    let mut body = watchdog_stream(
        inner,
        Duration::from_millis(50),
        Arc::clone(&metrics),
        guard,
    );
    tokio::task::yield_now().await;
    for _ in 0..4 {
        tokio::time::advance(Duration::from_millis(30)).await;
        tx.send(Ok(Bytes::from_static(b"progress"))).unwrap();
        assert!(body.next().await.unwrap().is_ok());
        tokio::task::yield_now().await;
        assert_eq!(metrics.stream_idle_aborts_total(), 0);
        assert_eq!(semaphore.available_permits(), 0);
    }
    // Buffered upstream bytes don't justify retaining an unconsumed body forever.
    tx.send(Ok(Bytes::from_static(b"unconsumed"))).unwrap();
    tokio::time::advance(Duration::from_millis(60)).await;
    tokio::task::yield_now().await;
    assert_eq!(metrics.stream_idle_aborts_total(), 1);
    assert_eq!(semaphore.available_permits(), 1);
    assert!(matches!(
        body.next().await,
        Some(Err(ForwardBodyError::IdleTimeout))
    ));
    assert!(body.next().await.is_none());
    assert!(tx.is_closed());
}

#[tokio::test(start_paused = true)]
async fn dropping_the_body_cancels_the_timer_without_an_idle_abort() {
    let metrics = Arc::new(ProxyMetrics::new());
    let semaphore = Arc::new(Semaphore::new(1));
    let guard = StreamGuard::new(
        Arc::clone(&metrics),
        Arc::clone(&semaphore).acquire_owned().await.unwrap(),
    );
    let body = watchdog_stream(
        futures::stream::pending(),
        Duration::from_millis(50),
        Arc::clone(&metrics),
        guard,
    );
    tokio::task::yield_now().await;
    drop(body);
    assert_eq!(semaphore.available_permits(), 1);
    assert_eq!(metrics.streams_open(), 0);
    tokio::time::advance(Duration::from_millis(60)).await;
    tokio::task::yield_now().await;
    assert_eq!(metrics.stream_idle_aborts_total(), 0);
}

#[tokio::test(start_paused = true)]
async fn timeout_wakes_a_pending_body_poll() {
    let metrics = Arc::new(ProxyMetrics::new());
    let semaphore = Arc::new(Semaphore::new(1));
    let guard = StreamGuard::new(
        Arc::clone(&metrics),
        Arc::clone(&semaphore).acquire_owned().await.unwrap(),
    );
    let mut body = watchdog_stream(
        futures::stream::pending(),
        Duration::from_millis(50),
        Arc::clone(&metrics),
        guard,
    );
    assert!(matches!(
        body.next().await,
        Some(Err(ForwardBodyError::IdleTimeout))
    ));
    assert_eq!(semaphore.available_permits(), 1);
    assert_eq!(metrics.stream_idle_aborts_total(), 1);
    assert!(body.next().await.is_none());
}
