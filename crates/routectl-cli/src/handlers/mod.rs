pub mod capability_ack_drain;
pub mod chat_completions;
pub mod control;
pub mod health;
pub mod ingress_handle;
mod ingress_wire;
pub mod messages;
pub mod messages_count_tokens;
pub mod models;
pub mod opening_diagnostics;
pub(crate) mod opening_meter;
pub mod pure_proxy_admission;
pub mod pure_proxy_metrics;
mod response_wire;
pub mod responses;
pub mod status;
pub mod usage_capture;

#[cfg(test)]
#[path = "zero_detection_tests.rs"]
mod zero_detection_tests;
