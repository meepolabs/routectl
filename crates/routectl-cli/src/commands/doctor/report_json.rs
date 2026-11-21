//! The `doctor --json` document: the report plus the daemon's needs-attention
//! list as a top-level `attention` member, written as pure-ASCII JSON.
//!
//! `attention.data` is the daemon's `data` exactly as received, and whatever
//! answers on the configured port writes its strings. serde_json escapes only
//! C0, so DEL, C1 controls, bidi overrides and line/paragraph separators would
//! otherwise reach a terminal as raw bytes. Every non-ASCII character and DEL
//! is written as a `\uXXXX` escape instead (a surrogate pair past the BMP), so
//! the bytes are printable ASCII while the decoded value is unchanged.

use std::io;

use routectl_router::DoctorReport;
use serde::Serialize;
use serde_json::Value;
use serde_json::ser::{Formatter, PrettyFormatter};

use super::AttentionRead;

#[derive(Serialize)]
struct DoctorJson<'a> {
    #[serde(flatten)]
    report: &'a DoctorReport,
    attention: AttentionJson<'a>,
}

#[derive(Serialize)]
struct AttentionJson<'a> {
    source: &'static str,
    reason: Option<&'static str>,
    address: Option<&'a str>,
    schema_version: Option<u64>,
    as_of: Option<&'a str>,
    data: Option<&'a Value>,
}

impl<'a> AttentionJson<'a> {
    fn of(attention: &'a AttentionRead) -> Self {
        match attention {
            Ok(served) => Self {
                source: "daemon",
                reason: None,
                address: Some(&served.address),
                schema_version: Some(served.schema_version),
                as_of: Some(&served.as_of),
                data: Some(&served.data),
            },
            Err(failure) => Self {
                source: "unavailable",
                reason: Some(failure.reason.as_str()),
                address: failure.address.as_deref(),
                schema_version: None,
                as_of: None,
                data: None,
            },
        }
    }
}

/// The pretty-printed document, ASCII only.
pub(super) fn to_ascii_json(
    report: &DoctorReport,
    attention: &AttentionRead,
) -> serde_json::Result<String> {
    let document = DoctorJson {
        report,
        attention: AttentionJson::of(attention),
    };
    let mut out = Vec::new();
    let mut serializer =
        serde_json::Serializer::with_formatter(&mut out, AsciiFormatter::default());
    document.serialize(&mut serializer)?;
    String::from_utf8(out).map_err(|e| serde_json::Error::io(io::Error::other(e)))
}

/// [`PrettyFormatter`] that writes every character outside printable ASCII
/// as a `\u` escape.
#[derive(Default)]
struct AsciiFormatter {
    pretty: PrettyFormatter<'static>,
}

impl Formatter for AsciiFormatter {
    fn write_string_fragment<W: ?Sized + io::Write>(
        &mut self,
        writer: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        let mut units = [0_u16; 2];
        for c in fragment.chars() {
            if c.is_ascii() && c != '\u{7f}' {
                writer.write_all(&[c as u8])?;
            } else {
                for unit in c.encode_utf16(&mut units) {
                    write!(writer, "\\u{unit:04x}")?;
                }
            }
        }
        Ok(())
    }

    fn begin_array<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.pretty.begin_array(writer)
    }

    fn end_array<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.pretty.end_array(writer)
    }

    fn begin_array_value<W: ?Sized + io::Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        self.pretty.begin_array_value(writer, first)
    }

    fn end_array_value<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.pretty.end_array_value(writer)
    }

    fn begin_object<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.pretty.begin_object(writer)
    }

    fn end_object<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.pretty.end_object(writer)
    }

    fn begin_object_key<W: ?Sized + io::Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        self.pretty.begin_object_key(writer, first)
    }

    fn begin_object_value<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.pretty.begin_object_value(writer)
    }

    fn end_object_value<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.pretty.end_object_value(writer)
    }
}
