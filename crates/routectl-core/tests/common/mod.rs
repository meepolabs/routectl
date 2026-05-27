//! Shared in-process tracing capture for the header-trace emit-path
//! integration tests. The `common/mod.rs` layout means Cargo treats this
//! as a shared module each `tests/*.rs` binary pulls in via `mod common;`
//! -- it is NOT compiled as its own test binary.
//!
//! Mirrors the hand-rolled capture subscriber already used in
//! crates/routectl-cli/tests/anthropic_forward_compat_stream.rs:
//! implements `tracing::Subscriber` directly (no `tracing-subscriber`
//! dev-dependency) and installs it as the thread-local default for the
//! duration of a closure. `enabled` returns true and `max_level_hint`
//! reports TRACE so the TRACE-gated header emitters actually fire.

// Each test binary exercises a different subset of these helpers, so an
// item unused in one binary would otherwise trip the dead_code lint.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};

/// One captured `tracing` event: its level, target (module path), the
/// special `message` field, and every other structured field rendered to
/// a string.
#[derive(Debug, Clone)]
pub struct CapturedEvent {
    pub level: tracing::Level,
    pub target: String,
    pub message: String,
    pub fields: Vec<(String, String)>,
}

impl CapturedEvent {
    /// Value of the named structured field, if the event carried it.
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Default)]
struct FieldCollector {
    message: String,
    fields: Vec<(String, String)>,
}

impl Visit for FieldCollector {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.push((field.name().into(), value.into()));
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // `%value` (Display) fields reach the visitor through `record_debug`
        // wrapped in a type whose Debug forwards to Display, so this yields
        // the Display string (the compact JSON) without surrounding quotes.
        let s = format!("{value:?}");
        if field.name() == "message" {
            self.message = s.trim_matches('"').to_string();
        } else {
            self.fields.push((field.name().into(), s));
        }
    }
}

struct CaptureSubscriber {
    captured: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl tracing::Subscriber for CaptureSubscriber {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        // Without an explicit TRACE hint the emitters' own
        // `event_enabled!(TRACE)` fast-path check can short-circuit
        // before reaching `enabled`, suppressing the very lines under test.
        Some(tracing::level_filters::LevelFilter::TRACE)
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let meta = event.metadata();
        let mut visitor = FieldCollector::default();
        event.record(&mut visitor);
        let captured = CapturedEvent {
            level: *meta.level(),
            target: meta.target().to_string(),
            message: visitor.message,
            fields: visitor.fields,
        };
        if let Ok(mut guard) = self.captured.lock() {
            guard.push(captured);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// Run `f` with the capture subscriber installed as the thread-local
/// default and return every event it emitted. Single-threaded by design:
/// the emitters run synchronously inside `f` on this thread, so the
/// thread-local default applies to them.
pub fn capture_events<F: FnOnce()>(f: F) -> Vec<CapturedEvent> {
    let captured: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let subscriber = CaptureSubscriber {
        captured: Arc::clone(&captured),
    };
    let guard = tracing::subscriber::set_default(subscriber);
    f();
    // Bind before returning so the `MutexGuard` temporary is released at
    // this statement's `;`, then restore the prior default subscriber.
    let events = captured.lock().expect("capture mutex poisoned").clone();
    drop(guard);
    events
}
