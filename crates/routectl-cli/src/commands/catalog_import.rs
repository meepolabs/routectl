//! `routectl catalog import` -- opt-in refresh of the catalog overlay from
//! the two vendored economics sources (litellm + models.dev), fetched live
//! or read from disk via `--litellm-file` / `--models-dev-file`.
//!
//! This is the CLI fetch boundary: `reqwest` lives here so
//! `routectl-router` stays reqwest-free (the pure candidate builder / diff
//! / shrink-guard machinery this module drives is
//! `routectl_router::catalog_import`). Never runs at startup -- an operator
//! invokes this explicitly.
//!
//! FLOW (load-bearing, matches `routectl_router::with_overlay_write_lock`'s
//! own lock-scope contract): fetch both sources -> build ONE candidate
//! against the overlay's revision at read time -> run the shrink guard ->
//! render the diff -> y/N confirm -> ACQUIRE the write lock, re-read, and
//! either save (revision unchanged) or release and recompute ONE fresh
//! diff+confirm against the latest overlay (a second revision change
//! aborts -- bounded, no retry loop). The lock is NEVER held across the
//! fetch or the confirm prompt.
//!
//! BOTH sources are required for every apply: a fetch failure on either
//! one (timeout, non-200, invalid JSON, or a top-level shape that is not a
//! JSON object) aborts before a candidate is even built, so the overlay is
//! never opened for writing on that path -- trivially byte-identical.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use routectl_router::{
    CandidateOrigin, CatalogOverlay, CatalogRow, ImpactClass, ImportCandidate, ImportDiff,
    OverlayError, ShrinkVerdict, SkipKind, baked_row_map, baked_shrink_counts,
    build_import_candidate, candidate_shrink_counts, catalog_import_state_default_path,
    diff_has_no_effective_change, diff_overlay, is_import_cell, load_catalog_import_baseline,
    load_catalog_overlay, overlay_default_path, persist_catalog_import_baseline, shrink_guard,
    with_overlay_write_lock,
};

const LITELLM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const MODELS_DEV_URL: &str = "https://models.dev/api.json";
/// Explicit per-attempt fetch timeout. Combined with [`fetch_url_with_retry`]'s
/// single retry, a fully unreachable source blocks for at most ~20s.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Hard cap on a fetched source's size. The vendored JSON snapshots this
/// module fetches are a few MB; a response many times that size (a
/// compromised or misbehaving vendor host) is rejected rather than buffered
/// into memory in full.
const MAX_FETCH_BYTES: u64 = 32 * 1024 * 1024;

/// `routectl catalog import` flags.
pub struct ImportArgs {
    /// Read the litellm source from disk instead of the network. Must be
    /// given together with `models_dev_file` (both, or neither) --
    /// primarily the test path against the vendored fixtures.
    pub litellm_file: Option<PathBuf>,
    pub models_dev_file: Option<PathBuf>,
    /// Skip the y/N confirmation prompt (scripting).
    pub yes: bool,
    /// Bypass ONLY the shrink guard's floors. Never bypasses a fetch
    /// failure, a cross-check skip, a `source: user` conflict, or a
    /// revision conflict.
    pub allow_shrink: bool,
}

/// Errors [`run`] can return. Every variant already carries an
/// operator-facing message; `main.rs` prints `Display` and exits non-zero.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error(
        "--litellm-file and --models-dev-file must be given together (both, or neither); mixing \
         one file source with the network is not supported"
    )]
    MixedFileFlags,

    #[error("fetch {source_name}: {reason}")]
    FetchFailed {
        source_name: &'static str,
        reason: String,
    },

    #[error(
        "import aborted: the shrink guard rejected this candidate (see the printed detail); \
         pass --allow-shrink to bypass the floors"
    )]
    Shrunk,

    #[error("import cancelled: not applied")]
    Cancelled,

    #[error(
        "import aborted: the overlay changed twice while confirming; re-run `catalog import` to \
         retry against the current overlay"
    )]
    RevisionConflictBounded,

    #[error(transparent)]
    Overlay(#[from] OverlayError),
}

/// `routectl catalog import` -- resolves the default overlay + baseline
/// paths and the real vendor URLs; see `run_at` for the testable core.
pub async fn run(args: &ImportArgs) -> Result<(), ImportError> {
    run_at(
        args,
        &overlay_default_path(),
        &catalog_import_state_default_path(),
        &SourceEndpoints::default_urls(),
    )
    .await
}

/// One fetched source: the parsed JSON plus its raw text (the latter only
/// for [`content_hash`]'s baseline fingerprint -- never re-parsed).
struct FetchedSource {
    value: Value,
    raw: String,
}

/// The two source URLs [`run_at`] fetches against. A struct (not two bare
/// `&str` args) so tests can point both at a `wiremock` server without
/// touching [`run`]'s production defaults.
struct SourceEndpoints {
    litellm_url: String,
    models_dev_url: String,
}

impl SourceEndpoints {
    fn default_urls() -> Self {
        Self {
            litellm_url: LITELLM_URL.to_string(),
            models_dev_url: MODELS_DEV_URL.to_string(),
        }
    }
}

/// Core of [`run`], taking the overlay path, baseline path, and source
/// URLs explicitly so tests can point every one of them at a temp
/// directory / mock server instead of the real network and
/// `catalog_overlay.json`.
async fn run_at(
    args: &ImportArgs,
    overlay_path: &Path,
    baseline_path: &Path,
    endpoints: &SourceEndpoints,
) -> Result<(), ImportError> {
    validate_file_flags(args)?;

    let verified_at = super::catalog::today_verified_at();
    tracing::info!(
        verified_at = %verified_at,
        litellm_mode = source_mode(args.litellm_file.as_deref()),
        models_dev_mode = source_mode(args.models_dev_file.as_deref()),
        "catalog import: start",
    );

    let client = build_http_client()?;
    let litellm = fetch_logged(
        &client,
        "litellm",
        &endpoints.litellm_url,
        args.litellm_file.as_deref(),
    )
    .await?;
    let models_dev = fetch_logged(
        &client,
        "models_dev",
        &endpoints.models_dev_url,
        args.models_dev_file.as_deref(),
    )
    .await?;

    let candidate = build_import_candidate(
        CandidateOrigin::DocRefresh,
        &litellm.value,
        &models_dev.value,
        &verified_at,
    );
    tracing::info!(
        skipped = candidate.skipped.len(),
        "catalog import: cross-check summary",
    );

    let baseline = load_catalog_import_baseline(baseline_path, baked_shrink_counts());
    let candidate_counts = candidate_shrink_counts(&candidate);
    let verdict = shrink_guard(&candidate_counts, &baseline);
    let disagreement_skips = candidate
        .skipped
        .iter()
        .filter(|skip| skip.kind == SkipKind::CrossCheckDisagreement)
        .count();
    log_shrink_verdict(&verdict, args.allow_shrink, disagreement_skips);
    if verdict.is_shrunk() {
        println!("shrink guard: the following source(s)/family(ies) fell below their floor:");
        print_shrink_verdict(&verdict, disagreement_skips);
        if !args.allow_shrink {
            return Err(ImportError::Shrunk);
        }
        println!("--allow-shrink: proceeding despite the shrink above.");
    }

    let baked = baked_row_map();
    let initial_overlay = load_catalog_overlay(overlay_path)?;
    let (diff, saved, wrote) = confirm_and_apply(
        overlay_path,
        &candidate,
        &baked,
        initial_overlay,
        args.yes,
        || {},
    )?;

    let mut source_hashes = BTreeMap::new();
    source_hashes.insert("litellm".to_string(), content_hash(&litellm.raw));
    source_hashes.insert("models_dev".to_string(), content_hash(&models_dev.raw));
    persist_catalog_import_baseline(
        baseline_path,
        &verified_at,
        &candidate_counts,
        source_hashes,
    );

    print_summary(&diff, &saved, wrote);
    Ok(())
}

const fn validate_file_flags(args: &ImportArgs) -> Result<(), ImportError> {
    if args.litellm_file.is_some() != args.models_dev_file.is_some() {
        return Err(ImportError::MixedFileFlags);
    }
    Ok(())
}

const fn source_mode(file: Option<&Path>) -> &'static str {
    if file.is_some() { "file" } else { "network" }
}

/// Build the fetch client. Redirect-following is DISABLED, matching every
/// egress client in `routectl-providers` (the per-lane policy table lives
/// in that crate's `http_client` module docs). This lane carries no
/// credentials, so nothing can leak on a followed hop -- but the two
/// sources are fixed public URLs that answer 200 directly, so a 3xx here
/// means the vendor host started steering the fetch somewhere else, which
/// is an operator-visible source failure rather than a hop to chase.
/// [`fetch_url_once`] maps the returned 3xx explicitly.
fn build_http_client() -> Result<reqwest::Client, ImportError> {
    reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| ImportError::FetchFailed {
            source_name: "http-client",
            reason: e.to_string(),
        })
}

/// Fetch one source (file or network) and log the outcome.
async fn fetch_logged(
    client: &reqwest::Client,
    source_name: &'static str,
    url: &str,
    file: Option<&Path>,
) -> Result<FetchedSource, ImportError> {
    match fetch_source(client, source_name, url, file).await {
        Ok(fetched) => {
            tracing::info!(
                source = source_name,
                outcome = "ok",
                "catalog import: fetch result"
            );
            Ok(fetched)
        }
        Err(e) => {
            tracing::warn!(
                source = source_name,
                outcome = "err",
                reason = %e,
                "catalog import: fetch result",
            );
            Err(e)
        }
    }
}

/// Fetch one source's raw JSON, from `file` when given, else over the
/// network with [`fetch_url_with_retry`]. Any failure -- I/O, network,
/// non-200, invalid JSON, or a non-object top-level shape (schema drift)
/// -- is a source-level failure: the caller never builds a candidate on
/// this path.
async fn fetch_source(
    client: &reqwest::Client,
    source_name: &'static str,
    url: &str,
    file: Option<&Path>,
) -> Result<FetchedSource, ImportError> {
    let raw = if let Some(path) = file {
        std::fs::read_to_string(path).map_err(|e| ImportError::FetchFailed {
            source_name,
            reason: format!("read {}: {e}", path.display()),
        })?
    } else {
        fetch_url_with_retry(client, url)
            .await
            .map_err(|reason| ImportError::FetchFailed {
                source_name,
                reason,
            })?
    };

    let value: Value = serde_json::from_str(&raw).map_err(|e| ImportError::FetchFailed {
        source_name,
        reason: format!("invalid JSON: {e}"),
    })?;
    if !value.is_object() {
        return Err(ImportError::FetchFailed {
            source_name,
            reason: "schema drift: expected a top-level JSON object".to_string(),
        });
    }

    Ok(FetchedSource { value, raw })
}

/// GET `url`, at most one retry beyond the initial attempt.
async fn fetch_url_with_retry(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let mut last_reason = String::new();
    for attempt in 0..2u8 {
        match fetch_url_once(client, url).await {
            Ok(text) => return Ok(text),
            Err(reason) => {
                last_reason = reason;
                if attempt == 0 {
                    continue;
                }
            }
        }
    }
    Err(last_reason)
}

async fn fetch_url_once(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let response = client.get(url).send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    // The client does not follow redirects, so reqwest hands the 3xx back as
    // a normal response. Name it explicitly -- the generic `HTTP {status}`
    // below would read as a vendor outage rather than "this source moved".
    if status.is_redirection() {
        return Err(format!(
            "HTTP {status}: source redirected; redirects are not followed"
        ));
    }
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    if let Some(len) = response.content_length()
        && len > MAX_FETCH_BYTES
    {
        return Err(format!(
            "response Content-Length {len} exceeds the {MAX_FETCH_BYTES}-byte fetch cap"
        ));
    }
    read_capped_body(response).await
}

/// Stream `response`'s body, rejecting mid-transfer the moment the total
/// read exceeds [`MAX_FETCH_BYTES`] -- catches a response whose
/// `Content-Length` is absent or understates the real body size (chunked
/// transfer, a lying proxy), which the header check in [`fetch_url_once`]
/// cannot see.
async fn read_capped_body(mut response: reqwest::Response) -> Result<String, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        body.extend_from_slice(&chunk);
        if body_cap_exceeded(body.len()) {
            return Err(format!(
                "response body exceeded the {MAX_FETCH_BYTES}-byte fetch cap"
            ));
        }
    }
    String::from_utf8(body).map_err(|e| e.to_string())
}

/// The running-total cap check [`read_capped_body`]'s streaming loop
/// applies after every chunk. Isolated as a pure function so the
/// mid-transfer enforcement is unit-testable without a live response --
/// constructing a `reqwest::Response` whose `Content-Length` under- or
/// mis-states its real body requires a live HTTP round trip, which
/// the `oversized_response_is_rejected_by_the_fetch_cap` test already
/// covers for the header-check path.
const fn body_cap_exceeded(total_len: usize) -> bool {
    total_len as u64 > MAX_FETCH_BYTES
}

fn log_shrink_verdict(verdict: &ShrinkVerdict, allow_shrink: bool, disagreement_skips: usize) {
    tracing::info!(
        shrunk = verdict.is_shrunk(),
        allow_shrink,
        bypassed = allow_shrink && verdict.is_shrunk(),
        shrunk_sources = verdict.shrunk_sources.len(),
        zero_sources = verdict.zero_sources.len(),
        shrunk_families = verdict.shrunk_families.len(),
        disagreement_skips,
        "catalog import: shrink guard",
    );
}

/// The operator-facing shrink-refusal report: one line per shrunk source
/// / family carrying its `baseline` / `candidate` / floor, plus a
/// trailing count of selectors skipped for an EXPECTED cross-check
/// disagreement -- those count as present toward the totals, so surfacing
/// them lets an operator tell a real shrink from source-refresh
/// disagreement noise.
fn shrink_verdict_report(verdict: &ShrinkVerdict, disagreement_skips: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for s in &verdict.zero_sources {
        lines.push(format!(
            "  source `{}` dropped to zero rows (baseline {})",
            s.source, s.baseline
        ));
    }
    for s in &verdict.shrunk_sources {
        lines.push(format!(
            "  source `{}` shrank to {} rows (baseline {})",
            s.source, s.candidate, s.baseline
        ));
    }
    for f in &verdict.shrunk_families {
        lines.push(format!(
            "  family `{}` shrank to {} rows (baseline {}, floor {})",
            f.family, f.candidate, f.baseline, f.required
        ));
    }
    lines.push(format!(
        "  cross-check disagreement skips (counted as present, not shrink): {disagreement_skips}"
    ));
    lines
}

fn print_shrink_verdict(verdict: &ShrinkVerdict, disagreement_skips: usize) {
    for line in shrink_verdict_report(verdict, disagreement_skips) {
        println!("{line}");
    }
}

// ---------------------------------------------------------------------------
// Diff -> confirm -> apply, with the bounded single re-diff on an
// interleaved overlay write.
// ---------------------------------------------------------------------------

/// Distinguishes the write-closure's intentional abort (the overlay's
/// revision moved since the diff was computed) from a genuine
/// [`OverlayError`] -- [`confirm_and_apply`] reacts very differently to
/// the two (recompute-and-retry vs. propagate).
enum ApplyError {
    Overlay(OverlayError),
    RevisionChanged(CatalogOverlay),
}

impl From<OverlayError> for ApplyError {
    fn from(e: OverlayError) -> Self {
        Self::Overlay(e)
    }
}

/// Merge `diff.applied`'s rows into the overlay loaded under the write
/// lock and REMOVE `diff.cleared`'s stale `source: import` cells,
/// aborting (without writing) if the overlay's revision no longer matches
/// `expected_revision` -- see [`ApplyError::RevisionChanged`].
/// `diff.skipped` / `diff.conflicted` are never written, by construction:
/// this function never reads them.
///
/// The revision guard already proves the overlay still holds what the
/// diff was computed against, but each clear re-checks `is_import_cell`
/// anyway: removal is destructive and must never take out a
/// `source: user` cell, so the guarantee is asserted at the write itself
/// rather than trusted across the two phases.
fn apply_diff(
    path: &Path,
    expected_revision: u64,
    diff: &ImportDiff,
) -> Result<CatalogOverlay, ApplyError> {
    with_overlay_write_lock::<ApplyError, _>(path, |overlay| {
        if overlay.revision != expected_revision {
            return Err(ApplyError::RevisionChanged(overlay));
        }
        let mut next = overlay;
        for row in &diff.applied {
            next.cells
                .insert(row.selector.clone(), Some(row.candidate.clone()));
        }
        for selector in &diff.cleared {
            if is_import_cell(&next, selector) {
                next.cells.remove(selector);
            }
        }
        Ok(next)
    })
}

/// Diff `candidate` against `initial_overlay`, render + confirm, then
/// apply. On a revision conflict at apply time, recomputes ONE fresh diff
/// against the overlay [`apply_diff`] actually saw and confirms again; a
/// SECOND conflict aborts (bounded, no retry loop) -- the verbatim
/// lock-scope flow this module's doc describes. When the confirmed diff
/// carries no EFFECTIVE change (see
/// [`routectl_router::diff_has_no_effective_change`] -- either
/// `applied` is empty, or every applied row is byte-identical to what the
/// overlay already carries for that selector, the byte-identical
/// re-import case), the write lock is never acquired: instead, a cheap
/// unlocked re-read of `overlay_path` confirms nothing has changed since
/// the diff was computed (the confirm window the lock never covers). A
/// revision match there means the no-op verdict is still valid, so the
/// lock is skipped entirely -- acquiring it anyway would still bump the
/// overlay's revision (and fire a hot-reload watch) for zero content
/// change. A revision MISMATCH there is treated exactly like a
/// [`ApplyError::RevisionChanged`] at apply time: one fresh diff is
/// recomputed and confirmed again against the re-read overlay, sharing
/// the SAME bounded single-retry budget as the write path (a second
/// mismatch of EITHER kind aborts). The returned `bool` is `true`
/// exactly when a write actually landed -- callers use it to report "no
/// changes" instead of a written count, and to skip the reload pickup
/// note.
///
/// `before_apply` runs once per confirmed attempt, right after
/// confirmation and right before this function acts on the diff (the
/// unlocked no-op re-read, or the write-lock acquisition): production
/// passes a no-op; tests use it to inject a concurrent write into the
/// confirm window the lock never covers.
fn confirm_and_apply(
    overlay_path: &Path,
    candidate: &ImportCandidate,
    baked: &BTreeMap<String, CatalogRow>,
    initial_overlay: CatalogOverlay,
    yes: bool,
    mut before_apply: impl FnMut(),
) -> Result<(ImportDiff, CatalogOverlay, bool), ImportError> {
    let mut overlay = initial_overlay;
    let mut attempt: u8 = 0;
    loop {
        let diff = diff_overlay(&overlay, candidate, baked);
        tracing::info!(
            attempt,
            applied = diff.applied.len(),
            skipped = diff.skipped.len(),
            conflicted = diff.conflicted.len(),
            cleared = diff.cleared.len(),
            "catalog import: diff summary",
        );
        print_diff(&diff);

        if !confirm(yes) {
            return Err(ImportError::Cancelled);
        }

        before_apply();

        if diff_has_no_effective_change(&diff) {
            let on_disk = load_catalog_overlay(overlay_path)?;
            if on_disk.revision == overlay.revision {
                // Nothing to write: skip the lock entirely rather than pay
                // for a no-op merge that would still bump the overlay's
                // revision (`with_overlay_write_lock`'s `save` always
                // writes revision + 1 once the closure returns `Ok`, even
                // with unchanged cell content). The unlocked re-read just
                // above proves no concurrent writer moved the overlay out
                // from under this verdict, so it is safe to trust.
                tracing::info!(attempt, outcome = "noop", "catalog import: commit result",);
                return Ok((diff, overlay, false));
            }
            attempt += 1;
            if attempt >= 2 {
                return Err(ImportError::RevisionConflictBounded);
            }
            println!(
                "note: the overlay changed since this diff was shown; recomputing one \
                 fresh diff..."
            );
            overlay = on_disk;
            continue;
        }

        let expected_revision = overlay.revision;
        let started = Instant::now();
        match apply_diff(overlay_path, expected_revision, &diff) {
            Ok(saved) => {
                log_commit_result(expected_revision, Some(saved.revision), started.elapsed());
                return Ok((diff, saved, true));
            }
            Err(ApplyError::RevisionChanged(fresh)) => {
                log_commit_result(expected_revision, None, started.elapsed());
                attempt += 1;
                if attempt >= 2 {
                    return Err(ImportError::RevisionConflictBounded);
                }
                println!(
                    "note: the overlay changed since this diff was shown; recomputing one \
                     fresh diff..."
                );
                overlay = fresh;
            }
            Err(ApplyError::Overlay(e)) => return Err(e.into()),
        }
    }
}

fn log_commit_result(expected_revision: u64, saved_revision: Option<u64>, lock_wait: Duration) {
    tracing::info!(
        expected_revision,
        saved_revision = ?saved_revision,
        lock_wait_ms = lock_wait.as_millis() as u64,
        outcome = if saved_revision.is_some() { "committed" } else { "revision_conflict" },
        "catalog import: commit result",
    );
}

fn confirm(yes: bool) -> bool {
    if yes {
        return true;
    }
    use std::io::{IsTerminal as _, Write as _};
    // A non-interactive caller with an open-but-silent stdin (a pipe that
    // never sends a line or EOF) would otherwise block `read_line`
    // forever. With no TTY there is no one to answer the prompt, so
    // decline immediately -- the documented non-interactive contract is
    // `--force`.
    if !std::io::stdin().is_terminal() {
        return false;
    }
    print!("apply this import? [y/N] ");
    let _ = std::io::stdout().flush();
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

// ---------------------------------------------------------------------------
// Diff rendering: reuses `commands::catalog::render_table`.
// ---------------------------------------------------------------------------

const DIFF_HEADER: &[&str] = &[
    "selector",
    "impact",
    "cheaper?",
    "wm",
    "rm",
    "ttl(s)",
    "min_prefix",
    "max_ctx",
    "note",
];

fn print_diff(diff: &ImportDiff) {
    println!("applied ({}):", diff.applied.len());
    print_rows_or_none(&diff.applied, |_| "-");

    println!("\nskipped: source disagreement ({}):", diff.skipped.len());
    if diff.skipped.is_empty() {
        println!("  (none)");
    } else {
        for skip in &diff.skipped {
            println!("  {}  {}", skip.selector, skip.reason);
        }
    }

    println!("\nconflicted ({}):", diff.conflicted.len());
    print_rows_or_none(&diff.conflicted, conflict_note);

    println!(
        "\ncleared: stale import cell removed, baked value restored ({}):",
        diff.cleared.len()
    );
    if diff.cleared.is_empty() {
        println!("  (none)");
    } else {
        for selector in &diff.cleared {
            println!("  {selector}");
        }
    }
}

fn print_rows_or_none(
    rows: &[routectl_router::DiffRow],
    note_for: impl Fn(&routectl_router::DiffRow) -> &'static str,
) {
    if rows.is_empty() {
        println!("  (none)");
        return;
    }
    print!(
        "{}",
        super::catalog::render_table(&diff_table(rows, note_for))
    );
}

fn diff_table(
    rows: &[routectl_router::DiffRow],
    note_for: impl Fn(&routectl_router::DiffRow) -> &'static str,
) -> Vec<Vec<String>> {
    let mut table = vec![DIFF_HEADER.iter().map(|s| s.to_string()).collect()];
    for row in rows {
        table.push(vec![
            row.selector.clone(),
            row.impact.label().to_string(),
            if row.cheaper_direction {
                "cheaper"
            } else {
                "-"
            }
            .to_string(),
            opt_f32(row.candidate.wm),
            opt_f32(row.candidate.rm),
            opt_u32(row.candidate.ttl_seconds),
            opt_u32(row.candidate.min_prefix_tokens),
            opt_u32(row.candidate.max_context_tokens),
            note_for(row).to_string(),
        ]);
    }
    table
}

/// Conflicted-row note: a `source: user` cell that already matches every
/// value field the candidate would set (impact stayed display-only, and
/// the row is not an explicit disable) reads as "identical" rather than
/// "conflicted" -- the operator's own value happens to already be what
/// the import would have written.
fn conflict_note(row: &routectl_router::DiffRow) -> &'static str {
    let unchanged = row.impact == ImpactClass::DisplayOnly
        && !matches!(row.existing, routectl_router::ExistingCell::Disabled);
    if unchanged {
        "identical (user cell preserved)"
    } else {
        "conflicted (user cell preserved)"
    }
}

fn opt_f32(v: Option<f32>) -> String {
    v.map_or_else(|| "-".to_string(), |n| format!("{n:.4}"))
}

fn opt_u32(v: Option<u32>) -> String {
    v.map_or_else(|| "-".to_string(), |n| n.to_string())
}

fn print_summary(diff: &ImportDiff, saved: &CatalogOverlay, wrote: bool) {
    println!("\n{}", summary_line(diff, saved, wrote));
    if wrote {
        super::catalog::print_pickup_note();
    }
}

/// The one-line commit summary [`print_summary`] prints. Pure (returns a
/// `String` rather than printing directly) so the "no changes" vs.
/// "applied" wording is unit-testable without capturing stdout. `wrote`
/// is `false` exactly when [`confirm_and_apply`] took its no-effective-
/// change path (see its doc) -- the write lock was never acquired, so
/// `saved` is whatever overlay [`confirm_and_apply`] last saw rather than
/// a freshly-persisted one, and no reload pickup note follows.
fn summary_line(diff: &ImportDiff, saved: &CatalogOverlay, wrote: bool) -> String {
    if wrote {
        format!(
            "import applied: {} selector(s) written, {} cleared, {} skipped, {} conflicted; \
             overlay revision is now {}.",
            diff.applied.len(),
            diff.cleared.len(),
            diff.skipped.len(),
            diff.conflicted.len(),
            saved.revision,
        )
    } else {
        format!(
            "import: no changes ({} selector(s) already up to date, {} skipped, {} conflicted); \
             overlay revision remains {}.",
            diff.applied.len(),
            diff.skipped.len(),
            diff.conflicted.len(),
            saved.revision,
        )
    }
}

/// A cheap, non-cryptographic content fingerprint for
/// `catalog_import_state.json`'s `source_hashes` -- purely an
/// observability aid (see `routectl_router::CatalogImportState`'s
/// non-behavioral posture), so `DefaultHasher` is deliberately preferred
/// over pulling in a real hash crate for this.
fn content_hash(raw: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    raw.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    use routectl_router::{
        OverlayCell, OverlaySource, persist_catalog_import_baseline, save_catalog_overlay,
    };
    use serial_test::serial;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn confirm_declines_immediately_on_non_tty_without_yes() {
        // Under the test harness stdin is not a TTY, so the terminal gate
        // must fire and decline WITHOUT reaching read_line -- a silent
        // pipe can no longer hang the prompt.
        use std::io::IsTerminal as _;
        assert!(
            !std::io::stdin().is_terminal(),
            "test harness stdin must be non-interactive for this assertion",
        );
        assert!(!confirm(false), "non-TTY without --force must decline");
        assert!(confirm(true), "--force must still proceed byte-identically");
    }

    const LITELLM_FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../routectl-router/catalog_data/litellm_model_prices_and_context_window.json"
    );
    const MODELS_DEV_FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../routectl-router/catalog_data/models_dev.json"
    );

    /// Copy the two vendored fixtures into `dir`, returning their new
    /// paths. Tests must never hand-edit the checked-in `catalog_data/`
    /// files, so every file-path test works off a disposable copy.
    fn copy_fixtures(dir: &std::path::Path) -> (PathBuf, PathBuf) {
        let litellm = dir.join("litellm.json");
        let models_dev = dir.join("models_dev.json");
        std::fs::copy(LITELLM_FIXTURE, &litellm).expect("copy litellm fixture");
        std::fs::copy(MODELS_DEV_FIXTURE, &models_dev).expect("copy models.dev fixture");
        (litellm, models_dev)
    }

    fn file_args(litellm: PathBuf, models_dev: PathBuf, allow_shrink: bool) -> ImportArgs {
        ImportArgs {
            litellm_file: Some(litellm),
            models_dev_file: Some(models_dev),
            yes: true,
            allow_shrink,
        }
    }

    fn noop_endpoints() -> SourceEndpoints {
        // Never reached when both file flags are set; a placeholder value
        // keeps `run_at`'s signature uniform across file- and
        // network-sourced tests.
        SourceEndpoints {
            litellm_url: "http://127.0.0.1:0/unused".to_string(),
            models_dev_url: "http://127.0.0.1:0/unused".to_string(),
        }
    }

    // -----------------------------------------------------------------------
    // Shrink-refusal report content.
    // -----------------------------------------------------------------------

    #[test]
    fn shrink_verdict_report_carries_per_family_counts_and_a_disagreement_skip_count() {
        // Arrange: one shrunk family plus 2 cross-check disagreement skips.
        let verdict = ShrinkVerdict {
            shrunk_families: vec![routectl_router::ShrunkFamily {
                family: "anthropic-api".to_string(),
                baseline: 3,
                candidate: 2,
                required: 3,
            }],
            ..ShrinkVerdict::default()
        };

        // Act
        let report = shrink_verdict_report(&verdict, 2);

        // Assert: the family line carries baseline / candidate / floor,
        // and a distinct line reports the disagreement-skip count.
        let joined = report.join("\n");
        assert!(
            joined.contains("family `anthropic-api`")
                && joined.contains("baseline 3")
                && joined.contains("floor 3")
                && joined.contains("2 rows"),
            "family line missing counts: {joined}"
        );
        assert!(
            joined.contains("cross-check disagreement skips") && joined.contains(": 2"),
            "disagreement-skip count missing: {joined}"
        );
    }

    // -----------------------------------------------------------------------
    // --file happy flow against the vendored fixtures.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn file_sourced_import_applies_against_an_empty_overlay() {
        // Arrange: seed a baseline that exactly matches the real
        // fixture-derived candidate's own counts -- the import path's
        // empty-allowlist cross-check strictness can legitimately skip a
        // selector the codegen path's vendored allowlist resolves (see
        // the shrink-guard tests below for that behavior in isolation),
        // so this happy-path test isolates "does a file-sourced import
        // apply" from "does the shrink guard flag first-run drift".
        let dir = tempfile::tempdir().unwrap();
        let real_candidate = real_candidate_from_fixtures(dir.path());
        let baseline_path = dir.path().join("catalog_import_state.json");
        inflated_baseline(&baseline_path, &real_candidate, 1);

        let (litellm, models_dev) = copy_fixtures(dir.path());
        let overlay_path = dir.path().join("catalog_overlay.json");
        let args = file_args(litellm, models_dev, false);

        // Act
        let result = run_at(&args, &overlay_path, &baseline_path, &noop_endpoints()).await;

        // Assert
        result.expect("file-sourced import must succeed");
        let overlay = load_catalog_overlay(&overlay_path).expect("load");
        assert!(overlay.revision >= 1, "the import must have written cells");
        assert!(
            !overlay.cells.is_empty(),
            "at least one selector must have been admitted"
        );
    }

    #[tokio::test]
    async fn a_byte_identical_reimport_through_run_at_does_not_bump_the_overlay_revision() {
        // Arrange: run the same file-sourced import twice against the same
        // unchanged fixtures. The first run persists a baseline that
        // exactly matches the real candidate's own counts (see the sibling
        // happy-path test's doc for why), so the second run's shrink guard
        // stays healthy too.
        let dir = tempfile::tempdir().unwrap();
        let real_candidate = real_candidate_from_fixtures(dir.path());
        let baseline_path = dir.path().join("catalog_import_state.json");
        inflated_baseline(&baseline_path, &real_candidate, 1);

        let (litellm, models_dev) = copy_fixtures(dir.path());
        let overlay_path = dir.path().join("catalog_overlay.json");

        // Act: first import writes the candidate's cells.
        run_at(
            &file_args(litellm.clone(), models_dev.clone(), false),
            &overlay_path,
            &baseline_path,
            &noop_endpoints(),
        )
        .await
        .expect("first import must succeed");
        let after_first = load_catalog_overlay(&overlay_path).expect("load after first import");
        let revision_after_first = after_first.revision;
        let bytes_after_first = std::fs::read(&overlay_path).unwrap();

        // Act: an immediate re-import of the exact same fixtures.
        run_at(
            &file_args(litellm, models_dev, false),
            &overlay_path,
            &baseline_path,
            &noop_endpoints(),
        )
        .await
        .expect("a byte-identical re-import must still succeed");

        // Assert: the overlay is byte-unchanged -- no revision bump, no
        // write at all -- for zero content change.
        let bytes_after_second = std::fs::read(&overlay_path).unwrap();
        assert_eq!(
            bytes_after_first, bytes_after_second,
            "a byte-identical re-import must leave the overlay file byte-unchanged"
        );
        let after_second = load_catalog_overlay(&overlay_path).expect("load after second import");
        assert_eq!(after_second.revision, revision_after_first);
    }

    #[tokio::test]
    async fn mixed_file_flags_are_rejected_before_any_fetch() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let (litellm, _models_dev) = copy_fixtures(dir.path());
        let args = ImportArgs {
            litellm_file: Some(litellm),
            models_dev_file: None,
            yes: true,
            allow_shrink: false,
        };
        let overlay_path = dir.path().join("catalog_overlay.json");
        let baseline_path = dir.path().join("catalog_import_state.json");

        // Act
        let err = run_at(&args, &overlay_path, &baseline_path, &noop_endpoints())
            .await
            .expect_err("mixed file flags must be rejected");

        // Assert
        assert!(matches!(err, ImportError::MixedFileFlags));
        assert!(!overlay_path.exists(), "nothing should have been touched");
    }

    // -----------------------------------------------------------------------
    // Fetch failure: overlay byte-identical, no candidate ever built.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn source_fetch_failure_aborts_and_leaves_the_overlay_byte_identical() {
        // Arrange: seed a non-empty overlay so a byte-for-byte comparison
        // is meaningful, then point litellm at a mock that always 500s.
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let baseline_path = dir.path().join("catalog_import_state.json");
        let mut seed = BTreeMap::new();
        seed.insert(
            "openai-compat:grok-*".to_string(),
            Some(OverlayCell {
                source: OverlaySource::User,
                verified_at: "2026-01-01".to_string(),
                wm: Some(1.5),
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                max_output_tokens: None,
                input_cost_per_token: None,
                output_cost_per_token: None,
                capabilities: None,
            }),
        );
        save_catalog_overlay(&overlay_path, 0, seed).expect("seed overlay");
        let before = std::fs::read(&overlay_path).unwrap();

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/litellm.json"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/models_dev.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;
        let endpoints = SourceEndpoints {
            litellm_url: format!("{}/litellm.json", server.uri()),
            models_dev_url: format!("{}/models_dev.json", server.uri()),
        };
        let args = ImportArgs {
            litellm_file: None,
            models_dev_file: None,
            yes: true,
            allow_shrink: false,
        };

        // Act
        let err = run_at(&args, &overlay_path, &baseline_path, &endpoints)
            .await
            .expect_err("a 500 from litellm must abort the import");

        // Assert
        assert!(matches!(
            err,
            ImportError::FetchFailed {
                source_name: "litellm",
                ..
            }
        ));
        let after = std::fs::read(&overlay_path).unwrap();
        assert_eq!(before, after, "the overlay must be byte-identical");
        assert!(
            !baseline_path.exists(),
            "the baseline must not be touched on a fetch failure"
        );
    }

    // -----------------------------------------------------------------------
    // Fetch size cap: a response over MAX_FETCH_BYTES is rejected.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn oversized_response_is_rejected_by_the_fetch_cap() {
        // Arrange: a body genuinely larger than the cap, so the mock
        // server's own Content-Length header (computed from the body)
        // already exceeds it -- the early header check must reject this
        // before the body is ever streamed.
        let server = MockServer::start().await;
        let oversized_body = vec![b'a'; (MAX_FETCH_BYTES + 1) as usize];
        Mock::given(method("GET"))
            .and(path("/oversized.json"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(oversized_body))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();

        // Act
        let err = fetch_url_once(&client, &format!("{}/oversized.json", server.uri()))
            .await
            .expect_err("a response over the fetch cap must be rejected");

        // Assert
        assert!(err.contains("fetch cap"), "err: {err}");
        assert!(err.contains("Content-Length"), "err: {err}");
    }

    /// A no-redirect client hands a 3xx back as a normal response, so the
    /// fetch path must name it: a bare `HTTP 301` would read as a vendor
    /// outage, and without the explicit arm a 3xx body would fall through
    /// to the JSON parse and surface as "invalid JSON".
    #[tokio::test]
    async fn redirect_response_is_rejected_with_a_named_reason() {
        // Arrange
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/moved.json"))
            .respond_with(
                ResponseTemplate::new(301)
                    .insert_header("location", "https://example.com/new.json"),
            )
            .mount(&server)
            .await;
        let client = build_http_client().expect("client build must not fail");

        // Act
        let err = fetch_url_once(&client, &format!("{}/moved.json", server.uri()))
            .await
            .expect_err("a 3xx must be rejected rather than followed or parsed");

        // Assert
        assert!(err.contains("301"), "the real status must survive: {err}");
        assert!(
            err.contains("redirects are not followed"),
            "the reason must name the redirect: {err}"
        );
    }

    #[test]
    fn body_cap_exceeded_flags_only_once_the_running_total_passes_the_cap() {
        // The mid-transfer enforcement `read_capped_body` applies after
        // every chunk -- a response whose `Content-Length` is absent or
        // understates the real size (chunked transfer, a lying proxy)
        // relies on exactly this check, not the header short-circuit the
        // test above exercises.
        assert!(!body_cap_exceeded(MAX_FETCH_BYTES as usize));
        assert!(body_cap_exceeded(MAX_FETCH_BYTES as usize + 1));
    }

    // -----------------------------------------------------------------------
    // Shrink guard: abort by default, proceed with --allow-shrink.
    // -----------------------------------------------------------------------

    fn inflated_baseline(
        baseline_path: &std::path::Path,
        real_candidate: &ImportCandidate,
        multiplier: usize,
    ) {
        let real_counts = candidate_shrink_counts(real_candidate);
        let inflated = routectl_router::ShrinkCounts {
            per_source: real_counts
                .per_source
                .into_iter()
                .map(|(k, v)| (k, v * multiplier))
                .collect(),
            per_family: real_counts
                .per_family
                .into_iter()
                .map(|(k, v)| (k, v * multiplier))
                .collect(),
        };
        persist_catalog_import_baseline(baseline_path, "2026-01-01", &inflated, BTreeMap::new());
    }

    fn real_candidate_from_fixtures(dir: &std::path::Path) -> ImportCandidate {
        let (litellm_path, models_dev_path) = copy_fixtures(dir);
        let litellm: Value =
            serde_json::from_str(&std::fs::read_to_string(litellm_path).unwrap()).unwrap();
        let models_dev: Value =
            serde_json::from_str(&std::fs::read_to_string(models_dev_path).unwrap()).unwrap();
        build_import_candidate(
            CandidateOrigin::DocRefresh,
            &litellm,
            &models_dev,
            "2026-07-11",
        )
    }

    #[tokio::test]
    async fn shrink_guard_aborts_by_default_and_proceeds_with_allow_shrink() {
        // Arrange: an artificially inflated baseline (10x the real
        // fixture-derived candidate) guarantees every source/family trips
        // the shrink guard's floor.
        let dir = tempfile::tempdir().unwrap();
        let real_candidate = real_candidate_from_fixtures(dir.path());
        let baseline_path = dir.path().join("catalog_import_state.json");
        inflated_baseline(&baseline_path, &real_candidate, 10);

        let (litellm, models_dev) = copy_fixtures(dir.path());
        let overlay_path = dir.path().join("catalog_overlay.json");

        // Act: default (no --allow-shrink) must abort.
        let args = file_args(litellm.clone(), models_dev.clone(), false);
        let err = run_at(&args, &overlay_path, &baseline_path, &noop_endpoints())
            .await
            .expect_err("an inflated baseline must trip the shrink guard");
        assert!(matches!(err, ImportError::Shrunk));
        assert!(!overlay_path.exists(), "no write on a shrink abort");

        // Act: --allow-shrink must proceed to a normal apply.
        let args = file_args(litellm, models_dev, true);
        run_at(&args, &overlay_path, &baseline_path, &noop_endpoints())
            .await
            .expect("--allow-shrink must bypass the floor and apply");
        assert!(overlay_path.exists());
    }

    // -----------------------------------------------------------------------
    // User conflicts are diffed but never written.
    // -----------------------------------------------------------------------

    #[test]
    fn confirm_and_apply_never_writes_a_conflicted_selector() {
        // Arrange: a candidate targeting a selector the overlay already
        // carries as a `source: user` cell with a different wm.
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let selector = "anthropic-api:claude-opus-4-8*".to_string();
        let mut seed = BTreeMap::new();
        seed.insert(
            selector.clone(),
            Some(OverlayCell {
                source: OverlaySource::User,
                verified_at: "2026-01-01".to_string(),
                wm: Some(9.99),
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                max_output_tokens: None,
                input_cost_per_token: None,
                output_cost_per_token: None,
                capabilities: None,
            }),
        );
        save_catalog_overlay(&overlay_path, 0, seed).expect("seed overlay");
        let initial_overlay = load_catalog_overlay(&overlay_path).expect("reload seeded overlay");
        let initial_revision = initial_overlay.revision;
        assert_eq!(initial_revision, 1);

        let mut cells = BTreeMap::new();
        cells.insert(
            selector.clone(),
            OverlayCell {
                source: OverlaySource::Import,
                verified_at: "2026-07-11".to_string(),
                wm: Some(1.0),
                rm: Some(0.1),
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                max_output_tokens: None,
                input_cost_per_token: None,
                output_cost_per_token: None,
                capabilities: None,
            },
        );
        let candidate = ImportCandidate {
            origin: CandidateOrigin::DocRefresh,
            verified_at: "2026-07-11".to_string(),
            cells,
            skipped: Vec::new(),
        };
        let baked = baked_row_map();

        // Act
        let (diff, saved, wrote) = confirm_and_apply(
            &overlay_path,
            &candidate,
            &baked,
            initial_overlay,
            true,
            || {},
        )
        .expect("confirm_and_apply must still succeed with a purely-conflicted diff");

        // Assert: the row is reported as conflicted, and the overlay's own
        // cell for that selector is completely untouched (still the
        // original user wm, still revision 1 -- no write occurred at all
        // because `diff.applied` was empty).
        assert_eq!(diff.applied.len(), 0);
        assert_eq!(diff.conflicted.len(), 1);
        assert_eq!(saved.revision, initial_revision);
        assert!(!wrote, "an empty applied set must never write");
        let reloaded = load_catalog_overlay(&overlay_path).expect("reload");
        let cell = reloaded
            .cells
            .get(&selector)
            .and_then(Option::as_ref)
            .expect("cell still present");
        assert_eq!(cell.source, OverlaySource::User);
        assert_eq!(cell.wm, Some(9.99), "the user's value must be preserved");
    }

    // -----------------------------------------------------------------------
    // Byte-identical re-import: the no-effective-change guard skips the
    // write (and the revision bump), even though the row still sorts into
    // `applied` (its selector's existing cell is `source: import`).
    // -----------------------------------------------------------------------

    #[test]
    fn confirm_and_apply_skips_the_write_on_a_byte_identical_reimport() {
        // Arrange: seed the overlay with exactly the cell `opus_candidate`
        // would (re-)derive -- same source, verified_at, wm, and rm --
        // simulating a same-day re-import of unchanged upstream data.
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let selector = "anthropic-api:claude-opus-4-8*".to_string();
        let mut seed = BTreeMap::new();
        seed.insert(
            selector.clone(),
            Some(OverlayCell {
                source: OverlaySource::Import,
                verified_at: "2026-07-11".to_string(),
                wm: Some(1.0),
                rm: Some(0.1),
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                max_output_tokens: None,
                input_cost_per_token: None,
                output_cost_per_token: None,
                capabilities: None,
            }),
        );
        save_catalog_overlay(&overlay_path, 0, seed).expect("seed overlay");
        let initial_overlay = load_catalog_overlay(&overlay_path).expect("reload seeded overlay");
        let initial_revision = initial_overlay.revision;
        let candidate = opus_candidate();
        let baked = baked_row_map();

        // Act
        let (diff, saved, wrote) = confirm_and_apply(
            &overlay_path,
            &candidate,
            &baked,
            initial_overlay,
            true,
            || {},
        )
        .expect("a byte-identical re-import must still succeed");

        // Assert: the row is classified applied (an import cell is always
        // eligible), but nothing was actually written -- no revision bump.
        assert_eq!(diff.applied.len(), 1, "still classified as applied");
        assert!(!wrote, "a byte-identical re-import must not write");
        assert_eq!(saved.revision, initial_revision, "revision must not bump");
        let reloaded = load_catalog_overlay(&overlay_path).expect("reload");
        assert_eq!(
            reloaded.revision, initial_revision,
            "on-disk revision must not bump either"
        );
        let cell = reloaded
            .cells
            .get(&selector)
            .and_then(Option::as_ref)
            .expect("cell still present");
        assert_eq!(cell.wm, Some(1.0));
    }

    #[test]
    fn confirm_and_apply_still_writes_and_bumps_when_one_field_actually_changed() {
        // Arrange: the existing import cell's wm differs from the
        // candidate's -- a real change, not a re-import no-op.
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let selector = "anthropic-api:claude-opus-4-8*".to_string();
        let mut seed = BTreeMap::new();
        seed.insert(
            selector,
            Some(OverlayCell {
                source: OverlaySource::Import,
                verified_at: "2026-07-11".to_string(),
                wm: Some(1.5),
                rm: Some(0.1),
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                max_output_tokens: None,
                input_cost_per_token: None,
                output_cost_per_token: None,
                capabilities: None,
            }),
        );
        save_catalog_overlay(&overlay_path, 0, seed).expect("seed overlay");
        let initial_overlay = load_catalog_overlay(&overlay_path).expect("reload seeded overlay");
        let initial_revision = initial_overlay.revision;
        let candidate = opus_candidate();
        let baked = baked_row_map();

        // Act
        let (diff, saved, wrote) = confirm_and_apply(
            &overlay_path,
            &candidate,
            &baked,
            initial_overlay,
            true,
            || {},
        )
        .expect("a real change must apply");

        // Assert
        assert_eq!(diff.applied.len(), 1);
        assert!(wrote, "a changed field must write");
        assert_eq!(saved.revision, initial_revision + 1);
    }

    #[test]
    fn confirm_and_apply_treats_a_verified_at_only_change_as_a_real_change_when_the_date_differs() {
        // Arrange: every value field agrees with the candidate, but the
        // existing cell's verified_at is an earlier date -- a re-import on
        // a LATER day must still bump the revision, per the module's
        // no-effective-change contract.
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let selector = "anthropic-api:claude-opus-4-8*".to_string();
        let mut seed = BTreeMap::new();
        seed.insert(
            selector.clone(),
            Some(OverlayCell {
                source: OverlaySource::Import,
                verified_at: "2020-01-01".to_string(),
                wm: Some(1.0),
                rm: Some(0.1),
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                max_output_tokens: None,
                input_cost_per_token: None,
                output_cost_per_token: None,
                capabilities: None,
            }),
        );
        save_catalog_overlay(&overlay_path, 0, seed).expect("seed overlay");
        let initial_overlay = load_catalog_overlay(&overlay_path).expect("reload seeded overlay");
        let initial_revision = initial_overlay.revision;
        let candidate = opus_candidate();
        let baked = baked_row_map();

        // Act
        let (_, saved, wrote) = confirm_and_apply(
            &overlay_path,
            &candidate,
            &baked,
            initial_overlay,
            true,
            || {},
        )
        .expect("a verified_at-only change on a later day must apply");

        // Assert
        assert!(
            wrote,
            "a moved verified_at date must count as a real change"
        );
        assert_eq!(saved.revision, initial_revision + 1);
        let reloaded = load_catalog_overlay(&overlay_path).expect("reload");
        let cell = reloaded
            .cells
            .get(&selector)
            .and_then(Option::as_ref)
            .expect("cell still present");
        assert_eq!(cell.verified_at, "2026-07-11");
    }

    // -----------------------------------------------------------------------
    // Bounded single re-diff on an interleaved overlay write.
    // -----------------------------------------------------------------------

    fn opus_candidate() -> ImportCandidate {
        let mut cells = BTreeMap::new();
        cells.insert(
            "anthropic-api:claude-opus-4-8*".to_string(),
            OverlayCell {
                source: OverlaySource::Import,
                verified_at: "2026-07-11".to_string(),
                wm: Some(1.0),
                rm: Some(0.1),
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                max_output_tokens: None,
                input_cost_per_token: None,
                output_cost_per_token: None,
                capabilities: None,
            },
        );
        ImportCandidate {
            origin: CandidateOrigin::DocRefresh,
            verified_at: "2026-07-11".to_string(),
            cells,
            skipped: Vec::new(),
        }
    }

    #[test]
    #[serial]
    fn a_single_interleaved_write_triggers_exactly_one_fresh_diff_then_succeeds() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let initial_overlay =
            save_catalog_overlay(&overlay_path, 0, BTreeMap::new()).expect("seed");
        let candidate = opus_candidate();
        let baked = baked_row_map();

        let overlay_path_for_hook = overlay_path.clone();
        let mut interleaved_once = false;

        // Act: the FIRST attempt's `before_apply` hook writes to the
        // overlay -- simulating another writer racing in during the
        // confirm window the lock never covers.
        let result = confirm_and_apply(
            &overlay_path,
            &candidate,
            &baked,
            initial_overlay,
            true,
            move || {
                if !interleaved_once {
                    interleaved_once = true;
                    with_overlay_write_lock::<OverlayError, _>(&overlay_path_for_hook, |overlay| {
                        let mut next = overlay;
                        next.cells.insert("bedrock:*".to_string(), None);
                        Ok(next)
                    })
                    .expect("interleaved writer");
                }
            },
        );

        // Assert: exactly one re-diff recovers and the second attempt
        // succeeds.
        let (diff, saved, wrote) = result.expect("a single interleaved write must be recoverable");
        assert_eq!(diff.applied.len(), 1);
        assert_eq!(saved.revision, 3, "seed(1) + interleaved(2) + apply(3)");
        assert!(wrote);
    }

    #[test]
    #[serial]
    fn a_second_interleaved_write_aborts_with_no_retry_loop() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let initial_overlay =
            save_catalog_overlay(&overlay_path, 0, BTreeMap::new()).expect("seed");
        let candidate = opus_candidate();
        let baked = baked_row_map();
        let overlay_path_for_hook = overlay_path.clone();

        // Act: EVERY attempt's hook writes to the overlay, so the revision
        // has already moved again by the time the second `apply_diff`
        // call re-checks it.
        let result = confirm_and_apply(
            &overlay_path,
            &candidate,
            &baked,
            initial_overlay,
            true,
            move || {
                with_overlay_write_lock::<OverlayError, _>(&overlay_path_for_hook, |overlay| {
                    let mut next = overlay;
                    next.cells.insert("bedrock:*".to_string(), None);
                    Ok(next)
                })
                .expect("interleaved writer");
            },
        );

        // Assert: bounded -- no infinite retry loop, a clear abort instead.
        let err = result.expect_err("a second interleaved write must abort");
        assert!(matches!(err, ImportError::RevisionConflictBounded));
    }

    // -----------------------------------------------------------------------
    // The no-effective-change fast path never trusts a stale snapshot: a
    // concurrent write during the confirm window (which the lock never
    // covers) is caught by the unlocked revision re-read and forces one
    // bounded recompute, exactly like a revision conflict at apply time.
    // -----------------------------------------------------------------------

    #[test]
    #[serial]
    fn a_concurrent_write_to_an_unrelated_selector_during_the_noop_window_still_confirms_the_noop()
    {
        // Arrange: seed the overlay with exactly the cell `opus_candidate`
        // would re-derive -- a byte-identical re-import setup -- then have
        // the FIRST attempt's hook write to a DIFFERENT selector, moving
        // the revision without touching the selector the no-op verdict is
        // about.
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let selector = "anthropic-api:claude-opus-4-8*".to_string();
        let mut seed = BTreeMap::new();
        seed.insert(
            selector.clone(),
            Some(OverlayCell {
                source: OverlaySource::Import,
                verified_at: "2026-07-11".to_string(),
                wm: Some(1.0),
                rm: Some(0.1),
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                max_output_tokens: None,
                input_cost_per_token: None,
                output_cost_per_token: None,
                capabilities: None,
            }),
        );
        save_catalog_overlay(&overlay_path, 0, seed).expect("seed overlay");
        let initial_overlay = load_catalog_overlay(&overlay_path).expect("reload seeded overlay");
        let candidate = opus_candidate();
        let baked = baked_row_map();
        let overlay_path_for_hook = overlay_path.clone();
        let mut interleaved_once = false;

        // Act
        let result = confirm_and_apply(
            &overlay_path,
            &candidate,
            &baked,
            initial_overlay,
            true,
            move || {
                if !interleaved_once {
                    interleaved_once = true;
                    with_overlay_write_lock::<OverlayError, _>(&overlay_path_for_hook, |overlay| {
                        let mut next = overlay;
                        next.cells.insert("bedrock:*".to_string(), None);
                        Ok(next)
                    })
                    .expect("interleaved writer");
                }
            },
        );

        // Assert: the recompute against the fresh overlay still finds the
        // opus selector unaffected, so the no-op verdict holds -- but the
        // reported revision reflects the interleaved writer's bump, not a
        // second bump from this call.
        let (diff, saved, wrote) =
            result.expect("an unrelated interleaved write must not derail the no-op verdict");
        assert_eq!(diff.applied.len(), 1);
        assert!(!wrote, "the opus selector itself never changed");
        assert_eq!(
            saved.revision, 2,
            "seed(1) + interleaved bedrock disable(2)"
        );
        let cell = load_catalog_overlay(&overlay_path)
            .expect("reload")
            .cells
            .get(&selector)
            .and_then(Option::as_ref)
            .cloned()
            .expect("cell still present");
        assert_eq!(cell.wm, Some(1.0), "the opus cell must be untouched");
    }

    #[test]
    #[serial]
    fn a_concurrent_write_to_the_same_selector_during_the_noop_window_is_caught_and_applied() {
        // Arrange: same byte-identical-re-import seed as above, but this
        // time the interleaved writer changes the EXACT selector the
        // candidate targets -- a real concurrent edit that must invalidate
        // the pre-confirm no-op verdict, not silently vanish behind it.
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let selector = "anthropic-api:claude-opus-4-8*".to_string();
        let mut seed = BTreeMap::new();
        seed.insert(
            selector.clone(),
            Some(OverlayCell {
                source: OverlaySource::Import,
                verified_at: "2026-07-11".to_string(),
                wm: Some(1.0),
                rm: Some(0.1),
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                max_output_tokens: None,
                input_cost_per_token: None,
                output_cost_per_token: None,
                capabilities: None,
            }),
        );
        save_catalog_overlay(&overlay_path, 0, seed).expect("seed overlay");
        let initial_overlay = load_catalog_overlay(&overlay_path).expect("reload seeded overlay");
        let candidate = opus_candidate();
        let baked = baked_row_map();
        let overlay_path_for_hook = overlay_path.clone();
        let mut interleaved_once = false;

        // Act: the interleaved writer bumps the SAME selector's wm to 2.0
        // (still `source: import`) before this call's no-op check would
        // otherwise have trusted its stale pre-confirm snapshot.
        let result = confirm_and_apply(
            &overlay_path,
            &candidate,
            &baked,
            initial_overlay,
            true,
            move || {
                if !interleaved_once {
                    interleaved_once = true;
                    with_overlay_write_lock::<OverlayError, _>(&overlay_path_for_hook, |overlay| {
                        let mut next = overlay;
                        next.cells.insert(
                            selector.clone(),
                            Some(OverlayCell {
                                source: OverlaySource::Import,
                                verified_at: "2026-07-11".to_string(),
                                wm: Some(2.0),
                                rm: Some(0.1),
                                ttl_seconds: None,
                                min_prefix_tokens: None,
                                max_context_tokens: None,
                                max_output_tokens: None,
                                input_cost_per_token: None,
                                output_cost_per_token: None,
                                capabilities: None,
                            }),
                        );
                        Ok(next)
                    })
                    .expect("interleaved writer");
                }
            },
        );

        // Assert: the recompute against the fresh overlay sees a real
        // change (candidate wm=1.0 vs. the interleaved wm=2.0), applies
        // it, and the candidate's value -- not the interleaved one -- wins
        // on disk.
        let (diff, saved, wrote) =
            result.expect("a same-selector interleaved write must be recoverable");
        assert_eq!(diff.applied.len(), 1);
        assert!(
            wrote,
            "a real concurrent edit must not be reported as a no-op"
        );
        assert_eq!(
            saved.revision, 3,
            "seed(1) + interleaved same-selector write(2) + this apply(3)"
        );
        let cell = load_catalog_overlay(&overlay_path)
            .expect("reload")
            .cells
            .get("anthropic-api:claude-opus-4-8*")
            .and_then(Option::as_ref)
            .cloned()
            .expect("cell still present");
        assert_eq!(
            cell.wm,
            Some(1.0),
            "the candidate's own value must win, not the interleaved one"
        );
    }

    #[test]
    #[serial]
    fn a_second_concurrent_write_during_the_noop_window_aborts_with_no_retry_loop() {
        // Arrange: same byte-identical-re-import seed, but EVERY attempt's
        // hook writes to the overlay, so the revision has already moved
        // again by the time the second no-op re-read re-checks it.
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let selector = "anthropic-api:claude-opus-4-8*".to_string();
        let mut seed = BTreeMap::new();
        seed.insert(
            selector,
            Some(OverlayCell {
                source: OverlaySource::Import,
                verified_at: "2026-07-11".to_string(),
                wm: Some(1.0),
                rm: Some(0.1),
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                max_output_tokens: None,
                input_cost_per_token: None,
                output_cost_per_token: None,
                capabilities: None,
            }),
        );
        save_catalog_overlay(&overlay_path, 0, seed).expect("seed overlay");
        let initial_overlay = load_catalog_overlay(&overlay_path).expect("reload seeded overlay");
        let candidate = opus_candidate();
        let baked = baked_row_map();
        let overlay_path_for_hook = overlay_path.clone();

        // Act
        let result = confirm_and_apply(
            &overlay_path,
            &candidate,
            &baked,
            initial_overlay,
            true,
            move || {
                with_overlay_write_lock::<OverlayError, _>(&overlay_path_for_hook, |overlay| {
                    let mut next = overlay;
                    next.cells.insert("bedrock:*".to_string(), None);
                    Ok(next)
                })
                .expect("interleaved writer");
            },
        );

        // Assert: bounded -- no infinite retry loop, a clear abort instead.
        let err =
            result.expect_err("a second interleaved write during the no-op window must abort");
        assert!(matches!(err, ImportError::RevisionConflictBounded));
    }

    // -----------------------------------------------------------------------
    // Import-vs-verify serialization: no lost update across two writers.
    // -----------------------------------------------------------------------

    #[test]
    #[serial]
    fn import_apply_and_a_concurrent_verify_serialize_with_no_lost_update() {
        // Arrange: a baked-known selector `verify` can stamp (it needs an
        // existing overlay cell) and a DIFFERENT selector the import
        // candidate targets, so both writers change disjoint keys and
        // either interleaving must still preserve both.
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let verify_selector = "openai-compat:grok-*".to_string();
        let mut seed = BTreeMap::new();
        seed.insert(
            verify_selector.clone(),
            Some(OverlayCell {
                source: OverlaySource::Import,
                verified_at: "2020-01-01".to_string(),
                wm: Some(0.5),
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                max_output_tokens: None,
                input_cost_per_token: None,
                output_cost_per_token: None,
                capabilities: None,
            }),
        );
        save_catalog_overlay(&overlay_path, 0, seed).expect("seed");
        let initial_overlay = load_catalog_overlay(&overlay_path).expect("reload");
        let candidate = opus_candidate();
        let baked = baked_row_map();

        let verify_path = overlay_path.clone();
        let verifier = std::thread::spawn(move || {
            super::super::catalog::verify_at(&verify_selector, &verify_path)
                .map_err(|e| e.to_string())
        });

        // Act
        let (_, saved, wrote) = confirm_and_apply(
            &overlay_path,
            &candidate,
            &baked,
            initial_overlay,
            true,
            || {},
        )
        .expect("import apply must succeed");
        verifier
            .join()
            .expect("verify thread")
            .expect("verify must succeed");

        // Assert: both writes landed -- two serialized writes on top of
        // the seed, revision 3, and both selectors carry the expected
        // final state.
        let final_overlay = load_catalog_overlay(&overlay_path).expect("final load");
        assert_eq!(final_overlay.revision, 3);
        assert!(saved.revision >= 2);
        assert!(wrote);
        let opus = final_overlay
            .cells
            .get("anthropic-api:claude-opus-4-8*")
            .and_then(Option::as_ref)
            .expect("import cell present");
        assert_eq!(opus.source, OverlaySource::Import);
        let grok = final_overlay
            .cells
            .get("openai-compat:grok-*")
            .and_then(Option::as_ref)
            .expect("verified cell present");
        assert_eq!(grok.source, OverlaySource::User);
    }

    // -----------------------------------------------------------------------
    // Import-vs-set serialization: no lost update across two writers.
    // -----------------------------------------------------------------------

    #[test]
    #[serial]
    fn import_apply_and_a_concurrent_set_serialize_with_no_lost_update() {
        // Arrange: a baked-known selector `set` can act on (no overlay cell
        // required) and a DIFFERENT selector the import candidate targets,
        // so both writers change disjoint keys and either interleaving must
        // still preserve both.
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let initial_overlay =
            save_catalog_overlay(&overlay_path, 0, BTreeMap::new()).expect("seed");
        let candidate = opus_candidate();
        let baked = baked_row_map();

        let set_path = overlay_path.clone();
        let setter = std::thread::spawn(move || {
            super::super::catalog::set_at(
                "openai-compat:grok-*",
                &["min_prefix_tokens=256".to_string()],
                false,
                &set_path,
            )
            .map_err(|e| e.to_string())
        });

        // Act
        let (_, saved, wrote) = confirm_and_apply(
            &overlay_path,
            &candidate,
            &baked,
            initial_overlay,
            true,
            || {},
        )
        .expect("import apply must succeed");
        setter
            .join()
            .expect("set thread")
            .expect("set must succeed");

        // Assert: both writes landed -- two serialized writes on top of the
        // seed, revision 3, and both selectors carry the expected final
        // state.
        let final_overlay = load_catalog_overlay(&overlay_path).expect("final load");
        assert_eq!(final_overlay.revision, 3);
        assert!(saved.revision >= 2);
        assert!(wrote);
        let opus = final_overlay
            .cells
            .get("anthropic-api:claude-opus-4-8*")
            .and_then(Option::as_ref)
            .expect("import cell present");
        assert_eq!(opus.source, OverlaySource::Import);
        let grok = final_overlay
            .cells
            .get("openai-compat:grok-*")
            .and_then(Option::as_ref)
            .expect("set cell present");
        assert_eq!(grok.source, OverlaySource::User);
        assert_eq!(grok.min_prefix_tokens, Some(256));
    }

    // -----------------------------------------------------------------------
    // Stale-import clearing through the real write path.
    // -----------------------------------------------------------------------

    const GROK: &str = "openai-compat:grok-*";

    /// A candidate that admits nothing and skips Grok for a cross-check
    /// disagreement.
    fn grok_cross_check_skip_candidate() -> ImportCandidate {
        ImportCandidate {
            origin: CandidateOrigin::DocRefresh,
            verified_at: "2026-08-04".to_string(),
            cells: BTreeMap::new(),
            skipped: vec![routectl_router::SkippedSelector {
                selector: GROK.to_string(),
                reason: "cross-check mismatch".to_string(),
                kind: SkipKind::CrossCheckDisagreement,
            }],
        }
    }

    /// The Grok cell an OLDER snapshot pair would have imported:
    /// `rm = 0.25`, which the baked catalog now corrects to `0.15`.
    fn stale_grok_cell(source: OverlaySource) -> OverlayCell {
        OverlayCell {
            source,
            verified_at: "2026-01-01".to_string(),
            wm: Some(1.0),
            rm: Some(0.25),
            ttl_seconds: None,
            min_prefix_tokens: None,
            max_context_tokens: None,
            max_output_tokens: None,
            input_cost_per_token: None,
            output_cost_per_token: None,
            capabilities: None,
        }
    }

    #[test]
    #[serial]
    fn a_cross_check_skip_removes_the_stale_import_cell_so_the_baked_row_wins_again() {
        // Arrange: the overlay carries an OLD import's Grok rm = 0.25; the
        // refresh skips Grok on a cross-check disagreement, so no candidate
        // cell overwrites it. The baked row says 0.15.
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let mut seed = BTreeMap::new();
        seed.insert(
            GROK.to_string(),
            Some(stale_grok_cell(OverlaySource::Import)),
        );
        save_catalog_overlay(&overlay_path, 0, seed).expect("seed overlay");
        let initial_overlay = load_catalog_overlay(&overlay_path).expect("reload");
        let baked = baked_row_map();
        assert_eq!(
            baked.get(GROK).expect("grok is baked-known").rm,
            0.15_f32,
            "fixture guard: this test asserts the baked Grok rm is 0.15",
        );

        // Act
        let (diff, saved, wrote) = confirm_and_apply(
            &overlay_path,
            &grok_cross_check_skip_candidate(),
            &baked,
            initial_overlay,
            true,
            || {},
        )
        .expect("a clear-only diff must apply");

        // Assert: the write happened (a clear is never a no-op) and the
        // stale cell is GONE from the persisted overlay, so nothing
        // overrides the baked 0.15 any more.
        assert_eq!(diff.cleared, vec![GROK.to_string()]);
        assert!(diff.applied.is_empty());
        assert!(wrote, "removing a stale import cell is a real change");
        let reloaded = load_catalog_overlay(&overlay_path).expect("reload");
        assert!(
            !reloaded.cells.contains_key(GROK),
            "the stale import cell must be removed entirely, not merely blanked"
        );
        assert_eq!(saved.revision, 2, "seed(1) + clear(2)");
    }

    #[test]
    #[serial]
    fn a_cross_check_skip_leaves_an_operator_user_cell_in_place() {
        // Arrange: identical to the clear case except the Grok cell is a
        // `source: user` override -- an operator override outranks both the
        // import and the baked row, so it must survive untouched.
        let dir = tempfile::tempdir().unwrap();
        let overlay_path = dir.path().join("catalog_overlay.json");
        let mut seed = BTreeMap::new();
        seed.insert(GROK.to_string(), Some(stale_grok_cell(OverlaySource::User)));
        save_catalog_overlay(&overlay_path, 0, seed).expect("seed overlay");
        let initial_overlay = load_catalog_overlay(&overlay_path).expect("reload");

        // Act
        let (diff, _saved, wrote) = confirm_and_apply(
            &overlay_path,
            &grok_cross_check_skip_candidate(),
            &baked_row_map(),
            initial_overlay,
            true,
            || {},
        )
        .expect("a skip-only diff against a user cell must succeed");

        // Assert: nothing cleared, nothing written, the override intact.
        assert!(diff.cleared.is_empty());
        assert!(!wrote, "a preserved user cell is a no-op write");
        let reloaded = load_catalog_overlay(&overlay_path).expect("reload");
        let cell = reloaded
            .cells
            .get(GROK)
            .and_then(Option::as_ref)
            .expect("the user cell must survive");
        assert_eq!(cell.source, OverlaySource::User);
        assert_eq!(cell.rm, Some(0.25));
    }

    // -----------------------------------------------------------------------
    // Small pure-function unit coverage.
    // -----------------------------------------------------------------------

    #[test]
    fn content_hash_is_deterministic_and_sensitive_to_input() {
        assert_eq!(content_hash("abc"), content_hash("abc"));
        assert_ne!(content_hash("abc"), content_hash("abd"));
    }

    #[test]
    fn source_mode_reports_file_vs_network() {
        assert_eq!(source_mode(Some(std::path::Path::new("x"))), "file");
        assert_eq!(source_mode(None), "network");
    }

    #[test]
    fn summary_line_reports_no_changes_when_nothing_was_written() {
        let diff = ImportDiff::default();
        let saved = CatalogOverlay {
            revision: 3,
            ..CatalogOverlay::default()
        };

        let line = summary_line(&diff, &saved, false);

        assert!(line.contains("no changes"), "line: {line}");
        assert!(line.contains("remains 3"), "line: {line}");
    }

    #[test]
    fn summary_line_reports_a_written_count_when_a_write_occurred() {
        let diff = ImportDiff::default();
        let saved = CatalogOverlay {
            revision: 4,
            ..CatalogOverlay::default()
        };

        let line = summary_line(&diff, &saved, true);

        assert!(line.contains("written"), "line: {line}");
        assert!(line.contains("is now 4"), "line: {line}");
        assert!(!line.contains("no changes"), "line: {line}");
    }

    #[test]
    fn conflict_note_distinguishes_identical_from_a_real_conflict() {
        let mut candidate = opus_candidate();
        let candidate_cell = candidate
            .cells
            .remove("anthropic-api:claude-opus-4-8*")
            .unwrap();
        let identical = routectl_router::DiffRow {
            selector: "anthropic-api:claude-opus-4-8*".to_string(),
            candidate: candidate_cell,
            existing: routectl_router::ExistingCell::Present(OverlayCell {
                source: OverlaySource::User,
                verified_at: "2020-01-01".to_string(),
                wm: Some(1.0),
                rm: Some(0.1),
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                max_output_tokens: None,
                input_cost_per_token: None,
                output_cost_per_token: None,
                capabilities: None,
            }),
            impact: ImpactClass::DisplayOnly,
            cheaper_direction: false,
        };
        assert_eq!(conflict_note(&identical), "identical (user cell preserved)");

        let real_conflict = routectl_router::DiffRow {
            impact: ImpactClass::CostAffecting,
            ..identical
        };
        assert_eq!(
            conflict_note(&real_conflict),
            "conflicted (user cell preserved)"
        );

        let disabled = routectl_router::DiffRow {
            existing: routectl_router::ExistingCell::Disabled,
            ..real_conflict
        };
        let disabled = routectl_router::DiffRow {
            impact: ImpactClass::DisplayOnly,
            ..disabled
        };
        assert_eq!(
            conflict_note(&disabled),
            "conflicted (user cell preserved)",
            "a disabled selector is never 'identical', regardless of impact class"
        );
    }
}
