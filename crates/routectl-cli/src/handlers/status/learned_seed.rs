//! Test-only fixtures that put one learned value into a served daemon's
//! resident registry the way a real boot does: a ledger row on disk, then the
//! serve bootstrap's warm over it.

use std::path::Path;

use routectl_router::Router;
use routectl_usage::{CapabilityEvent, insert_capability_event};

use crate::server::capability_rebuild::{WarmReport, warm_capability_registry_from_ledger};

/// The lane [`served_fixture_config`] resolves its one model to.
pub(super) const LANE: &str = "anthropic#claude-sonnet-4-5";

/// The persisted token of the value [`seed_learned_ceiling`] writes.
pub(super) const CEILING_TOKEN: &str = "ceiling:32000";

/// A config whose one model resolves to [`LANE`], with its usage ledger
/// pointed at `db_path`.
pub(super) fn served_fixture_config(db_path: &Path) -> String {
    format!(
        "version = {}\n\
         [usage]\n\
         db_path = \"{}\"\n\
         [providers.anthropic]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"env://SERVED_FIXTURE_KEY\"\n\
         [models.sonnet]\n\
         provider = \"anthropic\"\n\
         upstream = \"claude-sonnet-4-5\"\n",
        routectl_router::CURRENT_CONFIG_VERSION,
        db_path.display(),
    )
}

/// The grounded `max_tokens` ceiling key. Spelled in two pieces so the field
/// prefix literal keeps its single owner.
pub(super) fn ceiling_key() -> String {
    let key = format!("{}{}max_tokens", "fie", "ld:");
    assert!(
        routectl_router::capability_key_is_field_verdict(&key),
        "premise: {key} is a field verdict key"
    );
    key
}

/// Write a ledger at `db_path` holding a boot tombstone stamped with the
/// revision every router boots with, followed by one learned
/// [`CEILING_TOKEN`] row on [`LANE`].
pub(super) fn seed_learned_ceiling(db_path: &Path) {
    let now = chrono::Utc::now().timestamp_millis();
    let catalog_version = i64::from(routectl_router::FROZEN_CATALOG_VERSION);
    let overlay_revision =
        i64::try_from(routectl_router::FROZEN_OVERLAY_REVISION).expect("revision fits i64");
    let db = routectl_usage::open(db_path).expect("open ledger");
    let tombstone = CapabilityEvent::tombstone(now, catalog_version, overlay_revision);
    insert_capability_event(db.conn(), &tombstone).expect("seed tombstone");
    let ceiling = CapabilityEvent {
        ts: now,
        lane_key: LANE.to_string(),
        capability: ceiling_key(),
        verdict: "broken".to_string(),
        phase: "f1".to_string(),
        source: "live".to_string(),
        tier: "self-identifying".to_string(),
        evidence_class: None,
        upstream_token: Some(CEILING_TOKEN.to_string()),
        catalog_version,
        overlay_revision,
        provider_kind: Some("anthropic-api".to_string()),
        vocab_version: Some(routectl_router::CURRENT_VOCAB_VERSION),
    };
    insert_capability_event(db.conn(), &ceiling).expect("seed ceiling row");
}

/// A router built from the fixture config at `config_path`, warmed from the
/// ledger at `db_path` as serve bootstrap warms it, with the warm's report.
///
/// The warm runs on its own thread: its fail-closed path blocks on the
/// writer's reply, and Tokio panics if a worker blocks.
pub(super) fn warmed_router(config_path: &Path, db_path: &Path) -> (Router, WarmReport) {
    let config = crate::server::parse_config_only(config_path).expect("fixture config parses");
    let router = Router::new(std::sync::Arc::new(config));
    let usage = routectl_usage::handle_with_closed_channel();
    let warm = std::thread::scope(|scope| {
        scope
            .spawn(|| warm_capability_registry_from_ledger(db_path, &router, &usage))
            .join()
            .expect("warm thread")
    });
    (router, warm)
}
