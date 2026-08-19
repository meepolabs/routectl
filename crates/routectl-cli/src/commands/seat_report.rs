//! Shared OAuth seat-pool reporting: the join of the config's `oauth://`
//! refs against the STORED seat keys, plus the one sentence every operator
//! surface renders for it.
//!
//! Both `routectl doctor` and `routectl config check` describe seat pools, so
//! the join and its wording live here once and cannot drift between them.
//! Everything in this module is pure and synchronous: the caller opens the
//! credential store (or fails to) and passes a seat-key snapshot in.
//!
//! Secret-material guard by construction: the entry points take seat KEYS
//! (`<provider>` / `<provider>#<label>`), never a `TokenRecord`, so no token,
//! account identity, or storage path can reach the rendered text. Seat labels
//! and config entry names are operator-controlled input, so both are rendered
//! through the shared log-safe sanitizer -- a label bearing newlines or ANSI
//! escapes cannot forge a finding line.

use routectl_auth::SecretRef;
use routectl_core::sanitize_for_log;
use routectl_router::Config;
use routectl_router::config::SeatSelection;

/// How many seats a pool ref resolves to, as far as the caller could tell.
///
/// `Known` carries DISPLAY-ONLY seat labels: the default seat renders as
/// `default` and comes first, followed by the labelled seats in sorted order.
/// `Unknown` means the credential store could not be read at all -- reserved
/// for an OPEN failure, never for an empty store (nothing logged in is
/// `Known(0)`, which is an accurate answer).
pub(crate) enum SeatCount {
    Known(Vec<String>),
    Unknown,
}

/// One `oauth://` reference expressed by one provider entry, joined against
/// the stored seats.
///
/// For a bare pool ref `seats` is what the store holds for that provider. A
/// pinned ref names exactly one seat by config, so its count comes from the
/// ref rather than the store; whether that seat is logged in is the
/// secret-presence check's story, not the pool's.
pub(crate) struct SeatPoolRow {
    /// The config entry key this ref belongs to (`[providers.<entry>]`).
    pub(crate) entry: String,
    /// The oauth provider id named by the ref.
    pub(crate) oauth_provider: String,
    /// `Some(label)` for a `#label`-pinned ref, `None` for a bare pool ref.
    pub(crate) pinned_label: Option<String>,
    pub(crate) seats: SeatCount,
    pub(crate) selection: SeatSelection,
}

/// Join every provider entry's `oauth://` refs against `stored_seat_keys`.
///
/// `stored_seat_keys` is a snapshot of the credential store's seat keys;
/// `None` means the store could not be opened, and every row then reports
/// `Unknown`. Refs are walked through `ProviderEntry::secret_uris` -- the
/// same basis the doctor orphan scan uses -- so the two surfaces always
/// agree on which refs count. Non-oauth refs yield no row.
pub(crate) fn stored_seat_pool_rows(
    config: &Config,
    stored_seat_keys: Option<&[String]>,
) -> Vec<SeatPoolRow> {
    let mut rows = Vec::new();
    for (entry, provider_entry) in &config.providers {
        let selection = provider_entry.runtime().seat_selection;
        for uri in provider_entry.secret_uris() {
            let Ok(SecretRef::OAuth { provider, label }) = SecretRef::parse(uri) else {
                continue;
            };
            let seats = match (stored_seat_keys, label.as_deref()) {
                (None, _) => SeatCount::Unknown,
                (Some(keys), None) => SeatCount::Known(pool_labels(&provider, keys)),
                (Some(keys), Some(label)) => {
                    SeatCount::Known(pinned_labels(&provider, label, keys))
                }
            };
            rows.push(SeatPoolRow {
                entry: safe(entry),
                oauth_provider: safe(&provider),
                pinned_label: label.as_deref().map(safe),
                seats,
                selection,
            });
        }
    }
    rows
}

/// THE shared seat-pool sentence. Purely informational: it states what the
/// ref resolves to and which selection strategy applies, and never advises.
pub(crate) fn describe_row(row: &SeatPoolRow) -> String {
    let provider = &row.oauth_provider;
    if let Some(label) = &row.pinned_label {
        return format!(
            "ref oauth://{provider}#{label} pins 1 seat; \
             seat_selection not applicable to a pinned ref"
        );
    }
    match &row.seats {
        // The strategy is config-derived, so it is known even when the seat
        // count is not; the single-seat "inactive" nuance cannot be claimed
        // here, so the plain label renders.
        SeatCount::Unknown => format!(
            "pool ref oauth://{provider}: seat count unknown \
             (credential store unavailable); seat_selection {}",
            selection_label(row.selection)
        ),
        SeatCount::Known(labels) if labels.is_empty() => {
            format!("pool ref oauth://{provider} has no stored seats")
        }
        SeatCount::Known(labels) => {
            let strategy = if labels.len() == 1 {
                single_seat_selection_label(row.selection)
            } else {
                selection_label(row.selection)
            };
            format!(
                "pool ref oauth://{provider} resolves to {} ({}); seat_selection {strategy}",
                seat_plural(labels.len()),
                labels.join(", ")
            )
        }
    }
}

/// The `config check` seat-pool block: a header plus one line per row. Empty
/// when no provider carries an oauth ref, so an api-key-only config renders
/// no header noise.
// Rendered as a returned line vector rather than printed, so the block is
// testable without stdout capture.
pub(crate) fn seat_pool_lines(rows: &[SeatPoolRow]) -> Vec<String> {
    if rows.is_empty() {
        return Vec::new();
    }
    let mut lines = vec!["oauth seat pools:".to_string()];
    lines.extend(
        rows.iter()
            .map(|row| format!("  {}: {}", row.entry, describe_row(row))),
    );
    lines
}

/// Operator-facing name of a seat-selection strategy. Presentation, not a
/// `Display` impl on the router config type: `fill-first (default)` is a CLI
/// wording choice, and the config token is indistinguishable from an explicit
/// setting post-parse.
pub(crate) const fn selection_label(selection: SeatSelection) -> &'static str {
    match selection {
        SeatSelection::FillFirst => "fill-first (default)",
        SeatSelection::RoundRobin => "round-robin",
        SeatSelection::StickyLeastLoaded => "sticky-least-loaded",
    }
}

/// [`selection_label`] for a pool that resolves to exactly one seat: the
/// strategy is configured but has nothing to choose between. Factual, not
/// advisory -- a single-seat pool is a normal configuration.
const fn single_seat_selection_label(selection: SeatSelection) -> &'static str {
    match selection {
        SeatSelection::FillFirst => "fill-first (default; inactive at 1 seat)",
        SeatSelection::RoundRobin => "round-robin (inactive at 1 seat)",
        SeatSelection::StickyLeastLoaded => "sticky-least-loaded (inactive at 1 seat)",
    }
}

/// Display labels of every stored seat a BARE pool ref expands to: the
/// default seat first (as `default`), then the labelled siblings sorted.
fn pool_labels(provider: &str, stored_seat_keys: &[String]) -> Vec<String> {
    let mut labels: Vec<String> = Vec::new();
    let mut has_default = false;
    for key in stored_seat_keys {
        match key.split_once('#') {
            None if key == provider => has_default = true,
            Some((key_provider, label)) if key_provider == provider => labels.push(safe(label)),
            _ => {}
        }
    }
    labels.sort();
    if has_default {
        labels.insert(0, "default".to_string());
    }
    labels
}

/// Display labels for a `#label`-pinned ref: the pinned seat when the store
/// holds it, nothing when it does not.
fn pinned_labels(provider: &str, label: &str, stored_seat_keys: &[String]) -> Vec<String> {
    let pinned = format!("{provider}#{label}");
    if stored_seat_keys.contains(&pinned) {
        vec![safe(label)]
    } else {
        Vec::new()
    }
}

fn seat_plural(count: usize) -> String {
    if count == 1 {
        "1 seat".to_string()
    } else {
        format!("{count} seats")
    }
}

/// One-line, ASCII-safe rendering of an operator-controlled string.
fn safe(s: &str) -> String {
    sanitize_for_log(s)
}

#[cfg(test)]
#[path = "seat_report_tests.rs"]
mod seat_report_tests;
