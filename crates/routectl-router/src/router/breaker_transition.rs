//! The INFO lines for circuit-breaker state transitions: a debit that opens
//! the circuit, an upstream reset hint that parks it, and a success that
//! closes an open or half-open circuit. One line per transition, carrying
//! the event token and the sanitized state key only; the failures that do
//! not change the circuit's state stay in the DEBUG retry decision.

use std::time::Duration;

use routectl_core::sanitize_for_log;

pub(super) const BREAKER_OPENED: &str = "breaker_opened";
pub(super) const BREAKER_PARKED: &str = "breaker_parked";
pub(super) const BREAKER_CLOSED: &str = "breaker_closed";

pub(super) fn log_opened(state_key: &str) {
    tracing::info!(
        event = BREAKER_OPENED,
        state_key = %sanitize_for_log(state_key),
        "circuit breaker opened",
    );
}

pub(super) fn log_parked(state_key: &str, cooldown: Duration) {
    tracing::info!(
        event = BREAKER_PARKED,
        state_key = %sanitize_for_log(state_key),
        cooldown_ms = u64::try_from(cooldown.as_millis()).unwrap_or(u64::MAX),
        "circuit breaker parked for an upstream reset hint",
    );
}

pub(super) fn log_closed(state_key: &str) {
    tracing::info!(
        event = BREAKER_CLOSED,
        state_key = %sanitize_for_log(state_key),
        "circuit breaker closed",
    );
}

#[cfg(test)]
#[path = "breaker_transition_tests.rs"]
mod tests;
