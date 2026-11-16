//! The one environment override of the live wire check's sampling:
//! [`SAMPLE_ONE_IN_ENV`] replaces the 1-in-[`DEFAULT_SAMPLE_ONE_IN`] rate.
//! First-N and the status window stay at their consts.
//!
//! `serve` resolves it once at boot, before the router is built, and a hot
//! reload carries the boot value forward instead of resolving again: the
//! rate is fixed for the life of the process, like the log knobs.
//!
//! [`DEFAULT_SAMPLE_ONE_IN`]: crate::wire_check_state::DEFAULT_SAMPLE_ONE_IN

use std::num::NonZeroU32;

use crate::wire_check_state::WireCheckParams;

/// Env var that sets the wire check's 1-in-M rate past first-N.
pub const SAMPLE_ONE_IN_ENV: &str = "ROUTECTL_WIRE_CHECK_SAMPLE_ONE_IN";

/// Where the resolved sample rate came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleRateSource {
    /// [`SAMPLE_ONE_IN_ENV`] set it.
    Env,
    /// The variable was unset; the rate is the code default.
    Default,
}

impl SampleRateSource {
    /// The `source` field value of the startup status line.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::Default => "default",
        }
    }
}

/// The sampling params a daemon runs on, with the layer that set the rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedWireCheckParams {
    /// What the router samples by.
    pub params: WireCheckParams,
    /// Who set [`WireCheckParams::sample_one_in`].
    pub source: SampleRateSource,
}

impl ResolvedWireCheckParams {
    /// Log the resolved rate and its source once, at INFO.
    pub fn log_status(&self) {
        tracing::info!(
            sample_one_in = self.params.sample_one_in.get(),
            source = self.source.as_str(),
            default = crate::wire_check_state::DEFAULT_SAMPLE_ONE_IN.get(),
            "ROUTECTL_WIRE_CHECK_SAMPLE_ONE_IN resolved (frozen for the rest of this process)"
        );
    }
}

/// [`SAMPLE_ONE_IN_ENV`] is set but is not a whole number from 1 to
/// `u32::MAX`. The message names the variable and never its value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "ROUTECTL_WIRE_CHECK_SAMPLE_ONE_IN must be a whole number from 1 to 4294967295; \
     unset it to check 1 in 100 successful attempts"
)]
pub struct SampleRateEnvError;

/// Resolve the wire-check params from the environment: the defaults, with
/// `sample_one_in` taken from [`SAMPLE_ONE_IN_ENV`] when it is set. The value
/// is trimmed, like the log knobs'; a set value that is empty, zero,
/// negative, not a number or past `u32::MAX` is refused rather than ignored,
/// so a typo cannot silently fall back to sampling. `env` looks up one
/// variable; `serve` passes [`routectl_core::process_env`].
///
/// # Errors
///
/// [`SampleRateEnvError`] when the variable is set to anything but a positive
/// integer that fits a `u32`.
pub fn resolve_wire_check_params(
    env: impl Fn(&str) -> Option<String>,
) -> Result<ResolvedWireCheckParams, SampleRateEnvError> {
    let Some(raw) = env(SAMPLE_ONE_IN_ENV) else {
        return Ok(ResolvedWireCheckParams {
            params: WireCheckParams::default(),
            source: SampleRateSource::Default,
        });
    };
    let sample_one_in = raw
        .trim()
        .parse::<NonZeroU32>()
        .map_err(|_| SampleRateEnvError)?;
    Ok(ResolvedWireCheckParams {
        params: WireCheckParams {
            sample_one_in,
            ..WireCheckParams::default()
        },
        source: SampleRateSource::Env,
    })
}

#[cfg(test)]
#[path = "wire_check_env_tests.rs"]
mod tests;
