//! Doctor data collection (context, probes, auth, secret classification).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use chrono::Local;
use routectl_auth::LocalProbe;
use routectl_auth::oauth::types::{TokenRecord, seat_key, unix_now};
use routectl_auth::{OAuthError, OAuthStore};
use routectl_auth::{SecretRef, default_secret_dir};
use routectl_core::ProbeOutcome;
use routectl_router::{
    CATALOG_VERSION, CatalogOverlay, Config, EffectiveRow, LearnedCapabilityRegistry, Source,
    catalog_import_state_default_path, derive_effective_view, load_last_import,
    rebuild_capabilities_into, today_epoch_day,
};

use crate::commands::capability_legacy::present_legacy_capability_keys;
use crate::commands::doctor_panels::compute_would_trim_panel;
use crate::commands::parse_error_redaction::redact_config_load_error;
use crate::commands::probe::{PROBE_DEADLINE, probe_all};
use crate::server::CompositeStore;
use crate::server::ledger_reader::{BoundaryOutcome, LedgerCapabilityReader, classify_boundary};

use super::{
    CapabilityConfig, CapabilityInputs, CapabilityMatrixSource, DoctorContext, FreshnessInputs,
    PriorCell,
};

/// The network doctor gather: the no-network context PLUS one upstream
/// reachability pass. Building the whole context in exactly ONE place
/// ([`gather_context_no_network`]) is what keeps the two entry points from
/// drifting -- a new context field is added once and both paths carry it; the
/// only difference between the paths is this `probe_results` assignment.
pub(super) async fn gather_context(config_path: &Path) -> DoctorContext {
    let ctx = gather_context_no_network(config_path).await;
    let probe_results = gather_probe_results(&ctx.config).await;
    DoctorContext {
        probe_results,
        ..ctx
    }
}

/// Gather every read-only input the no-network sections draw from, WITHOUT
/// any upstream dial: `probe_results` is left empty and
/// [`gather_probe_results`] (the only caller of `CompositeStore`/`probe_all`)
/// is never reached. Everything else -- per-layer config/overlay load, auth
/// via `probe_local` (no network, no refresh), secret presence checks, the
/// orphan scan, and the would-trim panel -- is retained.
pub async fn gather_context_no_network(config_path: &Path) -> DoctorContext {
    let raw_config = std::fs::read_to_string(config_path).ok();
    // Read-only, per-layer load: the config and the catalog overlay load
    // independently so the capability panel can degrade one without the
    // other. The version section keeps its coupled "config could not be
    // loaded" semantics -- a config parse error wins, else an overlay error
    // -- so a present-but-broken config still never reports all-Pass. On a
    // config failure the other sections run against defaults.
    let config_layer = crate::server::parse_config_only(config_path);
    let overlay_layer = crate::server::load_overlay_default();

    let (config, config_parse_error) = match config_layer {
        Ok(config) => (config, None),
        Err(e) => (Config::default(), Some(redact_config_load_error(&e))),
    };
    let config_load_error = config_parse_error.clone().or_else(|| {
        overlay_layer
            .as_ref()
            .err()
            .map(|e| redact_config_load_error(e))
    });

    // The learned matrix needs this run's revision to match the ledger's
    // replay boundary. The baked catalog version is fixed; the overlay
    // revision comes from the same read-only overlay load the priors use
    // (defaulting to zero when the overlay could not be read -- a foreign
    // boundary then classifies as unavailable, never a silent empty).
    let config_parse_failed = config_parse_error.is_some();
    let overlay_revision = overlay_layer
        .as_ref()
        .ok()
        .map_or(0, routectl_router::overlay_revision);

    let overlay = overlay_layer.ok();
    let overlay_verified_at = overlay
        .as_ref()
        .and_then(|overlay| freshest_overlay_verified_at(&config, overlay));
    let freshness = FreshnessInputs {
        catalog_version: CATALOG_VERSION,
        snapshot_date: routectl_router::CATALOG_SNAPSHOT_DATE,
        overlay_verified_at,
        staleness_hint_days: config.capability.staleness_hint_days,
        today_epoch_day: today_epoch_day(),
        last_import: load_last_import(&catalog_import_state_default_path()),
        import_result: None,
    };
    let capability = build_capability_inputs(&config, config_parse_error, overlay);
    let capability_matrix =
        gather_capability_matrix(&config, config_parse_failed, overlay_revision);

    let (probes, seats, auth_store_error) = gather_auth().await;
    let secret_checks = gather_secret_checks(&config, &probes);
    let orphan_secrets = gather_orphan_secrets(&config);
    let orphan_seats = gather_orphan_seats(&config, &seats);
    let would_trim = compute_would_trim_panel(&config, Local::now());

    DoctorContext {
        config,
        raw_config,
        config_load_error,
        probes,
        seats,
        auth_store_error,
        secret_checks,
        orphan_secrets,
        orphan_seats,
        probe_results: Vec::new(),
        would_trim,
        now_unix: unix_now(),
        binary_version: env!("CARGO_PKG_VERSION"),
        capability,
        capability_matrix,
        freshness,
    }
}

/// Rebuild the learned-capability matrix read-only from the usage ledger for
/// this run's revision, classifying availability as a first-class tri-state.
///
/// A config that would not parse yields `Unavailable("config_unavailable")`:
/// the usage db path and the revision knobs cannot be trusted, so an
/// empty-from-default read would misreport. Otherwise the replay boundary is
/// resolved against this run's baked catalog version + overlay revision and
/// either replayed into a bare, config-sized registry (`Available`, or honest
/// `Empty` on a matched-but-zero-row slice) or reported `Unavailable` with a
/// path-free class token. Read-only: the ledger is only ever opened
/// read-only, so the db is byte-identical afterward.
pub(super) fn gather_capability_matrix(
    config: &Config,
    config_parse_failed: bool,
    overlay_revision: u64,
) -> CapabilityMatrixSource {
    if config_parse_failed {
        return CapabilityMatrixSource::Unavailable("config_unavailable");
    }

    match classify_boundary(&config.usage.db_path, CATALOG_VERSION, overlay_revision) {
        BoundaryOutcome::Replay(tombstone) => {
            let reader = LedgerCapabilityReader::new(config.usage.db_path.clone(), tombstone);
            let registry = LearnedCapabilityRegistry::from_capability_config(&config.capability);
            let _ = rebuild_capabilities_into(&reader, &registry);
            let entries = registry.snapshot();
            if entries.is_empty() {
                CapabilityMatrixSource::Empty
            } else {
                CapabilityMatrixSource::Available {
                    entries,
                    now: reader.now(),
                    now_ms: reader.now_ms(),
                }
            }
        }
        BoundaryOutcome::Cold => CapabilityMatrixSource::Unavailable("no_data"),
        BoundaryOutcome::NoTombstone => CapabilityMatrixSource::Unavailable("no_tombstone"),
        BoundaryOutcome::RevisionMismatch => {
            CapabilityMatrixSource::Unavailable("revision_mismatch")
        }
        BoundaryOutcome::Unreadable(code) => CapabilityMatrixSource::Unavailable(code),
    }
}

/// Build the capability section's per-layer inputs. A config parse error
/// (already redacted) yields the "panel unavailable" state -- NOT an
/// empty-from-default-config view. Otherwise the legacy keys come from the
/// parsed config and the prior cells from the overlay (empty when the
/// overlay could not be read -- priors are then absent, while the matrix's
/// learned and override cells still render).
pub(super) fn build_capability_inputs(
    config: &Config,
    config_parse_error: Option<String>,
    overlay: Option<CatalogOverlay>,
) -> CapabilityInputs {
    if let Some(err) = config_parse_error {
        return CapabilityInputs {
            config: None,
            panel_unavailable: Some(err),
        };
    }

    let legacy_keys = present_legacy_capability_keys(config);
    let priors = overlay
        .map(|overlay| derive_prior_cells(config, &overlay))
        .unwrap_or_default();

    CapabilityInputs {
        config: Some(CapabilityConfig {
            legacy_keys,
            priors,
        }),
        panel_unavailable: None,
    }
}

/// Derive the catalog/overlay capability prior cells: one per `[models.X]`
/// entry whose resolved catalog row is `Present` AND carries capability
/// data, retaining its `verified_at` stamp. A `Missing` / `Disabled` cell or
/// a row with no capability keys yields NO prior -- the conservative
/// "unknown" baseline, never a fabricated row. Staleness is NOT filtered
/// here: the matrix panel flags a stale prior against the operator staleness
/// hint (via [`is_stale_days`]), so a stale-but-present stamp is surfaced
/// honestly rather than silently dropped.
pub(super) fn derive_prior_cells(config: &Config, overlay: &CatalogOverlay) -> Vec<PriorCell> {
    derive_effective_view(config, overlay)
        .models
        .into_iter()
        .filter_map(|cell| {
            let EffectiveRow::Present {
                row, verified_at, ..
            } = cell.row
            else {
                return None;
            };
            if row.capabilities.is_empty() {
                return None;
            }
            let capabilities = row.capabilities.into_iter().collect();
            Some(PriorCell {
                nickname: cell.nickname,
                verified_at,
                capabilities,
            })
        })
        .collect()
}

/// The freshest `verified_at` among the effective view's OVERLAY-sourced
/// cells (import / user, never baked). Reuses the same
/// [`derive_effective_view`] path the prior cells walk. Baked cells are
/// excluded on purpose: their stamp is the table-wide snapshot date already
/// shown in the baked row, so counting them would mask "running on baked
/// defaults" as a fresh overlay. `YYYY-MM-DD` stamps are zero-padded, so the
/// lexicographic max is the chronological max. `None` when no configured
/// model resolves to an overlay-verified cell.
pub(super) fn freshest_overlay_verified_at(
    config: &Config,
    overlay: &CatalogOverlay,
) -> Option<String> {
    derive_effective_view(config, overlay)
        .models
        .into_iter()
        .filter_map(|cell| match cell.row {
            EffectiveRow::Present {
                source,
                verified_at,
                ..
            } if source != Source::Baked => Some(verified_at),
            _ => None,
        })
        .max()
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
/// default store. Read-only: `probe_local` performs no network or refresh
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
            let mut probes = Vec::with_capacity(ids.len());
            for id in ids {
                probes.push((*id, store.probe_local(id).await));
            }
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
pub(super) fn sanitize_store_open_error(err: &OAuthError) -> String {
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
fn scheme_label(uri: &str) -> &'static str {
    if uri.starts_with("oauth://") {
        "oauth://"
    } else if uri.starts_with("env://") {
        "env://"
    } else if uri.starts_with("file://") {
        "file://"
    } else if uri.starts_with("literal:") {
        "literal:"
    } else {
        "unknown"
    }
}

/// Classify one secret ref without resolving its value or refreshing any
/// credential. `oauth://` reads the pre-gathered local probes (no network,
/// no token refresh); `env://` / `file://` / `literal:` do a non-mutating
/// existence / parse check.
fn classify_secret_ref(
    uri: &str,
    probes: &[(&'static str, LocalProbe)],
) -> (SecretPresence, Option<String>) {
    match SecretRef::parse(uri) {
        Ok(SecretRef::OAuth { provider, .. }) => {
            let presence = match probes.iter().find(|p| p.0 == provider.as_str()) {
                Some((_, LocalProbe::Present)) => SecretPresence::Present,
                Some((_, LocalProbe::Expired)) => SecretPresence::Expired,
                Some((_, LocalProbe::Missing)) => SecretPresence::Missing,
                Some((_, LocalProbe::StoreUnavailable)) => SecretPresence::StoreUnavailable,
                Some(_) | None => SecretPresence::UnknownOauthProvider,
            };
            (presence, Some(provider))
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
            let (presence, oauth_id) = classify_secret_ref(uri, probes);
            out.push(SecretCheck {
                provider: name.clone(),
                scheme,
                presence,
                oauth_id,
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

/// The `oauth://` coverage the config's provider entries express, split by
/// how each ref resolves at dispatch:
///
///   - `pooled_providers` -- providers named by a BARE `oauth://<provider>`
///     ref (label `None`). Such a ref is a POOL ref: the factory expands it
///     through `list_seats` into every stored seat of that provider, so a
///     bare ref covers the default seat AND every labelled sibling.
///   - `pinned_seats` -- full seat keys named by a LABELLED
///     `oauth://<provider>#<label>` ref. A labelled ref pins exactly that one
///     seat and covers no sibling, not even the default.
struct SeatCoverage {
    pooled_providers: BTreeSet<String>,
    pinned_seats: BTreeSet<String>,
}

fn referenced_seat_coverage(config: &Config) -> SeatCoverage {
    let mut pooled_providers = BTreeSet::new();
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
                    pooled_providers.insert(provider);
                }
            }
        }
    }
    SeatCoverage {
        pooled_providers,
        pinned_seats,
    }
}

/// Read-only diff of the STORED OAuth seats against the `oauth://` refs the
/// config expresses. Complements [`gather_orphan_secrets`], which covers
/// managed `file://` secrets only. Returns the seat keys (`<provider>` for the
/// default seat, `<provider>#<label>` for a labelled one) no provider entry
/// reaches; nothing is ever refreshed, rewritten, or removed.
///
/// Seat matching is by FULL seat identity, with pool expansion honored: a
/// labelled ref pins one seat, so the default seat of that provider is an
/// orphan unless something else reaches it; a bare pool ref reaches every
/// stored seat of its provider, so no sibling of a pooled provider is an
/// orphan. The returned strings carry only the provider id and the operator's
/// own seat label -- never token material, account data, or a storage path.
pub(super) fn gather_orphan_seats(config: &Config, seats: &[(String, TokenRecord)]) -> Vec<String> {
    let coverage = referenced_seat_coverage(config);
    let mut orphans: Vec<String> = seats
        .iter()
        .map(|(key, _)| key.as_str())
        .filter(|key| {
            let provider = key.split_once('#').map_or(*key, |(p, _)| p);
            !coverage.pooled_providers.contains(provider) && !coverage.pinned_seats.contains(*key)
        })
        .map(str::to_string)
        .collect();
    orphans.sort();
    orphans
}
