//! The one decision taken about a provider entry before any of its
//! credentials is read.

use crate::config::entry_shape::{carrier_refusal, shape_refusals};
use crate::config::provider_descriptor::{
    Auth, Carrier, Format, client_identity, containment, format_row, is_vendor_host,
    managed_vendor_of,
};
use crate::config::{Cloud, ProviderEntry};
use routectl_auth::{CredentialRef, SecretRef};
use routectl_core::wire_check::EgressKind;
use routectl_core::{Error, Result};

/// What the factory decided about one provider entry, from the entry alone.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProviderPlan {
    /// The parsed `credential`.
    pub credential: CredentialRef,
    /// Where the entry's requests go.
    pub destination: Destination,
    /// The wire surface the entry egresses through.
    pub egress: EgressKind,
    /// Whether the provider presents its vendor's generated client identity.
    pub client_identity: bool,
}

/// Where a planned entry's requests go: its configured base URL, or the AWS
/// region its endpoint is derived from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// The configured `base_url`.
    BaseUrl(String),
    /// The `cloud = { name = "aws", region }` carrier.
    Aws {
        /// The configured region.
        region: String,
    },
}

/// The format that may carry the client's own forwarded credential.
const FORWARDED_FORMAT: Format = Format::AnthropicMessages;

/// The only managed login the Cloud Code surface accepts.
const CLOUD_CODE_VENDOR: &str = "antigravity";

/// The managed vendor whose host rule the forwarded credential obeys.
const FORWARDED_HOST_VENDOR: &str = "anthropic";

/// Plan the build of `entry` (configured as provider `name`) without reading
/// any credential.
///
/// Checks, in order, stopping at the first refusal: the credential parses;
/// exactly one of `base_url` or `cloud` is set; the descriptor has a route
/// for the format over that carrier with that auth; `aws-sigv4` is used iff
/// the credential is an `aws://` form; `forwarded` only on
/// `anthropic-messages` to the Anthropic API host with `bearer`;
/// `gemini-cloud-code` only with a managed `oauth://antigravity` login;
/// managed-login containment over every reference slot; then the entry's
/// shape (`base_url` a plain http(s) URL, no key its format does not take).
/// Then it computes the client-identity verdict.
///
/// Every build path calls this first and passes the plan down: no
/// `SecretStore` read (token, probe, account id, session id, project id) and
/// no AWS credential resolution happens for an entry whose plan was refused.
/// Errors name the provider and the keys involved, never the base URL,
/// credential or seat label.
pub fn plan_provider(name: &str, entry: &ProviderEntry) -> Result<ProviderPlan> {
    let plan = plan_coherent_entry(name, entry)?;
    if let Some(refusal) = shape_refusals(name, entry).into_iter().next() {
        return Err(Error::Config(refusal));
    }
    Ok(plan)
}

/// [`plan_provider`] without the entry-shape rules past the carrier. Only
/// for a caller that reports every [`shape_refusals`] message itself.
pub(super) fn plan_coherent_entry(name: &str, entry: &ProviderEntry) -> Result<ProviderPlan> {
    let credential = parse_credential(name, &entry.credential)?;
    let destination = destination(name, entry)?;
    let egress = route(name, entry)?;
    check_sigv4_pairing(name, entry.auth, &credential)?;
    check_forwarded_pin(name, entry, &credential, &destination)?;
    check_cloud_code_login(name, entry)?;
    containment(name, entry)?;
    Ok(ProviderPlan {
        credential,
        destination,
        egress,
        client_identity: client_identity(entry),
    })
}

fn refuse(name: &str, rule: &str) -> Error {
    Error::Config(format!("provider `{name}`: {rule}"))
}

/// The parsed credential; the parse error names the scheme family only.
fn parse_credential(name: &str, credential: &str) -> Result<CredentialRef> {
    CredentialRef::parse(credential).map_err(|e| {
        let detail = match e {
            Error::Auth(msg) => msg,
            other => other.to_string(),
        };
        refuse(name, &format!("`credential` is not usable: {detail}"))
    })
}

fn destination(name: &str, entry: &ProviderEntry) -> Result<Destination> {
    match (&entry.base_url, &entry.cloud) {
        (Some(base), None) => Ok(Destination::BaseUrl(base.clone())),
        (None, Some(Cloud::Aws { region })) => Ok(Destination::Aws {
            region: region.clone(),
        }),
        _ => Err(Error::Config(carrier_refusal(name, entry))),
    }
}

/// The descriptor's route for the entry's format over its carrier, refused
/// when the carrier or the auth is not one the format accepts in this build.
fn route(name: &str, entry: &ProviderEntry) -> Result<EgressKind> {
    let format = entry.format();
    let carrier = entry.carrier();
    let Some(route) = format_row(format).route(carrier) else {
        return Err(refuse(
            name,
            &format!(
                "format = \"{}\" is not available over {} in this build",
                format.as_str(),
                carrier_key(carrier)
            ),
        ));
    };
    if !route.auths.contains(&entry.auth) {
        let accepted: Vec<String> = route
            .auths
            .iter()
            .map(|auth| format!("\"{}\"", auth.as_str()))
            .collect();
        return Err(refuse(
            name,
            &format!(
                "auth = \"{}\" is not accepted for format = \"{}\" over {}; use {}",
                entry.auth.as_str(),
                format.as_str(),
                carrier_key(carrier),
                accepted.join(" or ")
            ),
        ));
    }
    Ok(route.egress)
}

const fn carrier_key(carrier: Carrier) -> &'static str {
    match carrier {
        Carrier::BaseUrl => "`base_url`",
        Carrier::CloudAws => "`cloud` aws",
    }
}

fn check_sigv4_pairing(name: &str, auth: Auth, credential: &CredentialRef) -> Result<()> {
    let is_aws = matches!(
        credential,
        CredentialRef::AwsProfile(_) | CredentialRef::AwsChain
    );
    match (auth == Auth::AwsSigv4, is_aws) {
        (true, false) => Err(refuse(
            name,
            "auth = \"aws-sigv4\" needs credential = \"aws://profile/<name>\" or \"aws://chain\"",
        )),
        (false, true) => Err(refuse(
            name,
            "an aws:// credential signs requests; set auth = \"aws-sigv4\"",
        )),
        _ => Ok(()),
    }
}

/// The client's forwarded credential is a full-scope claude.ai bearer: it may
/// only go to the Anthropic API host, over https with no userinfo and no
/// port (the managed Anthropic login's own host rule), on the Anthropic
/// format, as a bearer.
fn check_forwarded_pin(
    name: &str,
    entry: &ProviderEntry,
    credential: &CredentialRef,
    destination: &Destination,
) -> Result<()> {
    if !matches!(credential, CredentialRef::Forwarded) {
        return Ok(());
    }
    let on_anthropic_host = matches!(destination, Destination::BaseUrl(base) if is_vendor_host(FORWARDED_HOST_VENDOR, base));
    let is_pinned = entry.format() == FORWARDED_FORMAT && entry.auth == Auth::Bearer;
    if on_anthropic_host && is_pinned {
        return Ok(());
    }
    Err(refuse(
        name,
        "credential = \"forwarded\" is accepted only with format = \"anthropic-messages\", \
         auth = \"bearer\" and an https base_url whose host is exactly api.anthropic.com, \
         with no userinfo and no port. The forwarded credential is the client's full-scope \
         claude.ai bearer and never leaves that host. The configured base_url is withheld",
    ))
}

/// The Cloud Code surface speaks only with a managed Antigravity login.
fn check_cloud_code_login(name: &str, entry: &ProviderEntry) -> Result<()> {
    if entry.format() != Format::GeminiCloudCode
        || managed_vendor_of(&entry.credential) == Some(CLOUD_CODE_VENDOR)
    {
        return Ok(());
    }
    Err(refuse(
        name,
        "format = \"gemini-cloud-code\" needs credential = \"oauth://antigravity[#label]\"; \
         run `routectl login antigravity`",
    ))
}

impl ProviderPlan {
    /// The credential as a store reference, or `None` for the AWS forms and
    /// `forwarded`, which no store resolves.
    #[must_use]
    pub const fn secret_ref(&self) -> Option<&SecretRef> {
        match &self.credential {
            CredentialRef::Secret(secret_ref) => Some(secret_ref),
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "plan_identity_wire_tests.rs"]
mod identity_wire_tests;

#[cfg(all(
    test,
    feature = "bedrock",
    feature = "openai-responses",
    feature = "gemini"
))]
#[path = "plan_retired_guidance_tests.rs"]
mod retired_guidance_tests;
