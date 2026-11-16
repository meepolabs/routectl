//! Provider descriptor: the compile-time facts about each wire format and
//! each routectl-managed OAuth vendor.
//!
//! Two const tables and their lookups, nothing else:
//!
//! - [`FORMAT_ROWS`]: per [`Format`], the stored family token (the value the
//!   usage ledger, catalog, calibration lanes and price lookup key on) and,
//!   per [`Carrier`], the [`EgressKind`] the lane sends with plus the
//!   [`Auth`] values it accepts. A carrier with no [`Route`] is not allowed
//!   for that format in this build.
//! - [`VENDOR_ROWS`]: per managed OAuth vendor, the format its token speaks,
//!   the base URL a writer emits for it, the exact first-party hosts the
//!   token may reach, and whether generated client identity applies.
//!
//! Two predicates read the vendor rows: [`containment`] (may this entry
//! carry a managed vendor token at all) and [`client_identity`] (does this
//! entry present the vendor's generated client identity). Both are pure and
//! read no credential.
//!
//! Provider construction, tuning defaults, catalog rows and documentation
//! are not owned here.

use crate::config::ProviderEntry;
use routectl_auth::{CredentialRef, SecretRef};
use routectl_core::provider_kind::config as kind;
use routectl_core::wire_check::EgressKind;
use serde::{Deserialize, Serialize};

/// The wire format a provider entry speaks. Each format maps to exactly one
/// stored family token (the value [`ProviderEntry::kind_str`] returns);
/// several formats may share a token.
///
/// [`ProviderEntry::kind_str`]: crate::ProviderEntry::kind_str
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Format {
    /// Anthropic Messages API.
    AnthropicMessages,
    /// OpenAI-compatible Chat Completions.
    OpenaiChat,
    /// OpenAI Responses API.
    OpenaiResponses,
    /// AWS Bedrock InvokeModel.
    BedrockInvoke,
    /// AWS Bedrock Converse.
    BedrockConverse,
    /// Gemini REST `generateContent` with an API key.
    Gemini,
    /// Gemini through the Cloud Code surface with an OAuth bearer.
    GeminiCloudCode,
}

impl Format {
    /// The value an operator writes as `format = "..."`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic-messages",
            Self::OpenaiChat => "openai-chat",
            Self::OpenaiResponses => "openai-responses",
            Self::BedrockInvoke => "bedrock-invoke",
            Self::BedrockConverse => "bedrock-converse",
            Self::Gemini => "gemini",
            Self::GeminiCloudCode => "gemini-cloud-code",
        }
    }
}

/// How a request authenticates on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Auth {
    /// `x-api-key: <key>` header.
    XApiKey,
    /// `Authorization: Bearer <token>` header.
    Bearer,
    /// `x-goog-api-key: <key>` header.
    XGoogApiKey,
    /// AWS SigV4 request signing.
    AwsSigv4,
}

impl Auth {
    /// The value an operator writes as `auth = "..."`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::XApiKey => "x-api-key",
            Self::Bearer => "bearer",
            Self::XGoogApiKey => "x-goog-api-key",
            Self::AwsSigv4 => "aws-sigv4",
        }
    }
}

/// Where a provider entry's requests are sent: an explicit base URL, or a
/// cloud whose endpoint is derived from its region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Carrier {
    /// An explicit http(s) base URL.
    BaseUrl,
    /// AWS, endpoint derived from the region.
    CloudAws,
}

/// One allowed (format, carrier) combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    /// The wire surface requests on this route egress through.
    pub egress: EgressKind,
    /// The auth values this route accepts. Never empty.
    pub auths: &'static [Auth],
}

/// The descriptor row for one [`Format`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatRow {
    /// The format this row describes.
    pub format: Format,
    /// The stored family token. Frozen: persisted ledger rows, calibration
    /// lanes and price lookups key on it.
    pub token: &'static str,
    /// The route over [`Carrier::BaseUrl`], or `None` when not allowed.
    pub base_url: Option<Route>,
    /// The route over [`Carrier::CloudAws`], or `None` when not allowed.
    pub cloud_aws: Option<Route>,
}

impl FormatRow {
    /// The route over `carrier`, or `None` when this format does not allow
    /// that carrier in this build.
    #[must_use]
    pub const fn route(&self, carrier: Carrier) -> Option<Route> {
        match carrier {
            Carrier::BaseUrl => self.base_url,
            Carrier::CloudAws => self.cloud_aws,
        }
    }
}

/// The descriptor row for one routectl-managed OAuth vendor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VendorRow {
    /// The provider id in `oauth://<vendor>[#label]`.
    pub vendor: &'static str,
    /// The format the vendor's token is used with.
    pub format: Format,
    /// The base URL a config writer emits for this vendor.
    pub default_base: &'static str,
    /// The only hosts (exact, https) the vendor's token may reach.
    pub hosts: &'static [&'static str],
    /// The `auth` value the vendor's token is sent with on its lane.
    pub auth: Auth,
    /// Whether generated client identity applies on the vendor's own lane.
    pub identity: bool,
}

const fn route(egress: EgressKind, auths: &'static [Auth]) -> Option<Route> {
    Some(Route { egress, auths })
}

const MANTLE_AUTHS: &[Auth] = &[Auth::Bearer, Auth::AwsSigv4];

const ANTHROPIC_MESSAGES_CLOUD: Option<Route> = if cfg!(feature = "bedrock") {
    route(EgressKind::AnthropicApiMantle, MANTLE_AUTHS)
} else {
    None
};

const OPENAI_CHAT_CLOUD: Option<Route> = if cfg!(feature = "bedrock") {
    route(EgressKind::OpenaiCompatMantle, MANTLE_AUTHS)
} else {
    None
};

const OPENAI_RESPONSES_URL: Option<Route> = if cfg!(feature = "openai-responses") {
    route(EgressKind::OpenaiResponses, &[Auth::Bearer])
} else {
    None
};

const OPENAI_RESPONSES_CLOUD: Option<Route> =
    if cfg!(all(feature = "openai-responses", feature = "bedrock")) {
        route(EgressKind::OpenaiResponsesMantle, MANTLE_AUTHS)
    } else {
        None
    };

const BEDROCK_INVOKE_CLOUD: Option<Route> = if cfg!(feature = "bedrock") {
    route(EgressKind::BedrockInvoke, MANTLE_AUTHS)
} else {
    None
};

const BEDROCK_CONVERSE_CLOUD: Option<Route> = if cfg!(feature = "bedrock") {
    route(EgressKind::BedrockConverse, MANTLE_AUTHS)
} else {
    None
};

const GEMINI_URL: Option<Route> = if cfg!(feature = "gemini") {
    route(EgressKind::Gemini, &[Auth::XGoogApiKey])
} else {
    None
};

const GEMINI_CLOUD_CODE_URL: Option<Route> = if cfg!(feature = "gemini") {
    route(EgressKind::Gemini, &[Auth::Bearer])
} else {
    None
};

/// One row per [`Format`], in declaration order. Every format keeps its row
/// (and so its family token) in every build; a format whose provider is not
/// compiled in has no routes.
pub const FORMAT_ROWS: &[FormatRow] = &[
    FormatRow {
        format: Format::AnthropicMessages,
        token: kind::ANTHROPIC_API,
        base_url: route(EgressKind::AnthropicApi, &[Auth::XApiKey, Auth::Bearer]),
        cloud_aws: ANTHROPIC_MESSAGES_CLOUD,
    },
    FormatRow {
        format: Format::OpenaiChat,
        token: kind::OPENAI_COMPAT,
        base_url: route(EgressKind::OpenaiCompat, &[Auth::Bearer]),
        cloud_aws: OPENAI_CHAT_CLOUD,
    },
    FormatRow {
        format: Format::OpenaiResponses,
        token: kind::OPENAI_RESPONSES,
        base_url: OPENAI_RESPONSES_URL,
        cloud_aws: OPENAI_RESPONSES_CLOUD,
    },
    FormatRow {
        format: Format::BedrockInvoke,
        token: kind::BEDROCK,
        base_url: None,
        cloud_aws: BEDROCK_INVOKE_CLOUD,
    },
    FormatRow {
        format: Format::BedrockConverse,
        token: kind::BEDROCK,
        base_url: None,
        cloud_aws: BEDROCK_CONVERSE_CLOUD,
    },
    FormatRow {
        format: Format::Gemini,
        token: kind::GEMINI,
        base_url: GEMINI_URL,
        cloud_aws: None,
    },
    FormatRow {
        format: Format::GeminiCloudCode,
        token: kind::GEMINI,
        base_url: GEMINI_CLOUD_CODE_URL,
        cloud_aws: None,
    },
];

/// One row per routectl-managed OAuth vendor, in the order
/// `routectl_auth::oauth::known_provider_ids()` lists them. Present in every
/// build so an id always maps to its family token, whether or not the
/// vendor's format is compiled in.
pub const VENDOR_ROWS: &[VendorRow] = &[
    VendorRow {
        vendor: "anthropic",
        format: Format::AnthropicMessages,
        default_base: "https://api.anthropic.com",
        hosts: &["api.anthropic.com"],
        auth: Auth::Bearer,
        identity: true,
    },
    VendorRow {
        vendor: "codex",
        format: Format::OpenaiResponses,
        default_base: "https://chatgpt.com/backend-api/codex",
        hosts: &["chatgpt.com"],
        auth: Auth::Bearer,
        identity: true,
    },
    VendorRow {
        vendor: "xai",
        format: Format::OpenaiChat,
        default_base: "https://api.x.ai/v1",
        hosts: &["api.x.ai"],
        auth: Auth::Bearer,
        identity: false,
    },
    VendorRow {
        vendor: "antigravity",
        format: Format::GeminiCloudCode,
        default_base: "https://daily-cloudcode-pa.googleapis.com",
        hosts: &[
            "cloudcode-pa.googleapis.com",
            "daily-cloudcode-pa.googleapis.com",
        ],
        auth: Auth::Bearer,
        identity: true,
    },
];

/// The row for `format`. Total: [`FORMAT_ROWS`] holds a row for every
/// [`Format`] in every build.
#[must_use]
pub const fn format_row(format: Format) -> &'static FormatRow {
    let mut i = 0;
    while i < FORMAT_ROWS.len() {
        if FORMAT_ROWS[i].format as u8 == format as u8 {
            return &FORMAT_ROWS[i];
        }
        i += 1;
    }
    panic!("FORMAT_ROWS holds a row for every Format");
}

/// The row for the managed OAuth vendor `vendor`, or `None` for an id
/// routectl does not manage.
#[must_use]
pub fn vendor_row(vendor: &str) -> Option<&'static VendorRow> {
    VENDOR_ROWS.iter().find(|row| row.vendor == vendor)
}

/// Text every containment refusal carries, whatever the vendor.
pub const CONTAINMENT_REFUSAL_MARKER: &str = "the managed ";

/// Refuse `entry` when any of its credential reference slots carries a
/// routectl-managed vendor token (`oauth://<vendor>[#label]`) and the entry
/// is not that vendor's own lane: the vendor's format over a `base_url`, on
/// one of the vendor's [`hosts`](VendorRow::hosts) exactly, over https,
/// with no userinfo and no explicit port, sent with the vendor's
/// [`auth`](VendorRow::auth). Static `env://` / `file://` credentials and
/// unmanaged ids are unaffected.
///
/// Pure: reads no credential. Every build path runs it (through the provider
/// plan) before the first store read for the entry, and config validation
/// runs it for every entry. The error names the provider and the vendor's
/// rule only.
pub fn containment(name: &str, entry: &ProviderEntry) -> routectl_core::Result<()> {
    VENDOR_ROWS
        .iter()
        .try_for_each(|row| vendor_containment(row, name, entry))
}

/// [`containment`] for one vendor row.
pub fn vendor_containment(
    row: &VendorRow,
    name: &str,
    entry: &ProviderEntry,
) -> routectl_core::Result<()> {
    if is_contained_by(row, entry) {
        return Ok(());
    }
    Err(routectl_core::Error::Config(format!(
        "provider `{name}`: {}",
        containment_refusal(row)
    )))
}

/// The containment rule for `row`, rendered from the row itself with the
/// entry's own key names. It names the vendor, the format, the auth and the
/// allowed hosts only: the configured base URL, the credential and the seat
/// label are withheld because each can carry credentials or identify a seat.
fn containment_refusal(row: &VendorRow) -> String {
    let hosts = row
        .hosts
        .iter()
        .map(|host| format!("https://{host}"))
        .collect::<Vec<_>>()
        .join(" or ");
    format!(
        "{CONTAINMENT_REFUSAL_MARKER}`oauth://{vendor}` credential is accepted only with \
         format = \"{format}\", auth = \"{auth}\" and base_url on {hosts} (no cloud, no \
         userinfo, no port). Elsewhere use a static env:// or file:// credential; no \
         override. The base_url and credential are withheld",
        vendor = row.vendor,
        format = row.format.as_str(),
        auth = row.auth.as_str(),
    )
}

/// True when `row`'s token is absent from every reference slot of the
/// entry, or the entry is that vendor's own lane.
fn is_contained_by(row: &VendorRow, entry: &ProviderEntry) -> bool {
    !carries_vendor_ref(entry, row.vendor) || is_on_vendor_lane(row, entry)
}

/// True when the entry presents a managed vendor's generated client
/// identity: its `credential` is `oauth://<vendor>[#label]`, the vendor row
/// has [`identity`](VendorRow::identity), and the entry is that vendor's own
/// lane (format, auth, base URL carrier, first-party host). Any other
/// credential on the same host, any other host for the same credential, and
/// any entry [`containment`] refuses (another slot carrying a second
/// vendor's token) answers `false`.
#[must_use]
pub fn client_identity(entry: &ProviderEntry) -> bool {
    let is_contained = VENDOR_ROWS.iter().all(|row| is_contained_by(row, entry));
    is_contained
        && VENDOR_ROWS.iter().any(|row| {
            row.identity && is_on_vendor_lane(row, entry) && is_vendor_login(entry, row.vendor)
        })
}

/// True when the entry's credential is routectl's stored login for `vendor`
/// (`oauth://<vendor>[#label]`), not a static key that happens to target the
/// vendor's host.
fn is_vendor_login(entry: &ProviderEntry, vendor: &str) -> bool {
    managed_vendor_of(&entry.credential) == Some(vendor)
}

/// The vendor id of an `oauth://<vendor>[#label]` reference, or `None` for
/// any other reference (or one that does not parse).
pub fn managed_vendor_of(uri: &str) -> Option<&'static str> {
    match CredentialRef::parse(uri) {
        Ok(CredentialRef::Secret(SecretRef::OAuth { provider, .. })) => {
            vendor_row(&provider).map(|row| row.vendor)
        }
        _ => None,
    }
}

/// True when any credential reference slot of the entry (`credential`,
/// `account_id_ref`) parses to an `oauth://<vendor>` reference, bare or
/// labeled. Every slot counts: an account-id slot resolves through the same
/// store and would carry the same token.
fn carries_vendor_ref(entry: &ProviderEntry, vendor: &str) -> bool {
    std::iter::once(entry.credential.as_str())
        .chain(entry.account_id_ref.as_deref())
        .any(|uri| managed_vendor_of(uri) == Some(vendor))
}

/// True when the entry is the one lane `row`'s token may ride: the vendor's
/// format, sent with the vendor's auth, over a base URL on one of the
/// vendor's hosts.
fn is_on_vendor_lane(row: &VendorRow, entry: &ProviderEntry) -> bool {
    entry.format == row.format
        && entry.auth == row.auth
        && entry.cloud.is_none()
        && entry
            .base_url
            .as_deref()
            .is_some_and(|base| is_exact_https_host_in(base, row.hosts))
}

/// True when `base_url` is on one of `vendor`'s first-party hosts (exact,
/// https, no userinfo, no port). `false` for a vendor routectl does not
/// manage.
pub fn is_vendor_host(vendor: &str, base_url: &str) -> bool {
    vendor_row(vendor).is_some_and(|row| is_exact_https_host_in(base_url, row.hosts))
}

/// True when `base_url` is an `https` URL with no userinfo, no explicit
/// non-default port, and a host exactly equal (ASCII case-insensitive) to
/// one of `hosts`. Parsed with the same WHATWG URL parser the egress uses,
/// so a backslash or `@` smuggle resolves to the host the request would
/// actually reach. Invalid URLs and URLs with no host return `false`.
fn is_exact_https_host_in(base_url: &str, hosts: &[&str]) -> bool {
    let Ok(url) = url::Url::parse(base_url) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && hosts
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(host))
}

#[cfg(test)]
#[path = "provider_descriptor_tests.rs"]
mod tests;
