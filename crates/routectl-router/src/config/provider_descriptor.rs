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
//! Provider construction, tuning defaults, catalog rows and documentation
//! are not owned here.

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

/// Where a provider entry's requests are sent: an explicit base URL, or a
/// cloud whose endpoint is derived from its region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Carrier {
    /// An explicit http(s) base URL.
    BaseUrl,
    /// AWS, endpoint derived from the region.
    #[cfg_attr(not(feature = "bedrock"), allow(dead_code))]
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
        identity: true,
    },
    VendorRow {
        vendor: "codex",
        format: Format::OpenaiResponses,
        default_base: "https://chatgpt.com/backend-api/codex",
        hosts: &["chatgpt.com"],
        identity: true,
    },
    VendorRow {
        vendor: "xai",
        format: Format::OpenaiChat,
        default_base: "https://api.x.ai/v1",
        hosts: &["api.x.ai"],
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

#[cfg(test)]
#[path = "provider_descriptor_tests.rs"]
mod tests;
