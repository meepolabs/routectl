//! The running daemon's needs-attention list, read over the wire for
//! `routectl doctor`, and the terminal lines that show it.
//!
//! One `GET /status/attention` goes to the loopback address derived from the
//! config's `[server]` host and port, through the CLI daemon client (no proxy,
//! no redirects, the first listener token as `x-api-key`, the body read only up
//! to its cap). Every way of not getting a list maps to one [`Reason`] from a
//! closed set, and no failure ever yields a verdict.
//!
//! # Reader rule
//!
//! This is an out-of-process reader of the `/status/attention` v1 contract,
//! and every such reader follows the same three rules:
//!
//! - **Refuse an unknown `schema_version`.** Only version 1 is read; any other
//!   version reads as `unrecognized`, never as a best-effort guess.
//! - **Tolerate unknown tokens and fields.** `kind`, `severity`, `state` and
//!   the coverage token are read as plain strings, never matched against the
//!   builder's enums, so a kind this build has never heard of still renders
//!   generically with its subject, cause and raw token; a field this build does
//!   not know is ignored.
//! - **Sanitize every string before display.** Whatever answers on the
//!   configured port writes these strings, and the daemon itself embeds
//!   operator config names, vendor-derived tokens and doctor details verbatim.
//!   Every displayed wire string passes [`sanitize_for_log_with_cap`], which
//!   keeps printable ASCII and space only and caps the length, so no control
//!   function (C0, ESC sequences, DEL, C1, bidi overrides) reaches the terminal
//!   and no planted line break starts a new output line.
//!
//! An answer outside the caps (1 MiB of body, [`MAX_ITEMS`]
//! items) is refused whole as `unrecognized`; nothing is truncated into a
//! partial list. A v1 envelope that carries `unavailable` instead of `data`
//! also reads as `unrecognized`, with the daemon's code shown sanitized.

use routectl_core::sanitize_for_log_with_cap;
use routectl_router::Config;
use serde::Deserialize;
use serde_json::Value;

use crate::commands::daemon_client::{self, BodyError, MAX_BODY_BYTES, REQUEST_TIMEOUT};

/// Path of the daemon's needs-attention route.
pub const ATTENTION_PATH: &str = "/status/attention";

/// The one wire version this reader understands.
pub const SCHEMA_VERSION: u64 = 1;

/// Most items an answer may carry. Past this the answer is refused whole.
pub const MAX_ITEMS: usize = 512;

/// Display caps, in characters, for each wire string.
pub const MAX_SUBJECT_CHARS: usize = 160;
pub const MAX_CAUSE_CHARS: usize = 400;
pub const MAX_RAW_TOKEN_CHARS: usize = 200;
pub const MAX_TOKEN_CHARS: usize = 64;
pub const MAX_FIX_CHARS: usize = 256;
pub const MAX_REMEDIATION_CHARS: usize = 400;
pub const MAX_VERDICT_CHARS: usize = 640;
pub const MAX_AS_OF_CHARS: usize = 64;
pub const MAX_DAEMON_CODE_CHARS: usize = 64;

/// Header line of the block.
pub const BLOCK_TITLE: &str = "[Needs attention]";

/// Why no list could be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Nothing accepted a connection on the derived address.
    NotRunning,
    /// The round trip outlasted the request timeout.
    Timeout,
    /// `[server] host` names no loopback address to dial.
    NoLoopbackAddress,
    /// The config could not be read, so no address is known.
    ConfigUnreadable,
    /// The daemon refused the listener credential.
    AuthRefused,
    /// The daemon shed the request.
    Busy,
    /// The daemon does not serve the route (an older build).
    NotServed,
    /// An answer this reader does not accept.
    Unrecognized,
}

impl Reason {
    /// Every reason, for tables over the closed set.
    pub const ALL: [Self; 8] = [
        Self::NotRunning,
        Self::Timeout,
        Self::NoLoopbackAddress,
        Self::ConfigUnreadable,
        Self::AuthRefused,
        Self::Busy,
        Self::NotServed,
        Self::Unrecognized,
    ];

    /// The reason token.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotRunning => "not_running",
            Self::Timeout => "timeout",
            Self::NoLoopbackAddress => "no_loopback_address",
            Self::ConfigUnreadable => "config_unreadable",
            Self::AuthRefused => "auth_refused",
            Self::Busy => "busy",
            Self::NotServed => "not_served",
            Self::Unrecognized => "unrecognized",
        }
    }
}

/// A read that produced no list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unavailable {
    pub reason: Reason,
    /// The URL dialed, when one was derived.
    pub address: Option<String>,
    /// The code of a v1 `unavailable` envelope, already sanitized and capped.
    pub daemon_code: Option<String>,
}

impl Unavailable {
    /// A failure with no daemon code.
    pub const fn new(reason: Reason, address: Option<String>) -> Self {
        Self {
            reason,
            address,
            daemon_code: None,
        }
    }

    fn at(reason: Reason, address: &str) -> Self {
        Self::new(reason, Some(address.to_string()))
    }
}

/// A list the daemon served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonAttention {
    /// The URL dialed.
    pub address: String,
    pub schema_version: u64,
    /// The envelope's `as_of`, as received.
    pub as_of: String,
    /// The envelope's `data`, as received (bounded by the body cap).
    pub data: Value,
    /// `data` read through this reader's lenient shape.
    pub list: AttentionList,
}

/// The lenient v1 `data` shape.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AttentionList {
    pub verdict: WireVerdict,
    pub counts: WireCounts,
    pub coverage: WireCoverage,
    pub items: Vec<WireItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WireVerdict {
    pub severity: String,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct WireCounts {
    pub broken: u64,
    pub degraded: u64,
    pub recovered: u64,
    pub doctor_fail: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WireCoverage {
    pub doctor: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WireItem {
    pub key: String,
    pub kind: String,
    pub severity: String,
    pub state: String,
    pub subject: String,
    pub cause: String,
    pub raw_token: String,
    #[serde(default)]
    pub fix: Option<String>,
    #[serde(default)]
    pub remediation: Option<String>,
}

#[derive(Deserialize)]
struct Envelope {
    schema_version: u64,
    #[serde(default)]
    as_of: Option<String>,
    #[serde(default)]
    data: Option<Value>,
    #[serde(default)]
    unavailable: Option<String>,
}

/// A body read by [`parse`]: the envelope's fields and the lenient list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedBody {
    pub schema_version: u64,
    pub as_of: String,
    pub data: Value,
    pub list: AttentionList,
}

/// Read the configured daemon's list.
pub async fn fetch(config: &Config) -> Result<DaemonAttention, Unavailable> {
    fetch_within(config, REQUEST_TIMEOUT).await
}

/// [`fetch`] with `timeout` bounding the whole round trip.
pub async fn fetch_within(
    config: &Config,
    timeout: std::time::Duration,
) -> Result<DaemonAttention, Unavailable> {
    let Some(url) =
        daemon_client::control_url(&config.server.host, config.server.port, ATTENTION_PATH)
    else {
        return Err(Unavailable::new(Reason::NoLoopbackAddress, None));
    };
    let client = daemon_client::build_client(timeout)
        .map_err(|_| Unavailable::at(Reason::Unrecognized, &url))?;
    let mut request = client.get(&url);
    if let Some(token) = daemon_client::first_listener_token(config).await {
        request = request.header("x-api-key", token);
    }
    let response = request
        .send()
        .await
        .map_err(|e| Unavailable::at(send_failure(&e), &url))?;
    if let Some(reason) = status_failure(response.status()) {
        return Err(Unavailable::at(reason, &url));
    }
    let body = daemon_client::read_capped_body(response)
        .await
        .map_err(|e| Unavailable::at(body_failure(&e), &url))?;
    let parsed = parse(&body).map_err(|mut failure| {
        failure.address = Some(url.clone());
        failure
    })?;
    Ok(DaemonAttention {
        address: url,
        schema_version: parsed.schema_version,
        as_of: parsed.as_of,
        data: parsed.data,
        list: parsed.list,
    })
}

fn send_failure(error: &reqwest::Error) -> Reason {
    if error.is_timeout() {
        Reason::Timeout
    } else if error.is_connect() {
        Reason::NotRunning
    } else {
        Reason::Unrecognized
    }
}

fn status_failure(status: reqwest::StatusCode) -> Option<Reason> {
    use reqwest::StatusCode;
    match status {
        StatusCode::OK => None,
        StatusCode::NOT_FOUND => Some(Reason::NotServed),
        StatusCode::UNAUTHORIZED => Some(Reason::AuthRefused),
        StatusCode::SERVICE_UNAVAILABLE => Some(Reason::Busy),
        _ => Some(Reason::Unrecognized),
    }
}

fn body_failure(error: &BodyError) -> Reason {
    match error {
        BodyError::Read(e) if e.is_timeout() => Reason::Timeout,
        BodyError::Read(_) | BodyError::TooLarge { .. } => Reason::Unrecognized,
    }
}

/// Read a `/status/attention` body under the reader rule. The returned
/// failure carries no address.
pub fn parse(body: &[u8]) -> Result<ParsedBody, Unavailable> {
    let unrecognized = || Unavailable::new(Reason::Unrecognized, None);
    if body.len() > MAX_BODY_BYTES {
        return Err(unrecognized());
    }
    let envelope: Envelope = serde_json::from_slice(body).map_err(|_| unrecognized())?;
    if envelope.schema_version != SCHEMA_VERSION {
        return Err(unrecognized());
    }
    if let Some(code) = envelope.unavailable {
        return Err(Unavailable {
            daemon_code: Some(sanitize_for_log_with_cap(&code, MAX_DAEMON_CODE_CHARS)),
            ..unrecognized()
        });
    }
    let (Some(as_of), Some(data)) = (envelope.as_of, envelope.data) else {
        return Err(unrecognized());
    };
    let list: AttentionList = AttentionList::deserialize(&data).map_err(|_| unrecognized())?;
    if list.items.len() > MAX_ITEMS {
        return Err(unrecognized());
    }
    Ok(ParsedBody {
        schema_version: envelope.schema_version,
        as_of,
        data,
        list,
    })
}

/// The block's lines for a served list: the title, the source line, the
/// verdict and counts, then each item with its raw token and, when set, its
/// fix or remediation on a line of its own.
pub fn render(attention: &DaemonAttention) -> Vec<String> {
    let list = &attention.list;
    let counts = list.counts;
    let mut lines = vec![
        BLOCK_TITLE.to_string(),
        format!(
            "  source: daemon at {}, as of {}, doctor coverage {}",
            attention.address,
            safe(&attention.as_of, MAX_AS_OF_CHARS),
            safe(&list.coverage.doctor, MAX_TOKEN_CHARS),
        ),
        format!(
            "  verdict: {}: {}",
            safe(&list.verdict.severity, MAX_TOKEN_CHARS),
            safe(&list.verdict.text, MAX_VERDICT_CHARS),
        ),
        format!(
            "  counts: broken {}  degraded {}  recovered {}  doctor_fail {}",
            counts.broken, counts.degraded, counts.recovered, counts.doctor_fail
        ),
    ];
    for item in &list.items {
        render_item(item, &mut lines);
    }
    lines
}

fn render_item(item: &WireItem, lines: &mut Vec<String>) {
    let state = if item.state == "active" {
        String::new()
    } else {
        format!(" ({})", safe(&item.state, MAX_TOKEN_CHARS))
    };
    lines.push(format!(
        "  {} {}{state}: {}: {}",
        safe(&item.severity, MAX_TOKEN_CHARS),
        safe(&item.kind, MAX_TOKEN_CHARS),
        safe(&item.subject, MAX_SUBJECT_CHARS),
        safe(&item.cause, MAX_CAUSE_CHARS),
    ));
    lines.push(format!(
        "      raw: {}",
        safe(&item.raw_token, MAX_RAW_TOKEN_CHARS)
    ));
    if let Some(fix) = &item.fix {
        lines.push(format!("      fix: {}", safe_fix(fix)));
    }
    if let Some(remediation) = &item.remediation {
        lines.push(format!(
            "      remediation: {}",
            safe(remediation, MAX_REMEDIATION_CHARS)
        ));
    }
}

/// The one line shown when no list could be read. It names the reason and
/// never carries a verdict.
pub fn render_unavailable(failure: &Unavailable) -> String {
    let code = failure
        .daemon_code
        .as_deref()
        .map(|code| format!(", daemon code {}", safe(code, MAX_DAEMON_CODE_CHARS)))
        .unwrap_or_default();
    let at = failure
        .address
        .as_deref()
        .map(|address| format!(" at {address}"))
        .unwrap_or_default();
    format!(
        "{BLOCK_TITLE} unavailable ({}{code}): the current-conditions list needs the running daemon{at}",
        failure.reason.as_str()
    )
}

fn safe(raw: &str, cap: usize) -> String {
    sanitize_for_log_with_cap(raw, cap)
}

/// A fix is copied as a bare command, so a backtick the wire carries is
/// dropped rather than shown.
fn safe_fix(raw: &str) -> String {
    safe(raw, MAX_FIX_CHARS).replace('`', "")
}

#[cfg(test)]
#[path = "daemon_attention_tests.rs"]
mod tests;
