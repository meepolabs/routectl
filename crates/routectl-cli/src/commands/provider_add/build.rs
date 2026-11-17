//! Provider entry construction from args (format / forwarded / managed login).

use routectl_auth::{SecretRef, env_ref};
use routectl_core::{Error, Result};
use routectl_router::{Auth, Format, ProviderEntry};

use super::capture::{PendingSecret, capture_from_stdin, ref_class, resolve_interactive};
use super::{AUTH_VALUES, AddIo, FORMAT_VALUES, LOGIN_VALUES, ProviderAddArgs};

/// The formats this command builds over a base URL. The Bedrock formats ride
/// a cloud carrier and `gemini-cloud-code` takes only a managed login, so
/// neither is reachable through `--format` here.
const SUPPORTED_FORMATS: [Format; 4] = [
    Format::AnthropicMessages,
    Format::OpenaiChat,
    Format::OpenaiResponses,
    Format::Gemini,
];

/// The `auth` values a base-URL entry can carry.
const SUPPORTED_AUTHS: [Auth; 3] = [Auth::XApiKey, Auth::Bearer, Auth::XGoogApiKey];

/// What the args ask this command to build.
enum Target {
    /// An entry of `format` whose credential is resolved from the secret flags.
    Format(Format),
    /// The entry consuming a managed login of this vendor.
    Login(&'static str),
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

/// The `auth` written when `--auth` is not given: what the format's own API
/// expects for an API key over a base URL. A forwarded credential is always
/// sent as a bearer.
pub(super) const fn default_auth(format: Format, forwarded: bool) -> Auth {
    if forwarded {
        return Auth::Bearer;
    }
    match format {
        Format::AnthropicMessages => Auth::XApiKey,
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
    let forwarded = wants_forwarded(args)?;
    let format = match target(args)? {
        Target::Login(vendor) => return build_login(args, vendor),
        Target::Format(format) => format,
    };
    let auth = match args.auth.as_deref() {
        Some(value) => parse_auth(value)?,
        None => default_auth(format, forwarded),
    };
    let shape = entry_over_base_url(format, args.base_url.as_deref())?.with_auth(auth);
    if forwarded {
        return build_forwarded(args, shape);
    }
    let (ref_str, cred_class, pending) = resolve_secret(args, shape.kind_str(), io)?;
    Ok((shape.with_credential(ref_str), cred_class, pending))
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
        other => Err(Error::Config(format!(
            "format `{}` cannot be added with this command",
            other.as_str()
        ))),
    }
}

/// Finish a `credential = "forwarded"` entry: it carries NO secret, and the
/// shared gate pins it to `anthropic-messages` on the Anthropic API host
/// with `auth = "bearer"`. Forwarded never mixes with a secret-source flag.
fn build_forwarded(
    args: &ProviderAddArgs,
    shape: ProviderEntry,
) -> Result<(ProviderEntry, &'static str, PendingSecret)> {
    if shape.format() != Format::AnthropicMessages {
        return Err(Error::Config(format!(
            "`--credential-source forwarded` is only valid for `--format anthropic-messages` \
             (got `{}`)",
            shape.format().as_str()
        )));
    }
    if has_secret_flag(args) {
        return Err(Error::Config(
            "`--credential-source forwarded` captures no credential; drop the \
             `--api-key-env` / `--secret-ref` / `--api-key-stdin` flag"
                .into(),
        ));
    }
    Ok((
        shape.with_credential("forwarded"),
        "forwarded",
        PendingSecret::None,
    ))
}

/// Build the entry consuming a managed login of `vendor` and defer the
/// login. The credential is `oauth://<vendor>`; no key is captured to the
/// file store, and the login runs post-confirm via `execute_pending`.
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
    let credential = SecretRef::OAuth {
        provider: vendor.to_string(),
        label: None,
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
        },
    ))
}

const fn has_secret_flag(args: &ProviderAddArgs) -> bool {
    args.api_key_env.is_some() || args.secret_ref.is_some() || args.api_key_stdin
}

/// Resolve the api-key secret source into the ref STRING that lands in
/// `credential`, its scheme CLASS, and any deferred capture. `--api-key-env
/// VAR` verifies the var resolves now and yields `env://VAR`; `--secret-ref
/// REF` validates the ref parses and writes it back verbatim (so a
/// `file://` ref is preserved exactly, never round-tripped through the
/// redacting `Display`; a `literal:` ref is rejected at parse); `--api-key-stdin`
/// captures the piped value to the managed store; with no flag on a TTY, an
/// already-resolvable conventional env var for the `family` token is
/// OFFERED, else a hidden prompt captures the value. A missing key with no
/// TTY errors actionably rather than hanging.
pub(super) fn resolve_secret(
    args: &ProviderAddArgs,
    family: &str,
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
        let sref = SecretRef::parse(reference)?;
        return Ok((reference.to_string(), ref_class(&sref), PendingSecret::None));
    }
    if args.api_key_stdin {
        return capture_from_stdin(args, io);
    }
    resolve_interactive(args, family, io)
}
