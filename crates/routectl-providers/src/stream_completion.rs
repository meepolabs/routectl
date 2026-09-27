//! Stream-completion tracking for egress lanes whose wire closes every turn
//! with an explicit terminal event.
//!
//! A streamed response that crossed HTTP 200 can still end early: the
//! connection drops, a gateway truncates the body, or the upstream closes
//! without its closing event. The SSE transport reports that as a clean end
//! of stream, so without this check a cut turn reaches the client as a
//! finished one. Each lane injects its own terminal predicate over its own
//! decoded event type; the tracker and the end-of-stream verdict are shared.
//!
//! Only lanes with an unambiguous terminal event belong here. A wire whose
//! end-of-stream IS the terminator would be flagged on every healthy stream.

use routectl_core::{Error, Result};

/// Per-stream record of whether the lane's terminal event has arrived.
pub struct StreamCompletion<E: ?Sized> {
    lane: &'static str,
    is_terminal: fn(&E) -> bool,
    terminated: bool,
}

impl<E: ?Sized> StreamCompletion<E> {
    /// Track one stream for `lane`, treating an event as terminal when
    /// `is_terminal` accepts it.
    pub const fn new(lane: &'static str, is_terminal: fn(&E) -> bool) -> Self {
        Self {
            lane,
            is_terminal,
            terminated: false,
        }
    }

    /// Record one decoded wire event. Sticky: once a terminal event is seen
    /// the stream stays terminated.
    pub fn observe(&mut self, event: &E) {
        if (self.is_terminal)(event) {
            self.terminated = true;
        }
    }

    /// Verdict at transport end of stream: `Ok` once the terminal event
    /// arrived, otherwise the error that tells the client the turn was cut.
    /// Status 0 classifies it with the other transport failures.
    pub fn end_of_stream(&self, provider_id: &str) -> Result<()> {
        if self.terminated {
            Ok(())
        } else {
            Err(Error::upstream(
                provider_id,
                0,
                format!("{}: stream ended without a terminal event", self.lane),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_stop(event_type: &str) -> bool {
        event_type == "stop"
    }

    #[test]
    fn end_of_stream_without_terminal_is_an_upstream_error_naming_the_lane() {
        let completion = StreamCompletion::new("lane-x", is_stop);

        let err = completion
            .end_of_stream("prov")
            .expect_err("no terminal seen");

        match err {
            Error::Upstream {
                provider,
                status,
                body,
                ..
            } => {
                assert_eq!(provider, "prov");
                assert_eq!(status, 0);
                assert_eq!(body, "lane-x: stream ended without a terminal event");
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[test]
    fn non_terminal_events_leave_the_stream_unterminated() {
        let mut completion = StreamCompletion::new("lane-x", is_stop);

        for event_type in ["start", "delta", "", "STOP", "stop "] {
            completion.observe(event_type);
        }

        assert!(completion.end_of_stream("prov").is_err());
    }

    #[test]
    fn terminal_event_completes_the_stream_and_stays_sticky() {
        let mut completion = StreamCompletion::new("lane-x", is_stop);

        completion.observe("delta");
        completion.observe("stop");
        completion.observe("delta");

        assert!(completion.end_of_stream("prov").is_ok());
    }
}
