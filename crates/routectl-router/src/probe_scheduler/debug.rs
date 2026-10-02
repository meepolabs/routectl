//! The scheduler's `Debug`, kept apart from the job table it refuses to print.

use super::ProbeScheduler;

/// Hand-written and field-less: the job table holds every lane's identity and
/// captured beta context, and a formatter may run while the lock is held, so
/// this never takes it.
impl std::fmt::Debug for ProbeScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProbeScheduler").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::super::{ProbeActivation, ProbePayload, ProbeScheduler, ProbeValidator};
    use crate::field_verdict::FieldVerdictKey;

    const PAYLOAD_SENTINELS: [&str; 3] =
        ["client-beta-sentinel", "operator-beta-sentinel", "omitted"];

    fn sentinel_payload() -> ProbePayload {
        ProbePayload::new(
            "thinking.enabled.display",
            "omitted".to_string(),
            &["client-beta-sentinel".to_string()],
            &["operator-beta-sentinel".to_string()],
            true,
        )
        .expect("sentinel tokens are within the retention bound")
    }

    #[test]
    fn scheduler_debug_renders_no_job_table_contents() {
        // Arrange: a queued job carrying sentinel identity and beta context.
        let scheduler = ProbeScheduler::new();
        let key = FieldVerdictKey::new(
            "state-sentinel#seat-sentinel",
            "pathsentinel.leaf",
            "provider-sentinel",
        )
        .expect("a well-formed dotted path mints an identity");
        let activation = scheduler.activate(
            &key,
            7,
            vec![ProbeValidator::CountTokens],
            sentinel_payload(),
        );
        assert_eq!(
            activation,
            ProbeActivation::Queued,
            "fixture premise: the sentinel job must be in the table"
        );

        // Act
        let renderings = [format!("{scheduler:?}"), format!("{scheduler:#?}")];

        // Assert
        for rendered in renderings {
            for sentinel in PAYLOAD_SENTINELS.iter().chain(&[
                "state-sentinel",
                "seat-sentinel",
                "pathsentinel",
                "provider-sentinel",
                "thinking.enabled.display",
                "jobs",
                "tombstones",
                "inner",
            ]) {
                assert!(
                    !rendered.contains(sentinel),
                    "scheduler debug leaked {sentinel:?}: {rendered}"
                );
            }
            assert!(
                rendered.contains("ProbeScheduler") && rendered.contains(".."),
                "scheduler debug is missing its name: {rendered}"
            );
        }
    }

    #[test]
    fn scheduler_debug_does_not_take_the_scheduler_lock() {
        // Arrange: hold the lock, then format from another thread. A Debug that
        // locked would block until the guard dropped, past the deadline.
        let scheduler = std::sync::Arc::new(ProbeScheduler::new());
        let guard = scheduler.inner.lock();
        let (tx, rx) = mpsc::channel();
        let formatter = std::sync::Arc::clone(&scheduler);

        // Act
        let handle = thread::spawn(move || {
            let _ = tx.send(format!("{formatter:?}"));
        });
        let rendered = rx.recv_timeout(Duration::from_secs(5));
        drop(guard);
        handle.join().expect("formatter thread completes");

        // Assert
        assert_eq!(
            rendered.as_deref(),
            Ok("ProbeScheduler { .. }"),
            "scheduler debug must render without the lock"
        );
    }
}
