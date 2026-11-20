//! Router-level tests for the pace rule's birth placement.
//!
//! The pure rule is tested in `quota::placement`; what needs a ROUTER is what
//! the rule cannot see: that the store read is keyed the way the poll and the
//! feed write, that the conversation's upstream model reaches the scorer, that
//! a healthy pin never moves, and that the keyed and keyless paths agree.
//!
//! Every seat here has `rpm_available: None`, which is what a real pool reports
//! (no RPM limit configured), so every seat ties on headroom and only the pace
//! rule or the anti-herd rotation can move a pick. Readings arrive as real
//! usage bodies through `Router::observe_polled_usage`.

use super::*;

use async_trait::async_trait;

use crate::config::{PoolEntry, ProviderEntry, SeatSelection};
use crate::quota::usage_body::UsageVendor;
use crate::seat_pool::SeatTarget;

const SEAT_POOL: &str = "seat-pool";
const OPUS: &str = "claude-opus-4-7";
/// The pool's members, paired with the seat label each one's credential
/// reference carries. The first member is the family's DEFAULT seat (keyed by
/// the bare provider) and the others are labelled siblings, so a key
/// derivation that disagreed about the default-vs-labelled distinction would
/// leave a seat unread here.
const SEAT_MEMBERS: [(&str, Option<&str>); 3] = [
    ("seat-default", None),
    ("seat-b", Some("seat-b")),
    ("seat-c", Some("seat-c")),
];

/// A provider that never dispatches. Every assertion here reads the resolved
/// CHAIN, so no seat is ever called.
struct SeatStub {
    id: String,
}

#[async_trait]
impl Provider for SeatStub {
    fn id(&self) -> &str {
        &self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(&self.id, "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        unreachable!("these tests read the resolved chain and never dispatch")
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        unreachable!("these tests read the resolved chain and never dispatch")
    }
}

/// Which vendor the pool's members log in to.
#[derive(Clone, Copy)]
enum Family {
    Anthropic,
    #[cfg(feature = "openai-responses")]
    Codex,
}

impl Family {
    const fn provider(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            #[cfg(feature = "openai-responses")]
            Self::Codex => "codex",
        }
    }

    const fn vendor(self) -> UsageVendor {
        match self {
            Self::Anthropic => UsageVendor::Anthropic,
            #[cfg(feature = "openai-responses")]
            Self::Codex => UsageVendor::Codex,
        }
    }

    fn entry(self, credential: String) -> ProviderEntry {
        match self {
            Self::Anthropic => ProviderEntry::anthropic_api(credential),
            #[cfg(feature = "openai-responses")]
            Self::Codex => ProviderEntry::openai_responses(credential),
        }
    }
}

fn label_of(member: &str) -> Option<&'static str> {
    SEAT_MEMBERS
        .iter()
        .find(|(name, _)| *name == member)
        .map(|(_, label)| *label)
        .expect("every member named by a test is declared in SEAT_MEMBERS")
}

fn secret_ref(family: Family, member: &str) -> routectl_auth::SecretRef {
    routectl_auth::SecretRef::OAuth {
        provider: family.provider().to_string(),
        label: label_of(member).map(str::to_string),
    }
}

/// The credential account key `member` is metered under.
fn account_of(family: Family, member: &str) -> String {
    crate::seat_pool::seat_identity(Some(&secret_ref(family, member)))
        .expect("an oauth ref has an account")
}

/// A three-seat pool of `family` logins under `selection`, serving one model
/// whose upstream id is `upstream`.
fn pool_router(family: Family, selection: SeatSelection, upstream: &str) -> Router {
    let mut seats: Vec<SeatTarget> = Vec::new();
    let mut providers = BTreeMap::new();
    for (member, _) in SEAT_MEMBERS {
        let provider: Arc<dyn Provider> = Arc::new(SeatStub {
            id: member.to_string(),
        });
        let member_ref = secret_ref(family, member);
        seats.push(SeatTarget::new(
            member.to_string(),
            provider,
            Some(member_ref.clone()),
            false,
        ));
        providers.insert(member.to_string(), family.entry(member_ref.to_string()));
    }
    let default_provider = seats[0].provider.clone().expect("seat 0 is built");
    let mut pools = BTreeMap::new();
    pools.insert(
        SEAT_POOL.to_string(),
        PoolEntry::new(
            SEAT_MEMBERS
                .iter()
                .map(|(member, _)| (*member).to_string())
                .collect(),
        )
        .with_seat_selection(selection),
    );
    let cfg = Config {
        providers,
        pools,
        ..Config::default()
    };
    let mut router = Router::new(Arc::new(cfg));
    let model =
        ResolvedModel::new("m", SEAT_POOL, default_provider, upstream).with_seats(Arc::from(seats));
    router.install_resolved_models(std::iter::once(("m".to_string(), Arc::new(model))).collect());
    router
}

fn anthropic_pool() -> Router {
    pool_router(Family::Anthropic, SeatSelection::StickyLeastLoaded, OPUS)
}

fn rfc3339_in(from_now: Duration) -> String {
    let at = time::OffsetDateTime::now_utc() + from_now;
    at.format(&time::format_description::well_known::Rfc3339)
        .expect("an RFC 3339 rendering")
}

/// An Anthropic usage body with a five-hour window at `percent`, `left`
/// before its reset, and the account-wide weekly window `weekly` (`None`
/// reports it as `null`), plus `extra` raw members.
fn anthropic_body(
    percent: u32,
    left: Duration,
    weekly: Option<(u32, Duration)>,
    extra: &str,
) -> String {
    let window = |p: u32, l: Duration| {
        format!(
            r#"{{"utilization": {p}, "resets_at": "{}"}}"#,
            rfc3339_in(l)
        )
    };
    let weekly = weekly.map_or_else(|| "null".to_string(), |(p, l)| window(p, l));
    format!(
        r#"{{"five_hour": {}, "seven_day": {weekly}{extra}}}"#,
        window(percent, left)
    )
}

/// A Codex usage body: a five-hour primary at `primary` with an hour left and
/// a weekly secondary at `secondary` with two days left.
#[cfg(feature = "openai-responses")]
fn codex_body(primary: u32, secondary: u32) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .expect("a post-epoch clock")
        .as_secs();
    let window = |percent: u32, length: u64, after: u64| {
        format!(
            r#"{{"used_percent": {percent}, "limit_window_seconds": {length}, "reset_after_seconds": {after}, "reset_at": {}}}"#,
            now + after
        )
    };
    format!(
        r#"{{"plan_type": "plus", "rate_limit": {{"allowed": true, "limit_reached": false, "primary_window": {}, "secondary_window": {}}}}}"#,
        window(primary, 18_000, 3_600),
        window(secondary, 604_800, 172_800),
    )
}

fn poll(router: &Router, family: Family, member: &str, body: &str) {
    let account = account_of(family, member);
    let generation = router.acquire_credential_generation(&account);
    router
        .observe_polled_usage(&account, family.vendor(), body.as_bytes(), generation)
        .expect("the poll is accepted");
}

/// Poll an Anthropic seat at `percent` of its five-hour window with
/// `left_hours` to run and no weekly window.
fn poll_pace(router: &Router, member: &str, percent: u32, left_hours: f64) {
    poll(
        router,
        Family::Anthropic,
        member,
        &anthropic_body(
            percent,
            Duration::from_secs_f64(left_hours * 3_600.0),
            None,
            "",
        ),
    );
}

/// The resolved walk order for `session` (`None` for keyless) as members.
fn walk(router: &Router, session: Option<&str>) -> Vec<String> {
    router
        .dispatch_chain("m", session)
        .expect("chain resolves")
        .into_iter()
        .map(|target| {
            target
                .state_key
                .strip_prefix("m#")
                .expect("a pooled state key")
                .to_string()
        })
        .collect()
}

fn home_of(router: &Router, session: &str) -> String {
    walk(router, Some(session)).remove(0)
}

// ---- the birth pick follows the pace rule ----

#[test]
fn a_keyed_and_a_keyless_birth_both_land_on_the_best_standing() {
    let rows: [(&str, Option<&str>); 2] = [("keyed", Some("S")), ("keyless", None)];
    for (name, session) in rows {
        // Arrange: seat-c paces best; seat-default holds the rotation's first
        // pick and seat-b sits just below seat-c.
        let router = anthropic_pool();
        poll_pace(&router, "seat-default", 40, 2.5);
        poll_pace(&router, "seat-b", 50, 2.5);
        poll_pace(&router, "seat-c", 20, 2.5);

        // Act
        let order = walk(&router, session);

        // Assert
        assert_eq!(order, ["seat-c", "seat-default", "seat-b"], "row: {name}");
    }
}

#[cfg(feature = "openai-responses")]
#[test]
fn a_codex_pool_places_keyed_and_keyless_births_on_the_best_standing() {
    let rows: [(&str, Option<&str>); 2] = [("keyed", Some("S")), ("keyless", None)];
    for (name, session) in rows {
        // Arrange: seat-b's weekly window is the slowest in the pool;
        // seat-c's is the fastest.
        let router = pool_router(
            Family::Codex,
            SeatSelection::StickyLeastLoaded,
            "gpt-5-codex",
        );
        poll(&router, Family::Codex, "seat-default", &codex_body(10, 50));
        poll(&router, Family::Codex, "seat-b", &codex_body(10, 80));
        poll(&router, Family::Codex, "seat-c", &codex_body(10, 20));

        // Act
        let order = walk(&router, session);

        // Assert
        assert_eq!(order[0], "seat-c", "row: {name}");
    }
}

#[test]
fn an_exhausted_weekly_window_loses_the_birth_to_a_roomy_seat() {
    // seat-default's five-hour window is nearly empty but its weekly is at 95%;
    // seat-b paces poorly on its five-hour window but has room everywhere.
    let router = anthropic_pool();
    poll(
        &router,
        Family::Anthropic,
        "seat-default",
        &anthropic_body(
            0,
            Duration::from_hours(4),
            Some((95, Duration::from_hours(72))),
            "",
        ),
    );
    poll_pace(&router, "seat-b", 85, 4.9);
    poll_pace(&router, "seat-c", 89, 4.9);

    assert_eq!(home_of(&router, "S"), "seat-b");
}

#[test]
fn a_scoped_weekly_window_counts_only_for_its_model_family() {
    // seat-default's opus weekly is at 95%; nothing else separates the seats.
    let opus_weekly = format!(
        r#", "seven_day_opus": {{"utilization": 95, "resets_at": "{}"}}"#,
        rfc3339_in(Duration::from_hours(72))
    );
    let rows: [(&str, &str, &str); 2] = [
        ("an opus conversation avoids it", OPUS, "seat-b"),
        (
            "a sonnet conversation does not",
            "claude-sonnet-4-5",
            "seat-default",
        ),
    ];
    for (name, upstream, expected) in rows {
        let router = pool_router(
            Family::Anthropic,
            SeatSelection::StickyLeastLoaded,
            upstream,
        );
        poll(
            &router,
            Family::Anthropic,
            "seat-default",
            &anthropic_body(10, Duration::from_hours(4), None, &opus_weekly),
        );
        poll_pace(&router, "seat-b", 15, 4.0);
        poll_pace(&router, "seat-c", 15, 4.0);

        assert_eq!(home_of(&router, "S"), expected, "row: {name}, keyed");
        assert_eq!(
            walk(&router, None)[0] == "seat-default",
            expected == "seat-default",
            "row: {name}, keyless"
        );
    }
}

#[test]
fn a_burst_of_near_tied_births_reaches_every_tied_seat_and_never_a_lower_one() {
    // Arrange: seat-default and seat-c score 1.2 within the 0.01 bucket;
    // seat-b scores 1.0, one bucket lower.
    let router = anthropic_pool();
    poll_pace(&router, "seat-default", 40, 2.5);
    poll_pace(&router, "seat-b", 50, 2.5);
    poll_pace(&router, "seat-c", 40, 2.5);

    // Act
    let homes: Vec<String> = (0..8)
        .map(|i| home_of(&router, &format!("burst-{i}")))
        .collect();

    // Assert
    assert!(homes.iter().any(|home| home == "seat-default"), "{homes:?}");
    assert!(homes.iter().any(|home| home == "seat-c"), "{homes:?}");
    assert!(!homes.iter().any(|home| home == "seat-b"), "{homes:?}");
}

#[test]
fn a_seat_with_no_reading_ranks_below_every_read_seat() {
    let router = anthropic_pool();
    poll_pace(&router, "seat-c", 95, 1.0);

    assert_eq!(home_of(&router, "S"), "seat-c");
}

// ---- dormant and unread pools place by headroom and rotation ----

/// The homes a run of births picks with no reading anywhere, which is the
/// pre-rule chooser's answer: every seat ties and the rotation decides.
fn rotation_homes(router: &Router, births: usize) -> Vec<String> {
    (0..births)
        .map(|i| home_of(router, &format!("rot-{i}")))
        .collect()
}

#[test]
fn a_pool_with_no_reading_key_picks_exactly_by_headroom_and_rotation() {
    fn api_key_entry(_: String) -> ProviderEntry {
        ProviderEntry::openai_compat("https://api.example.com/v1", "env://SEAT_KEY")
    }
    // Arrange: seats whose credential is not an OAuth login have no meter key.
    let mut seats: Vec<SeatTarget> = Vec::new();
    let mut providers = BTreeMap::new();
    for (member, _) in SEAT_MEMBERS {
        let provider: Arc<dyn Provider> = Arc::new(SeatStub {
            id: member.to_string(),
        });
        seats.push(SeatTarget::new(
            member.to_string(),
            provider,
            Some(routectl_auth::SecretRef::Env("SEAT_KEY".into())),
            false,
        ));
        providers.insert(member.to_string(), api_key_entry(String::new()));
    }
    let default_provider = seats[0].provider.clone().expect("seat 0 is built");
    let mut pools = BTreeMap::new();
    pools.insert(
        SEAT_POOL.to_string(),
        PoolEntry::new(SEAT_MEMBERS.iter().map(|(m, _)| (*m).to_string()).collect())
            .with_seat_selection(SeatSelection::StickyLeastLoaded),
    );
    let mut router = Router::new(Arc::new(Config {
        providers,
        pools,
        ..Config::default()
    }));
    let model =
        ResolvedModel::new("m", SEAT_POOL, default_provider, OPUS).with_seats(Arc::from(seats));
    router.install_resolved_models(std::iter::once(("m".to_string(), Arc::new(model))).collect());

    // Act
    let keyed = rotation_homes(&router, 4);
    let keyless = walk(&router, None);

    // Assert
    assert_eq!(keyed, ["seat-default", "seat-b", "seat-c", "seat-default"]);
    assert_eq!(keyless, ["seat-default", "seat-b", "seat-c"], "fill-first");
}

#[test]
fn a_fresh_router_and_store_rank_every_seat_unread_and_rotate() {
    // Arrange: a router that placed by readings, then a NEW router with its
    // own new store, as after a restart. The carried-Arc reload path is not
    // used, so nothing read before survives.
    let before = anthropic_pool();
    poll_pace(&before, "seat-c", 10, 4.0);
    assert_eq!(
        home_of(&before, "warm"),
        "seat-c",
        "premise: readings place"
    );
    let restarted = anthropic_pool();

    // Act
    let homes = rotation_homes(&restarted, 4);
    let keyless = walk(&restarted, None);
    let standings = restarted.birth_standings(
        restarted.resolved_models["m"]
            .seats
            .as_deref()
            .expect("a pool"),
        OPUS,
    );

    // Assert
    assert_eq!(keyless, ["seat-default", "seat-b", "seat-c"], "fill-first");
    assert!(
        standings
            .iter()
            .all(|standing| standing.tier == crate::quota::placement::Tier::Unread),
        "{standings:?}"
    );
    assert_eq!(homes, ["seat-default", "seat-b", "seat-c", "seat-default"]);
}

// ---- established conversations ----

#[test]
fn a_warm_pin_stays_when_its_seat_reads_exhausted() {
    // Arrange: the birth lands on seat-c, the only roomy seat; seat-b has
    // no reading yet.
    let router = anthropic_pool();
    poll_pace(&router, "seat-default", 95, 4.0);
    poll_pace(&router, "seat-c", 10, 4.0);
    assert_eq!(home_of(&router, "S"), "seat-c", "premise");

    // Act: seat-c is now exhausted and seat-b reads empty.
    poll(
        &router,
        Family::Anthropic,
        "seat-c",
        &anthropic_body(
            100,
            Duration::from_hours(4),
            Some((100, Duration::from_hours(72))),
            "",
        ),
    );
    poll_pace(&router, "seat-b", 0, 4.9);

    // Assert
    assert_eq!(home_of(&router, "S"), "seat-c");
}

#[test]
fn a_pace_birth_writes_a_pin_that_a_follow_up_keeps() {
    let router = anthropic_pool();
    poll_pace(&router, "seat-c", 0, 4.0);
    let pin_key = super::chain::sticky_pin_key("S", SEAT_POOL);

    let birth = walk(&router, Some("S"));
    let pin = router
        .sticky_pins
        .get(&pin_key)
        .expect("a keyed birth writes a pin");
    let stay = walk(&router, Some("S"));

    assert_eq!(birth[0], "seat-c");
    assert_eq!(pin.member, "seat-c");
    assert_eq!(stay, birth);
    assert_eq!(router.sticky_pins.get(&pin_key), Some(pin));
}

#[test]
fn a_keyless_request_creates_no_pin() {
    let router = anthropic_pool();
    poll_pace(&router, "seat-c", 0, 4.0);

    let order = walk(&router, None);

    assert_eq!(order[0], "seat-c");
    assert!(router.sticky_pins.is_empty());
}

// ---- which selections the rule reaches ----

#[test]
fn fill_first_drains_seat_zero_whatever_the_readings() {
    let router = pool_router(Family::Anthropic, SeatSelection::FillFirst, OPUS);
    poll_pace(&router, "seat-c", 0, 4.0);

    for session in [Some("S"), None, Some("S")] {
        assert_eq!(walk(&router, session), ["seat-default", "seat-b", "seat-c"]);
    }
    assert!(router.sticky_pins.is_empty(), "fill-first mints no pin");
}
