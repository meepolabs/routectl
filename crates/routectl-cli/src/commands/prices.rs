//! `routectl prices refresh [--from <url|file>]`: fetch a models.dev
//! `api.json` document (or read one from disk), convert it into routectl's
//! canonical price table, and atomically replace `<config_dir>/prices.json`.
//!
//! This is the only network path for prices, and it runs only when the
//! operator invokes it. The fetch uses its own client: a plain GET with no
//! default headers, no cookie store, redirects off and fixed timeouts. It
//! never reaches the secret store or a provider client, so no credential can
//! ride on it; userinfo in any URL it is given is dropped before the request
//! is built. The operator's proxy environment applies.
//!
//! The downloaded bytes are untrusted. They are bounded while they are read,
//! converted and validated before anything is written, and no string from
//! them is ever printed or logged: output carries counts and the table date.

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::Url;
use routectl_core::log_safe::sanitize_for_log;
use routectl_router::prices::{MAX_INPUT_BYTES, PriceTable, PriceTableError, TableDate};

/// The source fetched when `--from` is not given.
pub const DEFAULT_SOURCE_URL: &str = "https://models.dev/api.json";
/// The table file name inside the config folder.
pub const PRICES_FILE: &str = "prices.json";

/// Connect and whole-request timeouts of the fetch client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// Time allowed to establish the connection.
    pub connect: Duration,
    /// Time allowed for the whole request, body included.
    pub total: Duration,
}

/// The timeouts every refresh uses.
pub const TIMEOUTS: Timeouts = Timeouts {
    connect: Duration::from_secs(5),
    total: Duration::from_secs(15),
};

/// Which size check refused a source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeCheck {
    /// The declared size (file metadata or `Content-Length`).
    Declared,
    /// The bytes actually read.
    Read,
}

impl fmt::Display for SizeCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Declared => "declared size",
            Self::Read => "bytes read",
        })
    }
}

/// Why a refresh failed. URLs are rendered only in their stripped form
/// ([`display_url`]); nothing from a downloaded body is rendered.
#[derive(Debug, thiserror::Error)]
pub enum PricesError {
    /// `--from` looked like a URL but is not a usable http(s) URL.
    #[error("--from is not a usable http(s) URL")]
    UnusableUrl,
    /// The HTTP client could not be built.
    #[error("could not build the HTTP client")]
    Client,
    /// The request failed before a response arrived.
    #[error("fetch {url}: {cause}")]
    Request {
        /// The stripped URL.
        url: String,
        /// A fixed description of the failure.
        cause: &'static str,
    },
    /// The source answered with a redirect, which is never followed.
    #[error("fetch {url}: HTTP {status} redirect{}; redirects are not followed", render_location(.location.as_deref()))]
    Redirect {
        /// The stripped URL that was fetched.
        url: String,
        /// The response status code.
        status: u16,
        /// The `scheme://host[:port]` of the redirect target, when it
        /// resolves to an http(s) URL. Its path and query are never rendered.
        location: Option<String>,
    },
    /// The source answered with a non-success status.
    #[error("fetch {url}: HTTP {status}")]
    Status {
        /// The stripped URL.
        url: String,
        /// The response status code.
        status: u16,
    },
    /// The source is larger than the input bound.
    #[error("the source exceeds {limit} bytes ({check})")]
    TooLarge {
        /// The byte bound.
        limit: usize,
        /// Which check refused it.
        check: SizeCheck,
    },
    /// The `--from` file could not be read.
    #[error("read {path}: {source}")]
    ReadFile {
        /// The operator-supplied path.
        path: String,
        /// The I/O failure.
        source: std::io::Error,
    },
    /// The source is not a usable price table.
    #[error("the source is not a usable price table: {0}")]
    Table(#[from] PriceTableError),
    /// The current date could not be turned into a table date.
    #[error("could not determine today's date")]
    Date,
    /// Writing `prices.json` failed; any previous copy is unchanged.
    #[error("write {path}: {stage}: {source}")]
    Write {
        /// The target path.
        path: String,
        /// The step that failed.
        stage: &'static str,
        /// The I/O failure.
        source: std::io::Error,
    },
}

fn render_location(location: Option<&str>) -> String {
    location.map_or_else(String::new, |l| format!(" to {l}"))
}

/// What a successful refresh wrote. Its `Display` is the operator line:
/// counts and the table date, never an id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshSummary {
    /// Providers in the written table.
    pub providers: usize,
    /// Model rows in the written table.
    pub models: usize,
    /// Source rows left out because their provider or model id was unusable.
    pub skipped: usize,
    /// The table date.
    pub generated_at: TableDate,
}

impl RefreshSummary {
    fn of(table: &PriceTable) -> Self {
        Self {
            providers: table.provider_count(),
            models: table.model_count(),
            skipped: table.skipped_row_count(),
            generated_at: table.generated_at(),
        }
    }
}

impl fmt::Display for RefreshSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "prices refreshed: {} models from {} providers, table dated {}",
            self.models, self.providers, self.generated_at
        )?;
        if self.skipped > 0 {
            write!(
                f,
                "; {} source rows skipped for an unusable id",
                self.skipped
            )?;
        }
        Ok(())
    }
}

/// Where a refresh reads its source from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// An http(s) URL, userinfo already removed.
    Url(Url),
    /// A local file.
    File(PathBuf),
}

impl Source {
    /// The default URL when `from` is `None`; an http(s) URL when `from`
    /// starts with `http://` or `https://` (any case); a file path otherwise.
    pub fn parse(from: Option<&str>) -> Result<Self, PricesError> {
        let Some(from) = from else {
            return Self::url(DEFAULT_SOURCE_URL);
        };
        let is_url = ["http://", "https://"].iter().any(|scheme| {
            from.get(..scheme.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(scheme))
        });
        if is_url {
            Self::url(from)
        } else {
            Ok(Self::File(PathBuf::from(from)))
        }
    }

    fn url(text: &str) -> Result<Self, PricesError> {
        let url = Url::parse(text).map_err(|_| PricesError::UnusableUrl)?;
        if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
            return Err(PricesError::UnusableUrl);
        }
        without_userinfo(&url)
            .map(Self::Url)
            .ok_or(PricesError::UnusableUrl)
    }
}

/// `url` with its username and password removed; `None` for a URL that
/// cannot carry userinfo at all (no host).
fn without_userinfo(url: &Url) -> Option<Url> {
    let mut stripped = url.clone();
    stripped.set_username("").ok()?;
    stripped.set_password(None).ok()?;
    Some(stripped)
}

/// `url` with userinfo, query and fragment removed, sanitized for a log
/// line or an error message. `None` for a URL that is not http(s).
pub fn display_url(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let mut stripped = without_userinfo(url)?;
    stripped.set_query(None);
    stripped.set_fragment(None);
    Some(sanitize_for_log(stripped.as_str()))
}

/// The refresh client: no default headers, no cookie store, redirects off.
/// Built fresh for every refresh and shared with nothing.
pub fn build_client(timeouts: Timeouts) -> Result<reqwest::Client, PricesError> {
    reqwest::Client::builder()
        .connect_timeout(timeouts.connect)
        .timeout(timeouts.total)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| PricesError::Client)
}

/// `routectl prices refresh`: refresh into the shared config folder, dated
/// today (UTC).
pub async fn run(from: Option<&str>) -> Result<RefreshSummary, PricesError> {
    let config_dir = routectl_core::config_dir::config_dir_or_relative();
    refresh_into(&config_dir, from, today()?).await
}

fn today() -> Result<TableDate, PricesError> {
    let text = chrono::Utc::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string();
    TableDate::parse(&text).ok_or(PricesError::Date)
}

/// Acquire, convert, validate and write: the whole refresh against
/// `config_dir`. Nothing is written unless the source converts into a valid
/// table, and only `<config_dir>/prices.json` (through a temp file beside
/// it) is ever created.
pub async fn refresh_into(
    config_dir: &Path,
    from: Option<&str>,
    generated_at: TableDate,
) -> Result<RefreshSummary, PricesError> {
    let raw = match Source::parse(from)? {
        Source::Url(url) => {
            let client = build_client(TIMEOUTS)?;
            fetch(&client, &url, MAX_INPUT_BYTES).await?
        }
        Source::File(path) => read_file(&path, MAX_INPUT_BYTES)?,
    };
    let table = routectl_router::prices::from_models_dev(&raw, generated_at)?;
    let bytes = table.to_canonical_json()?;
    write_atomic(&config_dir.join(PRICES_FILE), &bytes)?;
    let summary = RefreshSummary::of(&table);
    tracing::info!(
        providers = summary.providers,
        models = summary.models,
        generated_at = %summary.generated_at,
        "prices refresh: table written"
    );
    Ok(summary)
}

/// GET `url` without its userinfo and return the body, refusing more than
/// `limit` bytes.
pub async fn fetch(
    client: &reqwest::Client,
    url: &Url,
    limit: usize,
) -> Result<Vec<u8>, PricesError> {
    let shown = display_url(url).ok_or(PricesError::UnusableUrl)?;
    let url = &without_userinfo(url).ok_or(PricesError::UnusableUrl)?;
    tracing::info!(url = %shown, "prices refresh: fetching");
    let response = client
        .get(url.clone())
        .send()
        .await
        .map_err(|e| PricesError::Request {
            url: shown.clone(),
            cause: request_cause(&e),
        })?;
    let status = response.status();
    if status.is_redirection() {
        return Err(PricesError::Redirect {
            url: shown,
            status: status.as_u16(),
            location: redirect_origin(url, &response),
        });
    }
    if !status.is_success() {
        return Err(PricesError::Status {
            url: shown,
            status: status.as_u16(),
        });
    }
    if response
        .content_length()
        .is_some_and(|len| len > limit as u64)
    {
        return Err(PricesError::TooLarge {
            limit,
            check: SizeCheck::Declared,
        });
    }
    read_capped_body(response, &shown, limit).await
}

fn request_cause(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        "timed out"
    } else if err.is_connect() {
        "could not connect"
    } else {
        "request failed"
    }
}

/// The `scheme://host[:port]` of a redirect's `Location` resolved against
/// `base`; `None` when absent or not an http(s) URL with a host. The path
/// and query are chosen by the server and are never rendered.
fn redirect_origin(base: &Url, response: &reqwest::Response) -> Option<String> {
    let raw = response
        .headers()
        .get(reqwest::header::LOCATION)?
        .to_str()
        .ok()?;
    let target = base.join(raw).ok()?;
    if !matches!(target.scheme(), "http" | "https") || target.host().is_none() {
        return None;
    }
    Some(sanitize_for_log(&target.origin().ascii_serialization()))
}

async fn read_capped_body(
    mut response: reqwest::Response,
    shown: &str,
    limit: usize,
) -> Result<Vec<u8>, PricesError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| PricesError::Request {
        url: shown.to_owned(),
        cause: request_cause(&e),
    })? {
        if body.len() + chunk.len() > limit {
            return Err(PricesError::TooLarge {
                limit,
                check: SizeCheck::Read,
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Read `path`, refusing a file whose metadata or content exceeds `limit`.
pub fn read_file(path: &Path, limit: usize) -> Result<Vec<u8>, PricesError> {
    let read_err = |source| PricesError::ReadFile {
        path: path.display().to_string(),
        source,
    };
    tracing::info!(path = %sanitize_for_log(&path.display().to_string()), "prices refresh: reading file");
    let file = std::fs::File::open(path).map_err(read_err)?;
    if file.metadata().map_err(read_err)?.len() > limit as u64 {
        return Err(PricesError::TooLarge {
            limit,
            check: SizeCheck::Declared,
        });
    }
    read_bounded(file, limit).map_err(|e| match e {
        BoundedRead::TooLarge => PricesError::TooLarge {
            limit,
            check: SizeCheck::Read,
        },
        BoundedRead::Io(source) => read_err(source),
    })
}

/// Why [`read_bounded`] failed.
#[derive(Debug)]
pub enum BoundedRead {
    /// More than the limit was available.
    TooLarge,
    /// The reader failed.
    Io(std::io::Error),
}

/// Read at most `limit + 1` bytes from `reader`; more than `limit` is
/// [`BoundedRead::TooLarge`]. A source whose metadata understates its
/// content (a device, a pipe, a file growing under the read) stops here.
pub fn read_bounded(reader: impl Read, limit: usize) -> Result<Vec<u8>, BoundedRead> {
    let mut out = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut out)
        .map_err(BoundedRead::Io)?;
    if out.len() > limit {
        return Err(BoundedRead::TooLarge);
    }
    Ok(out)
}

/// Replace `path` with `bytes` through routectl's shared owner-only atomic
/// writer: a temp file in the same directory, fsync, rename over `path`, then
/// a best-effort directory fsync. The parent directory is created when
/// missing and held at `0o700`, like routectl's other config-folder writers.
/// A failure before the rename leaves any previous file untouched and removes
/// the temp file. On Unix, a directory fsync that fails after the rename logs
/// a WARN and still succeeds: the new table is in place, only its durability
/// across a crash is unconfirmed.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), PricesError> {
    write_then_sync_parent(path, bytes, |dir| {
        std::fs::File::open(dir).and_then(|d| d.sync_all())
    })
}

/// [`write_atomic`] with the directory fsync it observes supplied by the
/// caller.
fn write_then_sync_parent(
    path: &Path,
    bytes: &[u8],
    sync_dir: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<(), PricesError> {
    routectl_auth::atomic_write::write_0600_atomic_with_policy(
        path,
        bytes,
        routectl_auth::atomic_write::FsyncPolicy::BestEffort,
    )
    .map_err(|reason| PricesError::Write {
        path: path.display().to_string(),
        stage: "atomic write",
        source: std::io::Error::other(reason),
    })?;
    // The shared writer discards its best-effort directory fsync result, so
    // the directory is synced again here to observe it. A Windows directory
    // handle cannot be flushed this way, so there the attempt always fails.
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        warn_on_parent_sync_failure(sync_dir(parent));
    }
    #[cfg(not(unix))]
    let _ = sync_dir;
    Ok(())
}

/// WARN with the error kind alone (no path) when the directory fsync after a
/// table write failed.
#[cfg(unix)]
fn warn_on_parent_sync_failure(result: std::io::Result<()>) {
    if let Err(e) = result {
        tracing::warn!(
            io_error = ?e.kind(),
            "price table written but its directory fsync failed; the write may not survive a crash"
        );
    }
}

#[cfg(test)]
#[path = "prices_tests.rs"]
mod tests;
