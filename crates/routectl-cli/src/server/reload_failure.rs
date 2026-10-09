//! The closed vocabulary a rejected config / overlay reload is recorded under.
//!
//! A recorded failure reaches the status surface, so it carries only a fixed
//! token: never the loader's error text, which can inline a config value or a
//! local path.

/// Why a config / overlay reload was rejected, as a path-free closed token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadFailure {
    /// config.toml could not be read, parsed, or validated.
    ConfigLoadFailed,
    /// The catalog overlay could not be read or failed its load checks.
    OverlayLoadFailed,
    /// The blocking loader task panicked.
    LoaderPanicked,
    /// The candidate router could not be built from the loaded config.
    RouterBuildFailed,
    /// The capability replay boundary was refused before it was queued.
    BoundaryNotAdmitted,
    /// The capability replay boundary was queued but did not commit.
    BoundaryNotDurable,
}

impl ReloadFailure {
    /// The wire / render token for this class.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConfigLoadFailed => "config_load_failed",
            Self::OverlayLoadFailed => "overlay_load_failed",
            Self::LoaderPanicked => "loader_panicked",
            Self::RouterBuildFailed => "router_build_failed",
            Self::BoundaryNotAdmitted => "boundary_not_admitted",
            Self::BoundaryNotDurable => "boundary_not_durable",
        }
    }
}

/// The last rejected reload as a reader sees it: its class and how long ago it
/// was recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReloadFailureSnapshot {
    pub class: ReloadFailure,
    pub age_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::ReloadFailure;

    #[test]
    fn every_class_renders_its_own_token() {
        let rows = [
            (ReloadFailure::ConfigLoadFailed, "config_load_failed"),
            (ReloadFailure::OverlayLoadFailed, "overlay_load_failed"),
            (ReloadFailure::LoaderPanicked, "loader_panicked"),
            (ReloadFailure::RouterBuildFailed, "router_build_failed"),
            (ReloadFailure::BoundaryNotAdmitted, "boundary_not_admitted"),
            (ReloadFailure::BoundaryNotDurable, "boundary_not_durable"),
        ];

        for (class, token) in rows {
            assert_eq!(class.as_str(), token, "{class:?}");
        }
    }
}
