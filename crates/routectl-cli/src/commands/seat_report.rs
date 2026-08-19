//! Shared OAuth seat-pool reporting: the join of the config's `[pools]`
//! blocks and `oauth://` refs against the STORED seat keys, plus the sentences
//! every operator surface renders for them.
//!
//! Both `routectl doctor` and `routectl config check` describe seat pools, so
//! the join and its wording live here once and cannot drift between them.
//! Everything in this module is pure and synchronous: the caller opens the
//! credential store (or fails to) and passes a seat-key snapshot in.
//!
//! A `[pools.<name>]` block is THE multi-seat shape: the pool is the rendered
//! unit (its members, its strategy, whether it accepts new logins), while a
//! `oauth://<provider>` ref on a standalone provider entry pins exactly one
//! seat -- the default one -- and carries no strategy of its own.
//!
//! Secret-material guard by construction: the entry points take seat KEYS
//! (`<provider>` / `<provider>#<label>`), never a `TokenRecord`, so no token,
//! account identity, or storage path can reach the rendered text. Seat labels,
//! pool names, and config entry names are operator-controlled input, so all
//! three are rendered through the shared log-safe sanitizer -- a label bearing
//! newlines or ANSI escapes cannot forge a finding line.

use routectl_auth::SecretRef;
use routectl_core::sanitize_for_log;
use routectl_router::Config;
use routectl_router::config::SeatSelection;

/// Whether the seat a ref names is present in the credential store, as far as
/// the caller could tell.
///
/// `Known` carries the DISPLAY-ONLY labels the ref resolves to: `default` for
/// a bare ref that the store holds a default seat for, the pinned label for a
/// `#label` ref, and nothing at all when the store holds no such seat.
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
/// Every ref names exactly ONE seat: a bare `oauth://<provider>` ref the
/// provider's default seat, a `#label` ref that labelled seat. `seats` is
/// therefore a presence answer, not a count -- it lists the one label when the
/// store holds the seat and nothing when it does not.
pub(crate) struct SeatPoolRow {
    /// The config entry key this ref belongs to (`[providers.<entry>]`).
    pub(crate) entry: String,
    /// The oauth provider id named by the ref.
    pub(crate) oauth_provider: String,
    /// `Some(label)` for a `#label`-pinned ref, `None` for a bare
    /// default-seat ref.
    pub(crate) pinned_label: Option<String>,
    pub(crate) seats: SeatCount,
    pub(crate) selection: SeatSelection,
    /// The `[pools.<name>]` block claiming this entry, when one does. A member
    /// row's own sentence names its pool; the pool itself renders as a
    /// [`PoolRow`], which is the unit an operator reasons about.
    pub(crate) pool: Option<String>,
}

/// One `[pools.<name>]` block: its members joined against the stored seats,
/// plus the two policy facts an operator sets on it.
pub(crate) struct PoolRow {
    /// The `[pools]` table key.
    pub(crate) pool: String,
    /// Every declared member, in declaration order.
    pub(crate) members: Vec<PoolMember>,
    /// The strategy dispatch picks members with.
    pub(crate) selection: SeatSelection,
    /// Whether a future `routectl login` may propose joining a new account.
    pub(crate) accepts_new_logins: bool,
    /// What the config plus the store snapshot say this pool can serve.
    pub(crate) health: PoolHealth,
}

/// One declared pool member: the provider entry, the seat its ref names, and
/// whether the store holds that seat.
pub(crate) struct PoolMember {
    /// The `[providers]` table key.
    pub(crate) entry: String,
    /// The seat the member's ref names: `default` for a bare ref, the label
    /// for a pinned one. `None` when the member declares no `oauth://` ref at
    /// all (validation rejects that; the render stays defensive).
    pub(crate) seat: Option<String>,
    /// Whether the store holds that seat. `None` when the store could not be
    /// opened, or when the member names no oauth seat to look for.
    pub(crate) stored: Option<bool>,
}

/// What a pool can serve, derived from the config plus the store snapshot.
///
/// This is a CONFIG-AND-STORE verdict, not the build's: a member whose
/// credential is stored can still fail to compile into a dispatch seat (a
/// refused refresh, a provider block the factory rejects). Only the build
/// observes that, and no doctor or `config check` run builds a router.
pub(crate) enum PoolHealth {
    /// The store holds the seat every member names.
    Ready,
    /// The store holds at least one member's seat and is missing at least one.
    Degraded,
    /// The store holds no member's seat: every model naming this pool is
    /// unroutable.
    Unusable,
    /// The credential store could not be opened, so presence is unknowable.
    Unknown,
}

/// Join every `[pools.<name>]` block against its members' refs and
/// `stored_seat_keys`.
///
/// `stored_seat_keys` is a snapshot of the credential store's seat keys;
/// `None` means the store could not be opened, and every pool then reports
/// `Unknown` health with no per-member presence claim.
pub(crate) fn pool_rows(config: &Config, stored_seat_keys: Option<&[String]>) -> Vec<PoolRow> {
    config
        .pools
        .iter()
        .map(|(name, pool)| {
            let members: Vec<PoolMember> = pool
                .members
                .iter()
                .map(|member| pool_member(config, member, stored_seat_keys))
                .collect();
            PoolRow {
                pool: safe(name),
                health: pool_health(&members, stored_seat_keys.is_none()),
                members,
                selection: pool.seat_selection,
                accepts_new_logins: pool.accepts_new_logins,
            }
        })
        .collect()
}

/// One member's join: the seat its first `oauth://` ref names, and whether
/// the snapshot holds it.
fn pool_member(config: &Config, member: &str, stored_seat_keys: Option<&[String]>) -> PoolMember {
    let seat_ref = config
        .providers
        .get(member)
        .into_iter()
        .flat_map(routectl_router::ProviderEntry::secret_uris)
        .find_map(|uri| match SecretRef::parse(uri) {
            Ok(SecretRef::OAuth { provider, label }) => Some((provider, label)),
            _ => None,
        });
    let (seat, stored) = match (seat_ref, stored_seat_keys) {
        (None, _) => (None, None),
        (Some((_, label)), None) => (Some(seat_display(label.as_deref())), None),
        (Some((provider, label)), Some(keys)) => {
            let wanted = seat_key(&provider, label.as_deref());
            (
                Some(seat_display(label.as_deref())),
                Some(keys.contains(&wanted)),
            )
        }
    };
    PoolMember {
        entry: safe(member),
        seat,
        stored,
    }
}

/// The pool's verdict over its members' presence answers.
fn pool_health(members: &[PoolMember], store_unreadable: bool) -> PoolHealth {
    if store_unreadable {
        return PoolHealth::Unknown;
    }
    let stored = members.iter().filter(|m| m.stored == Some(true)).count();
    if stored == 0 {
        PoolHealth::Unusable
    } else if stored < members.len() {
        PoolHealth::Degraded
    } else {
        PoolHealth::Ready
    }
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
        // The strategy is a pool property: an entry claimed by a pool
        // renders that pool's strategy, a standalone entry has no strategy
        // to apply because its ref names one seat.
        let selection = config.seat_selection_for(entry);
        let pool = claiming_pool(config, entry).map(safe);
        for uri in provider_entry.secret_uris() {
            let Ok(SecretRef::OAuth { provider, label }) = SecretRef::parse(uri) else {
                continue;
            };
            let seats = match stored_seat_keys {
                None => SeatCount::Unknown,
                Some(keys) => {
                    SeatCount::Known(present_seat_labels(&provider, label.as_deref(), keys))
                }
            };
            rows.push(SeatPoolRow {
                entry: safe(entry),
                oauth_provider: safe(&provider),
                pinned_label: label.as_deref().map(safe),
                seats,
                selection,
                pool: pool.clone(),
            });
        }
    }
    rows
}

/// The `[pools]` key of the pool claiming `entry`, when one does. Validation
/// rejects a provider claimed by two pools, so there is at most one answer for
/// any config that passed the gate.
fn claiming_pool<'a>(config: &'a Config, entry: &str) -> Option<&'a str> {
    config
        .pools
        .iter()
        .find(|(_, pool)| pool.members.iter().any(|member| member == entry))
        .map(|(name, _)| name.as_str())
}

/// THE shared per-ref sentence. Purely informational: it states which seat the
/// ref pins and, for a pool member, which pool owns the strategy. It never
/// advises.
pub(crate) fn describe_row(row: &SeatPoolRow) -> String {
    let provider = &row.oauth_provider;
    if let Some(label) = &row.pinned_label {
        return format!(
            "ref oauth://{provider}#{label} pins 1 seat{}{}",
            presence_clause(&row.seats),
            pool_clause(row),
        );
    }
    // A bare ref pins the provider's DEFAULT seat and nothing else -- it is
    // not a set, so there is no count to state and no strategy to apply.
    format!(
        "ref oauth://{provider} pins the default seat{}{}",
        presence_clause(&row.seats),
        pool_clause(row),
    )
}

/// The store-presence clause of a per-ref sentence. A ref names its seat by
/// CONFIG whether or not that seat is logged in, so presence is reported
/// alongside the pin rather than folded into it.
const fn presence_clause(seats: &SeatCount) -> &'static str {
    match seats {
        SeatCount::Unknown => " (store presence unknown - credential store unavailable)",
        SeatCount::Known(labels) if labels.is_empty() => " (no stored credential for it)",
        SeatCount::Known(_) => "",
    }
}

/// The strategy clause. A pool owns the strategy, so a member row names its
/// pool and the strategy in force; a standalone entry states plainly that
/// `seat_selection` has nothing to select between.
fn pool_clause(row: &SeatPoolRow) -> String {
    match &row.pool {
        Some(pool) => format!(
            "; member of pool `{pool}` with seat_selection {}",
            selection_label(row.selection)
        ),
        None => "; seat_selection not applicable to a single-seat ref".to_string(),
    }
}

/// THE shared pool sentence: what the pool is made of, how dispatch picks
/// among it, whether it grows, and which members have no stored credential.
pub(crate) fn describe_pool(row: &PoolRow) -> String {
    let mut sentence = format!(
        "pool `{}` has {} ({}); seat_selection {}; accepts new logins: {}",
        row.pool,
        plural(row.members.len(), "member"),
        render_labels(&member_labels(&row.members)),
        selection_label(row.selection),
        if row.accepts_new_logins { "yes" } else { "no" },
    );
    match row.health {
        PoolHealth::Ready => {}
        PoolHealth::Degraded => {
            let missing = render_labels(&missing_member_entries(&row.members));
            sentence.push_str(&format!("; no stored credential for {missing}"));
        }
        PoolHealth::Unusable => {
            sentence.push_str("; no member has a stored credential");
        }
        PoolHealth::Unknown => {
            sentence
                .push_str("; member credential presence unknown (credential store unavailable)");
        }
    }
    sentence
}

/// `entry=seat` display pairs, one per member. A member declaring no
/// `oauth://` ref renders `entry=none` rather than being dropped, so the
/// member count and the listing always agree.
fn member_labels(members: &[PoolMember]) -> Vec<String> {
    members
        .iter()
        .map(|member| {
            let seat = member.seat.as_deref().unwrap_or("none");
            format!("{}={seat}", member.entry)
        })
        .collect()
}

/// The `[providers]` keys of the members whose seat the store does not hold.
fn missing_member_entries(members: &[PoolMember]) -> Vec<String> {
    members
        .iter()
        .filter(|member| member.stored != Some(true))
        .map(|member| member.entry.clone())
        .collect()
}

/// The `config check` seat-pool block: a header, one line per pool, then one
/// line per provider entry NOT claimed by a pool. Empty when the config
/// declares no pool and no provider carries an oauth ref, so an api-key-only
/// config renders no header noise.
///
/// Pool members are deliberately not repeated as standalone rows: the pool
/// line already names every member and its seat, and a duplicated per-member
/// line would read as a second, independent dispatch target.
// Rendered as a returned line vector rather than printed, so the block is
// testable without stdout capture.
pub(crate) fn seat_pool_lines(pools: &[PoolRow], rows: &[SeatPoolRow]) -> Vec<String> {
    let standalone: Vec<&SeatPoolRow> = rows.iter().filter(|row| row.pool.is_none()).collect();
    if pools.is_empty() && standalone.is_empty() {
        return Vec::new();
    }
    let mut lines = vec!["oauth seat pools:".to_string()];
    lines.extend(
        pools
            .iter()
            .map(|pool| format!("  {}", describe_pool(pool))),
    );
    lines.extend(
        standalone
            .iter()
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

/// The display label of the seat a ref names: `default` for a bare ref, the
/// operator's own label for a pinned one.
fn seat_display(label: Option<&str>) -> String {
    label.map_or_else(|| "default".to_string(), safe)
}

/// The credential-store seat key a ref resolves to: the bare provider id for
/// the default seat, `<provider>#<label>` for a labelled one. Mirrors the
/// store's own key shape, so a ref and a stored seat compare as strings.
fn seat_key(provider: &str, label: Option<&str>) -> String {
    match label {
        Some(label) => format!("{provider}#{label}"),
        None => provider.to_string(),
    }
}

/// The display label of the ONE seat this ref names, when the store holds it;
/// nothing when it does not.
///
/// A bare ref names the default seat alone -- schema version 4 retired the
/// bare-ref-expands-to-every-stored-seat reading, and `[pools]` is the
/// multi-seat shape that replaced it.
fn present_seat_labels(
    provider: &str,
    label: Option<&str>,
    stored_seat_keys: &[String],
) -> Vec<String> {
    let wanted = seat_key(provider, label);
    if stored_seat_keys.contains(&wanted) {
        vec![seat_display(label)]
    } else {
        Vec::new()
    }
}

/// How many labels a sentence lists before collapsing the tail. A pool may
/// hold up to 32 members, and the sentence is printed ahead of the `config
/// check` warnings and errors -- an uncapped list would bury them. The COUNT
/// stays exact; only the listing is bounded.
const MAX_LISTED_LABELS: usize = 10;

/// A comma-separated listing: the first [`MAX_LISTED_LABELS`] entries, with
/// the remainder collapsed to `and K more`.
fn render_labels(labels: &[String]) -> String {
    if labels.len() <= MAX_LISTED_LABELS {
        return labels.join(", ");
    }
    let shown = labels[..MAX_LISTED_LABELS].join(", ");
    let hidden = labels.len() - MAX_LISTED_LABELS;
    format!("{shown}, and {hidden} more")
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// One-line, ASCII-safe rendering of an operator-controlled string.
///
/// Beyond the control-byte/ANSI filtering the shared sanitizer performs, every
/// character this module's own grammar carries meaning with (`( ) , ; : =` and
/// the backtick) is neutralized: a label such as `a); seat_selection
/// round-robin (b` would otherwise close the member list and forge a
/// syntactically perfect second strategy clause, a `ghost=default`-shaped one
/// would forge a member of the listing, a backtick would close the quoting a
/// caller wraps the value in, and a `:`-bearing config entry key would forge a
/// second `config check` row on the one line the block gives it.
pub(crate) fn safe(s: &str) -> String {
    sanitize_for_log(s).replace(['(', ')', ',', ';', ':', '=', '`'], "?")
}

#[cfg(test)]
#[path = "seat_report_tests.rs"]
mod seat_report_tests;
