//! Interactive/stdin secret capture + pending-secret execution.

use routectl_auth::{ManagedSecretStore, SecretRef, default_secret_dir, env_ref};
use routectl_core::{Error, Result};

use super::{AddIo, ProviderAddArgs};
use crate::commands::provider_env::env_var_for_kind;

/// The secret side effect deferred until AFTER the confirmation, so a
/// declined confirm writes no secret file and runs no login. The ref
/// STRING that lands in `api_key_ref` is already fixed in the built entry;
/// this only carries the bytes/login still owed.
pub(super) enum PendingSecret {
    /// A ref already final at build time (env / secret-ref / forwarded):
    /// nothing to execute post-confirm.
    None,
    /// A captured value to `put` into the already-opened managed store
    /// under `name`. The store is opened (and its base canonicalized) once
    /// during capture and carried here so the pre-confirm ref-string
    /// computation and the post-confirm `put` share ONE canonical base.
    File {
        store: ManagedSecretStore,
        name: String,
        value: String,
    },
    /// An oauth provider to log in against post-confirm.
    OAuth { provider: String },
}

impl PendingSecret {
    /// Whether this pending action performs a real side effect (a capture
    /// or a login) -- the signal that a post-capture config-write conflict
    /// must surface the explicit recovery message.
    pub(super) const fn is_side_effect(&self) -> bool {
        !matches!(self, Self::None)
    }

    /// Whether this pending action rewrites a managed secret file. A fresh
    /// file capture always rewrites it: the `file://` ref is derived from the
    /// provider name, not the value, so the serialized block can be
    /// byte-identical while the on-disk secret rotates. An identical-block
    /// re-add must therefore NOT short-circuit for a file capture (that would
    /// silently discard the new key); every other pending kind keeps the
    /// idempotent no-op.
    pub(super) const fn rewrites_secret(&self) -> bool {
        matches!(self, Self::File { .. })
    }
}

/// Perform the deferred secret side effect (a managed-store `put` or an
/// oauth login) AFTER the confirmation. The advisory config lock is not
/// held here -- the config write happens in a later, separate step.
pub(super) async fn execute_pending(pending: PendingSecret, io: &dyn AddIo) -> Result<()> {
    match pending {
        PendingSecret::None => Ok(()),
        PendingSecret::File { store, name, value } => {
            store.put(&name, &value)?;
            Ok(())
        }
        PendingSecret::OAuth { provider } => io.login(&provider).await,
    }
}

/// Open the managed secret store at the default directory, canonicalizing
/// its base ONCE. The opened store is carried through
/// [`PendingSecret::File`] so the pre-confirm ref-string computation and
/// the post-confirm `put` share the SAME canonical base -- a symlinked
/// ancestor swapped between the two phases cannot redirect where a managed
/// secret lands.
fn open_secret_store() -> Result<ManagedSecretStore> {
    let dir = default_secret_dir()?;
    Ok(ManagedSecretStore::open(dir)?)
}

/// Capture the piped `--api-key-stdin` value into the managed store. Errors
/// IMMEDIATELY (never blocks) when stdin is a TTY; the ref string is the
/// deterministic managed-store path, and the bytes are `put` only later,
/// post-confirm.
pub(super) fn capture_from_stdin(
    args: &ProviderAddArgs,
    io: &dyn AddIo,
) -> Result<(String, &'static str, PendingSecret)> {
    if io.stdin_is_terminal() {
        return Err(Error::Config(
            "`--api-key-stdin` reads a piped key from stdin, but stdin is a TTY; \
             pipe the key in (e.g. `printf %s \"$KEY\" | routectl provider add ... \
             --api-key-stdin`) or use `--api-key-env`/`--secret-ref`"
                .into(),
        ));
    }
    let value = clean_key(&io.read_stdin()?);
    capture_value(args, value)
}

/// Interactive resolution for a missing key: on a TTY, offer an
/// already-resolvable conventional env var, else prompt hidden and capture;
/// off a TTY, error actionably rather than hang.
pub(super) fn resolve_interactive(
    args: &ProviderAddArgs,
    io: &dyn AddIo,
) -> Result<(String, &'static str, PendingSecret)> {
    if !io.stdin_is_terminal() {
        return Err(Error::Config(format!(
            "provider kind `{}` needs a credential and stdin is not a TTY; pass \
             `--api-key-env <VAR>`, `--secret-ref <REF>`, or pipe one with \
             `--api-key-stdin`",
            args.kind
        )));
    }
    if let Some(var) = env_var_for_kind(&args.kind)
        && let Ok(sref) = env_ref(var)
        && io.confirm_env_offer(var)
    {
        return Ok((sref.to_string(), ref_class(&sref), PendingSecret::None));
    }
    let value = clean_key(&io.prompt_hidden(&args.name)?);
    capture_value(args, value)
}

/// Turn a validated captured `value` into a managed `file://` ref string
/// plus the deferred `put`. The ref path is computed deterministically
/// (no bytes written yet); the store is opened ONCE here so an unwritable /
/// wrong-perms store fails EARLY, before the confirm, and the opened store
/// is carried into the pending so the post-confirm `put` reuses the same
/// canonical base.
pub(super) fn capture_value(
    args: &ProviderAddArgs,
    value: String,
) -> Result<(String, &'static str, PendingSecret)> {
    if value.is_empty() {
        return Err(Error::Config(
            "the provided API key is empty; nothing was captured".into(),
        ));
    }
    let store = open_secret_store()?;
    let ref_str = SecretRef::File(store.ref_path(&args.name)).to_string();
    Ok((
        ref_str,
        "file",
        PendingSecret::File {
            store,
            name: args.name.clone(),
            value,
        },
    ))
}

/// Strip a single trailing newline (`\n` or `\r\n`) from a captured key so
/// a piped `echo` value matches a `printf` one; the resolver trims on read,
/// so interior/leading bytes are preserved as given.
fn clean_key(raw: &str) -> String {
    raw.strip_suffix('\n')
        .map_or(raw, |s| s.strip_suffix('\r').unwrap_or(s))
        .to_string()
}

/// Scheme/class label for a secret ref -- what the audit event records in
/// place of the value or the full ref string.
pub(super) const fn ref_class(sref: &SecretRef) -> &'static str {
    match sref {
        SecretRef::Env(_) => "env",
        SecretRef::File(_) => "file",
        SecretRef::Literal(_) => "literal",
        SecretRef::OAuth { .. } => "oauth",
        _ => "unknown",
    }
}
