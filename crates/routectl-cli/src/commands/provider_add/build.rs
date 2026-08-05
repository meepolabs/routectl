//! Provider entry construction from args (direct / forwarded / oauth).

use routectl_auth::{SecretRef, env_ref};
use routectl_core::{Error, Result};
use routectl_providers::anthropic_api::AuthKind;
use routectl_router::ProviderEntry;
use routectl_router::config::CredentialSource;

use super::capture::{PendingSecret, capture_from_stdin, ref_class, resolve_interactive};
use super::{AddIo, ProviderAddArgs};

/// Provider kinds this flag-driven command can construct from a single
/// `api_key_ref`. Kinds needing richer inputs are out of this command's
/// non-interactive scope: Bedrock takes a multi-field credential block, and
/// OpenAI Responses defaults to OAuth; both are configured by hand or through
/// the interactive flow.
const SUPPORTED_KINDS: &[&str] = &["openai-compat", "anthropic-api", "gemini"];

/// The login provider id an oauth-backed `--kind` delegates to, or `None`
/// for an ordinary api-key kind. Hardcode-then-abstract: the login flow
/// backs `anthropic` (claude.ai subscription -> `anthropic-api` provider
/// with an `oauth://anthropic` ref and the oauth-bearer auth kind). Other
/// oauth login providers map to provider variants this command does not
/// yet construct, so they are added here as those constructors come into
/// scope rather than guessed at now.
fn oauth_provider_for_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "anthropic" => Some("anthropic"),
        _ => None,
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

/// Validate the kind and resolve the secret source into a [`ProviderEntry`]
/// plus its credential CLASS label (scheme only) and any [`PendingSecret`]
/// side effect owed after the confirm. Errors actionably -- and never hangs
/// -- when a key-requiring kind has no usable secret source.
pub(super) fn build_entry(
    args: &ProviderAddArgs,
    io: &dyn AddIo,
) -> Result<(ProviderEntry, &'static str, PendingSecret)> {
    if wants_forwarded(args)? {
        return build_forwarded(args);
    }
    if let Some(provider) = oauth_provider_for_kind(&args.kind) {
        return build_oauth(args, provider);
    }

    match args.kind.as_str() {
        "openai-compat" => {
            let base_url = args.base_url.as_deref().ok_or_else(|| {
                Error::Config("`openai-compat` requires `--base-url <URL>`".into())
            })?;
            let (ref_str, cred_class, pending) = resolve_secret(args, io)?;
            Ok((
                ProviderEntry::openai_compat(base_url, ref_str),
                cred_class,
                pending,
            ))
        }
        "anthropic-api" => {
            let (ref_str, cred_class, pending) = resolve_secret(args, io)?;
            let entry = ProviderEntry::anthropic_api(ref_str);
            let entry = match args.base_url.as_deref() {
                Some(base_url) => entry.with_base_url(base_url),
                None => entry,
            };
            Ok((entry, cred_class, pending))
        }
        "gemini" => {
            // Gemini has no public base-URL setter; its constructor pins the
            // public v1beta endpoint. A custom endpoint is a hand-edit, so a
            // `--base-url` here is rejected rather than silently ignored.
            if args.base_url.is_some() {
                return Err(Error::Config(
                    "`gemini` uses its built-in base URL; `--base-url` is not \
                     supported for this kind"
                        .into(),
                ));
            }
            let (ref_str, cred_class, pending) = resolve_secret(args, io)?;
            Ok((ProviderEntry::gemini(ref_str), cred_class, pending))
        }
        other => Err(Error::Config(format!(
            "provider kind `{other}` cannot be added with this command; \
             supported kinds: {}",
            SUPPORTED_KINDS.join(", ")
        ))),
    }
}

/// Build a `credential_source = "forwarded"` anthropic-api entry: it
/// carries NO secret (`api_key_ref` stays empty) and its base URL is pinned
/// to the Anthropic origin the shared gate requires. Forwarded is valid for
/// `anthropic-api` ONLY, and never mixes with a secret-source flag.
fn build_forwarded(args: &ProviderAddArgs) -> Result<(ProviderEntry, &'static str, PendingSecret)> {
    if args.kind != "anthropic-api" {
        return Err(Error::Config(format!(
            "`--credential-source forwarded` is only valid for `--kind anthropic-api` \
             (got `{}`)",
            args.kind
        )));
    }
    if args.api_key_env.is_some() || args.secret_ref.is_some() || args.api_key_stdin {
        return Err(Error::Config(
            "`--credential-source forwarded` captures no credential; drop the \
             `--api-key-env` / `--secret-ref` / `--api-key-stdin` flag"
                .into(),
        ));
    }
    // The constructor already pins `base_url` to https://api.anthropic.com;
    // an explicit `--base-url` (e.g. a pinned path on that host) passes
    // through and is host-checked by the shared gate.
    let entry =
        ProviderEntry::anthropic_api("").with_credential_source(CredentialSource::Forwarded);
    let entry = match args.base_url.as_deref() {
        Some(base_url) => entry.with_base_url(base_url),
        None => entry,
    };
    Ok((entry, "forwarded", PendingSecret::None))
}

/// Build an oauth-backed anthropic-api entry and defer the login. The ref
/// is `oauth://<provider>` and no key is captured to the file store; the
/// login runs post-confirm via `execute_pending`.
fn build_oauth(
    args: &ProviderAddArgs,
    provider: &'static str,
) -> Result<(ProviderEntry, &'static str, PendingSecret)> {
    if args.api_key_env.is_some() || args.secret_ref.is_some() || args.api_key_stdin {
        return Err(Error::Config(format!(
            "`--kind {}` authenticates via oauth; drop the `--api-key-env` / \
             `--secret-ref` / `--api-key-stdin` flag",
            args.kind
        )));
    }
    if args.base_url.is_some() {
        return Err(Error::Config(format!(
            "`--kind {}` uses the pinned Anthropic endpoint; `--base-url` is not \
             supported for this kind",
            args.kind
        )));
    }
    let entry = ProviderEntry::anthropic_api(format!("oauth://{provider}"))
        .with_auth_kind(AuthKind::OauthBearer);
    Ok((
        entry,
        "oauth",
        PendingSecret::OAuth {
            provider: provider.to_string(),
        },
    ))
}

/// Resolve the api-key secret source into the ref STRING that lands in
/// `api_key_ref`, its scheme CLASS, and any deferred capture. `--api-key-env
/// VAR` verifies the var resolves now and yields `env://VAR`; `--secret-ref
/// REF` validates the ref parses and writes it back verbatim (so a
/// `file://` ref is preserved exactly, never round-tripped through the
/// redacting `Display`; a `literal:` ref is rejected at parse); `--api-key-stdin`
/// captures the piped value to the managed store; with no flag on a TTY, an
/// already-resolvable conventional env var is OFFERED, else a hidden prompt
/// captures the value. A missing key with no TTY errors actionably rather
/// than hanging.
pub(super) fn resolve_secret(
    args: &ProviderAddArgs,
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
    resolve_interactive(args, io)
}
