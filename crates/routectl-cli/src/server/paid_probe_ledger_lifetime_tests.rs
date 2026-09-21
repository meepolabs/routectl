// Writer-lifetime cases: an adapter retained by the live Router must not turn
// the daemon's shutdown into a wait, and no reservation may be authorized once
// the accounting subsystem has begun going away.

/// How long a drain that has nothing left to wait for may take.
///
/// Well under the writer's own abandon deadline, so a drain still held open by
/// a retained producer clone reads as "not finished" rather than as scheduler
/// noise. Every use of it follows a start signal from inside the drain's own
/// blocking closure (see `test_support::begin_writer_drain`), so it never
/// stands in for evidence that the drain began -- a bound alone cannot tell a
/// blocked drain from one the blocking pool has not picked up.
const PROMPT_SHUTDOWN: Duration = Duration::from_millis(500);

/// A retained adapter DOES hold the writer's channel open -- and releasing it
/// is what lets the writer finish.
///
/// The positive control for the serve-level ordering test below: without this,
/// a serve loop that released nothing would still pass a no-hang assertion on a
/// machine fast enough, and nothing would say the release mattered.
///
/// ONE drain, observed twice. The bounded wait borrows the drain's handle
/// rather than consuming it, so the same in-flight drain that had to time out
/// while the adapter lived is the one that completes after it is released --
/// which is what makes this a statement about the adapter rather than about two
/// unrelated attempts.
#[tokio::test]
async fn a_retained_adapter_holds_the_writer_channel_open_until_it_is_dropped() {
    // Arrange: a live writer, its own handle released, and ONLY the adapter's
    // clone left holding the producer side.
    let (_dir, _path, handle, writer) = live_writer();
    let ledger = paid_probe_ledger(&handle);
    drop(handle);

    // Act: the drain, dispatched as the serve loop dispatches it, and PROVABLY
    // under way before anything is concluded from it.
    let mut drain = begin_writer_drain(writer).await;

    // Assert: it cannot finish while the adapter holds a producer clone.
    assert!(
        !drain.completes_within(PROMPT_SHUTDOWN).await,
        "the premise: a retained adapter keeps the writer's channel open",
    );

    // Act: release the adapter.
    let released_at = Instant::now();
    drop(ledger);

    // Assert: the SAME drain finishes promptly once nothing holds its channel.
    drain.finish().await;
    assert!(
        released_at.elapsed() < Duration::from_secs(4),
        "the writer must finish on the release, not on its abandon deadline",
    );
}

/// Dropping the accounting subsystem while a reservation is in flight resolves
/// that reservation as a refusal rather than leaving the caller awaiting
/// forever.
///
/// The fail-closed direction at the one moment it is hardest to get right: the
/// receipt's sender goes away with the writer, and what the Router must see is a
/// refusal, not a hang and not an authorization.
#[tokio::test]
async fn a_reservation_outstanding_at_teardown_resolves_as_a_refusal() {
    // Arrange
    let (_dir, _path, handle, writer) = live_writer();
    let ledger = paid_probe_ledger(&handle);
    assert_eq!(
        reserve(&ledger, "anthropic", 3).await,
        PaidProbeReservation::Committed { used: 1, cap: 3 },
        "the control: this adapter commits while the subsystem is running",
    );

    // Act: tear the writer down, then ask.
    drop(handle);
    begin_writer_drain(writer).await.finish().await;
    let outcome = tokio::time::timeout(Duration::from_secs(5), reserve(&ledger, "anthropic", 3))
        .await
        .expect("a reservation after teardown must resolve, never hang");

    // Assert
    assert_eq!(outcome, PaidProbeReservation::Unavailable);
}
