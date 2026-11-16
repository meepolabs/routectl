//! Config-diff classification shared by the `serve` hot-reload logging
//! path and `config set`/`config unset` (`commands::config_edit`).
//!
//! Two runtime diff functions plus a drift tripwire. The functions encode
//! `serve` RUNTIME semantics -- what a live config swap actually applies vs
//! what waits for a daemon restart, and which egress-defining edits warrant
//! an operator confirmation -- so they live next to the reload code that
//! defines them, not in the schema-owning router crate.

use std::collections::BTreeSet;

use routectl_router::config::{Auth, Cloud, Format};
use routectl_router::{Config, ProviderEntry};

/// Top-level `[Config]` sections whose change requires a daemon restart to
/// take effect. Coarse per-SECTION bucketing for the drift tripwire (see
/// [`every_top_level_field_is_classified`]); the runtime
/// [`collect_restart_required_changes`] diffs the individual knobs inside.
#[cfg(test)]
pub const RESTART_REQUIRED_SECTIONS: &[&str] = &["server", "usage", "mitm"];

/// Top-level sections carrying egress-defining knobs (where requests go,
/// which credential authenticates them) that warrant an operator prompt on
/// `config set`. A `pools` edit changes which credential authenticates a
/// request just as directly as a provider edit does: adding or removing a
/// member repoints traffic onto a different account. The `[mitm]` section is
/// egress-relevant too, but its whole block is restart-required, so it is
/// bucketed there; the runtime [`collect_high_consequence_changes`] still
/// flags its egress fields.
#[cfg(test)]
pub const HIGH_CONSEQUENCE_SECTIONS: &[&str] = &["providers", "pools"];

/// Top-level sections that hot-reload cleanly on the next config swap with
/// no restart and no confirmation prompt.
#[cfg(test)]
pub const HOT_RELOADABLE_SECTIONS: &[&str] = &[
    "version",
    "aliases",
    "retry",
    "models",
    "cache",
    "reduction",
    "window_gate",
    "capability",
];

/// Diff the previous config against the new one and return the names of
/// fields whose change requires a daemon restart to take effect. Per the
/// architect-validated classification: bind, listener auth and the
/// `DefaultBodyLimit` axum layer all stay restart-required.
/// `usage.db_path` (the writer holds a handle opened at boot) and
/// `usage.retention_days` (pruning runs only at startup, so a changed
/// value takes effect only on the next daemon start) are
/// restart-required too. A `[mitm]` edit
/// is restart-required because the front-proxy listener is spawned once at
/// boot.
pub fn collect_restart_required_changes(prev: &Config, next: &Config) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();

    if prev.server.host != next.server.host {
        out.push("server.host");
    }
    if prev.server.port != next.server.port {
        out.push("server.port");
    }
    let prev_tokens: &[String] = prev
        .server
        .auth
        .as_ref()
        .map_or(&[], |a| a.tokens.as_slice());
    let next_tokens: &[String] = next
        .server
        .auth
        .as_ref()
        .map_or(&[], |a| a.tokens.as_slice());
    if prev_tokens != next_tokens {
        out.push("server.auth.tokens");
    }
    if prev.server.max_body_bytes != next.server.max_body_bytes {
        out.push("server.max_body_bytes");
    }

    if prev.usage.db_path != next.usage.db_path {
        out.push("usage.db_path");
    }
    if prev.usage.retention_days != next.usage.retention_days {
        out.push("usage.retention_days");
    }

    if prev.mitm != next.mitm {
        out.push("mitm");
    }

    // codex_version is stamped into a process-global identity set ONCE at
    // boot (routectl-core identity/codex.rs), so a hot reload cannot flip
    // it -- classify it restart-required to make that set-once contract
    // honest. Diffed per provider across the union of keys; a value
    // appearing, disappearing, or changing on any entry counts.
    let mut codex_keys: BTreeSet<&str> = BTreeSet::new();
    codex_keys.extend(prev.providers.keys().map(String::as_str));
    codex_keys.extend(next.providers.keys().map(String::as_str));
    let codex_changed = codex_keys.into_iter().any(|key| {
        let prev_v = prev
            .providers
            .get(key)
            .and_then(ProviderEntry::codex_version);
        let next_v = next
            .providers
            .get(key)
            .and_then(ProviderEntry::codex_version);
        prev_v != next_v
    });
    if codex_changed {
        out.push("providers.codex_version");
    }

    out
}

/// Diff the previous config against the new one and return the names of
/// egress-defining fields whose change is high-consequence: the operator is
/// prompted before such an edit lands (`config set` reuses the confirmation
/// pattern; `--yes` bypasses). Covers every provider entry's identity keys
/// (`format`, `base_url`, `cloud`, `credential`, `auth`, `account_id_ref`,
/// `cloud_project_id`: where requests go, in which API, with which
/// credential) and every pool's membership (which repoints traffic onto a
/// different account's credential). `[mitm]` carries only local knobs
/// (`listen_port`, `cert_dir`; its upstream is pinned in code), so it stays
/// out; so does a pool's `seat_selection`, which reorders members without
/// changing the set of credentials in play.
pub fn collect_high_consequence_changes(prev: &Config, next: &Config) -> Vec<&'static str> {
    let mut keys: BTreeSet<&str> = BTreeSet::new();
    keys.extend(prev.providers.keys().map(String::as_str));
    keys.extend(next.providers.keys().map(String::as_str));

    let mut changed = [false; PROVIDER_IDENTITY_FIELDS.len()];
    for key in keys {
        let prev_identity = ProviderIdentity::of(prev.providers.get(key));
        let next_identity = ProviderIdentity::of(next.providers.get(key));
        for (slot, differs) in changed
            .iter_mut()
            .zip(prev_identity.differing_fields(&next_identity))
        {
            *slot |= differs;
        }
    }
    let mut out: Vec<&'static str> = PROVIDER_IDENTITY_FIELDS
        .iter()
        .zip(changed)
        .filter_map(|(label, differs)| differs.then_some(*label))
        .collect();

    let mut pool_keys: BTreeSet<&str> = BTreeSet::new();
    pool_keys.extend(prev.pools.keys().map(String::as_str));
    pool_keys.extend(next.pools.keys().map(String::as_str));
    let members_of = |config: &Config, key: &str| {
        config
            .pools
            .get(key)
            .map(|pool| pool.members.clone())
            .unwrap_or_default()
    };
    if pool_keys
        .into_iter()
        .any(|key| members_of(prev, key) != members_of(next, key))
    {
        out.push("pools.members");
    }

    out
}

/// The change label of each [`ProviderIdentity`] field, in the order
/// [`ProviderIdentity::differing_fields`] answers.
const PROVIDER_IDENTITY_FIELDS: [&str; 7] = [
    "providers.format",
    "providers.base_url",
    "providers.cloud",
    "providers.credential",
    "providers.auth",
    "providers.account_id_ref",
    "providers.cloud_project_id",
];

/// The egress-defining projection of a provider entry: the API it speaks
/// (`format`), where requests go (`base_url` or `cloud`, whose region
/// derives the endpoint), which credential authenticates them and how it is
/// sent (`credential`, `auth`), and the companions that select an account
/// or project. An absent entry projects to all `None`, so adding or
/// removing a provider flags every key it sets.
#[derive(Default)]
struct ProviderIdentity<'a> {
    format: Option<Format>,
    base_url: Option<&'a str>,
    cloud: Option<&'a Cloud>,
    credential: Option<&'a str>,
    auth: Option<Auth>,
    account_id_ref: Option<&'a str>,
    cloud_project_id: Option<&'a str>,
}

impl<'a> ProviderIdentity<'a> {
    fn of(entry: Option<&'a ProviderEntry>) -> Self {
        entry.map_or_else(Self::default, |entry| Self {
            format: Some(entry.format),
            base_url: entry.base_url.as_deref(),
            cloud: entry.cloud.as_ref(),
            credential: Some(entry.credential.as_str()),
            auth: Some(entry.auth),
            account_id_ref: entry.account_id_ref.as_deref(),
            cloud_project_id: entry.cloud_project_id.as_deref(),
        })
    }

    /// Per field, in [`PROVIDER_IDENTITY_FIELDS`] order: whether it differs
    /// from `other`.
    fn differing_fields(&self, other: &Self) -> [bool; PROVIDER_IDENTITY_FIELDS.len()] {
        [
            self.format != other.format,
            self.base_url != other.base_url,
            self.cloud != other.cloud,
            self.credential != other.credential,
            self.auth != other.auth,
            self.account_id_ref != other.account_id_ref,
            self.cloud_project_id != other.cloud_project_id,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_restart_required_changes_flags_bind_and_log() {
        use routectl_router::{Config, ServerAuth, ServerConfig};

        // `next` is cloned from `prev` (not a second `Config::default()`) so
        // both share an identical baseline -- including `usage.db_path`, whose
        // default reads `XDG_CONFIG_HOME`/`HOME` and so can differ between two
        // independent `Config::default()` calls if a concurrent test mutates
        // those env vars. Cloning keeps this test hermetic against that.
        let mut prev = Config::default();
        let mut next = prev.clone();

        // Baseline: identical configs -> empty list.
        assert!(collect_restart_required_changes(&prev, &next).is_empty());

        // Host change -> server.host.
        next.server = ServerConfig {
            host: "0.0.0.0".into(),
            ..ServerConfig::default()
        };
        let changes = collect_restart_required_changes(&prev, &next);
        assert!(changes.contains(&"server.host"), "got {changes:?}");

        // Token change -> server.auth.tokens.
        prev.server = ServerConfig::default();
        next.server = ServerConfig {
            auth: Some(ServerAuth {
                tokens: vec!["literal:tok-1".into()],
            }),
            ..ServerConfig::default()
        };
        let changes = collect_restart_required_changes(&prev, &next);
        assert!(changes.contains(&"server.auth.tokens"), "got {changes:?}");

        // usage.db_path change -> restart-required.
        prev = Config::default();
        next = prev.clone();
        next.usage.db_path = std::path::PathBuf::from("/tmp/other-usage.db");
        let changes = collect_restart_required_changes(&prev, &next);
        assert!(changes.contains(&"usage.db_path"), "got {changes:?}");

        // usage.retention_days change -> restart-required (pruning is
        // startup-only, so a changed value takes effect only at the next
        // daemon start; the reload printout must not silently drop it).
        prev = Config::default();
        next = prev.clone();
        next.usage.retention_days = prev.usage.retention_days + 1;
        let changes = collect_restart_required_changes(&prev, &next);
        assert!(
            changes.contains(&"usage.retention_days"),
            "retention_days must be restart-required; got {changes:?}"
        );

        // [mitm] edit -> restart-required (the MITM listener is
        // startup-only; a hot-reloaded edit has no live listener to
        // apply to).
        use routectl_router::MitmConfig;
        prev = Config::default();
        next = prev.clone();
        next.mitm = Some(MitmConfig::default());
        let changes = collect_restart_required_changes(&prev, &next);
        assert!(changes.contains(&"mitm"), "got {changes:?}");

        prev = Config::default();
        prev.mitm = Some(MitmConfig::default());
        next = prev.clone();
        next.mitm = Some(MitmConfig {
            listen_port: prev.mitm.as_ref().unwrap().listen_port + 1,
            ..MitmConfig::default()
        });
        let changes = collect_restart_required_changes(&prev, &next);
        assert!(changes.contains(&"mitm"), "got {changes:?}");
    }

    /// `codex_version` is stamped into the process-global codex identity
    /// once at boot, so a hot reload cannot flip it -- classify it
    /// restart-required. A no-op when neither side sets it; flagged when a
    /// value appears, changes, or disappears.
    #[test]
    fn collect_restart_required_flags_codex_version() {
        use routectl_router::ProviderEntry;

        // Baseline: no codex_version on either side -> not flagged.
        let mut prev = Config::default();
        prev.providers.insert(
            "codex".into(),
            ProviderEntry::openai_responses("oauth://codex"),
        );
        let mut next = prev.clone();
        assert!(
            !collect_restart_required_changes(&prev, &next).contains(&"providers.codex_version")
        );

        // Value appears -> flagged.
        next.providers.insert(
            "codex".into(),
            ProviderEntry::openai_responses("oauth://codex")
                .with_openai_responses_codex_version("0.200.0"),
        );
        assert!(
            collect_restart_required_changes(&prev, &next).contains(&"providers.codex_version"),
            "a newly-set codex_version must be restart-required"
        );

        // Value changes -> flagged.
        prev = next.clone();
        next.providers.insert(
            "codex".into(),
            ProviderEntry::openai_responses("oauth://codex")
                .with_openai_responses_codex_version("0.201.0"),
        );
        assert!(
            collect_restart_required_changes(&prev, &next).contains(&"providers.codex_version"),
            "a changed codex_version must be restart-required"
        );
    }

    #[test]
    fn high_consequence_flags_provider_base_url() {
        use routectl_router::ProviderEntry;

        let mut prev = Config::default();
        prev.providers.insert(
            "anthropic".into(),
            ProviderEntry::anthropic_api("env://KEY"),
        );
        let mut next = prev.clone();

        // No-op: identical configs -> empty.
        assert!(collect_high_consequence_changes(&prev, &next).is_empty());

        // base_url change -> providers.base_url.
        next.providers.insert(
            "anthropic".into(),
            ProviderEntry::anthropic_api("env://KEY")
                .with_base_url("https://elsewhere.example.com"),
        );
        let changes = collect_high_consequence_changes(&prev, &next);
        assert!(changes.contains(&"providers.base_url"), "got {changes:?}");
    }

    /// A pool membership edit repoints traffic onto a different account's
    /// credential, so it is high-consequence. A `seat_selection` edit
    /// reorders the same credentials and is not.
    #[test]
    fn high_consequence_flags_pool_membership_but_not_its_strategy() {
        use routectl_router::PoolEntry;
        use routectl_router::config::SeatSelection;

        // Arrange
        let mut prev = Config::default();
        prev.pools.insert(
            "anthropic".into(),
            PoolEntry::new(vec!["anthropic-default".into()]),
        );
        let mut next = prev.clone();

        // No-op: identical configs -> empty.
        assert!(collect_high_consequence_changes(&prev, &next).is_empty());

        // Strategy-only edit -> not egress.
        next.pools.insert(
            "anthropic".into(),
            PoolEntry::new(vec!["anthropic-default".into()])
                .with_seat_selection(SeatSelection::FillFirst),
        );
        assert!(
            collect_high_consequence_changes(&prev, &next).is_empty(),
            "a strategy edit changes no credential"
        );

        // Membership edit -> pools.members.
        let mut grown = prev.clone();
        grown.pools.insert(
            "anthropic".into(),
            PoolEntry::new(vec!["anthropic-default".into(), "anthropic-work".into()]),
        );
        let changes = collect_high_consequence_changes(&prev, &grown);
        assert!(changes.contains(&"pools.members"), "got {changes:?}");

        // A pool appearing or disappearing counts too.
        let changes = collect_high_consequence_changes(&Config::default(), &prev);
        assert!(changes.contains(&"pools.members"), "got {changes:?}");
    }

    /// A pool's breaker and RPM keys reach the live router through the
    /// reload's retune of carried slots, so editing them needs no restart
    /// and changes no credential.
    #[test]
    fn pool_tuning_edit_is_neither_restart_required_nor_high_consequence() {
        use routectl_router::PoolEntry;

        // Arrange
        let mut prev = Config::default();
        prev.pools
            .insert("pool-a".into(), PoolEntry::new(vec!["seat-a".into()]));
        let mut next = prev.clone();
        next.pools.insert(
            "pool-a".into(),
            PoolEntry::new(vec!["seat-a".into()])
                .with_circuit_failures(3)
                .with_circuit_cooldown_ms(5_000)
                .with_rpm_limit(10),
        );

        // Act
        let restart = collect_restart_required_changes(&prev, &next);
        let high = collect_high_consequence_changes(&prev, &next);

        // Assert
        assert!(restart.is_empty(), "got {restart:?}");
        assert!(high.is_empty(), "got {high:?}");
    }

    /// Switching an entry to the client's forwarded credential changes who
    /// pays and which bearer reaches the upstream: `credential` and `auth`
    /// both flag, while the unchanged carrier and format do not.
    #[test]
    fn high_consequence_flags_a_switch_to_the_forwarded_credential() {
        use routectl_router::{Auth, ProviderEntry};

        let mut prev = Config::default();
        prev.providers.insert(
            "anthropic".into(),
            ProviderEntry::anthropic_api("env://KEY"),
        );
        assert!(collect_high_consequence_changes(&prev, &prev.clone()).is_empty());

        let mut next = prev.clone();
        next.providers.insert(
            "anthropic".into(),
            ProviderEntry::anthropic_api("forwarded").with_auth(Auth::Bearer),
        );
        let changes = collect_high_consequence_changes(&prev, &next);
        assert_eq!(
            changes,
            vec!["providers.credential", "providers.auth"],
            "got {changes:?}"
        );
    }

    /// Each egress-defining key flags under its own label and no other, so
    /// the prompt names exactly the key the operator touched. One row per
    /// [`PROVIDER_IDENTITY_FIELDS`] label; the `prev` side is shared.
    #[test]
    fn high_consequence_names_exactly_the_one_identity_key_that_changed() {
        let prev_text = "[providers.p]\n\
                         format = \"openai-responses\"\n\
                         base_url = \"https://chatgpt.com/backend-api/codex\"\n\
                         credential = \"oauth://codex\"\n\
                         auth = \"bearer\"\n";
        let rows: &[(&str, &str, &str)] = &[
            (
                "format",
                "providers.format",
                "[providers.p]\nformat = \"openai-chat\"\n\
                 base_url = \"https://chatgpt.com/backend-api/codex\"\n\
                 credential = \"oauth://codex\"\nauth = \"bearer\"\n",
            ),
            (
                "base_url",
                "providers.base_url",
                "[providers.p]\nformat = \"openai-responses\"\n\
                 base_url = \"https://api.openai.com/v1\"\n\
                 credential = \"oauth://codex\"\nauth = \"bearer\"\n",
            ),
            (
                "credential",
                "providers.credential",
                "[providers.p]\nformat = \"openai-responses\"\n\
                 base_url = \"https://chatgpt.com/backend-api/codex\"\n\
                 credential = \"oauth://codex#work\"\nauth = \"bearer\"\n",
            ),
            (
                "auth",
                "providers.auth",
                "[providers.p]\nformat = \"openai-responses\"\n\
                 base_url = \"https://chatgpt.com/backend-api/codex\"\n\
                 credential = \"oauth://codex\"\nauth = \"x-api-key\"\n",
            ),
            (
                "account_id_ref",
                "providers.account_id_ref",
                "[providers.p]\nformat = \"openai-responses\"\n\
                 base_url = \"https://chatgpt.com/backend-api/codex\"\n\
                 credential = \"oauth://codex\"\nauth = \"bearer\"\n\
                 account_id_ref = \"env://ACCOUNT\"\n",
            ),
            (
                "cloud_project_id",
                "providers.cloud_project_id",
                "[providers.p]\nformat = \"openai-responses\"\n\
                 base_url = \"https://chatgpt.com/backend-api/codex\"\n\
                 credential = \"oauth://codex\"\nauth = \"bearer\"\n\
                 cloud_project_id = \"proj-1\"\n",
            ),
        ];
        let prev: Config = toml::from_str(prev_text).expect("prev parses");
        for (row, label, next_text) in rows {
            let next: Config = toml::from_str(next_text)
                .unwrap_or_else(|e| panic!("row `{row}` next parses: {e}"));

            let changes = collect_high_consequence_changes(&prev, &next);

            assert_eq!(changes, vec![*label], "row `{row}`");
        }
    }

    /// The cloud carrier is egress-defining (its region derives the
    /// endpoint host and SigV4 scope), so a region change is
    /// high-consequence -- prompted like `base_url`.
    #[test]
    fn high_consequence_flags_a_cloud_region_change() {
        let prev: Config = toml::from_str(
            "[providers.mantle]\n\
             format = \"anthropic-messages\"\n\
             cloud = { name = \"aws\", region = \"us-east-1\" }\n\
             credential = \"aws://chain\"\n\
             auth = \"aws-sigv4\"\n",
        )
        .expect("cloud config parses");

        // No-op: identical configs -> no cloud flag.
        assert!(collect_high_consequence_changes(&prev, &prev.clone()).is_empty());

        let next: Config = toml::from_str(
            "[providers.mantle]\n\
             format = \"anthropic-messages\"\n\
             cloud = { name = \"aws\", region = \"eu-west-1\" }\n\
             credential = \"aws://chain\"\n\
             auth = \"aws-sigv4\"\n",
        )
        .expect("cloud config parses");
        let changes = collect_high_consequence_changes(&prev, &next);
        assert_eq!(changes, vec!["providers.cloud"], "got {changes:?}");
    }

    /// Moving an OpenAI-shape entry from a base URL onto the cloud carrier
    /// flags both carrier keys and the credential pair, on `openai-chat`
    /// and `openai-responses` alike; a cloud region change on the Responses
    /// format flags the cloud alone.
    #[test]
    fn high_consequence_flags_openai_formats_moving_onto_the_cloud() {
        let on_cloud = |format: &str, region: &str| -> Config {
            toml::from_str(&format!(
                "[providers.p]\n\
                 format = \"{format}\"\n\
                 cloud = {{ name = \"aws\", region = \"{region}\" }}\n\
                 credential = \"aws://chain\"\n\
                 auth = \"aws-sigv4\"\n"
            ))
            .expect("cloud config parses")
        };
        for format in ["openai-chat", "openai-responses"] {
            let prev: Config = toml::from_str(&format!(
                "[providers.p]\n\
                 format = \"{format}\"\n\
                 base_url = \"https://example.invalid/v1\"\n\
                 credential = \"env://KEY\"\n\
                 auth = \"bearer\"\n"
            ))
            .expect("base_url config parses");

            let changes = collect_high_consequence_changes(&prev, &on_cloud(format, "us-east-1"));

            assert_eq!(
                changes,
                vec![
                    "providers.base_url",
                    "providers.cloud",
                    "providers.credential",
                    "providers.auth"
                ],
                "format `{format}`"
            );
        }

        let changes = collect_high_consequence_changes(
            &on_cloud("openai-responses", "us-east-1"),
            &on_cloud("openai-responses", "eu-west-1"),
        );
        assert_eq!(changes, vec!["providers.cloud"], "got {changes:?}");
    }

    /// `[mitm]` names no egress: enabling the block or editing a local knob
    /// raises no high-consequence flag.
    #[test]
    fn high_consequence_never_flags_a_mitm_edit() {
        use routectl_router::MitmConfig;

        let prev = Config::default();
        let mut next = prev.clone();
        next.mitm = Some(MitmConfig::default());
        assert!(collect_high_consequence_changes(&prev, &next).is_empty());

        let prev = Config {
            mitm: Some(MitmConfig::default()),
            ..Config::default()
        };
        let mut next = prev.clone();
        next.mitm = Some(MitmConfig {
            listen_port: prev.mitm.as_ref().unwrap().listen_port + 1,
            ..MitmConfig::default()
        });
        assert!(collect_high_consequence_changes(&prev, &next).is_empty());
    }

    /// Drift tripwire: every top-level `Config` field the schema exposes must
    /// be classified into EXACTLY ONE of the three sets. A new unclassified
    /// `Config` field fails this test, forcing a deliberate reload-semantics
    /// decision instead of a silent omission from the diff classifiers.
    #[test]
    fn every_top_level_field_is_classified() {
        let schema: serde_json::Value =
            serde_json::from_str(&routectl_router::schema_gen::render_schema_json())
                .expect("rendered config schema parses");
        let properties = schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .expect("config schema root carries top-level properties");

        let classified: BTreeSet<&str> = RESTART_REQUIRED_SECTIONS
            .iter()
            .chain(HIGH_CONSEQUENCE_SECTIONS)
            .chain(HOT_RELOADABLE_SECTIONS)
            .copied()
            .collect();

        // Disjoint: the three sets must not overlap (exactly-one membership).
        let total = RESTART_REQUIRED_SECTIONS.len()
            + HIGH_CONSEQUENCE_SECTIONS.len()
            + HOT_RELOADABLE_SECTIONS.len();
        assert_eq!(
            total,
            classified.len(),
            "classification sets overlap; each field must appear in exactly one"
        );

        let schema_fields: BTreeSet<&str> = properties.keys().map(String::as_str).collect();
        assert_eq!(
            schema_fields,
            classified,
            "top-level Config fields and classification sets diverged: \
             unclassified={:?}, stale={:?}",
            schema_fields.difference(&classified).collect::<Vec<_>>(),
            classified.difference(&schema_fields).collect::<Vec<_>>(),
        );
    }
}
