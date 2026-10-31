//! The one precedence for the tiered tuning keys: breaker threshold and
//! cooldown, RPM limit, and the two per-attempt timeouts.
//!
//! Each key resolves independently through the tiers it supports, and the
//! first tier holding a value wins:
//!
//! | key                            | tiers, highest first                  |
//! |--------------------------------|---------------------------------------|
//! | `circuit_failures`             | provider > pool > default             |
//! | `circuit_cooldown_ms`          | provider > pool > default             |
//! | `rpm_limit`                    | provider > pool > default             |
//! | `request_timeout_ms`           | provider > pool > global > default    |
//! | `stream_first_byte_timeout_ms` | model > provider > pool > global > default |
//!
//! The shipped defaults live only in this module. Every resolved value
//! carries the tier that supplied it, so a diagnostic can say where a
//! number came from without re-deriving the precedence.

use std::num::NonZeroU32;
use std::time::Duration;

use super::{Config, PoolEntry, ProviderRuntimePolicy, RetryPolicy};

/// Breaker cooldown when no tier sets `circuit_cooldown_ms`.
const DEFAULT_CIRCUIT_COOLDOWN_MS: u64 = 30_000;
/// Stream first-content timeout when no tier sets
/// `stream_first_byte_timeout_ms`: a pinging-but-contentless upstream must
/// stay bounded even when the operator configures nothing.
const DEFAULT_STREAM_FIRST_BYTE_TIMEOUT_MS: u64 = 600_000;

/// The tier a resolved tuning value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuningSource {
    /// The `[models.X]` entry.
    Model,
    /// The `[providers.X]` entry the target dispatches through.
    Provider,
    /// The `[pools.X]` block that is the target, or that claims it.
    Pool,
    /// The workspace-wide `[retry]` table.
    Global,
    /// No tier set the key; the shipped default applies.
    ShippedDefault,
}

impl TuningSource {
    /// Stable render token: `model`, `provider`, `pool`, `global` or
    /// `default`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Provider => "provider",
            Self::Pool => "pool",
            Self::Global => "global",
            Self::ShippedDefault => "default",
        }
    }
}

/// One resolved tuning value and the tier that supplied it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved<T> {
    /// The value in force.
    pub value: T,
    /// The tier the value came from.
    pub source: TuningSource,
}

/// When the circuit breaker opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerThreshold {
    /// The breaker never opens on consecutive failures.
    Disabled,
    /// The breaker opens once this many consecutive attempts fail.
    After(NonZeroU32),
}

impl BreakerThreshold {
    /// The threshold for a configured `circuit_failures` value. `0` opens
    /// on the first failure, the same as `1`.
    #[must_use]
    pub const fn from_configured(failures: u32) -> Self {
        match NonZeroU32::new(failures) {
            Some(n) => Self::After(n),
            None => Self::After(NonZeroU32::MIN),
        }
    }
}

/// The tuning in force for one dispatch target, every cell resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ResolvedTuning {
    /// Consecutive failures that open the breaker.
    pub circuit_failures: Resolved<BreakerThreshold>,
    /// How long an opened breaker stays open, in milliseconds.
    pub circuit_cooldown_ms: Resolved<u64>,
    /// Requests per minute; `None` is unlimited.
    pub rpm_limit: Resolved<Option<u32>>,
    /// Per-attempt deadline for complete and count-tokens requests;
    /// `None` is no deadline.
    pub request_timeout_ms: Resolved<Option<u64>>,
    /// Per-attempt first-content deadline for streams; `None` is no
    /// deadline.
    pub stream_first_byte_timeout_ms: Resolved<Option<u64>>,
}

/// The resolved values a runtime gate (breaker plus RPM bucket) is seeded
/// from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GateTuning {
    pub(crate) threshold: BreakerThreshold,
    pub(crate) cooldown: Duration,
    pub(crate) rpm_limit: Option<u32>,
}

impl ResolvedTuning {
    /// The breaker and RPM values a runtime gate is seeded from.
    #[must_use]
    pub(crate) const fn gate(&self) -> GateTuning {
        GateTuning {
            threshold: self.circuit_failures.value,
            cooldown: Duration::from_millis(self.circuit_cooldown_ms.value),
            rpm_limit: self.rpm_limit.value,
        }
    }

    /// Resolve with a provider's runtime block as the only configured tier,
    /// for a runtime gate built outside any `Config`.
    #[must_use]
    pub(crate) fn from_provider_runtime(runtime: &ProviderRuntimePolicy) -> Self {
        resolve_tiers(&Tiers {
            model_first_byte_ms: None,
            provider: Some(runtime),
            pool: None,
            global: None,
        })
    }
}

/// Resolve the tuning for a dispatch target.
///
/// `entry` is a `[providers.X]` name or a `[pools.X]` name. The pool tier is
/// that pool, or the pool whose members include the provider entry. `None`
/// resolves with no provider or pool tier. `model_first_byte_ms` is the
/// `[models.X] stream_first_byte_timeout_ms` override; `Some(0)` is treated
/// as unset.
#[must_use]
pub fn resolve_tuning(
    config: &Config,
    entry: Option<&str>,
    model_first_byte_ms: Option<u64>,
) -> ResolvedTuning {
    resolve_tiers(&Tiers {
        model_first_byte_ms,
        provider: entry
            .and_then(|name| config.providers.get(name))
            .map(super::ProviderEntry::runtime),
        pool: entry.and_then(|name| pool_tier(config, name)),
        global: Some(&config.retry),
    })
}

/// The pool `name` names, or the pool that claims provider entry `name`.
fn pool_tier<'a>(config: &'a Config, name: &str) -> Option<&'a PoolEntry> {
    config.pools.get(name).or_else(|| {
        config
            .pools
            .values()
            .find(|pool| pool.members.iter().any(|member| member == name))
    })
}

struct Tiers<'a> {
    model_first_byte_ms: Option<u64>,
    provider: Option<&'a ProviderRuntimePolicy>,
    pool: Option<&'a PoolEntry>,
    global: Option<&'a RetryPolicy>,
}

fn resolve_tiers(tiers: &Tiers<'_>) -> ResolvedTuning {
    let provider = tiers.provider;
    let pool = tiers.pool;
    let global = tiers.global;
    let failures = first_set(&[
        (
            provider.and_then(|p| p.circuit_failures),
            TuningSource::Provider,
        ),
        (pool.and_then(|p| p.circuit_failures), TuningSource::Pool),
    ]);
    ResolvedTuning {
        circuit_failures: failures.map_or(
            Resolved {
                value: BreakerThreshold::Disabled,
                source: TuningSource::ShippedDefault,
            },
            |r| Resolved {
                value: BreakerThreshold::from_configured(r.value),
                source: r.source,
            },
        ),
        circuit_cooldown_ms: first_set(&[
            (
                provider.and_then(|p| p.circuit_cooldown_ms),
                TuningSource::Provider,
            ),
            (pool.and_then(|p| p.circuit_cooldown_ms), TuningSource::Pool),
        ])
        .unwrap_or(Resolved {
            value: DEFAULT_CIRCUIT_COOLDOWN_MS,
            source: TuningSource::ShippedDefault,
        }),
        rpm_limit: optional_cell(
            &[
                (provider.and_then(|p| p.rpm_limit), TuningSource::Provider),
                (pool.and_then(|p| p.rpm_limit), TuningSource::Pool),
            ],
            None,
        ),
        request_timeout_ms: optional_cell(
            &[
                (
                    provider.and_then(|p| p.request_timeout_ms),
                    TuningSource::Provider,
                ),
                (pool.and_then(|p| p.request_timeout_ms), TuningSource::Pool),
                (
                    global.and_then(|g| g.request_timeout_ms),
                    TuningSource::Global,
                ),
            ],
            None,
        ),
        stream_first_byte_timeout_ms: optional_cell(
            &[
                (
                    tiers.model_first_byte_ms.filter(|&ms| ms > 0),
                    TuningSource::Model,
                ),
                (
                    provider.and_then(|p| p.stream_first_byte_timeout_ms),
                    TuningSource::Provider,
                ),
                (
                    pool.and_then(|p| p.stream_first_byte_timeout_ms),
                    TuningSource::Pool,
                ),
                (
                    global.and_then(|g| g.stream_first_byte_timeout_ms),
                    TuningSource::Global,
                ),
            ],
            Some(DEFAULT_STREAM_FIRST_BYTE_TIMEOUT_MS),
        ),
    }
}

/// The first tier, in order, that holds a value.
fn first_set<T: Copy>(tiers: &[(Option<T>, TuningSource)]) -> Option<Resolved<T>> {
    tiers
        .iter()
        .find_map(|&(value, source)| value.map(|value| Resolved { value, source }))
}

/// [`first_set`] for a cell whose value is itself optional, falling back to
/// the shipped `default`.
fn optional_cell<T: Copy>(
    tiers: &[(Option<T>, TuningSource)],
    default: Option<T>,
) -> Resolved<Option<T>> {
    first_set(tiers).map_or(
        Resolved {
            value: default,
            source: TuningSource::ShippedDefault,
        },
        |r| Resolved {
            value: Some(r.value),
            source: r.source,
        },
    )
}

#[cfg(test)]
#[path = "tuning_tests.rs"]
mod tests;
