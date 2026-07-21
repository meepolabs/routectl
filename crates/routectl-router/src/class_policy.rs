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

/// Capability token stamped on a [`FailureClass::FeatureUnsupported`]
/// synthesized from an operator config remap. A stable, provenance-only
/// constant: it marks the class as config-sourced rather than lifted from
/// a real upstream envelope, so a downstream consumer can tell the two
/// apart without inspecting anything else.
pub const OPERATOR_REMAP_CAPABILITY: &str = "operator-remap";

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
    /// `FeatureUnsupported` gains the stable [`OPERATOR_REMAP_CAPABILITY`]
    /// token marking its config-remap provenance.
    #[must_use]
    pub fn to_failure_class(self) -> FailureClass {
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
                capability: OPERATOR_REMAP_CAPABILITY.to_string(),
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
    /// class default, with any per-class [`ClassPolicy`] leaf the operator
    /// set layered over the matching leaf.
    #[must_use]
    pub fn resolved_class(&self, class: &FailureClass) -> (u32, bool) {
        let (mut retry_cap, mut fallback) = baked_class_defaults(self, class);
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
}

/// The baked `(retry_cap, fallback)` default for `class` under `policy`,
/// before any operator overlay. Reproduces the router's class-driven
/// retry-cap and fallback outcomes for an empty overlay.
fn baked_class_defaults(policy: &RetryPolicy, class: &FailureClass) -> (u32, bool) {
    match class {
        FailureClass::RateLimited => (policy.retry_on_429.unwrap_or(policy.max_attempts), true),
        FailureClass::ServerError | FailureClass::Overloaded => {
            (policy.retry_on_5xx.unwrap_or(policy.max_attempts), true)
        }
        // `Timeout` has no live producer in the classifier yet (router.rs's
        // predicate wildcard currently gives it a 0 cap); this arm
        // anticipates the classifier gaining a Timeout producer and
        // intentionally diverges from that wildcard so the two don't
        // silently drift once it does.
        FailureClass::Timeout | FailureClass::NetworkError => {
            (policy.retry_on_network.unwrap_or(policy.max_attempts), true)
        }
        FailureClass::Auth
        | FailureClass::BadRequest
        | FailureClass::ContentPolicy
        | FailureClass::ContextWindow
        | FailureClass::FeatureUnsupported { .. } => (0, true),
        FailureClass::Unknown => (0, false),
        _ => (0, false),
    }
}

/// (De)serialize a `u16`-keyed status-to-class map across serde's flatten
/// path. `#[serde(flatten)]` buffers a table into string-keyed `Content`,
/// which drops the toml crate's usual string-to-`u16` map-key coercion, so
/// a plain `BTreeMap<u16, _>` field fails to deserialize when flattened.
/// Routing the keys through an explicit `String` map restores the
/// coercion in both directions while keeping the field's public type
/// `u16`-keyed.
pub(crate) mod status_class_overrides {
    use super::ConfigFailureClass;
    use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
    use std::collections::BTreeMap;

    pub fn serialize<S>(
        map: &BTreeMap<u16, ConfigFailureClass>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let string_keyed: BTreeMap<String, ConfigFailureClass> = map
            .iter()
            .map(|(code, class)| (code.to_string(), *class))
            .collect();
        string_keyed.serialize(serializer)
    }

    pub fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<BTreeMap<u16, ConfigFailureClass>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let string_keyed = BTreeMap::<String, ConfigFailureClass>::deserialize(deserializer)?;
        let mut result = BTreeMap::new();
        for (key, class) in string_keyed {
            let code = parse_status_code::<D::Error>(&key)?;
            if result.insert(code, class).is_some() {
                return Err(D::Error::custom(format!(
                    "duplicate status code key {key:?}: another key already maps to status \
                     code {code}"
                )));
            }
        }
        Ok(result)
    }

    /// Parse a status-code map key, distinguishing a non-numeric key from
    /// one that is numeric but outside `u16` range (e.g. `"99999"`) so the
    /// error names the actual problem instead of calling an in-range
    /// integer "not an integer".
    fn parse_status_code<E: serde::de::Error>(key: &str) -> Result<u16, E> {
        key.parse::<u16>().map_err(|_| {
            if key.parse::<i64>().is_ok() {
                E::custom(format!(
                    "invalid status code key {key:?}: expected an integer status code in \
                     0..=65535"
                ))
            } else {
                E::custom(format!(
                    "invalid status code key {key:?}: expected an integer"
                ))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{ClassPolicy, ConfigFailureClass, OPERATOR_REMAP_CAPABILITY};
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
    fn to_failure_class_feature_unsupported_carries_the_constant_capability_token() {
        // Act
        let got = ConfigFailureClass::FeatureUnsupported.to_failure_class();

        // Assert
        assert_eq!(
            got,
            FailureClass::FeatureUnsupported {
                capability: OPERATOR_REMAP_CAPABILITY.to_string(),
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

    /// A policy whose per-class retry caps are all set to distinct values,
    /// so a resolved cap reveals which `retry_on_*` knob fed it.
    fn distinct_caps_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 4,
            retry_on_429: Some(1),
            retry_on_5xx: Some(2),
            retry_on_network: Some(3),
            ..Default::default()
        }
    }

    fn expected_baked(class: &FailureClass) -> (u32, bool) {
        match class {
            FailureClass::RateLimited => (1, true),
            FailureClass::ServerError | FailureClass::Overloaded => (2, true),
            FailureClass::Timeout | FailureClass::NetworkError => (3, true),
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
    fn resolved_class_empty_map_matches_baked_matrix_for_all_core_variants() {
        // NOTE: `expected_baked` is a hand-duplicated twin of
        // `baked_class_defaults`, not the router's live retry-cap
        // predicates (module-private to router.rs), so this only pins the
        // two copies here against each other. True differential coverage
        // against the router's predicates lands once those predicates
        // consume `resolved_class` directly.
        // Arrange
        let policy = distinct_caps_policy();

        // Act + Assert
        for class in all_core_variants() {
            assert_eq!(
                policy.resolved_class(&class),
                expected_baked(&class),
                "class {class:?}"
            );
        }
    }

    #[test]
    fn resolved_class_empty_map_falls_back_to_max_attempts_when_knobs_unset() {
        // Arrange: default leaves every retry_on_* knob unset, so retryable
        // classes fall back to max_attempts.
        let policy = RetryPolicy {
            max_attempts: 7,
            ..Default::default()
        };

        // Act + Assert
        for class in [
            FailureClass::RateLimited,
            FailureClass::ServerError,
            FailureClass::Overloaded,
            FailureClass::Timeout,
            FailureClass::NetworkError,
        ] {
            assert_eq!(
                policy.resolved_class(&class),
                (7, true),
                "class {class:?} must default to max_attempts"
            );
        }
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
    // The two new fields live on serde surfaces (`[retry].classes` on the
    // normal path, and `class_overrides` FLATTENED off
    // `ProviderRuntimePolicy` onto every `[providers.X]`). These pin that
    // both parse from real TOML and survive a serialize round-trip.

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

    #[test]
    fn provider_class_overrides_parse_from_flattened_toml() {
        // Arrange: class_overrides is flattened onto the provider table
        // with numeric (u16) status keys.
        let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"

[providers.anthropic.class_overrides]
429 = "rate-limited"
503 = "server-error"
"#;

        // Act
        let cfg: crate::config::Config = toml::from_str(toml_text).expect("parse");
        let runtime = cfg.providers.get("anthropic").expect("provider").runtime();

        // Assert
        assert_eq!(
            runtime.class_overrides.get(&429),
            Some(&ConfigFailureClass::RateLimited)
        );
        assert_eq!(
            runtime.class_overrides.get(&503),
            Some(&ConfigFailureClass::ServerError)
        );
    }

    #[test]
    fn provider_class_overrides_reject_a_typo_class() {
        // Arrange: a status mapped to a non-existent class token.
        let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"

[providers.anthropic.class_overrides]
429 = "rate_limited"
"#;

        // Act
        let parsed: Result<crate::config::Config, _> = toml::from_str(toml_text);

        // Assert
        assert!(parsed.is_err(), "a typo'd class token must fail the load");
    }

    #[test]
    fn provider_class_overrides_reject_a_non_numeric_key() {
        // Arrange: a status key that is not an integer at all.
        let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"

[providers.anthropic.class_overrides]
"4a29" = "rate-limited"
"#;

        // Act
        let parsed: Result<crate::config::Config, _> = toml::from_str(toml_text);

        // Assert
        assert!(parsed.is_err(), "a non-numeric status key must be rejected");
    }

    #[test]
    fn provider_class_overrides_reject_an_out_of_range_key_with_range_message() {
        // Arrange: "99999" is a valid integer but outside u16 range, so the
        // error must name the range rather than call it "not an integer".
        let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"

[providers.anthropic.class_overrides]
"99999" = "rate-limited"
"#;

        // Act
        let err = toml::from_str::<crate::config::Config>(toml_text)
            .expect_err("an out-of-range status key must be rejected");

        // Assert
        assert!(
            err.to_string().contains("0..=65535"),
            "error must name the valid range, got: {err}"
        );
    }

    #[test]
    fn provider_class_overrides_reject_duplicate_keys_mapping_to_the_same_status_code() {
        // Arrange: "029" and "29" are distinct string keys that both parse
        // to status code 29, so a naive collect would silently pick one via
        // last-write-wins.
        let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"

[providers.anthropic.class_overrides]
"029" = "rate-limited"
"29" = "server-error"
"#;

        // Act
        let parsed: Result<crate::config::Config, _> = toml::from_str(toml_text);

        // Assert
        assert!(
            parsed.is_err(),
            "two keys collapsing to the same status code must be rejected"
        );
    }

    #[test]
    fn provider_class_overrides_survive_a_serialize_round_trip() {
        // Arrange: parse a config carrying flattened u16 overrides.
        let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"

[providers.anthropic.class_overrides]
529 = "overloaded"
"#;
        let cfg: crate::config::Config = toml::from_str(toml_text).expect("parse");

        // Act: serialize back out (exercises the flatten-aware serializer)
        // and re-parse.
        let rendered = toml::to_string(&cfg).expect("serialize");
        let reparsed: crate::config::Config = toml::from_str(&rendered).expect("reparse");

        // Assert: the u16 key survives both directions.
        let runtime = reparsed
            .providers
            .get("anthropic")
            .expect("provider")
            .runtime();
        assert_eq!(
            runtime.class_overrides.get(&529),
            Some(&ConfigFailureClass::Overloaded)
        );
    }
}
