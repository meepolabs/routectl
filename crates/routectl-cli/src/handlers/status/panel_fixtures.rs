//! Test-only fully populated `/status` payloads: the `/status` aggregate built
//! from each panel's own populated fixture, the shared config and clock those
//! fixtures read, and the density walk that proves a fixture leaves no array
//! empty and no key always null.
//!
//! Every payload is a real panel DTO serialized by its real serializer, so a
//! test that reads these keys reads what the daemon would send. The per-panel
//! builders live in each panel's `fixture` child module, where the panel's
//! private DTO fields and mappers are reachable.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use routectl_core::failure_class::LastOutcome;
use routectl_core::{EligibilityReason, UpstreamReason};
use routectl_router::router::RouteTargetStatus;
use routectl_router::runtime_state::{
    CircuitPhase, CredentialParkStatus, CredentialUnusableStatus, ProviderGateStatus, UnusableCause,
};
use routectl_router::{Config, Router, StateKey};
use serde_json::Value;

use super::{AggregatePanels, StatusAggregate};

/// The instant every fixture reads as "now": 2026-11-18T12:00:00Z.
pub const FIXTURE_NOW_MS: i64 = 1_795_003_200_000;

/// [`FIXTURE_NOW_MS`] as an `OffsetDateTime`.
pub fn fixture_now() -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(FIXTURE_NOW_MS / 1000)
        .expect("the fixture instant is representable")
}

/// The `as_of` every fixture envelope carries.
pub fn fixture_as_of() -> String {
    super::utc_rfc3339(FIXTURE_NOW_MS)
}

/// The served config the config, doctor and query fixtures share: a priced
/// API-key provider, a subscription seat, an unpriced compat provider and a
/// cloud provider, a model per provider, both alias shapes, an operator retry
/// count and both capability override verdicts.
pub fn fixture_config() -> Config {
    let text = format!(
        r#"version = {version}

[providers.anthropic]
format = "anthropic-messages"
base_url = "https://api.anthropic.com"
credential = "env://FIXTURE_ANTHROPIC_KEY"
auth = "x-api-key"
rpm_limit = 50

[providers.claude-seat]
format = "anthropic-messages"
base_url = "https://api.anthropic.com"
credential = "oauth://anthropic#a"
auth = "bearer"

[providers.compat]
format = "openai-chat"
base_url = "https://compat.example/v1"
credential = "env://FIXTURE_COMPAT_KEY"
auth = "bearer"

[providers.bed]
format = "bedrock-invoke"
cloud = {{ name = "aws", region = "us-east-1" }}
credential = "aws://chain"
auth = "aws-sigv4"

[models.sonnet]
provider = "anthropic"
upstream = "claude-sonnet-4-5"
context_window = 200000
max_output_tokens = 32000

[models.sonnet.pricing]
output_per_mtok = 15.0

[models.seat-sonnet]
provider = "claude-seat"
upstream = "claude-sonnet-4-5"

[models.glm]
provider = "compat"
upstream = "glm-5"

[models.bedclaude]
provider = "bed"
upstream = "anthropic.claude-test-v1:0"

[aliases]
fast = "sonnet"
smart = ["seat-sonnet", "sonnet", "glm"]

[retry.classes]
rate-limited = 3

[capability.overrides.anthropic]
unsupported = ["computer_use"]
force_supported = ["structured_output"]
"#,
        version = routectl_router::CURRENT_CONFIG_VERSION,
    );
    toml::from_str(&text).expect("the fixture config parses")
}

/// A live router over [`fixture_config`].
pub fn fixture_router() -> Router {
    Router::new(Arc::new(fixture_config()))
}

/// One route target per gate state the health and doctor panels tell apart,
/// each served by a [`fixture_config`] model:
/// - `sonnet`: eligible, closed, last outcome ok, a capped RPM bucket;
/// - `seat-sonnet`: an OAuth seat whose credential is unusable;
/// - `glm`: breaker open after a transport error, unlimited RPM, a probe
///   claimed;
/// - `bedclaude`: parked, half-open, nothing settled yet, no learned lane, and
///   the client's own credential forwarded (no account).
pub fn fixture_targets() -> Vec<RouteTargetStatus> {
    let target = |nickname: &str, provider: &str, upstream: &str, gate| RouteTargetStatus {
        state_key: nickname.to_string(),
        nickname: nickname.to_string(),
        provider_name: provider.to_string(),
        upstream: upstream.to_string(),
        seat_label: None,
        account: Some(format!("#{provider}")),
        learned_lane: StateKey::parse(&format!("{provider}#{upstream}")),
        gate,
    };
    let closed = ProviderGateStatus {
        reason: None,
        rpm_available: Some(42.5),
        circuit: CircuitPhase::Closed,
        half_open_probe_in_flight: false,
        circuit_open_elapsed: None,
        last_outcome: Some(LastOutcome::Ok),
        last_outcome_elapsed: Some(Duration::from_secs(4)),
        credential_park: None,
        credential_unusable: None,
    };
    let unusable = RouteTargetStatus {
        seat_label: Some("a".to_string()),
        account: Some("anthropic#a".to_string()),
        ..target(
            "seat-sonnet",
            "claude-seat",
            "claude-sonnet-4-5",
            ProviderGateStatus {
                reason: Some(EligibilityReason::CredentialUnusable),
                credential_unusable: Some(CredentialUnusableStatus {
                    cause: UnusableCause::Expired,
                    since_elapsed: Duration::from_mins(15),
                }),
                ..closed
            },
        )
    };
    let breaker_open = target(
        "glm",
        "compat",
        "glm-5",
        ProviderGateStatus {
            reason: Some(EligibilityReason::BreakerOpen),
            rpm_available: None,
            circuit: CircuitPhase::Open,
            half_open_probe_in_flight: true,
            circuit_open_elapsed: Some(Duration::from_secs(20)),
            last_outcome: Some(LastOutcome::TransportError),
            ..closed
        },
    );
    let parked = RouteTargetStatus {
        account: None,
        learned_lane: None,
        ..target(
            "bedclaude",
            "bed",
            "anthropic.claude-test-v1:0",
            ProviderGateStatus {
                reason: Some(EligibilityReason::AccountParked),
                circuit: CircuitPhase::HalfOpenReady,
                last_outcome: None,
                last_outcome_elapsed: None,
                credential_park: Some(CredentialParkStatus {
                    reason: UpstreamReason::CredentialInvalid,
                    remaining: Duration::from_mins(2),
                }),
                ..closed
            },
        )
    };
    vec![
        target("sonnet", "anthropic", "claude-sonnet-4-5", closed),
        unusable,
        breaker_open,
        parked,
    ]
}

/// The `/status` aggregate, composed of each panel's populated fixture.
pub fn aggregate_json() -> Value {
    let aggregate = StatusAggregate {
        panels: AggregatePanels {
            usage: super::usage::fixture::populated_usage_panel(),
            health: super::health::fixture::populated_health_panel(),
            config: super::config::fixture::populated_config_panel(),
            doctor: super::doctor::fixture::populated_doctor_panel(),
        },
    };
    serde_json::to_value(aggregate).expect("the aggregate serializes")
}

/// What a density exception lets a key path do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Allowance {
    /// The path is null at every occurrence.
    AlwaysNull,
    /// Some occurrences of the array at the path may be empty; at least one
    /// must still be populated.
    MayBeEmpty,
}

/// One named exception to the density rule, with the reason it holds.
#[derive(Debug, Clone, Copy)]
pub struct DensityException {
    /// The normalized key path (`data.groups[].provider`).
    pub path: &'static str,
    pub allowance: Allowance,
    pub reason: &'static str,
}

/// Every normalized key path of a payload, by what its occurrences held.
#[derive(Debug, Default)]
struct Census {
    seen: BTreeSet<String>,
    non_null: BTreeSet<String>,
    null: BTreeSet<String>,
    empty_arrays: BTreeSet<String>,
    populated_arrays: BTreeSet<String>,
}

impl Census {
    fn of(value: &Value) -> Self {
        let mut census = Self::default();
        census.walk(value, "");
        census
    }

    fn walk(&mut self, value: &Value, path: &str) {
        if !path.is_empty() {
            self.seen.insert(path.to_string());
            let held = if value.is_null() {
                &mut self.null
            } else {
                &mut self.non_null
            };
            held.insert(path.to_string());
        }
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    let child_path = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    self.walk(child, &child_path);
                }
            }
            Value::Array(items) => {
                let arrays = if items.is_empty() {
                    &mut self.empty_arrays
                } else {
                    &mut self.populated_arrays
                };
                arrays.insert(path.to_string());
                let item_path = format!("{path}[]");
                for item in items {
                    self.walk(item, &item_path);
                }
            }
            _ => {}
        }
    }
}

/// Every way `value` falls short of fully populated, one line each: an empty
/// array, or a key path null at every occurrence, that `exceptions` does not
/// name; an exception with no reason; and an exception that no longer
/// matches the payload, so the list cannot outlive the gap it excuses.
pub fn density_gaps(value: &Value, exceptions: &[DensityException]) -> Vec<String> {
    let census = Census::of(value);
    let excused = |path: &str, allowance: Allowance| {
        exceptions
            .iter()
            .any(|e| e.path == path && e.allowance == allowance)
    };
    let mut gaps = Vec::new();
    for path in census.empty_arrays.iter().filter(|p| !p.is_empty()) {
        if !excused(path, Allowance::MayBeEmpty) {
            gaps.push(format!("empty array at {path}"));
        }
    }
    for path in census.seen.difference(&census.non_null) {
        if !excused(path, Allowance::AlwaysNull) {
            gaps.push(format!("never non-null at {path}"));
        }
    }
    for exception in exceptions {
        if exception.reason.trim().is_empty() {
            gaps.push(format!("exception at {} gives no reason", exception.path));
        }
        let stale = match exception.allowance {
            Allowance::AlwaysNull => {
                !census.seen.contains(exception.path) || census.non_null.contains(exception.path)
            }
            Allowance::MayBeEmpty => !census.populated_arrays.contains(exception.path),
        };
        if stale {
            gaps.push(format!(
                "exception {:?} at {} no longer matches",
                exception.allowance, exception.path
            ));
        }
    }
    gaps
}

/// The paths of `both_states` that `value` never shows null, so a conditional
/// field the fixture claims to cover in both states is missing its null state.
pub fn missing_null_states(value: &Value, both_states: &[&str]) -> Vec<String> {
    let census = Census::of(value);
    both_states
        .iter()
        .filter(|path| !census.null.contains(**path))
        .map(|path| (*path).to_string())
        .collect()
}

#[cfg(test)]
#[path = "panel_fixtures_tests.rs"]
mod tests;
