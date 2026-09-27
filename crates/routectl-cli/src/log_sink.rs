//! The production tracing sink: the one subscriber builder the binary
//! installs, and the field formatter that keeps every record on one line.
//!
//! `DefaultFields` writes `%` (Display) values, error values, and message
//! text verbatim, so a caller- or upstream-controlled string carrying `\n`
//! forges a second record and one carrying ESC or a bidi override rewrites
//! what the operator sees. [`EscapingFields`] renders through
//! `DefaultFields` unchanged, then rewrites every control, format,
//! line/paragraph separator, and default-ignorable code point as a visible
//! ASCII escape before the text reaches an event line or a span's stored
//! fields. Printable text passes byte-for-byte. A field whose `Display` /
//! `Debug` impl fails still writes its escaped partial output plus
//! [`FORMAT_FAILED_MARKER`].
//!
//! Call-site sanitizers still own length caps, redaction, and value
//! normalization; this layer only guarantees record integrity.
//! `scripts/check-log-display.sh` fails the commit gate if any other
//! production file names a subscriber or global-dispatch API, or if the
//! builder and installer below drift from the exact text it pins.

use std::fmt;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format::{DefaultFields, FmtSpan, FormatFields, Writer};
use tracing_subscriber::util::SubscriberInitExt as _;

/// Env var holding the filter directives (deliberately not `RUST_LOG`).
pub const FILTER_ENV: &str = "ROUTECTL_LOG";

/// Directive used when [`FILTER_ENV`] is unset or unparseable.
pub const DEFAULT_DIRECTIVE: &str = "info";

/// Appended in place of whatever a failing `Display` / `Debug` impl did not
/// write, so a truncated record is visibly truncated.
pub const FORMAT_FAILED_MARKER: &str = "[field formatting failed]";

/// Field formatter that renders like `DefaultFields` with ANSI styling
/// off, then escapes every control, format, line/paragraph separator, and
/// default-ignorable character as a visible ASCII escape.
#[derive(Debug, Default, Clone, Copy)]
pub struct EscapingFields;

impl<'writer> FormatFields<'writer> for EscapingFields {
    fn format_fields<R: RecordFields>(
        &self,
        mut writer: Writer<'writer>,
        fields: R,
    ) -> fmt::Result {
        let mut rendered = String::new();
        let outcome = DefaultFields::new().format_fields(Writer::new(&mut rendered), fields);
        // Never return Err: tracing-subscriber answers a field-format error by
        // printing the record's raw values to stderr. Both writers it hands a
        // field formatter are String-backed, so their results carry nothing.
        let _ = write_escaped(&mut writer, &rendered);
        if outcome.is_err() {
            let separator = if rendered.is_empty() { "" } else { " " };
            let _ = write!(writer, "{separator}{FORMAT_FAILED_MARKER}");
        }
        Ok(())
    }
}

/// Inclusive code point ranges written as escapes, sorted and
/// non-overlapping: the union of general categories Cc, Cf, Zl and Zp
/// (`extracted/DerivedGeneralCategory.txt`), `Default_Ignorable_Code_Point`
/// (`DerivedCoreProperties.txt`), and the interlinear annotation controls
/// U+FFF9..U+FFFB, from the Unicode 18.0.0 UCD.
const ESCAPED_RANGES: &[(u32, u32)] = &[
    (0x0000, 0x001F),
    (0x007F, 0x009F),
    (0x00AD, 0x00AD),
    (0x034F, 0x034F),
    (0x0600, 0x0605),
    (0x061C, 0x061C),
    (0x06DD, 0x06DD),
    (0x070F, 0x070F),
    (0x0890, 0x0891),
    (0x08E2, 0x08E2),
    (0x115F, 0x1160),
    (0x17B4, 0x17B5),
    (0x180B, 0x180F),
    (0x200B, 0x200F),
    (0x2028, 0x202E),
    (0x2060, 0x206F),
    (0x3164, 0x3164),
    (0xFE00, 0xFE0F),
    (0xFEFF, 0xFEFF),
    (0xFFA0, 0xFFA0),
    (0xFFF0, 0xFFFB),
    (0x110BD, 0x110BD),
    (0x110CD, 0x110CD),
    (0x13430, 0x1343F),
    (0x1BCA0, 0x1BCA3),
    (0x1D173, 0x1D17A),
    (0xE0000, 0xE0FFF),
];

/// Whether `c` could break a record or hide text when written verbatim.
fn needs_escape(c: char) -> bool {
    let c = u32::from(c);
    let after = ESCAPED_RANGES.partition_point(|&(lo, _)| lo <= c);
    after > 0 && c <= ESCAPED_RANGES[after - 1].1
}

/// Copy `text` to `out`, replacing each [`needs_escape`] character with a
/// visible ASCII escape: `\n` / `\r` / `\t`, `\xNN` for other C0 and DEL,
/// and `\u{N}` above that -- the forms tracing-subscriber uses for message
/// text, so an already-escaped message and a newly escaped field read alike.
fn write_escaped(out: &mut impl fmt::Write, text: &str) -> fmt::Result {
    let mut clean_from = 0;
    for (at, c) in text.char_indices().filter(|(_, c)| needs_escape(*c)) {
        out.write_str(&text[clean_from..at])?;
        match c {
            '\n' => out.write_str("\\n")?,
            '\r' => out.write_str("\\r")?,
            '\t' => out.write_str("\\t")?,
            c if u32::from(c) <= 0x7f => write!(out, "\\x{:02x}", u32::from(c))?,
            c => write!(out, "\\u{{{:x}}}", u32::from(c))?,
        }
        clean_from = at + c.len_utf8();
    }
    out.write_str(&text[clean_from..])
}

/// Whether records carry a timestamp and span close lines carry timings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clock {
    /// Wall-clock timestamps and `time.busy` / `time.idle` (production).
    Wall,
    /// Neither, so two runs of the same events write identical bytes.
    Off,
}

/// Build the production subscriber: `filter` selects records, `writer` is
/// the sink, `ansi` styles only the formatter's own level/target decoration
/// (field text is never styled), and `clock` picks [`Clock`].
pub fn subscriber<W>(
    filter: EnvFilter,
    writer: W,
    ansi: bool,
    clock: Clock,
) -> Box<dyn tracing::Subscriber + Send + Sync>
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    let configured = tracing_subscriber::fmt()
        .fmt_fields(EscapingFields)
        .with_env_filter(filter)
        .with_target(true)
        .with_span_events(FmtSpan::CLOSE)
        .with_ansi(ansi)
        .with_writer(writer);
    match clock {
        Clock::Wall => Box::new(configured.finish()),
        Clock::Off => Box::new(configured.without_time().finish()),
    }
}

/// Install [`subscriber`] as the process-global default, writing to
/// stderr with the filter read from [`FILTER_ENV`].
pub fn init() {
    let filter =
        EnvFilter::try_from_env(FILTER_ENV).unwrap_or_else(|_| EnvFilter::new(DEFAULT_DIRECTIVE));
    // The fmt layer's own default: styled unless NO_COLOR is set non-empty.
    let ansi = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    subscriber(filter, std::io::stderr, ansi, Clock::Wall).init();
}
