//! Config-facing failure-class policy: the operator's per-class same-target
//! retry counts layered over the baked class defaults, plus the adapters
//! between the config-facing [`ConfigFailureClass`] and the canonical
//! [`FailureClass`].
//!
//! [`ConfigFailureClass`] is the closed, operator-nameable subset of the
//! canonical (`#[non_exhaustive]`) [`FailureClass`]: it omits `Unknown`
//! (an operator never targets the unclassified bucket) and it flattens
//! `FeatureUnsupported`'s upstream `capability` token away, since the
//! config surface names a class, not a specific upstream capability.

use routectl_core::failure_class::FailureClass;
use serde::{Deserialize, Serialize};

use crate::config::RetryPolicy;

/// The config-facing failure classes an operator can name in
/// `config.toml`. Serializes in kebab-case; a key outside this closed set
/// (a typo or an unknown class) fails deserialization rather than being
/// silently dropped.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum ConfigFailureClass {
    /// Upstream rate limit.
    RateLimited,
    /// Authentication or authorization rejection.
    Auth,
    /// Caller-side request error.
    BadRequest,
    /// Rejected by a content / safety policy.
    ContentPolicy,
    /// Prompt exceeded the model context window.
    ContextWindow,
    /// Upstream server-side failure.
    ServerError,
    /// Deadline exceeded.
    Timeout,
    /// Transport-level failure with no HTTP status.
    NetworkError,
    /// Upstream signalled temporary overload.
    Overloaded,
    /// A requested capability is not supported by the upstream.
    FeatureUnsupported,
}

impl ConfigFailureClass {
    /// Map this config-facing class to the canonical [`FailureClass`].
    /// `FeatureUnsupported` carries an empty `capability`: the config surface
    /// names a class, never an upstream capability.
    #[must_use]
    pub const fn to_failure_class(self) -> FailureClass {
        match self {
            Self::RateLimited => FailureClass::RateLimited,
            Self::Auth => FailureClass::Auth,
            Self::BadRequest => FailureClass::BadRequest,
            Self::ContentPolicy => FailureClass::ContentPolicy,
            Self::ContextWindow => FailureClass::ContextWindow,
            Self::ServerError => FailureClass::ServerError,
            Self::Timeout => FailureClass::Timeout,
            Self::NetworkError => FailureClass::NetworkError,
            Self::Overloaded => FailureClass::Overloaded,
            Self::FeatureUnsupported => FailureClass::FeatureUnsupported {
                capability: String::new(),
            },
        }
    }

    /// Map a canonical [`FailureClass`] back to its config-facing class.
    /// Returns `None` for `Unknown` and for any future
    /// (`#[non_exhaustive]`) variant this closed set does not name.
    #[must_use]
    pub const fn from_failure_class(class: &FailureClass) -> Option<Self> {
        match class {
            FailureClass::RateLimited => Some(Self::RateLimited),
            FailureClass::Auth => Some(Self::Auth),
            FailureClass::BadRequest => Some(Self::BadRequest),
            FailureClass::ContentPolicy => Some(Self::ContentPolicy),
            FailureClass::ContextWindow => Some(Self::ContextWindow),
            FailureClass::ServerError => Some(Self::ServerError),
            FailureClass::Timeout => Some(Self::Timeout),
            FailureClass::NetworkError => Some(Self::NetworkError),
            FailureClass::Overloaded => Some(Self::Overloaded),
            FailureClass::FeatureUnsupported { .. } => Some(Self::FeatureUnsupported),
            FailureClass::Unknown => None,
            _ => None,
        }
    }
}

impl RetryPolicy {
    /// Resolve the `(retry_cap, fallback)` pair for `class`: the built-in
    /// class default, with the retry cap replaced by the operator's
    /// `[retry.classes]` count for that class when one is set. Fallback is
    /// always the built-in value.
    #[must_use]
    pub fn resolved_class(&self, class: &FailureClass) -> (u32, bool) {
        let (built_in_cap, fallback) = baked_class_defaults(self, class);
        let retry_cap = ConfigFailureClass::from_failure_class(class)
            .and_then(|cfg_class| self.classes.get(&cfg_class).copied())
            .unwrap_or(built_in_cap);
        (retry_cap, fallback)
    }
}

/// The baked `(retry_cap, fallback)` default for `class` under `policy`,
/// before any operator overlay: `max_attempts` for the five health
/// classes, 0 for the rest.
const fn baked_class_defaults(policy: &RetryPolicy, class: &FailureClass) -> (u32, bool) {
    match class {
        FailureClass::RateLimited | FailureClass::ServerError | FailureClass::Overloaded => {
            (policy.max_attempts, true)
        }
        // `Timeout` has no live producer in the classifier yet (router.rs's
        // predicate wildcard currently gives it a 0 cap); this arm
        // anticipates the classifier gaining a Timeout producer and
        // intentionally diverges from that wildcard so the two don't
        // silently drift once it does.
        FailureClass::Timeout | FailureClass::NetworkError => (policy.max_attempts, true),
        FailureClass::Auth
        | FailureClass::BadRequest
        | FailureClass::ContentPolicy
        | FailureClass::ContextWindow
        | FailureClass::FeatureUnsupported { .. } => (0, true),
        FailureClass::Unknown => (0, false),
        _ => (0, false),
    }
}

#[cfg(test)]
mod tests {
    use super::ConfigFailureClass;
    use crate::config::RetryPolicy;
    use routectl_core::failure_class::FailureClass;

    /// Every config-facing class paired with the kebab-case token it must
    /// serialize to. Exhaustive: the ten operator-nameable classes.
    fn all_kebab_pairs() -> [(ConfigFailureClass, &'static str); 10] {
        [
            (ConfigFailureClass::RateLimited, "rate-limited"),
            (ConfigFailureClass::Auth, "auth"),
            (ConfigFailureClass::BadRequest, "bad-request"),
            (ConfigFailureClass::ContentPolicy, "content-policy"),
            (ConfigFailureClass::ContextWindow, "context-window"),
            (ConfigFailureClass::ServerError, "server-error"),
            (ConfigFailureClass::Timeout, "timeout"),
            (ConfigFailureClass::NetworkError, "network-error"),
            (ConfigFailureClass::Overloaded, "overloaded"),
            (
                ConfigFailureClass::FeatureUnsupported,
                "feature-unsupported",
            ),
        ]
    }

    /// Every current canonical variant. Constructed explicitly so a new
    /// `#[non_exhaustive]` variant forces a compile-time revisit here.
    fn all_core_variants() -> Vec<FailureClass> {
        vec![
            FailureClass::RateLimited,
            FailureClass::Auth,
            FailureClass::BadRequest,
            FailureClass::ContentPolicy,
            FailureClass::ContextWindow,
            FailureClass::ServerError,
            FailureClass::Timeout,
            FailureClass::NetworkError,
            FailureClass::Overloaded,
            FailureClass::FeatureUnsupported {
                capability: "some_upstream_token".to_string(),
            },
            FailureClass::Unknown,
        ]
    }

    // --- ConfigFailureClass deserialize / serialize ---

    #[test]
    fn deserialize_accepts_all_ten_kebab_keys() {
        for (variant, key) in all_kebab_pairs() {
            // Arrange
            let json = format!("\"{key}\"");

            // Act
            let got: ConfigFailureClass = serde_json::from_str(&json).expect("valid key");

            // Assert
            assert_eq!(got, variant, "key {key}");
        }
    }

    #[test]
    fn serialize_produces_kebab_keys() {
        for (variant, key) in all_kebab_pairs() {
            // Act
            let got = serde_json::to_string(&variant).expect("serialize");

            // Assert
            assert_eq!(got, format!("\"{key}\""), "variant {variant:?}");
        }
    }

    #[test]
    fn deserialize_rejects_typo_key() {
        for typo in ["rate_limited", "ratelimited", "bad_request", "unknown", ""] {
            // Arrange
            let json = format!("\"{typo}\"");

            // Act
            let got: Result<ConfigFailureClass, _> = serde_json::from_str(&json);

            // Assert
            assert!(got.is_err(), "typo {typo} must be rejected");
        }
    }

    // --- Adapters ---

    #[test]
    fn to_failure_class_maps_every_variant_exhaustively() {
        // Arrange + Act + Assert
        assert_eq!(
            ConfigFailureClass::RateLimited.to_failure_class(),
            FailureClass::RateLimited
        );
        assert_eq!(
            ConfigFailureClass::Auth.to_failure_class(),
            FailureClass::Auth
        );
        assert_eq!(
            ConfigFailureClass::BadRequest.to_failure_class(),
            FailureClass::BadRequest
        );
        assert_eq!(
            ConfigFailureClass::ContentPolicy.to_failure_class(),
            FailureClass::ContentPolicy
        );
        assert_eq!(
            ConfigFailureClass::ContextWindow.to_failure_class(),
            FailureClass::ContextWindow
        );
        assert_eq!(
            ConfigFailureClass::ServerError.to_failure_class(),
            FailureClass::ServerError
        );
        assert_eq!(
            ConfigFailureClass::Timeout.to_failure_class(),
            FailureClass::Timeout
        );
        assert_eq!(
            ConfigFailureClass::NetworkError.to_failure_class(),
            FailureClass::NetworkError
        );
        assert_eq!(
            ConfigFailureClass::Overloaded.to_failure_class(),
            FailureClass::Overloaded
        );
    }

    #[test]
    fn to_failure_class_feature_unsupported_carries_no_capability_token() {
        // Act
        let got = ConfigFailureClass::FeatureUnsupported.to_failure_class();

        // Assert
        assert_eq!(
            got,
            FailureClass::FeatureUnsupported {
                capability: String::new(),
            }
        );
    }

    #[test]
    fn from_failure_class_maps_named_variants_and_drops_capability() {
        assert_eq!(
            ConfigFailureClass::from_failure_class(&FailureClass::RateLimited),
            Some(ConfigFailureClass::RateLimited)
        );
        assert_eq!(
            ConfigFailureClass::from_failure_class(&FailureClass::Auth),
            Some(ConfigFailureClass::Auth)
        );
        assert_eq!(
            ConfigFailureClass::from_failure_class(&FailureClass::BadRequest),
            Some(ConfigFailureClass::BadRequest)
        );
        assert_eq!(
            ConfigFailureClass::from_failure_class(&FailureClass::ContentPolicy),
            Some(ConfigFailureClass::ContentPolicy)
        );
        assert_eq!(
            ConfigFailureClass::from_failure_class(&FailureClass::ContextWindow),
            Some(ConfigFailureClass::ContextWindow)
        );
        assert_eq!(
            ConfigFailureClass::from_failure_class(&FailureClass::ServerError),
            Some(ConfigFailureClass::ServerError)
        );
        assert_eq!(
            ConfigFailureClass::from_failure_class(&FailureClass::Timeout),
            Some(ConfigFailureClass::Timeout)
        );
        assert_eq!(
            ConfigFailureClass::from_failure_class(&FailureClass::NetworkError),
            Some(ConfigFailureClass::NetworkError)
        );
        assert_eq!(
            ConfigFailureClass::from_failure_class(&FailureClass::Overloaded),
            Some(ConfigFailureClass::Overloaded)
        );
        assert_eq!(
            ConfigFailureClass::from_failure_class(&FailureClass::FeatureUnsupported {
                capability: "unsupported_parameter".to_string(),
            }),
            Some(ConfigFailureClass::FeatureUnsupported),
            "capability token is dropped on the way back"
        );
    }

    #[test]
    fn from_failure_class_returns_none_for_unknown() {
        assert_eq!(
            ConfigFailureClass::from_failure_class(&FailureClass::Unknown),
            None
        );
    }

    #[test]
    fn adapters_round_trip_for_every_config_variant() {
        // Every config-facing variant maps to a core class and back to
        // itself -- FeatureUnsupported included (capability re-synthesized).
        for (variant, _key) in all_kebab_pairs() {
            let core = variant.to_failure_class();
            assert_eq!(
                ConfigFailureClass::from_failure_class(&core),
                Some(variant),
                "variant {variant:?}"
            );
        }
    }

    #[test]
    fn class_token_tripwire_agrees_with_config_serde_kebab() {
        // The canonical FailureClass::class_token must emit the EXACT string
        // ConfigFailureClass's kebab-case serde rename produces, for every
        // operator-nameable class. Compared against live serde_json output
        // (not a restated literal), so a drift on either surface fails here.
        for (variant, _key) in all_kebab_pairs() {
            let serde_token = serde_json::to_string(&variant)
                .expect("ConfigFailureClass serialization is infallible");
            let serde_token = serde_token.trim_matches('"');
            let core = variant.to_failure_class();
            assert_eq!(core.class_token(), Some(serde_token), "variant {variant:?}");
        }
    }

    // --- resolved_class ---

    fn expected_baked(class: &FailureClass, max_attempts: u32) -> (u32, bool) {
        match class {
            FailureClass::RateLimited
            | FailureClass::ServerError
            | FailureClass::Overloaded
            | FailureClass::Timeout
            | FailureClass::NetworkError => (max_attempts, true),
            FailureClass::Auth
            | FailureClass::BadRequest
            | FailureClass::ContentPolicy
            | FailureClass::ContextWindow
            | FailureClass::FeatureUnsupported { .. } => (0, true),
            FailureClass::Unknown => (0, false),
            _ => (0, false),
        }
    }

    fn parse_retry(toml_text: &str) -> RetryPolicy {
        let cfg: crate::config::Config = toml::from_str(toml_text).expect("parse");
        cfg.retry
    }

    #[test]
    fn resolved_class_without_class_counts_is_the_baked_row() {
        // Arrange
        let policy = RetryPolicy {
            max_attempts: 7,
            ..Default::default()
        };

        // Act + Assert: the five health classes resolve max_attempts, the
        // rest 0, and every class but unknown falls back.
        for class in all_core_variants() {
            assert_eq!(
                policy.resolved_class(&class),
                expected_baked(&class, 7),
                "class {class:?}"
            );
        }
    }

    #[test]
    fn a_class_count_sets_only_its_own_retry_cap_and_keeps_the_built_in_fallback() {
        // Arrange: one count per row; every other class must keep the baked row.
        for (cfg_class, _key) in all_kebab_pairs() {
            let mut policy = RetryPolicy {
                max_attempts: 7,
                ..Default::default()
            };
            policy.classes.insert(cfg_class, 2);
            let target = cfg_class.to_failure_class();

            // Act + Assert
            for class in all_core_variants() {
                let baked = expected_baked(&class, 7);
                let expected = if ConfigFailureClass::from_failure_class(&class) == Some(cfg_class)
                {
                    (2, baked.1)
                } else {
                    baked
                };
                assert_eq!(
                    policy.resolved_class(&class),
                    expected,
                    "count on {target:?}: class {class:?}"
                );
            }
        }
    }

    #[test]
    fn a_zero_count_caps_its_class_at_zero() {
        // Arrange: an explicit 0 is a cap, not "unset".
        let policy = parse_retry("[retry]\nmax_attempts = 7\n[retry.classes]\nserver-error = 0\n");

        // Act + Assert
        assert_eq!(policy.resolved_class(&FailureClass::ServerError), (0, true));
        assert_eq!(policy.resolved_class(&FailureClass::Overloaded), (7, true));
    }

    #[test]
    fn unknown_has_no_config_class_so_it_stays_terminal() {
        // Arrange: counts on every nameable class.
        let mut policy = RetryPolicy::default();
        for (cfg_class, _key) in all_kebab_pairs() {
            policy.classes.insert(cfg_class, 9);
        }

        // Act + Assert
        assert_eq!(policy.resolved_class(&FailureClass::Unknown), (0, false));
    }

    // --- Config deserialize boundary ---

    #[test]
    fn retry_class_counts_parse_from_toml() {
        // Arrange
        let toml_text =
            "[retry]\nmax_attempts = 5\n\n[retry.classes]\nserver-error = 2\nrate-limited = 1\n";

        // Act
        let policy = parse_retry(toml_text);

        // Assert
        assert_eq!(
            policy.classes.get(&ConfigFailureClass::ServerError),
            Some(&2)
        );
        assert_eq!(
            policy.classes.get(&ConfigFailureClass::RateLimited),
            Some(&1)
        );
        assert_eq!(policy.resolved_class(&FailureClass::ServerError), (2, true));
        assert_eq!(policy.resolved_class(&FailureClass::Timeout), (5, true));
    }

    #[test]
    fn a_class_sub_table_no_longer_parses() {
        // Arrange: the earlier `[retry.classes.<class>]` table shape, and a
        // control in the count shape.
        let old = "[retry.classes.server-error]\nretry = 2\n";
        let new = "[retry.classes]\nserver-error = 2\n";

        // Act
        let old_parsed: Result<crate::config::Config, _> = toml::from_str(old);
        let new_parsed: Result<crate::config::Config, _> = toml::from_str(new);

        // Assert
        assert!(old_parsed.is_err(), "the table shape must not parse");
        assert!(new_parsed.is_ok(), "control: the count shape parses");
    }

    #[test]
    fn empty_retry_classes_omitted_from_serialized_toml() {
        // Arrange: a default policy has an empty map.
        let policy = RetryPolicy::default();

        // Act
        let rendered = toml::to_string(&policy).expect("serialize");

        // Assert: skip_serializing_if keeps the empty map out of the file.
        assert!(
            !rendered.contains("classes"),
            "empty map must not materialize a [classes] table, got:\n{rendered}"
        );
    }
}
