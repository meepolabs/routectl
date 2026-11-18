//! Provider entry construction from args (format / forwarded / managed login).

use routectl_auth::{CredentialRef, SecretRef, env_ref};
use routectl_core::{Error, Result};
use routectl_router::{Auth, Format, ProviderEntry};

use super::capture::{PendingSecret, capture_from_stdin, ref_class, resolve_interactive};
use super::{AUTH_VALUES, AddIo, FORMAT_VALUES, LOGIN_VALUES, ProviderAddArgs};
use crate::commands::seat::validate_label;

/// The formats this command builds. `gemini-cloud-code` takes only a
/// managed login, so it is not reachable through `--format`.
const SUPPORTED_FORMATS: [Format; 6] = [
    Format::AnthropicMessages,
    Format::OpenaiChat,
    Format::OpenaiResponses,
    Format::BedrockInvoke,
    Format::BedrockConverse,
    Format::Gemini,
];

/// The `auth` values an entry this command builds can carry.
const SUPPORTED_AUTHS: [Auth; 4] = [
    Auth::XApiKey,
    Auth::Bearer,
    Auth::XGoogApiKey,
    Auth::AwsSigv4,
];

/// What the args ask this command to build.
enum Target {
    /// An entry of `format` whose credential is resolved from the secret flags.
    Format(Format),
    /// The entry consuming a managed login of this vendor.
    Login(&'static str),
}

/// Where the entry's requests go: `--base-url` (or the format's default
/// base), or the AWS cloud in `--region`.
enum Carrier<'a> {
    BaseUrl(Option<&'a str>),
    Aws { region: &'a str },
}

/// Resolve `--base-url` / `--region` into the one [`Carrier`] they name.
fn carrier(args: &ProviderAddArgs) -> Result<Carrier<'_>> {
    match (args.base_url.as_deref(), args.region.as_deref()) {
        (Some(_), Some(_)) => Err(Error::Config(
            "pass only one of `--base-url <URL>` or `--region <REGION>`".into(),
        )),
        (base_url, None) => Ok(Carrier::BaseUrl(base_url)),
        (None, Some(region)) => Ok(Carrier::Aws { region }),
    }
}

/// Whether `--credential-source forwarded` was requested. `own` (or the
/// unset default) yields `false`; an unrecognized value errors (the clap
/// layer already constrains the flag, but the library entry point does
/// not).
fn wants_forwarded(args: &ProviderAddArgs) -> Result<bool> {
    match args.credential_source.as_deref() {
        None | Some("own") => Ok(false),
        Some("forwarded") => Ok(true),
        Some(other) => Err(Error::Config(format!(
            "unknown `--credential-source` `{other}`; expected `own` or `forwarded`"
        ))),
    }
}

/// Resolve `--format` / `--login` into the one [`Target`] they name.
fn target(args: &ProviderAddArgs) -> Result<Target> {
    match (args.format.as_deref(), args.login.as_deref()) {
        (Some(format), None) => parse_format(format).map(Target::Format),
        (None, Some(vendor)) => parse_login(vendor).map(Target::Login),
        (Some(_), Some(_)) => Err(Error::Config(
            "pass only one of `--format <FORMAT>` or `--login <VENDOR>`".into(),
        )),
        (None, None) => Err(Error::Config(
            "pass `--format <FORMAT>` or `--login <VENDOR>`".into(),
        )),
    }
}

fn parse_format(value: &str) -> Result<Format> {
    SUPPORTED_FORMATS
        .into_iter()
        .find(|format| format.as_str() == value)
        .ok_or_else(|| {
            Error::Config(format!(
                "format `{value}` cannot be added with this command; supported formats: {}",
                FORMAT_VALUES.join(", ")
            ))
        })
}

fn parse_login(value: &str) -> Result<&'static str> {
    LOGIN_VALUES
        .iter()
        .copied()
        .find(|vendor| *vendor == value)
        .ok_or_else(|| {
            Error::Config(format!(
                "`--login {value}` is not supported; supported logins: {}",
                LOGIN_VALUES.join(", ")
            ))
        })
}

fn parse_auth(value: &str) -> Result<Auth> {
    SUPPORTED_AUTHS
        .into_iter()
        .find(|auth| auth.as_str() == value)
        .ok_or_else(|| {
            Error::Config(format!(
                "unknown `--auth` `{value}`; expected one of: {}",
                AUTH_VALUES.join(", ")
            ))
        })
}

/// The `auth` written when `--auth` is not given: an `aws://` credential
/// signs with SigV4, a forwarded credential is a bearer, and otherwise the
/// format's own key header (`x-api-key` for Anthropic Messages over a base
/// URL, `x-goog-api-key` for Gemini), else a bearer.
fn default_auth(format: Format, over_cloud: bool, credential: &str) -> Auth {
    match CredentialRef::parse(credential) {
        Ok(CredentialRef::AwsProfile(_) | CredentialRef::AwsChain) => return Auth::AwsSigv4,
        Ok(CredentialRef::Forwarded) => return Auth::Bearer,
        _ => {}
    }
    match format {
        Format::AnthropicMessages if !over_cloud => Auth::XApiKey,
        Format::Gemini => Auth::XGoogApiKey,
        _ => Auth::Bearer,
    }
}

/// Validate the args and resolve the secret source into a [`ProviderEntry`]
/// plus its credential CLASS label (scheme only) and any [`PendingSecret`]
/// side effect owed after the confirm. Errors actionably -- and never hangs
/// -- when a key-requiring entry has no usable secret source.
pub(super) fn build_entry(
    args: &ProviderAddArgs,
    io: &dyn AddIo,
) -> Result<(ProviderEntry, &'static str, PendingSecret)> {
    if args.label.is_some() && args.login.is_none() {
        return Err(Error::Config(
            "`--label` names a login seat; it is only valid with `--login <VENDOR>`".into(),
        ));
    }
    if args.pool.is_some() && args.login.is_none() {
        return Err(Error::Config(
            "`--pool` adds a login seat to a pool; it is only valid with `--login <VENDOR>`".into(),
        ));
    }
    let forwarded = wants_forwarded(args)?;
    let format = match target(args)? {
        Target::Login(vendor) => return build_login(args, vendor),
        Target::Format(format) => format,
    };
    let explicit_auth = args.auth.as_deref().map(parse_auth).transpose()?;
    let shape = match carrier(args)? {
        Carrier::BaseUrl(base_url) => entry_over_base_url(format, base_url)?,
        Carrier::Aws { region } => entry_over_aws(format, region)?,
    };
    let (credential, cred_class, pending) = if forwarded {
        forwarded_credential(args, &shape)?
    } else {
        let family = shape.cloud.is_none().then(|| shape.kind_str());
        resolve_secret(args, format, family, io)?
    };
    let auth =
        explicit_auth.unwrap_or_else(|| default_auth(format, shape.cloud.is_some(), &credential));
    Ok((
        shape.with_credential(credential).with_auth(auth),
        cred_class,
        pending,
    ))
}

/// The entry of `format` on the AWS cloud in `region` with no credential
/// yet: Bedrock for the `bedrock-*` formats, Bedrock's managed surface for
/// the Messages, Chat and Responses formats.
fn entry_over_aws(format: Format, region: &str) -> Result<ProviderEntry> {
    if matches!(format, Format::Gemini | Format::GeminiCloudCode) {
        return Err(Error::Config(format!(
            "`--format {}` has no AWS cloud form; `--region` is not supported for this format",
            format.as_str()
        )));
    }
    Ok(ProviderEntry::cloud_aws(
        format,
        region,
        String::new(),
        Auth::Bearer,
    ))
}

/// The entry of `format` over `base_url` with no credential yet. The
/// Anthropic API and the public Gemini REST base are the defaults for their
/// formats; the other formats need `--base-url`.
fn entry_over_base_url(format: Format, base_url: Option<&str>) -> Result<ProviderEntry> {
    let required = || {
        base_url.ok_or_else(|| {
            Error::Config(format!(
                "`--format {}` requires `--base-url <URL>`",
                format.as_str()
            ))
        })
    };
    match format {
        Format::AnthropicMessages => {
            let entry = ProviderEntry::anthropic_api(String::new());
            Ok(match base_url {
                Some(url) => entry.with_base_url(url),
                None => entry,
            })
        }
        Format::OpenaiChat => Ok(ProviderEntry::openai_compat(required()?, String::new())),
        Format::OpenaiResponses => {
            Ok(ProviderEntry::openai_responses(String::new()).with_base_url(required()?))
        }
        Format::Gemini => {
            if base_url.is_some() {
                return Err(Error::Config(
                    "`--format gemini` uses its built-in base URL; `--base-url` is not \
                     supported for this format"
                        .into(),
                ));
            }
            Ok(ProviderEntry::gemini(String::new()))
        }
        Format::BedrockInvoke | Format::BedrockConverse => Err(Error::Config(format!(
            "`--format {}` runs on the AWS cloud; pass `--region <REGION>` instead of \
             `--base-url`",
            format.as_str()
        ))),
        other => Err(Error::Config(format!(
            "format `{}` cannot be added with this command",
            other.as_str()
        ))),
    }
}

/// The `credential = "forwarded"` source: it carries NO secret, and the
/// shared gate pins it to `anthropic-messages` on the Anthropic API host
/// with `auth = "bearer"`. Forwarded never mixes with a secret-source flag
/// or the AWS cloud.
fn forwarded_credential(
    args: &ProviderAddArgs,
    shape: &ProviderEntry,
) -> Result<(String, &'static str, PendingSecret)> {
    if shape.format() != Format::AnthropicMessages {
        return Err(Error::Config(format!(
            "`--credential-source forwarded` is only valid for `--format anthropic-messages` \
             (got `{}`)",
            shape.format().as_str()
        )));
    }
    if shape.cloud.is_some() {
        return Err(Error::Config(
            "`--credential-source forwarded` relays to the Anthropic API; `--region` is not \
             supported with it"
                .into(),
        ));
    }
    if has_secret_flag(args) {
        return Err(Error::Config(
            "`--credential-source forwarded` captures no credential; drop the \
             `--api-key-env` / `--secret-ref` / `--api-key-stdin` flag"
                .into(),
        ));
    }
    Ok(("forwarded".to_string(), "forwarded", PendingSecret::None))
}

/// Build the entry consuming a managed login of `vendor` and defer the
/// login. The credential is `oauth://<vendor>`, or `oauth://<vendor>#<label>`
/// for a `--label` seat; no key is captured to the file store, and the login
/// runs post-confirm via `execute_pending`.
fn build_login(
    args: &ProviderAddArgs,
    vendor: &'static str,
) -> Result<(ProviderEntry, &'static str, PendingSecret)> {
    if has_secret_flag(args) {
        return Err(Error::Config(format!(
            "`--login {vendor}` authenticates via oauth; drop the `--api-key-env` / \
             `--secret-ref` / `--api-key-stdin` flag"
        )));
    }
    let refused_flag = [
        (args.base_url.is_some(), "--base-url"),
        (args.region.is_some(), "--region"),
        (args.auth.is_some(), "--auth"),
        (args.credential_source.is_some(), "--credential-source"),
    ]
    .into_iter()
    .find_map(|(given, flag)| given.then_some(flag));
    if let Some(flag) = refused_flag {
        return Err(Error::Config(format!(
            "`--login {vendor}` writes the vendor's own endpoint and auth; `{flag}` is not \
             supported with it"
        )));
    }
    let label = validate_label(args.label.as_deref())?.map(str::to_string);
    let credential = SecretRef::OAuth {
        provider: vendor.to_string(),
        label: label.clone(),
    }
    .to_string();
    let entry = ProviderEntry::for_managed_login(vendor, credential).ok_or_else(|| {
        Error::Config(format!("`--login {vendor}` has no managed provider entry"))
    })?;
    Ok((
        entry,
        "oauth",
        PendingSecret::OAuth {
            provider: vendor.to_string(),
            label,
        },
    ))
}

const fn has_secret_flag(args: &ProviderAddArgs) -> bool {
    args.api_key_env.is_some() || args.secret_ref.is_some() || args.api_key_stdin
}

/// Resolve the api-key secret source into the ref STRING that lands in
/// `credential`, its scheme CLASS, and any deferred capture. `--api-key-env
/// VAR` verifies the var resolves now and yields `env://VAR`; `--secret-ref
/// REF` validates the ref parses as a credential (`env://`, `file://`,
/// `oauth://`, `aws://profile/<name>`, `aws://chain`) and writes it back
/// verbatim (so a `file://` ref is preserved exactly, never round-tripped
/// through the redacting `Display`; a `literal:` ref is rejected at parse,
/// and `forwarded` is `--credential-source forwarded`'s); `--api-key-stdin`
/// captures the piped value to the managed store; with no flag on a TTY, an
/// already-resolvable conventional env var for the `family` token (if any) is
/// OFFERED, else a hidden prompt captures the value. A missing key with no
/// TTY errors actionably, naming `format`, rather than hanging.
pub(super) fn resolve_secret(
    args: &ProviderAddArgs,
    format: Format,
    family: Option<&str>,
    io: &dyn AddIo,
) -> Result<(String, &'static str, PendingSecret)> {
    if args.api_key_env.is_some() && args.secret_ref.is_some() {
        return Err(Error::Config(
            "provide only one of `--api-key-env` or `--secret-ref`".into(),
        ));
    }
    if let Some(var) = args.api_key_env.as_deref() {
        let sref = env_ref(var)?;
        return Ok((sref.to_string(), ref_class(&sref), PendingSecret::None));
    }
    if let Some(reference) = args.secret_ref.as_deref() {
        let credential = CredentialRef::parse(reference)?;
        if credential == CredentialRef::Forwarded {
            return Err(Error::Config(
                "`--secret-ref` names a stored credential; use `--credential-source forwarded` \
                 for a forwarded one"
                    .into(),
            ));
        }
        return Ok((
            reference.to_string(),
            credential.scheme_label(),
            PendingSecret::None,
        ));
    }
    if args.api_key_stdin {
        return capture_from_stdin(args, io);
    }
    resolve_interactive(args, format, family, io)
}
