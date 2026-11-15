//! Config-facing failure-class policy: the operator-authored overlay that
//! layers per-class retry / fallback overrides on top of the baked class
//! defaults, plus the adapters between the config-facing
//! [`ConfigFailureClass`] and the canonical [`FailureClass`].
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

/// Per-class retry / fallback overlay. Each leaf is independently
/// optional: an absent leaf defers to the baked class default, a present
/// leaf overrides only itself. Unknown keys are rejected so a typo cannot
/// silently become a no-op.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClassPolicy {
    /// Override for the same-provider retry cap of this class. `None`
    /// keeps the baked default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<u32>,
    /// Override for whether this class falls back to the next provider in
    /// the chain. `None` keeps the baked default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<bool>,
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
    /// Resolve the `(retry_cap, fallback)` pair for `class`: the baked
    /// class default, then the deprecated `retry_on_*` cap for its class
    /// family, then any per-class [`ClassPolicy`] leaf the operator set,
    /// each layer overriding the one below it.
    #[must_use]
    pub fn resolved_class(&self, class: &FailureClass) -> (u32, bool) {
        let (mut retry_cap, mut fallback) = baked_class_defaults(self, class);
        if let Some(compat_cap) = self.retry_on_compat_cap(class) {
            retry_cap = compat_cap;
        }
        if let Some(cfg_class) = ConfigFailureClass::from_failure_class(class)
            && let Some(overlay) = self.classes.get(&cfg_class)
        {
            if let Some(retry) = overlay.retry {
                retry_cap = retry;
            }
            if let Some(fb) = overlay.fallback {
                fallback = fb;
            }
        }
        (retry_cap, fallback)
    }

    /// The deprecated `retry_on_*` cap covering `class`, if the operator
    /// set it: `retry_on_429` for rate-limited, `retry_on_5xx` for
    /// server-error and overloaded, `retry_on_network` for timeout and
    /// network-error.
    const fn retry_on_compat_cap(&self, class: &FailureClass) -> Option<u32> {
        match class {
            FailureClass::RateLimited => self.retry_on_429,
            FailureClass::ServerError | FailureClass::Overloaded => self.retry_on_5xx,
            FailureClass::Timeout | FailureClass::NetworkError => self.retry_on_network,
            _ => None,
        }
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
    use super::{ClassPolicy, ConfigFailureClass};
    use crate::config::RetryPolicy;
    use routectl_core::failure_class::FailureClass;
    use std::collections::BTreeMap;

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

    // --- ClassPolicy deserialize / round-trip ---

    #[test]
    fn class_policy_rejects_unknown_field() {
        // Arrange: a valid leaf plus a stray key.
        let json = r#"{"retry": 2, "bogus": 1}"#;

        // Act
        let got: Result<ClassPolicy, _> = serde_json::from_str(json);

        // Assert
        assert!(got.is_err(), "unknown field must be rejected");
    }

    #[test]
    fn class_policy_partial_leaf_omits_absent_leaf_without_null() {
        // Arrange
        let json = r#"{"retry": 3}"#;

        // Act
        let parsed: ClassPolicy = serde_json::from_str(json).expect("parse");
        let reserialized = serde_json::to_string(&parsed).expect("serialize");

        // Assert
        assert_eq!(parsed.retry, Some(3));
        assert_eq!(parsed.fallback, None);
        assert_eq!(
            reserialized, r#"{"retry":3}"#,
            "absent leaf must not materialize as null"
        );
    }

    #[test]
    fn class_policy_default_serializes_empty() {
        // Arrange
        let policy = ClassPolicy::default();

        // Act
        let got = serde_json::to_string(&policy).expect("serialize");

        // Assert
        assert_eq!(got, "{}");
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

    // --- resolved_class: baked matrix over an empty overlay ---

    /// A policy whose deprecated `retry_on_*` caps are all set to distinct
    /// values, so a resolved cap reveals which knob fed it.
    fn distinct_caps_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 4,
            retry_on_429: Some(1),
            retry_on_5xx: Some(2),
            retry_on_network: Some(3),
            ..Default::default()
        }
    }

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

    #[test]
    fn resolved_class_empty_overlay_without_compat_keys_is_the_baked_row() {
        // Arrange: no retry_on_* key and no [retry.classes] block.
        let policy = RetryPolicy {
            max_attempts: 7,
            ..Default::default()
        };

        // Act + Assert: the five health classes resolve max_attempts, the
        // rest 0, for every core variant.
        for class in all_core_variants() {
            assert_eq!(
                policy.resolved_class(&class),
                expected_baked(&class, 7),
                "class {class:?}"
            );
        }
    }

    #[test]
    fn each_retry_on_key_caps_only_its_own_classes() {
        // Arrange: one row per deprecated key, naming the classes it caps.
        type Setter = fn(&mut RetryPolicy, u32);
        let rows: [(&str, Setter, &[FailureClass]); 3] = [
            (
                "retry_on_429",
                |p, n| p.retry_on_429 = Some(n),
                &[FailureClass::RateLimited],
            ),
            (
                "retry_on_5xx",
                |p, n| p.retry_on_5xx = Some(n),
                &[FailureClass::ServerError, FailureClass::Overloaded],
            ),
            (
                "retry_on_network",
                |p, n| p.retry_on_network = Some(n),
                &[FailureClass::Timeout, FailureClass::NetworkError],
            ),
        ];

        for (key, set, capped) in rows {
            let mut policy = RetryPolicy {
                max_attempts: 7,
                ..Default::default()
            };
            set(&mut policy, 1);

            // Act + Assert: the key's classes take its cap; every other
            // class keeps the baked row.
            for class in all_core_variants() {
                let expected = if capped.contains(&class) {
                    (1, true)
                } else {
                    expected_baked(&class, 7)
                };
                assert_eq!(
                    policy.resolved_class(&class),
                    expected,
                    "{key}: class {class:?}"
                );
            }
        }
    }

    #[test]
    fn retry_on_zero_caps_its_classes_at_zero() {
        // Arrange: an explicit 0 is a cap, not "unset".
        let policy = RetryPolicy {
            max_attempts: 7,
            retry_on_5xx: Some(0),
            ..Default::default()
        };

        // Act + Assert
        assert_eq!(policy.resolved_class(&FailureClass::ServerError), (0, true));
        assert_eq!(policy.resolved_class(&FailureClass::Overloaded), (0, true));
    }

    #[test]
    fn retry_classes_beat_retry_on_compat() {
        // Arrange: every compat key set, and a [retry.classes] retry leaf
        // on one class of each key's family.
        let mut policy = distinct_caps_policy();
        for class in [
            ConfigFailureClass::RateLimited,
            ConfigFailureClass::Overloaded,
            ConfigFailureClass::Timeout,
        ] {
            policy.classes.insert(
                class,
                ClassPolicy {
                    retry: Some(9),
                    fallback: None,
                },
            );
        }

        // Act + Assert: the class leaf wins over the compat key; a sibling
        // class with no leaf keeps the compat cap.
        assert_eq!(policy.resolved_class(&FailureClass::RateLimited), (9, true));
        assert_eq!(policy.resolved_class(&FailureClass::Overloaded), (9, true));
        assert_eq!(policy.resolved_class(&FailureClass::ServerError), (2, true));
        assert_eq!(policy.resolved_class(&FailureClass::Timeout), (9, true));
        assert_eq!(
            policy.resolved_class(&FailureClass::NetworkError),
            (3, true)
        );
    }

    #[test]
    fn fallback_only_class_block_keeps_the_retry_on_compat_cap() {
        // Arrange: retry_on_5xx set, and a server-error block that sets
        // only `fallback`; overloaded has no block.
        let toml_text = r"
[retry]
max_attempts = 7
retry_on_5xx = 4

[retry.classes.server-error]
fallback = false
";
        let cfg: crate::config::Config = toml::from_str(toml_text).expect("parse");

        // Act + Assert: the absent retry leaf falls through to the compat
        // cap, not to max_attempts; the fallback leaf applies on its own.
        assert_eq!(
            cfg.retry.resolved_class(&FailureClass::ServerError),
            (4, false)
        );
        assert_eq!(
            cfg.retry.resolved_class(&FailureClass::Overloaded),
            (4, expected_baked(&FailureClass::Overloaded, 7).1)
        );
    }

    // --- resolved_class: sparse operator overlay ---

    #[test]
    fn sparse_override_changes_only_the_overridden_retry_leaf() {
        // Arrange: override only RateLimited's retry cap.
        let mut policy = distinct_caps_policy();
        let mut classes = BTreeMap::new();
        classes.insert(
            ConfigFailureClass::RateLimited,
            ClassPolicy {
                retry: Some(9),
                fallback: None,
            },
        );
        policy.classes = classes;

        // Act + Assert: retry leaf overridden, fallback leaf still baked.
        assert_eq!(policy.resolved_class(&FailureClass::RateLimited), (9, true));
        // A class with no entry is untouched.
        assert_eq!(policy.resolved_class(&FailureClass::ServerError), (2, true));
    }

    #[test]
    fn sparse_override_changes_only_the_overridden_fallback_leaf() {
        // Arrange: override only Auth's fallback flag (baked cap is 0).
        let mut policy = distinct_caps_policy();
        let mut classes = BTreeMap::new();
        classes.insert(
            ConfigFailureClass::Auth,
            ClassPolicy {
                retry: None,
                fallback: Some(false),
            },
        );
        policy.classes = classes;

        // Act + Assert: fallback flipped, retry cap still the baked 0.
        assert_eq!(policy.resolved_class(&FailureClass::Auth), (0, false));
    }

    #[test]
    fn override_on_unknown_class_is_impossible_so_baked_wins() {
        // Unknown has no config-facing class, so no overlay entry can
        // target it: it always resolves to the baked (0, false).
        let mut policy = distinct_caps_policy();
        let mut classes = BTreeMap::new();
        classes.insert(
            ConfigFailureClass::RateLimited,
            ClassPolicy {
                retry: Some(9),
                fallback: Some(false),
            },
        );
        policy.classes = classes;

        assert_eq!(policy.resolved_class(&FailureClass::Unknown), (0, false));
    }

    // --- Config deserialize boundary ---
    //
    // `[retry].classes` parses from real TOML and an empty overlay stays
    // out of the serialized file.

    #[test]
    fn retry_classes_overlay_parses_from_toml() {
        // Arrange
        let toml_text = r"
[retry]
max_attempts = 5

[retry.classes.rate-limited]
retry = 2
fallback = false

[retry.classes.server-error]
fallback = true
";

        // Act
        let cfg: crate::config::Config = toml::from_str(toml_text).expect("parse");

        // Assert
        assert_eq!(
            cfg.retry.classes.get(&ConfigFailureClass::RateLimited),
            Some(&ClassPolicy {
                retry: Some(2),
                fallback: Some(false),
            })
        );
        assert_eq!(
            cfg.retry.classes.get(&ConfigFailureClass::ServerError),
            Some(&ClassPolicy {
                retry: None,
                fallback: Some(true),
            })
        );
        // resolved_class reflects the overlay: rate-limited's cap is
        // overridden to 2 and fallback flipped to false.
        assert_eq!(
            cfg.retry.resolved_class(&FailureClass::RateLimited),
            (2, false)
        );
    }

    #[test]
    fn empty_retry_classes_omitted_from_serialized_toml() {
        // Arrange: a default policy has an empty overlay map.
        let policy = RetryPolicy::default();

        // Act
        let rendered = toml::to_string(&policy).expect("serialize");

        // Assert: skip_serializing_if keeps the empty map out of the file.
        assert!(
            !rendered.contains("classes"),
            "empty overlay must not materialize a [classes] table, got:\n{rendered}"
        );
    }
}
