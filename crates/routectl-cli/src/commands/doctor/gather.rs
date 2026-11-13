//! Doctor data collection (context, probes, auth, secret classification).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use chrono::Local;
use routectl_auth::LocalProbe;
use routectl_auth::oauth::types::{TokenRecord, seat_key, unix_now};
use routectl_auth::{OAuthError, OAuthStore};
use routectl_auth::{SecretRef, SecretScheme, default_secret_dir};
use routectl_core::ProbeOutcome;
use routectl_router::{
    BetaSeedScope, CapabilityEventRow, CapabilityRebuildSummary, Config, EffectiveView,
    FROZEN_CATALOG_VERSION, FROZEN_OVERLAY_REVISION, FillSource, LearnedCapabilityRegistry,
    LearnedFills, LearnedLaneProjection, MatrixReplaySummary, ModelCell, PricingConfig,
    ProviderEntry, ReplayTombstone, StateKey, derive_effective_view,
    rebuild_capabilities_with_values,
};

use crate::commands::doctor_panels::compute_would_trim_panel;
use crate::commands::parse_error_redaction::redact_config_load_error;
use crate::commands::pricing::{
    CacheWriteSpellingConflict, PriceSnapshot, cache_write_spelling_conflicts, default_price_book,
    is_subscription,
};
use crate::commands::probe::{PROBE_DEADLINE, probe_all};
use crate::config_folder::LegacyConfigDir;
use crate::server::CompositeStore;
use crate::server::ledger_reader::{
    BoundaryOutcome, LedgerCapabilityReader, SliceReader, classify_boundary,
};

use super::cache_section::{gather_cache_ignore, read_budget};
use super::served::{GatherSources, GatheredLayers, served_layers};
use super::{
    CapabilityInputs, CapabilityMatrixSource, DoctorContext, KnobRow, LEFTOVER_CATALOG_FILES,
    LEGACY_FOLDER_NAME_CAP, LegacyFolder, MatrixOrigin, PricingBlock, PricingRow,
};

/// The network doctor gather: the no-network context PLUS one upstream
/// reachability pass. Building the whole context in exactly ONE place
/// ([`gather_context_no_network`]) is what keeps the two entry points from
/// drifting -- a new context field is added once and both paths carry it; the
/// only difference between the paths is this `probe_results` assignment.
pub(super) async fn gather_context(
    config_path: &Path,
    legacy_dirs: &[LegacyConfigDir],
) -> DoctorContext {
    let ctx = gather_context_no_network(config_path, GatherSources::Disk, legacy_dirs).await;
    let probe_results = gather_probe_results(&ctx.config).await;
    DoctorContext {
        probe_results,
        ..ctx
    }
}

/// Gather every read-only input the no-network sections draw from, WITHOUT
/// any upstream dial: `probe_results` is left empty and
/// [`gather_probe_results`] (the only caller of `CompositeStore`/`probe_all`)
/// is never reached. Everything else -- the config layer, auth via
/// `probe_local` (no network, no refresh), secret presence checks, the orphan
/// scan, the would-trim panel and the cache-ignore report -- is retained.
///
/// `sources` decides where the config and learned layer come from:
/// [`GatherSources::Disk`] loads them through [`disk_layers`];
/// [`GatherSources::Served`] takes a running daemon's accepted state as-is.
/// The raw config bytes are read from `config_path` either way, for the
/// validation findings. The version finding reads them too on the disk path;
/// a served gather takes it from the accepted config instead, since the file
/// on disk may hold an edit the daemon rejected.
///
/// `legacy_dirs` are the old config folders to list, usually
/// [`crate::config_folder::legacy_config_dirs_from_env`]; the gather itself
/// reads no environment variable for them.
pub async fn gather_context_no_network(
    config_path: &Path,
    sources: GatherSources,
    legacy_dirs: &[LegacyConfigDir],
) -> DoctorContext {
    let raw_config = std::fs::read_to_string(config_path).ok();
    let cache_read_budget = read_budget(&sources);
    let layers = match sources {
        GatherSources::Disk => disk_layers(config_path),
        GatherSources::Served(inputs) => served_layers(inputs),
    };
    let GatheredLayers {
        config,
        config_parse_error,
        config_load_error,
        accepted_config_version,
        capability_matrix,
        matrix_origin,
        reload_failure,
        parked_seats,
        unusable_seats,
        prices,
        usage_db_path,
    } = layers;

    let view = derive_effective_view(&config, prices.refreshed(), prices.shipped());
    let pricing = derive_pricing_rows(&config, &view);
    let knobs = derive_knob_rows(&config, &view, &capability_matrix);
    let tuning = view.tuning;
    let capability = CapabilityInputs {
        panel_unavailable: config_parse_error,
    };

    let (probes, seats, auth_store_error) = gather_auth().await;
    let secret_checks = gather_secret_checks(&config, &probes);
    let orphan_secrets = gather_orphan_secrets(&config);
    let orphan_seats = gather_orphan_seats(&config, &seats);
    let would_trim = compute_would_trim_panel(&config, Local::now());
    let cache_ignore = gather_cache_ignore(
        &usage_db_path,
        &config,
        cache_read_budget,
        chrono::Utc::now().timestamp_millis(),
    );
    let legacy_folders = legacy_dirs
        .iter()
        .map(|legacy| gather_legacy_folder(legacy, config_path))
        .filter(|folder| *folder != LegacyFolder::Absent)
        .collect();
    let leftover_catalog_files = routectl_core::config_dir::config_dir()
        .map(|dir| gather_leftover_catalog_files(&dir))
        .unwrap_or_default();

    DoctorContext {
        config,
        raw_config,
        config_load_error,
        accepted_config_version,
        probes,
        seats,
        auth_store_error,
        secret_checks,
        orphan_secrets,
        orphan_seats,
        probe_results: Vec::new(),
        would_trim,
        cache_ignore,
        now_unix: unix_now(),
        binary_version: env!("CARGO_PKG_VERSION"),
        capability,
        capability_matrix,
        matrix_origin,
        beta_seed: routectl_router::shipped_beta_seed_scope(),
        pricing,
        knobs,
        tuning,
        reload_failure,
        parked_seats,
        unusable_seats,
        leftover_catalog_files,
        legacy_folders,
    }
}

/// The [`LEFTOVER_CATALOG_FILES`] that exist in `config_dir`, in list order.
/// Any directory entry counts, a dangling symlink included: the operator
/// removes it the same way.
pub(super) fn gather_leftover_catalog_files(config_dir: &Path) -> Vec<&'static str> {
    LEFTOVER_CATALOG_FILES
        .into_iter()
        .filter(|name| config_dir.join(name).symlink_metadata().is_ok())
        .collect()
}

/// List what the old config folder `legacy` still holds. A folder that holds
/// `config_in_use` is the operator's explicit choice, not residue, so it
/// reads as [`LegacyFolder::Absent`]; the config counts as held there when
/// either the path as written or its symlink-resolved target sits directly in
/// the folder. Every entry counts, temp and lock files included: the operator
/// moves the folder's contents as a whole.
pub(super) fn gather_legacy_folder(legacy: &LegacyConfigDir, config_in_use: &Path) -> LegacyFolder {
    let LegacyConfigDir {
        path: legacy_dir,
        display,
    } = legacy;
    if holds_config_in_use(legacy_dir, config_in_use) {
        return LegacyFolder::Absent;
    }
    let inspect_error = |e: std::io::Error| LegacyFolder::InspectError {
        display,
        kind: e.kind(),
    };
    let entries = match std::fs::read_dir(legacy_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return LegacyFolder::Absent,
        Err(e) => return inspect_error(e),
    };
    let mut names = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => names.push(entry.file_name().to_string_lossy().into_owned()),
            Err(e) => return inspect_error(e),
        }
    }
    if names.is_empty() {
        return LegacyFolder::Absent;
    }
    names.sort();
    let has_config = names.iter().any(|name| name == "config.toml");
    let more = names.len().saturating_sub(LEGACY_FOLDER_NAME_CAP);
    names.truncate(LEGACY_FOLDER_NAME_CAP);
    LegacyFolder::Entries {
        display,
        names,
        more,
        has_config,
    }
}

/// Whether `config_in_use`, as written or with symlinks resolved, sits
/// directly in `folder`. The written path matters on its own: a `--config`
/// naming `<folder>/config.toml` that links out of the folder still breaks if
/// the folder is removed.
fn holds_config_in_use(folder: &Path, config_in_use: &Path) -> bool {
    let in_folder = |path: &Path| path.parent().is_some_and(|parent| same_dir(parent, folder));
    in_folder(config_in_use)
        || std::fs::canonicalize(config_in_use).is_ok_and(|resolved| in_folder(&resolved))
}

/// Whether `a` and `b` name the same folder, resolving symlinks where both
/// resolve and comparing the paths as written otherwise.
fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The on-disk layers: a read-only load of the config plus a read-only ledger
/// replay for the learned matrix. A config parse error is the version
/// section's "config could not be loaded" Fail, so a present-but-broken config
/// never reports all-Pass; the other sections then run against defaults.
pub(super) fn disk_layers(config_path: &Path) -> GatheredLayers {
    let (config, config_parse_error) = match crate::server::parse_config_only(config_path) {
        Ok(config) => (config, None),
        Err(e) => (Config::default(), Some(redact_config_load_error(&e))),
    };
    let config_load_error = config_parse_error.clone();

    let capability_matrix = gather_capability_matrix(
        &config,
        config_parse_error.is_some(),
        routectl_router::shipped_beta_seed_scope(),
    );

    let usage_db_path = config.usage.db_path.clone();
    GatheredLayers {
        config,
        config_parse_error,
        config_load_error,
        accepted_config_version: None,
        capability_matrix,
        matrix_origin: MatrixOrigin::LedgerReplay,
        reload_failure: None,
        parked_seats: Vec::new(),
        unusable_seats: Vec::new(),
        prices: PriceSnapshot::pin(&default_price_book()),
        usage_db_path,
    }
}

/// One knob row per `[models.X]` entry, naming the `max_tokens` its egress
/// sends when the caller omits one: the view's configured or default fill,
/// lowered by the learned ceiling resident for the model. The learned layer
/// is `learned`, the same one the capability matrix shows, never a second
/// read of the store.
pub(super) fn derive_knob_rows(
    config: &Config,
    view: &EffectiveView,
    learned: &CapabilityMatrixSource,
) -> Vec<KnobRow> {
    let learned = learned_fills(config, learned);
    view.models
        .iter()
        .map(|cell| {
            let fill = cell
                .fill
                .map(|fill| fill.lowered_by(learned.for_model(&cell.nickname)));
            let lowers = cell
                .fill
                .filter(|_| fill.is_some_and(|f| f.source == FillSource::Learned))
                .map(|base| base.tokens);
            KnobRow {
                nickname: cell.nickname.clone(),
                provider_kind: cell.provider_kind.clone(),
                upstream: cell.upstream.clone(),
                fill,
                lowers,
                configured: config
                    .models
                    .get(&cell.nickname)
                    .and_then(|entry| entry.max_output_tokens)
                    .filter(|tokens| *tokens > 0),
            }
        })
        .collect()
}

/// The learned ceilings resident in `learned` for the models of `config`.
fn learned_fills(config: &Config, learned: &CapabilityMatrixSource) -> LearnedFills {
    let CapabilityMatrixSource::Available { entries, now, .. } = learned else {
        return LearnedFills::default();
    };
    let lanes = LearnedLaneProjection::from_config(config);
    LearnedFills::resolve(config, &lanes, entries, *now)
}

/// One pricing row per model cell of `view`, whose prices resolve through
/// `effective_pricing` against the tables the usage report reads.
///
/// A managed-OAuth provider is classified as a subscription BEFORE any price
/// decides anything, matching the order the usage report classifies a row in:
/// a subscription is billed by seat, so its resolved prices are only the
/// basis of its API-equivalent value, never its bill.
pub(super) fn derive_pricing_rows(config: &Config, view: &EffectiveView) -> Vec<PricingRow> {
    view.models
        .iter()
        .map(|cell| PricingRow {
            nickname: cell.nickname.clone(),
            provider: cell.provider.clone(),
            provider_kind: cell.provider_kind.clone(),
            upstream: cell.upstream.clone(),
            subscription: is_subscription(config, &cell.provider),
            pricing: cell.pricing.clone(),
            sources: cell.price_sources.clone(),
            fields: cell.price_fields(),
            write_conflicts: write_conflicts(config, cell),
            registry_matched: registry_pricing(config, &cell.provider, &cell.upstream).is_some(),
            tables: view.price_tables,
        })
        .collect()
}

/// The `[registry]` row's pricing that matches `upstream` served through
/// `provider`, keyed on the entry a pool bills under, as the effective view
/// resolves it.
fn registry_pricing<'a>(
    config: &'a Config,
    provider: &str,
    upstream: &str,
) -> Option<&'a PricingConfig> {
    let billed = config
        .provider_entry_for_target(provider)
        .map_or(provider, |(key, _)| key);
    config.pricing_for(upstream, billed)
}

/// The double-spelled cache-write lifetimes in the model's own pricing
/// block, then in its matching `[registry]` row.
fn write_conflicts(
    config: &Config,
    cell: &ModelCell,
) -> Vec<(PricingBlock, CacheWriteSpellingConflict)> {
    let own = config
        .models
        .get(&cell.nickname)
        .and_then(|entry| entry.pricing.as_ref());
    let registry = registry_pricing(config, &cell.provider, &cell.upstream);
    [
        (PricingBlock::Model, own),
        (PricingBlock::Registry, registry),
    ]
    .into_iter()
    .filter_map(|(block, pricing)| Some((block, pricing?)))
    .flat_map(|(block, pricing)| {
        cache_write_spelling_conflicts(pricing)
            .into_iter()
            .map(move |conflict| (block, conflict))
    })
    .collect()
}

/// Rebuild the learned-capability matrix read-only from the usage ledger for
/// this run's revision, classifying availability as a first-class tri-state.
///
/// A config that would not parse yields `Unavailable("config_unavailable")`:
/// the usage db path and the revision knobs cannot be trusted, so an
/// empty-from-default read would misreport. Otherwise the replay boundary is
/// resolved against the frozen revision stamps the daemon writes; on a match
/// the post-boundary slice is read through the typed
/// [`LedgerCapabilityReader::try_read_events`], so a slice that cannot be
/// read is `Unavailable` with its path-free failure class rather than an
/// empty registry. A slice that was read replays into a bare, config-sized
/// registry (`Available`, or honest `Empty` when nothing stays resident).
/// Read-only: the ledger is only ever opened read-only, so the db is
/// byte-identical afterward.
pub(super) fn gather_capability_matrix(
    config: &Config,
    config_parse_failed: bool,
    beta_seed: BetaSeedScope,
) -> CapabilityMatrixSource {
    if config_parse_failed {
        return CapabilityMatrixSource::Unavailable("config_unavailable");
    }

    match classify_boundary(
        &config.usage.db_path,
        FROZEN_CATALOG_VERSION,
        FROZEN_OVERLAY_REVISION,
    ) {
        BoundaryOutcome::Replay(tombstone) => {
            let reader = LedgerCapabilityReader::new(config.usage.db_path.clone(), tombstone);
            match reader.try_read_events() {
                Ok(rows) => replay_matrix_slice(config, &reader, tombstone, rows, beta_seed),
                Err(failure) => CapabilityMatrixSource::Unavailable(failure.as_str()),
            }
        }
        BoundaryOutcome::Cold => CapabilityMatrixSource::Unavailable("no_data"),
        BoundaryOutcome::NoTombstone => CapabilityMatrixSource::Unavailable("no_tombstone"),
        BoundaryOutcome::RevisionMismatch { .. } => {
            CapabilityMatrixSource::Unavailable("revision_mismatch")
        }
        BoundaryOutcome::Unreadable(code) => CapabilityMatrixSource::Unavailable(code),
    }
}

/// Replay an already-read post-boundary slice into a bare registry bounded by
/// `beta_seed` and classify what stayed resident, carrying the replay tally
/// and the seed-clear markers the replay recorded either way. Each resident
/// row carries the value its identity's latest replayed row restored: the
/// replay retires an entry whose value changed, so that is the resident
/// lifecycle's value.
fn replay_matrix_slice(
    config: &Config,
    reader: &LedgerCapabilityReader,
    tombstone: ReplayTombstone,
    rows: Vec<CapabilityEventRow>,
    beta_seed: BetaSeedScope,
) -> CapabilityMatrixSource {
    let slice = SliceReader::new(tombstone, rows);
    let registry = LearnedCapabilityRegistry::from_capability_config(&config.capability);
    registry.set_seed_scope(beta_seed);
    let (summary, values) = rebuild_capabilities_with_values(&slice, &registry, &config.providers);
    let replay = replay_summary(&summary, reader.loaded_rows());
    let entries: Vec<_> = registry
        .owned_recorded_snapshot(|lane| lane_provider_kind(config, lane))
        .into_iter()
        .map(|recorded| values.attach(recorded).into_entry())
        .collect();
    let seed_clears = registry.seed_clear_snapshot();
    if entries.is_empty() {
        CapabilityMatrixSource::Empty {
            replay: Some(replay),
            seed_clears,
        }
    } else {
        CapabilityMatrixSource::Available {
            entries,
            now: reader.now(),
            now_ms: reader.now_ms(),
            replay: Some(replay),
            seed_clears,
        }
    }
}

/// The kind `config` gives the provider entry `lane` names, or `""` when the
/// key is not a lane or names no configured entry: the version of a learned
/// fact a router under this config would read.
fn lane_provider_kind<'c>(config: &'c Config, lane: &str) -> &'c str {
    StateKey::parse(lane)
        .and_then(|lane| config.providers.get(lane.provider_entry()))
        .map_or("", ProviderEntry::kind_str)
}

/// Fold the router's rebuild tally into the panel's replay summary.
/// `replayed` counts every row that reached an admission arm.
pub(super) const fn replay_summary(
    summary: &CapabilityRebuildSummary,
    loaded_rows: usize,
) -> MatrixReplaySummary {
    MatrixReplaySummary {
        loaded_rows,
        replayed: summary.replayed_verified
            + summary.replayed_negative
            + summary.replayed_cleared
            + summary.cleared_noop,
        skipped_vocab: summary.skipped_vocab,
        skipped_owner: summary.skipped_owner,
        skipped_revision: summary.skipped_revision,
        skipped_lane: summary.skipped_lane,
        skipped_unknown: summary.skipped_unknown,
        skipped_namespace: summary.skipped_namespace,
    }
}

/// Probe every configured provider read-only through the shared `probe_all`
/// orchestration, under the same bounded deadline the standalone `provider
/// probe` uses. Fail-closed: if the composite credential store cannot open,
/// every provider collapses to an `Unreachable` outcome (a WARN/FAIL finding
/// with a reason) rather than being dropped to a silent pass. That `Err` arm is
/// defensive and currently unreachable -- `CompositeStore::open_default`
/// degrades a store failure to an in-memory store (with a warning) and always
/// returns `Ok` -- but it is kept so a future non-degrading open path still
/// fails closed here.
async fn gather_probe_results(config: &Config) -> Vec<(String, ProbeOutcome)> {
    match CompositeStore::open_default().await {
        Ok(store) => probe_all(config, &store, PROBE_DEADLINE).await,
        Err(_) => config
            .providers
            .keys()
            .map(|name| {
                (
                    name.clone(),
                    ProbeOutcome::Unreachable("credential store unavailable".into()),
                )
            })
            .collect(),
    }
}

/// Probe every routectl-owned OAuth id and list stored seats through the
/// default store. Read-only: `probe_all_local` performs no network or refresh
/// and `list` no writes. A store that fails to OPEN (schema mismatch,
/// corrupted file) returns a sanitized, path-free error string (see
/// [`sanitize_store_open_error`]) rather than being masked as an absent
/// store; probes then read `StoreUnavailable`.
async fn gather_auth() -> (
    Vec<(&'static str, LocalProbe)>,
    Vec<(String, TokenRecord)>,
    Option<String>,
) {
    let ids = routectl_auth::oauth::known_provider_ids();
    match OAuthStore::open_default().await {
        Ok(store) => {
            let probes = store.probe_all_local().await;
            (probes, store.list().await, None)
        }
        Err(e) => (
            ids.iter()
                .map(|id| (*id, LocalProbe::StoreUnavailable))
                .collect(),
            Vec::new(),
            Some(sanitize_store_open_error(&e)),
        ),
    }
}

/// Map an OAuth-store OPEN failure to a path-free message. Multiple
/// [`OAuthError`] variants embed the FULL credentials-store path in their
/// Display -- `SchemaMismatch`/`CorruptedFile` carry a `path` field, and every
/// `Io` open failure interpolates the path mid-message (`open <path>: ...`,
/// `credentials file <path> has permissions ...`). The doctor auth section
/// prints this verbatim, disclosing a filesystem path. Keep only the failure
/// CLASS and the store basename (`credentials.json`); never forward a variant's
/// raw Display. The enum is `#[non_exhaustive]`, so the catch-all is itself a
/// path-free class message rather than a passthrough -- fail-safe against a
/// future path-bearing variant.
pub fn sanitize_store_open_error(err: &OAuthError) -> String {
    const STORE_BASENAME: &str = "credentials.json";
    match err {
        OAuthError::SchemaMismatch {
            found, expected, ..
        } => format!(
            "credentials store schema is v{found}; this binary expects v{expected}; \
             upgrade routectl or delete {STORE_BASENAME} and re-run `routectl login`"
        ),
        OAuthError::CorruptedFile { .. } => {
            format!("oauth credentials file ({STORE_BASENAME}) is corrupted")
        }
        OAuthError::Io(_) => {
            format!("oauth credentials file ({STORE_BASENAME}) could not be read")
        }
        _ => format!("oauth credentials store ({STORE_BASENAME}) could not be opened"),
    }
}

/// Read-only presence classification of one provider secret reference.
/// Carries only the scheme label and a discriminant -- never a secret
/// value, an env var name, or a full `file://` / `literal:` ref string.
pub(super) struct SecretCheck {
    pub(super) provider: String,
    pub(super) scheme: &'static str,
    pub(super) presence: SecretPresence,
    pub(super) oauth_id: Option<String>,
    pub(super) oauth_label: Option<String>,
}

/// Outcome of a read-only presence check. Discriminants only.
#[derive(Clone, Copy)]
pub(super) enum SecretPresence {
    Present,
    Missing,
    Expired,
    Unreadable,
    StoreUnavailable,
    UnknownOauthProvider,
    Invalid,
}

/// The leak-safe scheme prefix of a secret URI. A bare, unprefixed value
/// IS secret material, so it collapses to `"unknown"` rather than echoing
/// any of its bytes.
pub(super) fn scheme_label(uri: &str) -> &'static str {
    SecretScheme::classify(uri).map_or("unknown", SecretScheme::as_prefix)
}

/// Classify one secret ref without resolving its value or refreshing any
/// credential. `oauth://` reads the pre-gathered local probes (no network,
/// no token refresh); `env://` / `file://` / `literal:` do a non-mutating
/// existence / parse check.
fn classify_secret_ref(
    uri: &str,
    probes: &[(&'static str, LocalProbe)],
) -> (SecretPresence, Option<(String, Option<String>)>) {
    match SecretRef::parse(uri) {
        Ok(SecretRef::OAuth { provider, label }) => {
            let presence = match probes.iter().find(|p| p.0 == provider.as_str()) {
                Some((_, LocalProbe::Present)) => SecretPresence::Present,
                Some((_, LocalProbe::Expired)) => SecretPresence::Expired,
                Some((_, LocalProbe::Missing)) => SecretPresence::Missing,
                Some((_, LocalProbe::StoreUnavailable)) => SecretPresence::StoreUnavailable,
                Some(_) | None => SecretPresence::UnknownOauthProvider,
            };
            (presence, Some((provider, label)))
        }
        Ok(SecretRef::Env(var)) => {
            let present = std::env::var(&var).is_ok_and(|v| !v.is_empty());
            let presence = if present {
                SecretPresence::Present
            } else {
                SecretPresence::Missing
            };
            (presence, None)
        }
        Ok(SecretRef::File(path)) => (classify_file(&path), None),
        // A future `SecretRef` variant (the enum is `#[non_exhaustive]`)
        // whose presence this build cannot confirm: treat conservatively as
        // unresolved rather than assume it is usable. A `literal:` ref no
        // longer parses (rejected at the resolver), so it lands in `Err`
        // below and reports as Invalid.
        Ok(_) => (SecretPresence::Missing, None),
        Err(_) => (SecretPresence::Invalid, None),
    }
}

/// Read-only existence + readability check of a `file://` secret path.
fn classify_file(path: &Path) -> SecretPresence {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => {
            if std::fs::File::open(path).is_ok() {
                SecretPresence::Present
            } else {
                SecretPresence::Unreadable
            }
        }
        Ok(_) => SecretPresence::Unreadable,
        Err(_) => SecretPresence::Missing,
    }
}

pub(super) fn gather_secret_checks(
    config: &Config,
    probes: &[(&'static str, LocalProbe)],
) -> Vec<SecretCheck> {
    let mut out = Vec::new();
    for (name, entry) in &config.providers {
        for uri in entry.secret_uris() {
            let scheme = scheme_label(uri);
            let (presence, oauth) = classify_secret_ref(uri, probes);
            let (oauth_id, oauth_label) = oauth.unzip();
            out.push(SecretCheck {
                provider: name.clone(),
                scheme,
                presence,
                oauth_id,
                oauth_label: oauth_label.flatten(),
            });
        }
    }
    out
}

/// Canonical paths of every `file://` secret the config references.
fn referenced_secret_files(config: &Config) -> BTreeSet<PathBuf> {
    let mut set = BTreeSet::new();
    for entry in config.providers.values() {
        for uri in entry.secret_uris() {
            if let Ok(SecretRef::File(path)) = SecretRef::parse(uri) {
                let canon = std::fs::canonicalize(&path).unwrap_or(path);
                set.insert(canon);
            }
        }
    }
    set
}

/// Read-only diff of the managed secret directory against the `file://`
/// refs the config references. Never opens a `ManagedSecretStore` (which
/// would create the directory) and never removes a file -- it only reads
/// the directory listing. Returns the basenames of unreferenced files.
pub(super) fn gather_orphan_secrets(config: &Config) -> Vec<String> {
    let Ok(dir) = default_secret_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let referenced = referenced_secret_files(config);
    let mut orphans = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let canon = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if referenced.contains(&canon) {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            orphans.push(name.to_string());
        }
    }
    orphans.sort();
    orphans
}

/// The `oauth://` coverage the config expresses, split by which seat each ref
/// resolves to at dispatch:
///
///   - `default_seat_providers` -- providers named by a BARE
///     `oauth://<provider>` ref (label `None`). Schema version 4 made a bare
///     ref the DEFAULT SEAT alone: it covers no labelled sibling, and
///     `[pools]` is the multi-seat shape that replaced the old expansion.
///   - `pinned_seats` -- full seat keys named by a LABELLED
///     `oauth://<provider>#<label>` ref. A labelled ref pins exactly that one
///     seat and covers no sibling, not even the default.
///
/// A pool covers exactly the seats its MEMBERS' own refs name, so walking
/// every provider entry already covers pool membership: a member is a
/// `[providers.X]` entry like any other. Nothing extra is contributed by the
/// `[pools]` blocks themselves -- a pool holds no credential ref.
struct SeatCoverage {
    default_seat_providers: BTreeSet<String>,
    pinned_seats: BTreeSet<String>,
}

impl SeatCoverage {
    /// Whether any config ref reaches `seat_key`. A labelled seat needs a ref
    /// naming it (a pool member's pinned ref, or a standalone pinned entry);
    /// the default seat needs a bare ref for its provider.
    fn covers(&self, seat_key: &str) -> bool {
        match seat_key.split_once('#') {
            Some(_) => self.pinned_seats.contains(seat_key),
            None => self.default_seat_providers.contains(seat_key),
        }
    }
}

fn referenced_seat_coverage(config: &Config) -> SeatCoverage {
    let mut default_seat_providers = BTreeSet::new();
    let mut pinned_seats = BTreeSet::new();
    for entry in config.providers.values() {
        for uri in entry.secret_uris() {
            let Ok(SecretRef::OAuth { provider, label }) = SecretRef::parse(uri) else {
                continue;
            };
            match label {
                Some(label) => {
                    pinned_seats.insert(seat_key(&provider, Some(&label)));
                }
                None => {
                    default_seat_providers.insert(provider);
                }
            }
        }
    }
    SeatCoverage {
        default_seat_providers,
        pinned_seats,
    }
}

/// Read-only diff of the STORED OAuth seats against the `oauth://` refs the
/// config expresses. Complements [`gather_orphan_secrets`], which covers
/// managed `file://` secrets only. Returns the seat keys (`<provider>` for the
/// default seat, `<provider>#<label>` for a labelled one) no provider entry
/// reaches; nothing is ever refreshed, rewritten, or removed.
///
/// Seat matching is by FULL seat identity: every ref names exactly one seat.
/// A labelled ref pins that seat, so the default seat of that provider is an
/// orphan unless something else reaches it; a bare ref pins the DEFAULT seat,
/// so a labelled seat is an orphan unless some ref names it by label -- which
/// in a pooled configuration is a pool MEMBER's own pinned ref. The returned
/// strings carry only the provider id and the operator's own seat label --
/// never token material, account data, or a storage path.
pub(super) fn gather_orphan_seats(config: &Config, seats: &[(String, TokenRecord)]) -> Vec<String> {
    let coverage = referenced_seat_coverage(config);
    let mut orphans: Vec<String> = seats
        .iter()
        .map(|(key, _)| key.as_str())
        .filter(|key| !coverage.covers(key))
        .map(str::to_string)
        .collect();
    orphans.sort();
    orphans
}

#[cfg(test)]
#[path = "gather_value_tests.rs"]
mod value_tests;
