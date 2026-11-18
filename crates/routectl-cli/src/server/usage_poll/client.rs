//! The usage poll's HTTP client.
//!
//! The poll sends a seat's own OAuth bearer (and, for Codex, its ChatGPT
//! account id) to that vendor's first-party usage endpoint, so the client is
//! fenced in:
//!
//! - the only destinations are [`ANTHROPIC_USAGE_URL`] and
//!   [`CODEX_USAGE_URL`], compile-time constants; nothing takes a URL;
//! - redirects are never followed: every 3xx is [`FetchResult::Redirect`];
//! - `https` only, TLS 1.2 floor, 5 s connect and 10 s total timeouts;
//! - the body is read in chunks and abandoned past
//!   [`MAX_USAGE_BODY_BYTES`];
//! - the outcome is a closed [`FetchResult`] that carries no error, header
//!   or credential, and whose `Debug` / `Display` never print the body.
//!
//! Proxy handling follows every other vendor egress client: system proxy
//! discovery stays on for these non-loopback hosts. The request is TLS to
//! the vendor host end to end, so a proxy only ever sees a CONNECT tunnel.

use std::fmt;
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, HeaderValue, USER_AGENT};
use reqwest::{Client, ClientBuilder, RequestBuilder, Response};
use routectl_auth::oauth::SecretToken;
use routectl_core::identity::{anthropic as anthropic_identity, codex as codex_identity};
use routectl_providers::parse_retry_after;
use routectl_router::MAX_USAGE_BODY_BYTES;

/// The Anthropic OAuth usage endpoint.
pub const ANTHROPIC_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

/// The Codex (ChatGPT) usage endpoint.
pub const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(10);

const CHATGPT_ACCOUNT_ID_HEADER: &str = "chatgpt-account-id";

/// The longest `Retry-After` wait a 429 is reported with. The value comes
/// from the vendor and is turned into a deadline, so it is bounded here.
pub const MAX_USAGE_RETRY_AFTER: Duration = Duration::from_hours(1);

/// One seat's credential, naming the vendor whose endpoint it is sent to.
pub enum SeatCredential<'a> {
    /// An Anthropic managed login's access token.
    Anthropic { token: &'a SecretToken },
    /// A Codex managed login's access token and, when the login recorded
    /// one, its ChatGPT account id.
    Codex {
        token: &'a SecretToken,
        account_id: Option<&'a str>,
    },
}

/// The closed outcome of one usage fetch.
#[derive(Clone, PartialEq, Eq)]
pub enum FetchResult {
    /// A 200 whose whole body fit in [`MAX_USAGE_BODY_BYTES`].
    Ok(Vec<u8>),
    /// Any 3xx. The `Location` is never followed or kept.
    Redirect,
    /// 401 or 403.
    Auth,
    /// Any other status that is not 200, 3xx, 401, 403 or 429.
    Http(u16),
    /// The connect or total timeout elapsed.
    Timeout,
    /// Any other failure to connect, send or read.
    Transport,
    /// The body was longer than [`MAX_USAGE_BODY_BYTES`].
    TooLarge,
    /// 429, with the parsed `Retry-After` wait when one was sent, capped at
    /// [`MAX_USAGE_RETRY_AFTER`].
    RateLimited(Option<Duration>),
}

impl FetchResult {
    /// A stable lowercase token naming the outcome kind.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Ok(_) => "ok",
            Self::Redirect => "redirect",
            Self::Auth => "auth",
            Self::Http(_) => "http",
            Self::Timeout => "timeout",
            Self::Transport => "transport",
            Self::TooLarge => "too_large",
            Self::RateLimited(_) => "rate_limited",
        }
    }
}

impl fmt::Debug for FetchResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok(body) => write!(f, "Ok(<{} bytes>)", body.len()),
            Self::Http(status) => write!(f, "Http({status})"),
            Self::RateLimited(wait) => write!(f, "RateLimited({wait:?})"),
            Self::Redirect => f.write_str("Redirect"),
            Self::Auth => f.write_str("Auth"),
            Self::Timeout => f.write_str("Timeout"),
            Self::Transport => f.write_str("Transport"),
            Self::TooLarge => f.write_str("TooLarge"),
        }
    }
}

impl fmt::Display for FetchResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(status) => write!(f, "http {status}"),
            other => f.write_str(other.as_str()),
        }
    }
}

/// The dedicated client for vendor usage endpoints.
#[derive(Debug, Clone)]
pub struct UsagePollClient {
    client: Client,
}

impl UsagePollClient {
    /// Build the production client. Fails only when no TLS backend can be
    /// initialized.
    pub fn new() -> reqwest::Result<Self> {
        Ok(Self {
            client: base_builder().build()?,
        })
    }

    /// Fetch the usage endpoint of `credential`'s vendor with it.
    pub async fn fetch(&self, credential: SeatCredential<'_>) -> FetchResult {
        match self.request(credential).send().await {
            Ok(response) => classify_response(response).await,
            Err(err) => transport_failure(&err),
        }
    }

    fn request(&self, credential: SeatCredential<'_>) -> RequestBuilder {
        match credential {
            SeatCredential::Anthropic { token } => self
                .client
                .get(ANTHROPIC_USAGE_URL)
                .header(AUTHORIZATION, bearer(token))
                .header(
                    USER_AGENT,
                    anthropic_identity::default_claude_code_user_agent(),
                )
                .header("anthropic-beta", anthropic_identity::OAUTH_ANTHROPIC_BETA),
            SeatCredential::Codex { token, account_id } => {
                let mut builder = self
                    .client
                    .get(CODEX_USAGE_URL)
                    .header(AUTHORIZATION, bearer(token))
                    .header(USER_AGENT, codex_identity::codex_user_agent());
                for (name, value) in codex_identity::default_identity_headers() {
                    builder = builder.header(name, value);
                }
                if let Some(account_id) = account_id {
                    builder = builder.header(CHATGPT_ACCOUNT_ID_HEADER, sensitive(account_id));
                }
                builder
            }
        }
    }
}

fn base_builder() -> ClientBuilder {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .https_only(true)
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(TOTAL_TIMEOUT)
}

/// A header value reqwest redacts from its own `Debug` output. A value that
/// is not a valid header is sent as an empty one, which the vendor refuses
/// as `Auth`, rather than failing the build with an error that could carry it.
fn sensitive(value: &str) -> HeaderValue {
    let mut header = HeaderValue::from_str(value).unwrap_or_else(|_| HeaderValue::from_static(""));
    header.set_sensitive(true);
    header
}

fn bearer(token: &SecretToken) -> HeaderValue {
    sensitive(&format!("Bearer {}", token.expose()))
}

async fn classify_response(response: Response) -> FetchResult {
    match response.status().as_u16() {
        200 => read_capped(response).await,
        300..=399 => FetchResult::Redirect,
        401 | 403 => FetchResult::Auth,
        429 => FetchResult::RateLimited(
            parse_retry_after(response.headers()).map(|wait| wait.min(MAX_USAGE_RETRY_AFTER)),
        ),
        status => FetchResult::Http(status),
    }
}

async fn read_capped(mut response: Response) -> FetchResult {
    let limit = MAX_USAGE_BODY_BYTES;
    if response
        .content_length()
        .is_some_and(|announced| announced > limit as u64)
    {
        return FetchResult::TooLarge;
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len() + chunk.len() > limit {
                    return FetchResult::TooLarge;
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => return FetchResult::Ok(body),
            Err(err) => return transport_failure(&err),
        }
    }
}

fn transport_failure(err: &reqwest::Error) -> FetchResult {
    if err.is_timeout() {
        FetchResult::Timeout
    } else {
        FetchResult::Transport
    }
}

#[cfg(test)]
impl UsagePollClient {
    /// The production client with each named host resolved to its TLS
    /// front instead of DNS, the front's root trusted, and no proxy. The
    /// URLs, scheme, host, redirect policy and timeouts are unchanged.
    pub(super) fn with_fronts_for_tests(
        fronts: &[(&str, &routectl_testkit::tls_front::TlsFront)],
    ) -> Self {
        let mut builder = base_builder().no_proxy();
        for (host, front) in fronts {
            let root = reqwest::Certificate::from_pem(&front.ca_pem).expect("a PEM root");
            builder = builder.resolve(host, front.addr).add_root_certificate(root);
        }
        Self {
            client: builder.build().expect("test usage client"),
        }
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
