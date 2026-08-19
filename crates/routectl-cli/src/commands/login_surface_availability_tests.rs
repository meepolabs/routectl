//! Loaded via `#[cfg(test)] #[path = ...] mod tests;` in
//! `login_surface_availability.rs`.
//!
//! The property under test is that each scan reports a gap only when that
//! gap actually exists, and that the printed shape never invents an
//! upstream model id -- a plausible-looking wrong id is a 404 at the
//! operator's first request.

use routectl_router::Config;

use super::{UPSTREAM_PLACEHOLDER, availability_gap};

fn parse(text: &str) -> Config {
    toml::from_str(text).expect("fixture config parses")
}

/// A pooled anthropic seat with the routing rows the caller supplies.
fn pooled(routing: &str) -> Config {
    parse(&format!(
        "[providers.anthropic-default]\n\
         kind = \"anthropic-api\"\n\
         auth_kind = \"oauth-bearer\"\n\
         api_key_ref = \"oauth://anthropic\"\n\
         [pools.anthropic]\n\
         members = [\"anthropic-default\"]\n\
         accepts_new_logins = true\n\
         {routing}"
    ))
}

#[test]
fn a_seat_no_model_names_reports_the_model_shape_pointing_at_the_pool() {
    // Arrange: the pool exists, nothing routes to it.
    let config = pooled("");

    // Act
    let gap = availability_gap(&config, "anthropic-default", Some("anthropic"))
        .expect("no model means a gap");

    // Assert: the model points at the POOL, so a later login's seat serves
    // the same model without a second edit.
    assert!(gap.contains("[models."), "{gap}");
    assert!(gap.contains(r#"provider = "anthropic""#), "{gap}");
}

#[test]
fn the_model_shape_never_guesses_an_upstream_model_id() {
    // Arrange + Act
    let gap = availability_gap(&pooled(""), "anthropic-default", Some("anthropic")).expect("gap");

    // Assert: the upstream slot is the placeholder, and no vendor model id
    // shape leaked into it.
    assert!(
        gap.contains(UPSTREAM_PLACEHOLDER),
        "the upstream slot must stay a placeholder: {gap}"
    );
    for guess in ["claude-", "gpt-", "gemini-", "grok-"] {
        assert!(
            !gap.contains(guess),
            "guessed an upstream id `{guess}`: {gap}"
        );
    }
}

#[test]
fn an_unpooled_seat_reports_the_model_shape_pointing_at_the_entry() {
    // Arrange: no pool at all (the entry stands alone).
    let config = parse(
        "[providers.anthropic-default]\n\
         kind = \"anthropic-api\"\n\
         auth_kind = \"oauth-bearer\"\n\
         api_key_ref = \"oauth://anthropic\"\n",
    );

    // Act
    let gap = availability_gap(&config, "anthropic-default", None).expect("gap");

    // Assert
    assert!(gap.contains(r#"provider = "anthropic-default""#), "{gap}");
}

#[test]
fn a_model_naming_the_pool_with_no_alias_reports_the_alias_gap_not_the_model_gap() {
    // Arrange: routing exists one step in.
    let config = pooled(
        "[models.opus]\n\
         provider = \"anthropic\"\n\
         upstream = \"claude-opus-4-8\"\n",
    );

    // Act
    let gap = availability_gap(&config, "anthropic-default", Some("anthropic")).expect("gap");

    // Assert: the alias gap, naming the model that exists.
    assert!(gap.contains("[aliases]"), "{gap}");
    assert!(gap.contains(r#"default = "opus""#), "{gap}");
    assert!(!gap.contains("[models."), "the model step is done: {gap}");
}

#[test]
fn a_model_naming_the_entry_directly_also_counts_as_routed() {
    // A model may name the entry rather than its pool; either is servable,
    // so neither may be reported as a missing model.
    let config = pooled(
        "[models.opus]\n\
         provider = \"anthropic-default\"\n\
         upstream = \"claude-opus-4-8\"\n",
    );

    let gap = availability_gap(&config, "anthropic-default", Some("anthropic")).expect("gap");

    assert!(gap.contains("[aliases]"), "{gap}");
}

#[test]
fn a_fully_routed_seat_reports_no_gap() {
    // Arrange
    let config = pooled(
        "[models.opus]\n\
         provider = \"anthropic\"\n\
         upstream = \"claude-opus-4-8\"\n\
         [aliases]\n\
         default = \"opus\"\n",
    );

    // Act / Assert
    assert!(
        availability_gap(&config, "anthropic-default", Some("anthropic")).is_none(),
        "a servable seat has no gap to report"
    );
}

#[test]
fn an_alias_chain_reaching_the_model_counts_as_reachable() {
    // A chain alias is a list, so a scan reading only single-string
    // aliases would report a false gap.
    let config = pooled(
        "[models.opus]\n\
         provider = \"anthropic\"\n\
         upstream = \"claude-opus-4-8\"\n\
         [models.other]\n\
         provider = \"anthropic\"\n\
         upstream = \"claude-sonnet-4-5\"\n\
         [aliases]\n\
         default = [\"other\", \"opus\"]\n",
    );

    assert!(availability_gap(&config, "anthropic-default", Some("anthropic")).is_none());
}

#[test]
fn an_alias_reaching_only_an_unrelated_model_is_not_reachability() {
    // Arrange: an alias exists, but it names a model on another provider.
    let config = pooled(
        "[providers.other]\n\
         kind = \"openai-compat\"\n\
         base_url = \"http://127.0.0.1:1\"\n\
         api_key_ref = \"env://OTHER_KEY\"\n\
         [models.opus]\n\
         provider = \"anthropic\"\n\
         upstream = \"claude-opus-4-8\"\n\
         [models.gpt]\n\
         provider = \"other\"\n\
         upstream = \"gpt-4o\"\n\
         [aliases]\n\
         default = \"gpt\"\n",
    );

    // Act
    let gap = availability_gap(&config, "anthropic-default", Some("anthropic")).expect("gap");

    // Assert
    assert!(gap.contains("[aliases]"), "{gap}");
    assert!(gap.contains(r#"default = "opus""#), "{gap}");
}

#[test]
fn a_pool_name_needing_toml_quoting_is_quoted_in_the_model_shape() {
    // An operator-written pool name can carry a dot, which written bare
    // would split the key path in the pasteable shape.
    let config = parse(
        "[providers.anthropic-default]\n\
         kind = \"anthropic-api\"\n\
         auth_kind = \"oauth-bearer\"\n\
         api_key_ref = \"oauth://anthropic\"\n\
         [pools.\"a.b\"]\n\
         members = [\"anthropic-default\"]\n\
         accepts_new_logins = true\n",
    );

    let gap = availability_gap(&config, "anthropic-default", Some("a.b")).expect("gap");

    assert!(gap.contains(r#"provider = "a.b""#), "{gap}");
}
