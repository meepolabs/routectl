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
    /// An Anthropic managed login's access token and its configured
    /// `User-Agent` (see [`usage_user_agent`]); `None` sends the compiled
    /// Claude Code default.
    Anthropic {
        token: &'a SecretToken,
        user_agent: Option<HeaderValue>,
    },
    /// A Codex managed login's access token, its ChatGPT account id when
    /// the login recorded one, and its configured `User-Agent`; `None`
    /// sends the compiled Codex default.
    Codex {
        token: &'a SecretToken,
        account_id: Option<&'a str>,
        user_agent: Option<HeaderValue>,
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
        let builder = base_builder();
        #[cfg(test)]
        let builder = build_failure::apply(builder);
        Ok(Self {
            client: builder.build()?,
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
            SeatCredential::Anthropic { token, user_agent } => {
                let builder = self
                    .client
                    .get(ANTHROPIC_USAGE_URL)
                    .header(AUTHORIZATION, bearer(token));
                match user_agent {
                    Some(configured) => builder.header(USER_AGENT, configured),
                    None => builder.header(
                        USER_AGENT,
                        anthropic_identity::default_claude_code_user_agent(),
                    ),
                }
                .header("anthropic-beta", anthropic_identity::OAUTH_ANTHROPIC_BETA)
            }
            SeatCredential::Codex {
                token,
                account_id,
                user_agent,
            } => {
                let builder = self
                    .client
                    .get(CODEX_USAGE_URL)
                    .header(AUTHORIZATION, bearer(token));
                let mut builder = match user_agent {
                    Some(configured) => builder.header(USER_AGENT, configured),
                    None => builder.header(USER_AGENT, codex_identity::codex_user_agent()),
                };
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

/// A seat's configured `User-Agent` for its usage call, and whether a
/// configured value was set aside. `Debug` never prints the value.
#[derive(Clone, PartialEq, Eq)]
pub struct UsageUserAgent {
    /// The configured value to send; `None` sends the vendor's compiled
    /// default.
    pub configured: Option<HeaderValue>,
    /// A configured value was given but is not a valid header value, so the
    /// compiled default is sent in its place.
    pub configured_invalid: bool,
}

impl fmt::Debug for UsageUserAgent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UsageUserAgent")
            .field("configured_invalid", &self.configured_invalid)
            .finish_non_exhaustive()
    }
}

/// The `User-Agent` override for a usage call: the seat's configured
/// `user_agent` when it is a valid header value (parsed as reqwest parses
/// the request path's client-level override), else none, so the vendor's
/// compiled default is sent.
pub fn usage_user_agent(configured: Option<&str>) -> UsageUserAgent {
    match configured.map(HeaderValue::from_str) {
        None => UsageUserAgent {
            configured: None,
            configured_invalid: false,
        },
        Some(Ok(value)) => UsageUserAgent {
            configured: Some(value),
            configured_invalid: false,
        },
        Some(Err(_)) => UsageUserAgent {
            configured: None,
            configured_invalid: true,
        },
    }
}

fn base_builder() -> ClientBuilder {
    let builder = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .https_only(true)
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(TOTAL_TIMEOUT);
    #[cfg(any(test, feature = "test-utils"))]
    let builder = network_fence::apply(builder);
    builder
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

/// Keeps every test build's poll client off the network.
///
/// A test that boots a daemon with a managed login, a stored token and a
/// model on it would otherwise poll the vendor with that token once the seat
/// came due. In a unit test (`cfg(test)`) or an integration test (the
/// `test-utils` feature, which only this crate's own dev-dependency turns
/// on), every client this module builds resolves no host name and uses no
/// proxy, so it cannot open a connection to any vendor; a name pinned with
/// `ClientBuilder::resolve` (the TLS-front test client) still resolves. A
/// release build has neither, so this module does not exist there.
#[cfg(any(test, feature = "test-utils"))]
pub mod network_fence {
    use std::sync::{Arc, Mutex};

    use reqwest::ClientBuilder;
    use reqwest::dns::{Name, Resolve, Resolving};

    use super::{SeatCredential, UsagePollClient};

    /// Every host name a fenced client was asked to resolve, process-wide.
    static REFUSED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    struct RefuseEveryName;

    impl Resolve for RefuseEveryName {
        fn resolve(&self, name: Name) -> Resolving {
            REFUSED
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(name.as_str().to_owned());
            Box::pin(std::future::ready(Err(
                "the usage poll resolves no host name in a test build".into(),
            )))
        }
    }

    pub(super) fn apply(builder: ClientBuilder) -> ClientBuilder {
        builder.no_proxy().dns_resolver(Arc::new(RefuseEveryName))
    }

    /// What one fetch through the production client did.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct FenceProbe {
        /// The fetch outcome token for the Anthropic and then the Codex seat.
        pub outcomes: Vec<&'static str>,
        /// Every host name any fenced client was refused, process-wide.
        pub refused_hosts: Vec<String>,
    }

    /// Fetch both vendors' usage through the client the daemon builds, with
    /// a synthetic token, and report what happened.
    pub async fn probe_production_client() -> FenceProbe {
        let client = UsagePollClient::new().expect("the production client builds");
        let token = routectl_auth::oauth::SecretToken::new("fence-probe-token");
        let anthropic = client
            .fetch(SeatCredential::Anthropic {
                token: &token,
                user_agent: None,
            })
            .await;
        let codex = client
            .fetch(SeatCredential::Codex {
                token: &token,
                account_id: Some("fence-probe-account"),
                user_agent: None,
            })
            .await;
        FenceProbe {
            outcomes: vec![anthropic.as_str(), codex.as_str()],
            refused_hosts: REFUSED
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        }
    }
}

/// Makes [`UsagePollClient::new`] fail on this thread while a guard is held,
/// through reqwest's own builder error rather than a substitute one.
#[cfg(test)]
pub(in crate::server) mod build_failure {
    use std::cell::Cell;

    use reqwest::ClientBuilder;

    thread_local! {
        static ARMED: Cell<bool> = const { Cell::new(false) };
    }

    /// Fails every client build on this thread until dropped.
    #[must_use = "dropping the guard disarms the failure immediately"]
    pub struct Armed(());

    pub fn arm() -> Armed {
        ARMED.with(|armed| armed.set(true));
        Armed(())
    }

    impl Drop for Armed {
        fn drop(&mut self) {
            ARMED.with(|armed| armed.set(false));
        }
    }

    /// A ceiling below the client's TLS 1.2 floor leaves no version to
    /// offer, which reqwest refuses at build time.
    pub(super) fn apply(builder: ClientBuilder) -> ClientBuilder {
        if ARMED.with(Cell::get) {
            builder.max_tls_version(reqwest::tls::Version::TLS_1_0)
        } else {
            builder
        }
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
