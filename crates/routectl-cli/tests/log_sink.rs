//! Record integrity of the production tracing sink.
//!
//! Every case drives `routectl_cli::log_sink::subscriber` -- the builder
//! `main` installs -- into an in-memory writer and asserts on the exact
//! bytes: one physical line per record, no raw character from the Unicode
//! oracle in `fixtures/ucd_escape_extract.txt` anywhere in a line, the
//! visible escape present, and clean printable output identical to a stock
//! `DefaultFields` subscriber.
//!
//! Its own integration binary so every callsite here is first registered
//! under a subscriber that enables it; the lib test binary's sibling tests
//! would otherwise poison tracing's per-callsite `Interest` cache.

use std::error::Error;
use std::fmt;
use std::io;
use std::sync::{Arc, LazyLock, Mutex};

use routectl_cli::log_sink::{self, Clock};
use tracing::field::{Empty, display};
use tracing::{Level, info as log_info, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::FmtSpan;

/// A payload carrying one of every class the sink must neutralize.
const HOSTILE: &str =
    "a\nb\rc\td\u{1b}[2Je\u{7}f\u{7f}g\u{9b}h\u{200b}i\u{200f}j\u{202e}k\u{2066}l\u{2069}m";

/// `HOSTILE` as it must appear on the line.
const HOSTILE_ESCAPED: &str =
    r"a\nb\rc\td\x1b[2Je\x07f\x7fg\u{9b}h\u{200b}i\u{200f}j\u{202e}k\u{2066}l\u{2069}m";

#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl io::Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("buffer lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Buffer {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("buffer lock").clone()).expect("sink wrote UTF-8")
    }
}

/// Run `emit` under the production subscriber and return what it wrote.
fn production(emit: impl FnOnce()) -> String {
    let buffer = Buffer::default();
    let sink = buffer.clone();
    let subscriber = log_sink::subscriber(
        EnvFilter::new("trace"),
        move || sink.clone(),
        false,
        Clock::Off,
    );
    tracing::subscriber::with_default(subscriber, emit);
    buffer.text()
}

/// Run `emit` under a stock subscriber configured like production with
/// timing off but with tracing-subscriber's own `DefaultFields`.
fn stock(emit: impl FnOnce()) -> String {
    let buffer = Buffer::default();
    let sink = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("trace"))
        .with_target(true)
        .with_span_events(FmtSpan::CLOSE)
        .with_ansi(false)
        .with_writer(move || sink.clone())
        .without_time()
        .finish();
    tracing::subscriber::with_default(subscriber, emit);
    buffer.text()
}

/// Code point ranges the sink must escape, read from the UCD extract --
/// an oracle independent of the table inside the sink itself.
static ORACLE: LazyLock<Vec<(u32, u32)>> = LazyLock::new(|| {
    let raw = include_str!("fixtures/ucd_escape_extract.txt");
    let mut ranges: Vec<(u32, u32)> = raw
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| {
            let field = line.split(';').next().expect("range column").trim();
            let (lo, hi) = field.split_once("..").unwrap_or((field, field));
            let parse = |hex: &str| u32::from_str_radix(hex, 16).expect("hex code point");
            (parse(lo), parse(hi))
        })
        .collect();
    ranges.push((0xFFF9, 0xFFFB));
    assert!(ranges.len() > 50, "oracle fixture looks truncated");
    ranges
});

fn is_unsafe(c: char) -> bool {
    let c = u32::from(c);
    ORACLE.iter().any(|&(lo, hi)| (lo..=hi).contains(&c))
}

/// The escape the sink must write for `c`.
fn escape_of(c: char) -> String {
    match c {
        '\n' => r"\n".to_owned(),
        '\r' => r"\r".to_owned(),
        '\t' => r"\t".to_owned(),
        c if u32::from(c) <= 0x7f => format!(r"\x{:02x}", u32::from(c)),
        c => format!(r"\u{{{:x}}}", u32::from(c)),
    }
}

/// Split `out` into its records, asserting it holds exactly `expected`
/// newline-terminated lines and that no line carries a raw unsafe char.
fn records(out: &str, expected: usize) -> Vec<&str> {
    assert!(
        out.ends_with('\n'),
        "output must end on a record boundary: {out:?}"
    );
    let lines: Vec<&str> = out.strip_suffix('\n').unwrap_or(out).split('\n').collect();
    assert_eq!(lines.len(), expected, "physical line count: {out:?}");
    for line in &lines {
        let raw: Vec<char> = line.chars().filter(|c| is_unsafe(*c)).collect();
        assert!(raw.is_empty(), "raw unsafe chars {raw:?} in line {line:?}");
    }
    lines
}

#[derive(Debug)]
struct Multiline;

impl fmt::Display for Multiline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("first\nsecond\u{1b}[0m")
    }
}

impl Error for Multiline {}

#[test]
fn structured_display_field_stays_on_one_line_with_visible_escapes() {
    // Arrange
    let value = HOSTILE;

    // Act
    let out = production(|| tracing::warn!(peer = %value, "probe"));

    // Assert
    let lines = records(&out, 1);
    assert!(
        lines[0].ends_with(&format!("probe peer={HOSTILE_ESCAPED}")),
        "{:?}",
        lines[0]
    );
}

#[test]
fn structured_debug_field_is_not_escaped_twice() {
    // Arrange
    let value = "x\u{202e}y\u{200b}z\u{9b}";

    // Act
    let out = production(|| warn!(peer = ?value, "probe"));

    // Assert
    let lines = records(&out, 1);
    assert!(
        lines[0].ends_with(r#"probe peer="x\u{202e}y\u{200b}z\u{9b}""#),
        "{:?}",
        lines[0]
    );
}

#[test]
fn message_interpolation_escapes_captured_positional_and_format_spec_forms() {
    // Arrange
    let value = HOSTILE;

    // Act
    let out = production(|| {
        warn!("captured {value}");
        warn!("positional {}", value);
        warn!("width {:>3} {value:<2}", value);
        log_info!("pretty {:#?}", ("x", 1));
        tracing::event!(Level::ERROR, "qualified {value}");
    });

    // Assert
    let lines = records(&out, 5);
    assert!(
        lines[0].ends_with(&format!("captured {HOSTILE_ESCAPED}")),
        "{:?}",
        lines[0]
    );
    assert!(
        lines[1].ends_with(&format!("positional {HOSTILE_ESCAPED}")),
        "{:?}",
        lines[1]
    );
    assert!(
        lines[2].ends_with(&format!("width {HOSTILE_ESCAPED} {HOSTILE_ESCAPED}")),
        "{:?}",
        lines[2]
    );
    assert!(
        lines[3].ends_with(r#"pretty (\n    "x",\n    1,\n)"#),
        "{:?}",
        lines[3]
    );
    assert!(
        lines[4].ends_with(&format!("qualified {HOSTILE_ESCAPED}")),
        "{:?}",
        lines[4]
    );
}

#[test]
fn escaped_and_raw_string_literal_messages_stay_on_one_line() {
    // Arrange
    let expected_raw = r"raw\nbody";

    // Act
    let out = production(|| {
        warn!("escaped\nbody\u{202e}");
        warn!(
            r"raw
body"
        );
    });

    // Assert
    let lines = records(&out, 2);
    assert!(
        lines[0].ends_with(r"escaped\nbody\u{202e}"),
        "{:?}",
        lines[0]
    );
    assert!(lines[1].ends_with(expected_raw), "{:?}", lines[1]);
}

#[test]
fn error_field_and_its_source_chain_stay_on_one_line() {
    // Arrange
    let err = Multiline;

    // Act
    let out = production(|| warn!(error = &err as &dyn Error, "failed"));

    // Assert
    let lines = records(&out, 1);
    assert!(
        lines[0].ends_with(r"failed error=first\nsecond\x1b[0m"),
        "{:?}",
        lines[0]
    );
}

#[test]
fn span_fields_and_later_record_updates_are_escaped_on_every_line() {
    // Arrange
    let value = HOSTILE;

    // Act
    let out = production(|| {
        let span = tracing::info_span!("req", peer = %value, late = Empty);
        let _entered = span.enter();
        warn!("inside");
        span.record("late", display(value));
        warn!("after record");
    });

    // Assert
    let lines = records(&out, 3);
    let opened = format!("req{{peer={HOSTILE_ESCAPED}}}");
    let recorded = format!("req{{peer={HOSTILE_ESCAPED} late={HOSTILE_ESCAPED}}}");
    assert!(
        lines[0].contains(&opened) && lines[0].ends_with("inside"),
        "{:?}",
        lines[0]
    );
    assert!(lines[1].contains(&recorded), "{:?}", lines[1]);
    assert!(
        lines[2].contains(&recorded) && lines[2].ends_with(": log_sink: close"),
        "{:?}",
        lines[2]
    );
}

#[test]
fn ansi_decoration_does_not_unescape_field_payloads() {
    // Arrange
    let buffer = Buffer::default();
    let sink = buffer.clone();
    let subscriber = log_sink::subscriber(
        EnvFilter::new("trace"),
        move || sink.clone(),
        true,
        Clock::Off,
    );

    // Act
    tracing::subscriber::with_default(subscriber, || warn!(peer = %"\u{1b}[2J", "probe"));

    // Assert
    let out = buffer.text();
    assert_eq!(out.matches('\n').count(), 1, "{out:?}");
    assert!(out.contains(r"peer=\x1b[2J"), "{out:?}");
    assert!(!out.contains("\u{1b}[2J"), "{out:?}");
}

#[test]
fn clean_printable_output_matches_stock_default_fields_byte_for_byte() {
    // Arrange
    let emit = || {
        let span = tracing::info_span!("req", id = 7_u64, host = %"example.com", note = Empty);
        let _entered = span.enter();
        tracing::info!(provider = %"openai", status = 200_u16, ok = true, "sent {} bytes", 12);
        warn!(path = ?"C:\\dir\\file", ratio = 0.5_f64, "back\\slash {:>6}|", "pad");
        span.record(
            "note",
            "caf\u{e9} \u{65e5}\u{672c} \u{1f600} \u{a0}\u{2010}\u{fffc}",
        );
        log_info!(quoted = ?"say \"hi\"", "done");
        tracing::debug!(r#type = "kw", "raw ident");
        let err = io::Error::other("disk gone");
        tracing::error!(error = &err as &dyn Error, "io");
    };

    // Act
    let ours = production(emit);
    let theirs = stock(emit);

    // Assert
    records(&ours, 6);
    assert_eq!(ours, theirs);
}

/// Every oracle range edge, plus both neighbours, as one string per side:
/// `inside` holds each first and last escaped code point, `outside` each
/// adjacent code point the oracle does not cover.
fn boundary_probes() -> (String, String) {
    let valid = |c: u32| char::from_u32(c);
    let mut inside = String::new();
    let mut outside = String::new();
    for &(lo, hi) in ORACLE.iter() {
        for c in [lo, hi].into_iter().filter_map(valid) {
            inside.push(c);
        }
        for c in [lo.wrapping_sub(1), hi + 1].into_iter().filter_map(valid) {
            if !is_unsafe(c) && !outside.contains(c) {
                outside.push(c);
            }
        }
    }
    (inside, outside)
}

fn escaped(text: &str) -> String {
    text.chars()
        .map(|c| {
            if is_unsafe(c) {
                escape_of(c)
            } else {
                c.to_string()
            }
        })
        .collect()
}

#[test]
fn reviewed_record_breaking_characters_are_all_escaped() {
    // Arrange
    let reviewed = "\u{ad}\u{61c}\u{2028}\u{2029}\u{2060}\u{2064}\u{206f}\u{feff}\u{fff9}\u{fffb}\u{e0000}\u{e0001}\u{e007f}";

    // Act
    let out = production(|| warn!(v = %reviewed, "reviewed"));

    // Assert
    let lines = records(&out, 1);
    let expected = r"reviewed v=\u{ad}\u{61c}\u{2028}\u{2029}\u{2060}\u{2064}\u{206f}\u{feff}\u{fff9}\u{fffb}\u{e0000}\u{e0001}\u{e007f}";
    assert_eq!(lines[0], format!(" WARN log_sink: {expected}"));
}

#[test]
fn every_oracle_boundary_is_escaped_on_event_message_and_span_paths() {
    // Arrange
    let (inside, outside) = boundary_probes();
    let probe = format!("{inside}|{outside}");
    let want = format!("{}|{outside}", escaped(&inside));

    // Act
    let out = production(|| {
        warn!(v = %probe, "field");
        warn!("message {probe}");
        let span = tracing::info_span!("s", open = %probe, late = Empty);
        span.record("late", display(&probe));
        let _entered = span.enter();
        warn!("in span");
    });

    // Assert
    let lines = records(&out, 4);
    assert_eq!(lines[0], format!(" WARN log_sink: field v={want}"));
    assert_eq!(lines[1], format!(" WARN log_sink: message {want}"));
    assert_eq!(
        lines[2],
        format!(" WARN s{{open={want} late={want}}}: log_sink: in span")
    );
    assert_eq!(
        lines[3],
        format!(" INFO s{{open={want} late={want}}}: log_sink: close")
    );
}

#[test]
fn every_scalar_value_is_escaped_exactly_when_the_oracle_lists_it() {
    // Arrange
    let all: String = (0..=0x10_FFFF).filter_map(char::from_u32).collect();

    // Act
    let out = production(|| warn!(v = %all, "all"));

    // Assert
    let lines = records(&out, 1);
    let body = lines[0]
        .strip_prefix(" WARN log_sink: all v=")
        .expect("record prefix");
    assert_eq!(body, escaped(&all));
}

/// Writes `good` then fails, like a `Display` impl whose inner write erred.
struct Failing(&'static str);

impl fmt::Display for Failing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)?;
        Err(fmt::Error)
    }
}

#[test]
fn a_failing_field_writes_escaped_partial_output_and_the_marker_on_one_line() {
    // Arrange
    let bad = Failing("part\n\u{202e}ial");

    // Act
    let out = production(|| {
        warn!(ok = 1_u8, bad = %bad, "failing");
        let span = tracing::info_span!("s", bad = %Failing("sp\nan"));
        let _entered = span.enter();
        warn!("in span");
    });

    // Assert
    let lines = records(&out, 3);
    let marker = log_sink::FORMAT_FAILED_MARKER;
    assert_eq!(
        lines[0],
        format!(r" WARN log_sink: failing ok=1 bad=part\n\u{{202e}}ial {marker}")
    );
    assert_eq!(
        lines[1],
        format!(r" WARN s{{bad=sp\nan {marker}}}: log_sink: in span")
    );
    assert!(!out.contains("Unable to format"), "{out:?}");
}
