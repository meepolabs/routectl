#![deny(rustdoc::broken_intra_doc_links)]
#![warn(missing_docs)]
//! Shared in-process `tracing` capture test double for routectl's
//! dev-dependency test suites -- both `#[cfg(test)]` unit tests and
//! `tests/` integration binaries, which are separate compilation units
//! that only see a crate's public API plus its dev-dependencies. A
//! dev-dependency crate is the only shape that reaches both.
//!
//! Three calling conventions so callers never need to hand-roll a
//! `tracing::Subscriber`:
//!
//! - [`capture_events`] -- synchronous, structured [`CapturedEvent`]s.
//! - [`with_capture`] -- async, structured [`CapturedEvent`]s.
//! - [`capture_lines`] -- async, one formatted line per event (for
//!   assertions that want to grep rendered output rather than inspect
//!   structured fields).
//!
//! All three install the capture subscriber as the THREAD-LOCAL
//! default (`tracing::subscriber::set_default`) for the duration of
//! the captured closure/future, then restore the prior default. None
//! of them capture events emitted on a spawned thread or a spawned
//! tokio task -- only events emitted on the calling thread, inside the
//! captured closure/future, are seen. Drive the async variants on a
//! `current_thread` runtime (the `#[tokio::test]` default) so every
//! await point in the captured future stays on this thread.

use std::future::Future;
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};

pub mod bench_alloc;
pub mod bench_fixtures;

mod scoped_env;
pub use scoped_env::ScopedEnv;

/// One captured `tracing` event: its level, target (module path), the
/// special `message` field, and every other structured field rendered
/// to a string.
#[derive(Debug, Clone)]
pub struct CapturedEvent {
    /// Severity level the event was emitted at.
    pub level: tracing::Level,
    /// Target the event was emitted under, typically the emitting
    /// module path.
    pub target: String,
    /// The event's special `message` field, or empty if it carried none.
    pub message: String,
    /// Every structured field other than `message`, as
    /// `(name, rendered-value)` pairs in emission order.
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
        // `%value` (Display) fields reach the visitor through
        // `record_debug` wrapped in a type whose Debug forwards to
        // Display, so this yields the Display string (e.g. compact
        // JSON) without surrounding quotes. Numeric and bool fields
        // also land here via `Visit`'s default `record_u64` /
        // `record_i64` / `record_bool` methods, whose Debug output
        // equals their Display output, so this one override covers
        // every field shape callers use.
        let s = format!("{value:?}");
        if field.name() == "message" {
            self.message = s.trim_matches('"').to_string();
        } else {
            self.fields.push((field.name().into(), s));
        }
    }
}

#[derive(Default)]
struct CaptureSubscriber {
    captured: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl tracing::Subscriber for CaptureSubscriber {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        // Without an explicit TRACE hint, an emitter's own
        // `event_enabled!(TRACE)` fast-path check can short-circuit
        // before `enabled` is ever called, suppressing TRACE-level
        // lines under test.
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

/// Minimal `tracing::Subscriber` that renders every event's fields
/// (via `Debug`, matching what `%field` / `?field` interpolation
/// produces) into a flat `Vec<String>`, one line per event.
struct LineVisitor<'a>(&'a mut String);

impl Visit for LineVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        let _ = write!(self.0, " {}={value:?}", field.name());
    }
}

#[derive(Default)]
struct LineSubscriber {
    lines: Arc<Mutex<Vec<String>>>,
}

impl tracing::Subscriber for LineSubscriber {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::TRACE)
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = String::new();
        let mut visitor = LineVisitor(&mut line);
        event.record(&mut visitor);
        if let Ok(mut lines) = self.lines.lock() {
            lines.push(line);
        }
    }

    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// Bundles the thread-local default subscriber guard together with a
/// second, independent, live `Dispatch`.
///
/// `tracing-core`'s callsite-interest cache has a fast path that,
/// while exactly one `Dispatch` is alive process-wide, resolves a
/// callsite's first-ever registration through the CALLING THREAD's own
/// ambient dispatch rather than the real registry. A sibling test on
/// another thread with no subscriber installed can then win that race
/// and cache the callsite's `Interest` as "never" process-wide, even
/// though this capture subscriber is live and would otherwise see it.
/// Keeping a second `Dispatch` alive for the whole capture scope forces
/// the registry to have more than one entry, which routes every
/// callsite registration through the real (thread-agnostic) dispatcher
/// list instead of that single-dispatch shortcut. Baked in
/// unconditionally so no caller can forget it.
struct CaptureGuard {
    _keepalive: tracing::Dispatch,
    _default: tracing::subscriber::DefaultGuard,
}

fn install<S>(subscriber: S, keepalive: S) -> CaptureGuard
where
    S: tracing::Subscriber + Send + Sync + 'static,
{
    CaptureGuard {
        _keepalive: tracing::Dispatch::new(keepalive),
        _default: tracing::subscriber::set_default(subscriber),
    }
}

/// Run `f` with the capture subscriber installed as the thread-local
/// default and return every event it emitted. Single-threaded by
/// design: `f` must run synchronously on the calling thread for the
/// thread-local default to apply to it.
pub fn capture_events<F: FnOnce()>(f: F) -> Vec<CapturedEvent> {
    let captured: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let subscriber = CaptureSubscriber {
        captured: Arc::clone(&captured),
    };
    let guard = install(subscriber, CaptureSubscriber::default());
    f();
    // Bind before returning so the `MutexGuard` temporary is released
    // at this statement's `;`, then restore the prior default
    // subscriber.
    let events = captured.lock().expect("capture mutex poisoned").clone();
    drop(guard);
    events
}

/// Drive `fut` under the capture subscriber installed as the
/// thread-local default and return its output alongside every event it
/// emitted. `#[tokio::test]` defaults to a `current_thread` runtime,
/// which is required here: `set_default` installs a thread-local
/// subscriber, so a `multi_thread` runtime that moves `fut` to a
/// worker thread would not see it.
pub async fn with_capture<F: Future>(fut: F) -> (F::Output, Vec<CapturedEvent>) {
    let captured: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let subscriber = CaptureSubscriber {
        captured: Arc::clone(&captured),
    };
    let guard = install(subscriber, CaptureSubscriber::default());
    let out = fut.await;
    let events = captured.lock().expect("capture mutex poisoned").clone();
    drop(guard);
    (out, events)
}

/// Drive `fut` under the capture subscriber installed as the
/// thread-local default and return its output alongside one formatted
/// line per emitted event (each structured field appended as
/// `" name=value"`, Debug-rendered). For assertions that want to grep
/// rendered log lines (e.g. "does this token ever appear in any log
/// line") rather than inspect structured fields -- see [`with_capture`]
/// for the latter. Same single-thread caveat as `with_capture`.
pub async fn capture_lines<F: Future>(fut: F) -> (F::Output, Vec<String>) {
    let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let subscriber = LineSubscriber {
        lines: Arc::clone(&lines),
    };
    let guard = install(subscriber, LineSubscriber::default());
    let out = fut.await;
    let out_lines = lines.lock().expect("capture mutex poisoned").clone();
    drop(guard);
    (out, out_lines)
}
