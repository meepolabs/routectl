//! A direct build of an entry the plan refuses leaves the process-global
//! codex identity unset, so the next accepted entry installs its own
//! configured version.
//!
//! `resolved_identity` is a set-once process-global, so this lives in its
//! own test binary with a SINGLE test.

#![cfg(feature = "openai-responses")]

use std::sync::Arc;

use routectl_auth::{MemoryStore, SecretStore};
use routectl_core::identity::codex::resolved_identity;
use routectl_router::{BuildOptions, Config, build_provider_with_options};

const PROVIDERS: &str = "[providers.refused]\n\
     format = \"openai-responses\"\n\
     base_url = \"https://gateway.example/backend-api/codex\"\n\
     credential = \"oauth://codex#seat-b\"\n\
     auth = \"bearer\"\n\
     codex_version = \"9.9.9-refused\"\n\
     [providers.accepted]\n\
     format = \"openai-responses\"\n\
     base_url = \"https://api.openai.com/v1\"\n\
     credential = \"env://ROUTECTL_REFUSED_ENTRY_TEST_KEY\"\n\
     auth = \"bearer\"\n\
     codex_version = \"3.4.5-accepted\"\n";

#[tokio::test]
async fn a_refused_entry_does_not_seed_the_codex_identity() {
    // Arrange
    let config: Config = toml::from_str(PROVIDERS).expect("fixture parses");
    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);

    // Act
    let refused = build_provider_with_options(
        "refused",
        &config.providers["refused"],
        Arc::clone(&store),
        BuildOptions::default(),
    )
    .await;
    let _accepted = build_provider_with_options(
        "accepted",
        &config.providers["accepted"],
        store,
        BuildOptions::default(),
    )
    .await;

    // Assert
    let err = refused.err().expect("the plan refuses the foreign host");
    assert!(err.to_string().contains("managed `oauth://codex`"), "{err}");
    assert_eq!(resolved_identity().version(), "3.4.5-accepted");
}
