//! `routectl provider add` -- add or overwrite a `[providers.<name>]` block
//! in `config.toml`, funneled through the ONE write path
//! ([`routectl_router::edit_config_toml`]) exactly as [`super::config_edit`]
//! does.
//!
//! Secret sources: `--secret-ref` / `--api-key-env` (flag-driven, no
//! capture), `--api-key-stdin` (piped value captured to the managed
//! `file://` store), an interactive hidden prompt (missing key + a TTY),
//! `--credential-source forwarded` (no secret at all), and oauth-backed
//! kinds (delegated to the `login` flow, ref `oauth://<provider>`).
//!
//! Every step runs IN MEMORY before any byte touches disk, so any refusal
//! (a stale/legacy version, an unknown secret scheme, a candidate that fails
//! the shared gate, an existing-block conflict, a declined confirmation)
//! leaves the file byte-identical. The lock-discipline invariant: the
//! secret file `put` (and any oauth login) happens AFTER the confirmation
//! and BEFORE the config write, and the advisory config lock is NEVER held
//! across a prompt, an oauth login, env probing, or the `put`.
//!
//!   1. RAW version + legacy preflights on the snapshot bytes
//!      ([`super::edit_pipeline::preflight`], shared with `config set`).
//!   2. Resolve the kind (validated against this command's supported set),
//!      the name, the base URL, and the secret source into a
//!      [`ProviderEntry`] plus a credential CLASS label (scheme only, never
//!      the value) and a [`PendingSecret`] (the deferred `put`/login, if
//!      any). A captured file ref's STRING is computed deterministically
//!      here via [`ManagedSecretStore::ref_path`] -- no bytes written yet.
//!   3. Serialize the entry to a standard `toml_edit` table. If a provider
//!      of the same name already exists, a byte-identical (normalized) block
//!      is an idempotent NO-OP; a different block is refused unless
//!      `--overwrite` is given.
//!   4. Gate the candidate through the SAME shared gate the reload path runs
//!      ([`super::edit_pipeline::gate`]); any failure renders and writes
//!      nothing.
//!   5. A new/overwritten provider block is always egress-defining, so it
//!      prompts for confirmation before the lock is acquired (`--yes`
//!      bypasses). A declined confirm captures NO secret and logs in NOT.
//!   6. NOW execute the pending secret side effect (file `put` / oauth
//!      login), then [`edit_config_toml`] re-reads under the advisory lock,
//!      re-applies the same deterministic insert, re-gates, and commits
//!      atomically (or writes nothing on a no-op). A post-capture write
//!      conflict emits an explicit recovery message -- the secret/login
//!      persists, the config is unchanged, re-run to complete.
//!   7. On a real write, emit exactly one audit event -- surface, verb,
//!      name, kind, credential class -- NEVER the value and NEVER the full
//!      ref string.

use std::io::{IsTerminal, Read};
use std::path::Path;

use async_trait::async_trait;
use routectl_auth::{ManagedSecretStore, SecretRef, default_secret_dir, env_ref};
use routectl_core::{Error, Result};
use routectl_providers::anthropic_api::AuthKind;
use routectl_router::config::CredentialSource;
use routectl_router::{EditOutcome, ProviderEntry, edit_config_toml, parse_config};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table};

use super::edit_pipeline::{
    RelockValidationError, confirm_high_consequence, gate, preflight, render_gate_errors,
    render_write_error,
};
use super::provider_env::env_var_for_kind;
use crate::config_classify::collect_high_consequence_changes;

/// Provider kinds this flag-driven command can construct from a single
/// `api_key_ref`. `openai-compat` / `anthropic-api` are always compiled;
/// `gemini` (api-key mode) is added when its cargo feature is on -- default
/// for the shipped binary, and cfg-gated here to mirror the router's own
/// gate on the `Gemini` variant. Kinds needing richer inputs are out of this
/// command's non-interactive scope: Bedrock takes a multi-field credential
/// block, and OpenAI Responses defaults to OAuth; both are configured by
/// hand or through the interactive flow.
#[cfg(feature = "gemini")]
const SUPPORTED_KINDS: &[&str] = &["openai-compat", "anthropic-api", "gemini"];
#[cfg(not(feature = "gemini"))]
const SUPPORTED_KINDS: &[&str] = &["openai-compat", "anthropic-api"];

/// Flag-driven inputs for `provider add`, assembled from the clap surface.
pub struct ProviderAddArgs {
    pub kind: String,
    pub name: String,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub secret_ref: Option<String>,
    pub api_key_stdin: bool,
    pub credential_source: Option<String>,
    pub overwrite: bool,
    pub yes: bool,
}

/// The interactive / side-effecting seams `provider add` touches OUTSIDE
/// the pure config-editing pipeline: reading stdin, the hidden key prompt,
/// the env-detect offer, and the oauth login delegation. Injected so the
/// pipeline stays testable without a real TTY or a live browser flow;
/// production wires [`RealAddIo`].
#[async_trait]
pub trait AddIo: Send + Sync {
    /// Whether the process stdin is a terminal. Gates the stdin-capture
    /// error path and the interactive prompt path.
    fn stdin_is_terminal(&self) -> bool;
    /// Read all of stdin to a string (the piped `--api-key-stdin` value).
    fn read_stdin(&self) -> Result<String>;
    /// Offer to use an already-resolvable env var as the credential;
    /// `true` accepts `env://VAR`, `false` falls through to the prompt.
    fn confirm_env_offer(&self, var: &str) -> bool;
    /// Prompt for the API key without echoing it (interactive path).
    fn prompt_hidden(&self, provider_name: &str) -> Result<String>;
    /// Run the oauth login flow for `provider`, persisting its tokens.
    async fn login(&self, provider: &str) -> Result<()>;
}

/// Production [`AddIo`]: std stdin + `rpassword` for the hidden prompt +
/// the existing `login` command for oauth delegation. The oauth login is
/// awaited directly on the async `main` runtime; `provider add` carries the
/// login through its own async path rather than blocking a worker thread.
pub struct RealAddIo;

#[async_trait]
impl AddIo for RealAddIo {
    fn stdin_is_terminal(&self) -> bool {
        std::io::stdin().is_terminal()
    }

    fn read_stdin(&self) -> Result<String> {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| Error::Config(format!("failed to read the API key from stdin: {e}")))?;
        Ok(buf)
    }

    fn confirm_env_offer(&self, var: &str) -> bool {
        use std::io::Write as _;
        println!("`{var}` is set in the environment and resolves now.");
        print!("use it as this provider's credential (env://{var})? [Y/n] ");
        let _ = std::io::stdout().flush();
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_err() {
            return false;
        }
        matches!(input.trim().to_ascii_lowercase().as_str(), "" | "y" | "yes")
    }

    fn prompt_hidden(&self, provider_name: &str) -> Result<String> {
        rpassword::prompt_password(format!("API key for provider `{provider_name}`: "))
            .map_err(|e| Error::Config(format!("failed to read the API key prompt: {e}")))
    }

    async fn login(&self, provider: &str) -> Result<()> {
        super::login::run(provider, false, None, None).await
    }
}

/// The secret side effect deferred until AFTER the confirmation, so a
/// declined confirm writes no secret file and runs no login. The ref
/// STRING that lands in `api_key_ref` is already fixed in the built entry;
/// this only carries the bytes/login still owed.
enum PendingSecret {
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
    const fn is_side_effect(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Outcome of a completed [`run`], for the caller and for tests. Refusals
/// (unknown kind, missing credential, gate rejection, conflict without
/// `--overwrite`, write conflict) surface as `Err` instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddResult {
    /// The provider block was written (added or overwritten).
    Written,
    /// The provider already existed with identical settings; nothing written.
    NoChange,
    /// The high-consequence confirmation was declined.
    Aborted,
}

/// Run the `provider add` pipeline against `config_path` with the
/// production I/O seams ([`RealAddIo`]). Thin wrapper over
/// [`run_with_io`].
pub async fn run(config_path: &Path, args: ProviderAddArgs) -> Result<AddResult> {
    run_with_io(config_path, args, &RealAddIo).await
}

/// Run the `provider add` pipeline against `config_path`. Prints the human
/// result line and emits the audit event on a real write; returns the
/// structured outcome for the caller/tests. Validation and refusal paths
/// render their diagnostics and return `Err`. The `io` seam supplies the
/// interactive / side-effecting steps (stdin, hidden prompt, env offer,
/// oauth login) so the pipeline is testable without a TTY or a browser.
pub async fn run_with_io(
    config_path: &Path,
    args: ProviderAddArgs,
    io: &dyn AddIo,
) -> Result<AddResult> {
    let snapshot = std::fs::read(config_path).map_err(|e| {
        Error::Config(format!(
            "cannot read config `{}`: {e}",
            config_path.display()
        ))
    })?;
    let snapshot_text = String::from_utf8(snapshot.clone()).map_err(|e| {
        Error::Config(format!(
            "config `{}` is not UTF-8: {e}",
            config_path.display()
        ))
    })?;

    preflight(&snapshot_text)?;

    if args.name.trim().is_empty() {
        return Err(Error::Config("provider name must not be empty".into()));
    }

    let (entry, cred_class, pending) = build_entry(&args, io)?;
    let name = args.name.as_str();
    // The RESOLVED kind, not the CLI-supplied `--kind`: an oauth kind
    // (`anthropic`) constructs an `anthropic-api` block, so the audit event
    // and the human line must report what actually lands on disk.
    let kind = entry.kind_str();

    let block = provider_table(&entry)?;
    let new_block_norm = block.to_string();

    let prev = parse_config(&snapshot_text).map_err(|e| {
        Error::Config(format!(
            "current config does not parse; fix it before editing: {e}"
        ))
    })?;

    if let Some(existing) = prev.providers.get(name) {
        let existing_norm = provider_table(existing)?.to_string();
        if existing_norm == new_block_norm {
            println!("provider `{name}` is already configured identically; nothing written.");
            return Ok(AddResult::NoChange);
        }
        if !args.overwrite {
            return Err(Error::Config(format!(
                "provider `{name}` already exists with different settings; \
                 pass `--overwrite` to overwrite it"
            )));
        }
    }

    let candidate_text = {
        let mut doc = parse_document(&snapshot_text)?;
        insert_provider_block(&mut doc, name, block.clone())?;
        doc.to_string()
    };
    let next = gate(&candidate_text).map_err(|errors| {
        render_gate_errors(&errors);
        Error::Config(format!("{} config error(s)", errors.len()))
    })?;

    // A new or overwritten provider block always sets base_url + credential
    // source, so it is always egress-defining; the collector names the
    // specific fields for the prompt, with a fallback should it ever be empty.
    let mut high = collect_high_consequence_changes(&prev, &next);
    if high.is_empty() {
        high.push("providers.base_url");
    }
    if !confirm_high_consequence(&high, args.yes) {
        println!("aborted; nothing written.");
        return Ok(AddResult::Aborted);
    }

    // Only NOW -- after the confirm -- does any secret file get written or
    // any login run. A declined confirm above returns before this point.
    let did_side_effect = pending.is_side_effect();
    execute_pending(pending, io).await?;

    let outcome = match commit(config_path, &snapshot, &snapshot_text, name, block) {
        Ok(outcome) => outcome,
        Err(e) => {
            if did_side_effect {
                return Err(Error::Config(format!(
                    "the credential was captured/logged in successfully, but the config was \
                     NOT changed ({e}). config.toml is unchanged; the captured credential \
                     persists -- re-run `provider add` to complete the wiring"
                )));
            }
            return Err(e);
        }
    };

    if outcome == EditOutcome::Unchanged {
        println!("provider `{name}` already present; nothing written.");
        return Ok(AddResult::NoChange);
    }

    tracing::info!(
        surface = "cli",
        verb = "provider-add",
        name,
        kind,
        credential_source = cred_class,
        "provider add committed",
    );

    println!("added provider `{name}` ({kind}).");
    Ok(AddResult::Written)
}

/// Perform the deferred secret side effect (a managed-store `put` or an
/// oauth login) AFTER the confirmation. The advisory config lock is not
/// held here -- the config write happens in a later, separate step.
async fn execute_pending(pending: PendingSecret, io: &dyn AddIo) -> Result<()> {
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
fn build_entry(
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
        #[cfg(feature = "gemini")]
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
/// login runs post-confirm via [`execute_pending`].
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
fn resolve_secret(
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

/// Capture the piped `--api-key-stdin` value into the managed store. Errors
/// IMMEDIATELY (never blocks) when stdin is a TTY; the ref string is the
/// deterministic managed-store path, and the bytes are `put` only later,
/// post-confirm.
fn capture_from_stdin(
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
fn resolve_interactive(
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
fn capture_value(
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
const fn ref_class(sref: &SecretRef) -> &'static str {
    match sref {
        SecretRef::Env(_) => "env",
        SecretRef::File(_) => "file",
        SecretRef::Literal(_) => "literal",
        SecretRef::OAuth { .. } => "oauth",
        _ => "unknown",
    }
}

/// Serialize a [`ProviderEntry`] into a standard (non-inline) `toml_edit`
/// table, dropping the empty collection defaults serde emits (an empty
/// `header_extras` map, empty `allowed_betas` list) so the written block
/// stays minimal. The re-validate gate is the backstop for anything pruned.
fn provider_table(entry: &ProviderEntry) -> Result<Table> {
    let text = toml::to_string(entry)
        .map_err(|e| Error::Config(format!("serialize provider entry: {e}")))?;
    let doc = parse_document(&text)?;
    let mut table = doc.as_table().clone();
    table.set_implicit(false);
    prune_empty_children(&mut table);
    Ok(table)
}

/// Drop top-level keys of `table` whose value is an empty table, array, or
/// inline table -- serde-emitted defaults that carry no operator intent.
fn prune_empty_children(table: &mut Table) {
    let empties: Vec<String> = table
        .iter()
        .filter(|(_, item)| is_empty_item(item))
        .map(|(k, _)| k.to_string())
        .collect();
    for key in empties {
        table.remove(&key);
    }
}

fn is_empty_item(item: &Item) -> bool {
    match item {
        Item::None => true,
        Item::Table(t) => t.is_empty(),
        Item::ArrayOfTables(a) => a.is_empty(),
        Item::Value(v) => v
            .as_array()
            .map(Array::is_empty)
            .or_else(|| v.as_inline_table().map(InlineTable::is_empty))
            .unwrap_or(false),
    }
}

fn parse_document(text: &str) -> Result<DocumentMut> {
    text.parse::<DocumentMut>()
        .map_err(|e| Error::Config(format!("config does not parse: {e}")))
}

/// Insert `block` at `[providers.<name>]`, descending into (or creating) the
/// `providers` table via `as_table_like_mut` so existing providers' comments
/// and ordering survive. A same-name insert replaces the whole block
/// (`--overwrite`). Deterministic given the same input document (the
/// write closure relies on this).
fn insert_provider_block(doc: &mut DocumentMut, name: &str, block: Table) -> Result<()> {
    let root = doc.as_table_mut();
    if !root.contains_key("providers") {
        let mut providers = Table::new();
        providers.set_implicit(true);
        root.insert("providers", Item::Table(providers));
    }
    let providers = root
        .get_mut("providers")
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| Error::Config("`providers` exists but is not a table".into()))?;
    providers.insert(name, Item::Table(block));
    Ok(())
}

/// Re-read `config_path` under the advisory lock + revision check and commit
/// the same deterministic insert atomically. The `snapshot` bytes MUST be the
/// bytes the caller read earlier; a mismatch is a stale-snapshot conflict and
/// nothing is written.
fn commit(
    config_path: &Path,
    snapshot: &[u8],
    snapshot_text: &str,
    name: &str,
    block: Table,
) -> Result<EditOutcome> {
    let result = edit_config_toml::<RelockValidationError, _>(config_path, snapshot, |doc| {
        insert_provider_block(doc, name, block).map_err(|_| RelockValidationError)?;
        let text = doc.to_string();
        if text == snapshot_text {
            return Ok(EditOutcome::Unchanged);
        }
        match gate(&text) {
            Ok(_) => Ok(EditOutcome::Modified),
            Err(_) => Err(RelockValidationError),
        }
    })
    .map_err(render_write_error)?;
    Ok(result.outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    const V3_BASE: &str = "\
version = 3

[server]
host = \"127.0.0.1\"
port = 8787

[providers.fast]
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"literal:test-key\"

[models.gpt]
provider = \"fast\"
upstream = \"gpt-4o\"

[aliases]
default = \"gpt\"
";

    fn write_config(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("config.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    fn args(kind: &str, name: &str) -> ProviderAddArgs {
        ProviderAddArgs {
            kind: kind.to_string(),
            name: name.to_string(),
            base_url: None,
            api_key_env: None,
            secret_ref: None,
            api_key_stdin: false,
            credential_source: None,
            overwrite: false,
            yes: true,
        }
    }

    fn set_env(key: &str, val: &str) {
        // SAFETY: env-touching tests are serialized via serial_test, so no
        // other thread reads or writes the process environment concurrently.
        unsafe { std::env::set_var(key, val) };
    }

    fn unset_env(key: &str) {
        // SAFETY: see set_env.
        unsafe { std::env::remove_var(key) };
    }

    // -----------------------------------------------------------------
    // Secret resolution: the ref STRING is computed without the value; a
    // `literal:` secret-ref is refused so no inline key reaches argv/config.
    // -----------------------------------------------------------------

    #[test]
    #[serial_test::serial]
    fn resolve_secret_env_yields_scheme_ref_not_the_value() {
        let key = "ROUTECTL_PROVIDER_ADD_RESOLVE_KEY";
        set_env(key, "the-actual-secret-value");

        let mut a = args("anthropic-api", "x");
        a.api_key_env = Some(key.to_string());
        let (ref_str, class, _pending) = resolve_secret(&a, &FakeIo::default()).unwrap();

        assert_eq!(ref_str, format!("env://{key}"));
        assert_eq!(class, "env");
        assert!(
            !ref_str.contains("the-actual-secret-value"),
            "the env value must never appear in the ref string"
        );
        unset_env(key);
    }

    #[test]
    fn resolve_secret_rejects_literal_ref() {
        // `--secret-ref literal:...` is refused: the inline key would land
        // on argv and be persisted in plaintext in config. The error must
        // steer to the safe paths and never echo the key value.
        let mut a = args("openai-compat", "x");
        a.secret_ref = Some("literal:keep-me-exactly".to_string());
        let err = match resolve_secret(&a, &FakeIo::default()) {
            Ok(_) => panic!("a literal: secret-ref must be rejected"),
            Err(e) => e,
        };

        let msg = err.to_string();
        assert!(
            !msg.contains("keep-me-exactly"),
            "rejection must not echo the key value: {msg}"
        );
        assert!(
            msg.contains("--api-key-stdin") && msg.contains("prompt") && msg.contains("env://"),
            "rejection must name the safe paths: {msg}"
        );
    }

    // -----------------------------------------------------------------
    // Happy path: a flag-driven openai-compat add writes a valid v3 block.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn adds_openai_compat_via_secret_ref() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);

        let mut a = args("openai-compat", "grok");
        a.base_url = Some("https://api.x.example/v1".to_string());
        a.secret_ref = Some("file:///abs/key".to_string());

        let result = run(&path, a).await.expect("add");
        assert_eq!(result, AddResult::Written);

        let text = std::fs::read_to_string(&path).unwrap();
        let config = parse_config(&text).expect("written config parses");
        let entry = config.providers.get("grok").expect("provider present");
        assert_eq!(entry.kind_str(), "openai-compat");
        assert_eq!(entry.api_key_ref(), Some("file:///abs/key"));
        assert!(text.contains("[providers.grok]"), "{text}");
        assert!(text.contains("api_key_ref = \"file:///abs/key\""), "{text}");
    }

    // -----------------------------------------------------------------
    // --api-key-env writes env://VAR; the var VALUE never appears anywhere.
    // -----------------------------------------------------------------

    #[tokio::test]
    #[serial_test::serial]
    async fn adds_via_api_key_env_without_leaking_the_value() {
        let key = "ROUTECTL_PROVIDER_ADD_TEST_KEY";
        let secret_value = "super-secret-token-value-not-real";
        set_env(key, secret_value);

        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);

        let mut a = args("anthropic-api", "claude");
        a.api_key_env = Some(key.to_string());

        let result = run(&path, a).await.expect("add");
        assert_eq!(result, AddResult::Written);

        let text = std::fs::read_to_string(&path).unwrap();
        let config = parse_config(&text).unwrap();
        let entry = config.providers.get("claude").unwrap();
        assert_eq!(entry.kind_str(), "anthropic-api");
        assert_eq!(entry.api_key_ref(), Some(format!("env://{key}").as_str()));
        assert!(
            !text.contains(secret_value),
            "the env var value must never land in the config"
        );

        unset_env(key);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn key_env_that_is_unset_errors_without_writing() {
        let key = "ROUTECTL_PROVIDER_ADD_UNSET_KEY";
        unset_env(key);

        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);
        let before = std::fs::read(&path).unwrap();

        let mut a = args("anthropic-api", "claude");
        a.api_key_env = Some(key.to_string());

        let err = run(&path, a).await.expect_err("unset env var must error");
        assert!(err.to_string().contains(key), "err: {err}");
        assert_eq!(std::fs::read(&path).unwrap(), before, "must not write");
    }

    // -----------------------------------------------------------------
    // Format preservation: comments + ordering survive the surgical insert.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn preserves_comments_and_existing_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let body = "\
# operator note
version = 3

[server]
host = \"127.0.0.1\"
port = 8787

[providers.fast]
# keep this comment
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"literal:test-key\"

[models.gpt]
provider = \"fast\"
upstream = \"gpt-4o\"

[aliases]
default = \"gpt\"
";
        let path = write_config(dir.path(), body);

        let mut a = args("openai-compat", "grok");
        a.base_url = Some("https://api.x.example/v1".to_string());
        a.secret_ref = Some("file:///abs/key".to_string());

        run(&path, a).await.expect("add");

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# operator note"), "{text}");
        assert!(text.contains("# keep this comment"), "{text}");
        assert!(
            text.find("# operator note").unwrap() < text.find("[server]").unwrap(),
            "{text}"
        );
        // The pre-existing provider is untouched; the new one is appended.
        assert!(text.contains("[providers.fast]"), "{text}");
        assert!(text.contains("[providers.grok]"), "{text}");
    }

    // -----------------------------------------------------------------
    // Idempotent re-add: a byte-identical re-add writes nothing.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn identical_re_add_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);

        let mut first = args("openai-compat", "grok");
        first.base_url = Some("https://api.x.example/v1".to_string());
        first.secret_ref = Some("file:///abs/key".to_string());
        assert_eq!(
            run(&path, first).await.expect("first add"),
            AddResult::Written
        );

        let after_first = std::fs::read(&path).unwrap();

        let mut second = args("openai-compat", "grok");
        second.base_url = Some("https://api.x.example/v1".to_string());
        second.secret_ref = Some("file:///abs/key".to_string());
        assert_eq!(
            run(&path, second).await.expect("re-add"),
            AddResult::NoChange
        );

        assert_eq!(
            std::fs::read(&path).unwrap(),
            after_first,
            "an identical re-add must leave the file byte-identical"
        );
    }

    // -----------------------------------------------------------------
    // Existing name, different block: refused without --overwrite, overwrites
    // with it.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn different_block_on_existing_name_is_refused_without_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);
        let before = std::fs::read(&path).unwrap();

        // "fast" already exists with a different base_url + api_key_ref.
        let mut a = args("openai-compat", "fast");
        a.base_url = Some("https://elsewhere.example/v1".to_string());
        a.secret_ref = Some("file:///abs/key".to_string());

        let err = run(&path, a)
            .await
            .expect_err("must refuse a conflicting overwrite");
        assert!(err.to_string().contains("--overwrite"), "err: {err}");
        assert_eq!(std::fs::read(&path).unwrap(), before, "must not write");
    }

    #[tokio::test]
    async fn overwrite_replaces_an_existing_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);

        let mut a = args("openai-compat", "fast");
        a.base_url = Some("https://elsewhere.example/v1".to_string());
        a.secret_ref = Some("file:///abs/key".to_string());
        a.overwrite = true;

        let result = run(&path, a).await.expect("overwrite");
        assert_eq!(result, AddResult::Written);

        let config = parse_config(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let entry = config.providers.get("fast").unwrap();
        assert_eq!(entry.api_key_ref(), Some("file:///abs/key"));
    }

    #[tokio::test]
    async fn overwrite_still_passes_through_the_confirm_gate() {
        // `--overwrite` clears the existing-block refusal but NOT the
        // high-consequence confirmation: with yes=false and an EOF stdin the
        // overwrite is declined and the original block is left byte-identical.
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);
        let before = std::fs::read(&path).unwrap();

        let mut a = args("openai-compat", "fast");
        a.base_url = Some("https://elsewhere.example/v1".to_string());
        a.secret_ref = Some("file:///abs/key".to_string());
        a.overwrite = true;
        a.yes = false;

        let result = run(&path, a).await.expect("declining is not an error");
        assert_eq!(result, AddResult::Aborted);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "a declined overwrite must leave the original block untouched"
        );
    }

    // -----------------------------------------------------------------
    // Gate failure: a candidate that fails the shared validator writes
    // nothing.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn candidate_failing_the_gate_writes_nothing() {
        // The base parses but carries a latent semantic error (an alias
        // pointing at an undefined model). It loads far enough for `prev`,
        // but the shared gate re-validates the whole candidate and rejects
        // it -- so `provider add` refuses to write and leaves the file
        // byte-identical, before it ever reaches the confirmation prompt.
        let body = "\
version = 3

[server]
host = \"127.0.0.1\"
port = 8787

[providers.fast]
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"literal:test-key\"

[models.gpt]
provider = \"fast\"
upstream = \"gpt-4o\"

[aliases]
default = \"no-such-model\"
";
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), body);
        let before = std::fs::read(&path).unwrap();

        let mut a = args("openai-compat", "grok");
        a.base_url = Some("https://api.x.example/v1".to_string());
        a.secret_ref = Some("file:///abs/key".to_string());

        let err = run(&path, a).await;
        assert!(err.is_err(), "a candidate failing the gate must be refused");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "gate failure must leave the file byte-identical"
        );
    }

    // -----------------------------------------------------------------
    // Missing credential: a key-requiring kind with no secret flag errors
    // actionably (and never hangs -- no prompt in this command).
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn missing_secret_source_errors_actionably() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);
        let before = std::fs::read(&path).unwrap();

        let mut a = args("openai-compat", "grok");
        a.base_url = Some("https://api.x.example/v1".to_string());
        // no api_key_env, no secret_ref

        let err = run(&path, a).await.expect_err("must require a credential");
        let msg = err.to_string();
        assert!(
            msg.contains("--api-key-env") && msg.contains("--secret-ref"),
            "{msg}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before, "must not write");
    }

    #[cfg(feature = "gemini")]
    #[tokio::test]
    async fn adds_gemini_with_default_base_url() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);

        let mut a = args("gemini", "gem");
        a.secret_ref = Some("env://GEMINI_API_KEY".to_string());

        let result = run(&path, a).await.expect("add gemini");
        assert_eq!(result, AddResult::Written);

        let config = parse_config(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let entry = config.providers.get("gem").unwrap();
        assert_eq!(entry.kind_str(), "gemini");
        assert_eq!(entry.api_key_ref(), Some("env://GEMINI_API_KEY"));
    }

    #[cfg(feature = "gemini")]
    #[tokio::test]
    async fn gemini_rejects_base_url_flag() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);
        let before = std::fs::read(&path).unwrap();

        let mut a = args("gemini", "gem");
        a.base_url = Some("https://example/v1beta".to_string());
        a.secret_ref = Some("env://GEMINI_API_KEY".to_string());

        let err = run(&path, a)
            .await
            .expect_err("gemini must reject --base-url");
        assert!(err.to_string().contains("--base-url"), "err: {err}");
        assert_eq!(std::fs::read(&path).unwrap(), before, "must not write");
    }

    #[tokio::test]
    async fn unsupported_kind_errors_actionably() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);

        let mut a = args("bedrock", "aws");
        a.secret_ref = Some("file:///abs/key".to_string());

        let err = run(&path, a)
            .await
            .expect_err("unsupported kind must error");
        let msg = err.to_string();
        assert!(
            msg.contains("cannot be added with this command"),
            "err: {msg}"
        );
        assert!(
            msg.contains("supported kinds"),
            "err lists the supported set: {msg}"
        );
    }

    // -----------------------------------------------------------------
    // High-consequence confirm: --yes bypasses; declining leaves the file
    // byte-identical.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn declining_the_confirmation_writes_nothing() {
        // yes=false with a non-interactive stdin (EOF) -> confirm returns
        // false -> abort with the file untouched.
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);
        let before = std::fs::read(&path).unwrap();

        let mut a = args("openai-compat", "grok");
        a.base_url = Some("https://api.x.example/v1".to_string());
        a.secret_ref = Some("file:///abs/key".to_string());
        a.yes = false;

        let result = run(&path, a).await.expect("declining is not an error");
        assert_eq!(result, AddResult::Aborted);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "decline must not write"
        );
    }

    // -----------------------------------------------------------------
    // Audit event: exactly one, with the required fields and NO value / NO
    // full ref string.
    // -----------------------------------------------------------------

    #[tokio::test]
    #[serial_test::serial]
    async fn emits_one_audit_event_without_value_or_full_ref() {
        let key = "ROUTECTL_PROVIDER_ADD_AUDIT_KEY";
        let secret_value = "audit-secret-value-not-real";
        set_env(key, secret_value);

        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);

        let (_res, events) = routectl_testkit::with_capture(async {
            let mut a = args("anthropic-api", "claude");
            a.api_key_env = Some(key.to_string());
            run(&path, a).await.expect("add");
        })
        .await;

        let audit: Vec<_> = events
            .iter()
            .filter(|e| {
                e.field("surface") == Some("cli") && e.field("verb") == Some("provider-add")
            })
            .collect();
        assert_eq!(audit.len(), 1, "exactly one audit event expected");

        let event = audit[0];
        assert_eq!(event.field("name"), Some("claude"));
        assert_eq!(event.field("kind"), Some("anthropic-api"));
        assert_eq!(
            event.field("credential_source"),
            Some("env"),
            "credential_source is the scheme class only"
        );
        assert!(
            event.field("value").is_none(),
            "the value must never be audited"
        );
        assert!(
            event.field("api_key_ref").is_none(),
            "the full ref must never be audited"
        );

        unset_env(key);
    }

    // -----------------------------------------------------------------
    // Stale-snapshot conflict: the write refuses and the file is unchanged.
    // -----------------------------------------------------------------

    #[test]
    fn stale_snapshot_conflict_leaves_file_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);
        let stale = std::fs::read(&path).unwrap();
        let stale_text = String::from_utf8(stale.clone()).unwrap();

        // Something else rewrote the file after the caller snapshotted it.
        let rewritten = format!("{V3_BASE}# added out of band\n");
        std::fs::write(&path, &rewritten).unwrap();

        let entry = ProviderEntry::openai_compat("https://api.x.example/v1", "file:///abs/key");
        let block = provider_table(&entry).unwrap();

        let err = commit(&path, &stale, &stale_text, "grok", block)
            .expect_err("a stale snapshot must conflict");
        assert!(err.to_string().contains("changed on disk"), "err: {err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            rewritten,
            "a conflict must leave the on-disk file untouched"
        );
    }

    // -----------------------------------------------------------------
    // Secret-source breadth: stdin capture, TTY guards, interactive prompt,
    // env-detect offer, oauth delegation, forwarded, post-capture conflict.
    // -----------------------------------------------------------------

    /// Configurable [`AddIo`] fake: no real TTY, stdin, prompt, or browser.
    /// Interior state uses `Mutex` (not `RefCell`) so the fake is `Send +
    /// Sync`, as the `AddIo` supertrait now requires for its async `login`.
    struct FakeIo {
        is_tty: bool,
        stdin_value: String,
        stdin_hook: Option<Box<dyn Fn() + Send + Sync>>,
        offer_env: bool,
        prompt_value: String,
        login_ok: bool,
        login_calls: std::sync::Mutex<Vec<String>>,
        stdin_reads: std::sync::Mutex<u32>,
        prompt_calls: std::sync::Mutex<u32>,
    }

    impl Default for FakeIo {
        fn default() -> Self {
            Self {
                is_tty: false,
                stdin_value: String::new(),
                stdin_hook: None,
                offer_env: false,
                prompt_value: String::new(),
                login_ok: true,
                login_calls: std::sync::Mutex::new(Vec::new()),
                stdin_reads: std::sync::Mutex::new(0),
                prompt_calls: std::sync::Mutex::new(0),
            }
        }
    }

    #[async_trait]
    impl AddIo for FakeIo {
        fn stdin_is_terminal(&self) -> bool {
            self.is_tty
        }
        fn read_stdin(&self) -> Result<String> {
            *self.stdin_reads.lock().unwrap() += 1;
            if let Some(hook) = &self.stdin_hook {
                hook();
            }
            Ok(self.stdin_value.clone())
        }
        fn confirm_env_offer(&self, _var: &str) -> bool {
            self.offer_env
        }
        fn prompt_hidden(&self, _provider_name: &str) -> Result<String> {
            *self.prompt_calls.lock().unwrap() += 1;
            Ok(self.prompt_value.clone())
        }
        async fn login(&self, provider: &str) -> Result<()> {
            self.login_calls.lock().unwrap().push(provider.to_string());
            if self.login_ok {
                Ok(())
            } else {
                Err(Error::Auth("login failed".into()))
            }
        }
    }

    /// Point `default_secret_dir` at a temp XDG root so captures land in an
    /// isolated store. Returns the guard tempdir (keep it alive) and the
    /// secrets dir the store will use.
    fn scoped_secret_dir(tmp: &std::path::Path) -> std::path::PathBuf {
        set_env("XDG_CONFIG_HOME", tmp.to_str().unwrap());
        tmp.join("routectl").join("secrets")
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn api_key_stdin_captures_to_managed_store_and_writes_only_the_ref() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);
        let xdg = tempfile::tempdir().unwrap();
        let secrets = scoped_secret_dir(xdg.path());
        let secret_value = "piped-secret-value-not-real";

        let mut a = args("openai-compat", "grok");
        a.base_url = Some("https://api.x.example/v1".to_string());
        a.api_key_stdin = true;
        let io = FakeIo {
            stdin_value: format!("{secret_value}\n"),
            ..Default::default()
        };

        let result = run_with_io(&path, a, &io).await.expect("stdin capture");
        assert_eq!(result, AddResult::Written);

        let text = std::fs::read_to_string(&path).unwrap();
        let config = parse_config(&text).unwrap();
        let entry = config.providers.get("grok").unwrap();
        let stored_ref = entry.api_key_ref().unwrap();
        assert!(stored_ref.starts_with("file://"), "ref: {stored_ref}");
        assert!(
            !text.contains(secret_value),
            "the piped value must never land in the config"
        );

        // The captured file holds the exact key (trailing newline stripped)
        // and nothing else references the value.
        let captured = std::fs::read_to_string(secrets.join("grok")).unwrap();
        assert_eq!(captured, secret_value);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(secrets.join("grok"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "captured secret must be 0600");
        }
        unset_env("XDG_CONFIG_HOME");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn api_key_stdin_on_a_tty_errors_immediately_without_reading() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);
        let before = std::fs::read(&path).unwrap();

        let mut a = args("openai-compat", "grok");
        a.base_url = Some("https://api.x.example/v1".to_string());
        a.api_key_stdin = true;
        let io = FakeIo {
            is_tty: true,
            ..Default::default()
        };

        let err = run_with_io(&path, a, &io)
            .await
            .expect_err("TTY stdin must error");
        assert!(err.to_string().contains("stdin is a TTY"), "err: {err}");
        assert_eq!(
            *io.stdin_reads.lock().unwrap(),
            0,
            "a TTY stdin must never be read (no hang)"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before, "must not write");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn missing_key_without_tty_errors_actionably_and_never_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);
        let before = std::fs::read(&path).unwrap();

        let mut a = args("openai-compat", "grok");
        a.base_url = Some("https://api.x.example/v1".to_string());
        let io = FakeIo::default(); // not a TTY, no flags

        let err = run_with_io(&path, a, &io)
            .await
            .expect_err("missing key + no TTY must error");
        let msg = err.to_string();
        assert!(msg.contains("--api-key-env"), "{msg}");
        assert!(msg.contains("--api-key-stdin"), "{msg}");
        assert_eq!(
            *io.prompt_calls.lock().unwrap(),
            0,
            "no interactive prompt when stdin is not a TTY"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before, "must not write");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn interactive_hidden_prompt_captures_when_tty_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);
        let xdg = tempfile::tempdir().unwrap();
        let secrets = scoped_secret_dir(xdg.path());
        // No conventional var set, so the env-offer is skipped and the prompt
        // fires. Use a kind whose conventional var we can guarantee is unset.
        let var = env_var_for_kind("openai-compat").unwrap();
        let prev_var = std::env::var(var).ok();
        unset_env(var);

        let mut a = args("openai-compat", "grok");
        a.base_url = Some("https://api.x.example/v1".to_string());
        let io = FakeIo {
            is_tty: true,
            prompt_value: "prompted-key-not-real".to_string(),
            ..Default::default()
        };

        let result = run_with_io(&path, a, &io)
            .await
            .expect("interactive capture");
        assert_eq!(result, AddResult::Written);
        assert_eq!(
            *io.prompt_calls.lock().unwrap(),
            1,
            "the hidden prompt must fire"
        );
        assert_eq!(
            std::fs::read_to_string(secrets.join("grok")).unwrap(),
            "prompted-key-not-real"
        );

        restore_env(var, prev_var);
        unset_env("XDG_CONFIG_HOME");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn interactive_offers_a_resolvable_env_var_and_writes_the_env_ref() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);
        let var = env_var_for_kind("anthropic-api").unwrap();
        let prev_var = std::env::var(var).ok();
        set_env(var, "resolvable-value-not-real");

        let a = args("anthropic-api", "claude");
        let io = FakeIo {
            is_tty: true,
            offer_env: true,
            ..Default::default()
        };

        let result = run_with_io(&path, a, &io)
            .await
            .expect("env-detect offer accepted");
        assert_eq!(result, AddResult::Written);
        assert_eq!(
            *io.prompt_calls.lock().unwrap(),
            0,
            "an accepted env offer must not fall through to the prompt"
        );

        let config = parse_config(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let entry = config.providers.get("claude").unwrap();
        assert_eq!(entry.api_key_ref(), Some(format!("env://{var}").as_str()));

        restore_env(var, prev_var);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn interactive_does_not_offer_an_unresolved_env_var() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);
        let xdg = tempfile::tempdir().unwrap();
        let secrets = scoped_secret_dir(xdg.path());
        let var = env_var_for_kind("anthropic-api").unwrap();
        let prev_var = std::env::var(var).ok();
        unset_env(var);

        let a = args("anthropic-api", "claude");
        // offer_env=true would accept IF asked; the unresolved var must mean
        // it is never offered, so the prompt captures instead.
        let io = FakeIo {
            is_tty: true,
            offer_env: true,
            prompt_value: "fallback-prompt-key".to_string(),
            ..Default::default()
        };

        let result = run_with_io(&path, a, &io).await.expect("prompt fallback");
        assert_eq!(result, AddResult::Written);
        assert_eq!(
            *io.prompt_calls.lock().unwrap(),
            1,
            "prompt must capture instead"
        );
        assert_eq!(
            std::fs::read_to_string(secrets.join("claude")).unwrap(),
            "fallback-prompt-key"
        );

        restore_env(var, prev_var);
        unset_env("XDG_CONFIG_HOME");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn interactive_does_not_offer_an_empty_env_var() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);
        let xdg = tempfile::tempdir().unwrap();
        let secrets = scoped_secret_dir(xdg.path());
        let var = env_var_for_kind("anthropic-api").unwrap();
        let prev_var = std::env::var(var).ok();
        // Set-but-empty must NOT satisfy "resolves non-empty NOW": no offer.
        set_env(var, "");

        let a = args("anthropic-api", "claude");
        let io = FakeIo {
            is_tty: true,
            offer_env: true,
            prompt_value: "prompt-over-empty-var".to_string(),
            ..Default::default()
        };

        let result = run_with_io(&path, a, &io).await.expect("prompt fallback");
        assert_eq!(result, AddResult::Written);
        assert_eq!(
            *io.prompt_calls.lock().unwrap(),
            1,
            "an empty env var is not offered; the prompt captures instead"
        );
        assert_eq!(
            std::fs::read_to_string(secrets.join("claude")).unwrap(),
            "prompt-over-empty-var"
        );

        restore_env(var, prev_var);
        unset_env("XDG_CONFIG_HOME");
    }

    #[tokio::test]
    async fn forwarded_anthropic_api_adds_without_a_secret_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);

        let mut a = args("anthropic-api", "fwd");
        a.credential_source = Some("forwarded".to_string());
        let io = FakeIo::default();

        let result = run_with_io(&path, a, &io).await.expect("forwarded add");
        assert_eq!(result, AddResult::Written);
        assert_eq!(
            *io.prompt_calls.lock().unwrap(),
            0,
            "forwarded prompts for nothing"
        );

        let text = std::fs::read_to_string(&path).unwrap();
        let config = parse_config(&text).unwrap();
        let entry = config.providers.get("fwd").unwrap();
        assert_eq!(entry.kind_str(), "anthropic-api");
        assert_eq!(
            entry.api_key_ref(),
            Some(""),
            "a forwarded provider carries no configured credential"
        );
        assert!(text.contains("credential_source = \"forwarded\""), "{text}");
        assert!(
            text.contains("api.anthropic.com"),
            "base URL pinned: {text}"
        );
    }

    #[tokio::test]
    async fn forwarded_on_a_non_anthropic_kind_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);
        let before = std::fs::read(&path).unwrap();

        let mut a = args("openai-compat", "x");
        a.base_url = Some("https://api.x.example/v1".to_string());
        a.credential_source = Some("forwarded".to_string());
        let io = FakeIo::default();

        let err = run_with_io(&path, a, &io)
            .await
            .expect_err("forwarded is anthropic-api only");
        assert!(err.to_string().contains("anthropic-api"), "err: {err}");
        assert_eq!(std::fs::read(&path).unwrap(), before, "must not write");
    }

    #[tokio::test]
    async fn forwarded_with_a_secret_flag_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);
        let before = std::fs::read(&path).unwrap();

        let mut a = args("anthropic-api", "fwd");
        a.credential_source = Some("forwarded".to_string());
        a.secret_ref = Some("file:///abs/key".to_string());
        let io = FakeIo::default();

        let err = run_with_io(&path, a, &io)
            .await
            .expect_err("forwarded must not combine with a secret-source flag");
        assert!(
            err.to_string().contains("captures no credential"),
            "err: {err}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before, "must not write");
    }

    #[tokio::test]
    async fn oauth_backed_kind_rejects_base_url_flag() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);
        let before = std::fs::read(&path).unwrap();

        let mut a = args("anthropic", "claude-sub");
        a.base_url = Some("https://api.anthropic.com/v1".to_string());
        let io = FakeIo::default();

        let err = run_with_io(&path, a, &io)
            .await
            .expect_err("an oauth-backed kind must reject --base-url");
        assert!(err.to_string().contains("--base-url"), "err: {err}");
        assert_eq!(std::fs::read(&path).unwrap(), before, "must not write");
    }

    #[tokio::test]
    async fn oauth_backed_kind_delegates_to_login_and_writes_the_oauth_ref() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);

        let a = args("anthropic", "claude-sub");
        let io = FakeIo::default(); // login_ok = true (already-logged-in seam)

        let result = run_with_io(&path, a, &io).await.expect("oauth add");
        assert_eq!(result, AddResult::Written);
        assert_eq!(
            *io.login_calls.lock().unwrap(),
            vec!["anthropic".to_string()],
            "the login flow must be delegated exactly once for `anthropic`"
        );

        let config = parse_config(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let entry = config.providers.get("claude-sub").unwrap();
        assert_eq!(entry.kind_str(), "anthropic-api");
        assert_eq!(entry.api_key_ref(), Some("oauth://anthropic"));
    }

    #[tokio::test]
    async fn oauth_login_failure_aborts_before_the_config_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);
        let before = std::fs::read(&path).unwrap();

        let a = args("anthropic", "claude-sub");
        let io = FakeIo {
            login_ok: false,
            ..Default::default()
        };

        let err = run_with_io(&path, a, &io)
            .await
            .expect_err("a failed login must abort");
        assert!(err.to_string().contains("login failed"), "err: {err}");
        assert_eq!(std::fs::read(&path).unwrap(), before, "must not write");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn declined_confirm_captures_no_secret_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);
        let before = std::fs::read(&path).unwrap();
        let xdg = tempfile::tempdir().unwrap();
        let secrets = scoped_secret_dir(xdg.path());

        let mut a = args("openai-compat", "grok");
        a.base_url = Some("https://api.x.example/v1".to_string());
        a.api_key_stdin = true;
        a.yes = false; // EOF stdin -> confirm declines
        let io = FakeIo {
            stdin_value: "declined-key-not-real".to_string(),
            ..Default::default()
        };

        let result = run_with_io(&path, a, &io)
            .await
            .expect("declining is not an error");
        assert_eq!(result, AddResult::Aborted);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "must not write config"
        );
        assert!(
            !secrets.join("grok").exists(),
            "a declined confirm must capture no secret file"
        );
        unset_env("XDG_CONFIG_HOME");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn post_capture_config_conflict_persists_secret_and_reports_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);
        let xdg = tempfile::tempdir().unwrap();
        let secrets = scoped_secret_dir(xdg.path());

        // The fake rewrites config.toml the moment stdin is read -- i.e.
        // AFTER `run` snapshotted it but before the locked commit -- so the
        // commit sees a changed file and conflicts, with the capture already
        // done.
        let conflict_path = path.clone();
        let mut a = args("openai-compat", "grok");
        a.base_url = Some("https://api.x.example/v1".to_string());
        a.api_key_stdin = true;
        let io = FakeIo {
            stdin_value: "captured-then-conflict".to_string(),
            stdin_hook: Some(Box::new(move || {
                std::fs::write(&conflict_path, format!("{V3_BASE}# out of band\n")).unwrap();
            })),
            ..Default::default()
        };

        let err = run_with_io(&path, a, &io)
            .await
            .expect_err("a post-capture conflict must error");
        let msg = err.to_string();
        assert!(
            msg.contains("captured"),
            "recovery names the capture: {msg}"
        );
        assert!(msg.contains("re-run"), "recovery says re-run: {msg}");
        assert!(
            secrets.join("grok").exists(),
            "the captured secret must persist across a config-write conflict"
        );
        unset_env("XDG_CONFIG_HOME");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn captured_value_never_appears_in_tracing_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), V3_BASE);
        let xdg = tempfile::tempdir().unwrap();
        let _secrets = scoped_secret_dir(xdg.path());
        let secret_value = "tracing-secret-value-not-real";

        let (_res, events) = routectl_testkit::with_capture(async {
            let mut a = args("openai-compat", "grok");
            a.base_url = Some("https://api.x.example/v1".to_string());
            a.api_key_stdin = true;
            let io = FakeIo {
                stdin_value: secret_value.to_string(),
                ..Default::default()
            };
            run_with_io(&path, a, &io).await.expect("stdin capture");
        })
        .await;

        for event in &events {
            for field in ["credential_source", "value", "api_key_ref", "message"] {
                if let Some(v) = event.field(field) {
                    assert!(
                        !v.contains(secret_value),
                        "the secret value leaked into tracing field `{field}`: {v}"
                    );
                }
            }
        }
        // And the recorded credential class is the scheme only.
        let audit: Vec<_> = events
            .iter()
            .filter(|e| e.field("verb") == Some("provider-add"))
            .collect();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].field("credential_source"), Some("file"));
        unset_env("XDG_CONFIG_HOME");
    }

    // -----------------------------------------------------------------
    // Canonicalize-once: the store is opened during the capture phase and
    // carried through PendingSecret::File, so a symlinked ancestor swapped
    // between capture and execute cannot redirect the put -- it lands under
    // the ORIGINAL canonical base the precomputed ref already points at.
    // -----------------------------------------------------------------

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn symlinked_ancestor_swap_between_phases_lands_at_original_base() {
        // Arrange: XDG_CONFIG_HOME points through a symlink at `real_a`, so
        // the store's default dir resolves under `real_a`.
        let root = tempfile::tempdir().unwrap();
        let real_a = root.path().join("real-a");
        let real_b = root.path().join("real-b");
        std::fs::create_dir_all(&real_a).unwrap();
        std::fs::create_dir_all(&real_b).unwrap();
        let link = root.path().join("xdg-link");
        std::os::unix::fs::symlink(&real_a, &link).unwrap();
        set_env("XDG_CONFIG_HOME", link.to_str().unwrap());

        let a = args("openai-compat", "grok");

        // Act (capture phase): opens the store, canonicalizes through the
        // symlink, and computes the ref off the resolved base.
        let (ref_str, class, pending) =
            capture_value(&a, "value-not-real".to_string()).expect("capture");
        assert_eq!(class, "file");
        let expected = std::fs::canonicalize(&real_a)
            .unwrap()
            .join("routectl")
            .join("secrets")
            .join("grok");
        assert_eq!(
            ref_str,
            SecretRef::File(expected.clone()).to_string(),
            "the precomputed ref points at the original canonical base"
        );

        // Swap the symlinked ancestor to `real_b` BETWEEN the two phases.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&real_b, &link).unwrap();

        // Act (execute phase): the put reuses the store canonicalized at
        // capture time, not the freshly-swapped symlink target.
        execute_pending(pending, &FakeIo::default())
            .await
            .expect("put");

        // Assert: the secret landed under the ORIGINAL canonical base,
        // matching the precomputed ref -- never under the swapped target.
        assert_eq!(
            std::fs::read_to_string(&expected).unwrap(),
            "value-not-real",
            "put must land at the precomputed ref path"
        );
        let swapped = std::fs::canonicalize(&real_b)
            .unwrap()
            .join("routectl")
            .join("secrets")
            .join("grok");
        assert!(
            !swapped.exists(),
            "the swapped symlink target must never receive the secret"
        );
        unset_env("XDG_CONFIG_HOME");
    }

    fn restore_env(key: &str, prev: Option<String>) {
        match prev {
            Some(v) => set_env(key, &v),
            None => unset_env(key),
        }
    }
}
