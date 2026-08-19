//! Every `[providers.X]` table published in the operator-facing docs must
//! parse as the `ProviderEntry` variant its own `kind` names.
//!
//! `ProviderEntry` is `deny_unknown_fields` and `kind`-tagged, so a key
//! written on a variant that has none (an auth selector copied between
//! provider kinds, a renamed field left behind) is a parse error. Without
//! this test that error surfaces at an operator's next startup, after they
//! copied the block out of the docs; with it, the docs and the schema drift
//! apart at commit time instead.
//!
//! Sources: `docs/CONFIGURATION.md` and `README.md` (every fenced ```toml
//! block) and `examples/config.toml` (whole file). All three are
//! `include_str!`d, so moving or renaming any of them is a compile error
//! rather than a silently skipped check.

use std::collections::BTreeMap;

const CONFIGURATION_MD: &str = include_str!("../../../docs/CONFIGURATION.md");
const README_MD: &str = include_str!("../../../README.md");
const EXAMPLE_CONFIG: &str = include_str!("../../../examples/config.toml");

/// Fenced ```toml blocks as `(1-based opening-fence line, body)` pairs, in
/// document order and without their fences.
fn toml_blocks(markdown: &str) -> Vec<(usize, String)> {
    let mut blocks = Vec::new();
    let mut body: Option<(usize, Vec<&str>)> = None;

    for (index, line) in markdown.lines().enumerate() {
        match (&mut body, line.trim_end()) {
            (None, "```toml") => body = Some((index + 1, Vec::new())),
            (Some(_), "```") => {
                let (fence_line, lines) = body.take().expect("open block");
                blocks.push((fence_line, lines.join("\n")));
            }
            (Some((_, lines)), _) => lines.push(line),
            (None, _) => {}
        }
    }
    blocks
}

/// The `kind` string each `[providers.<name>]` table in `toml_src` declares.
///
/// SKIP RULE (deliberate, do not "fix" this into flagging fragments): the
/// docs also publish FRAGMENTS -- a lone sub-table such as
/// `[providers.anthropic-managed.header_extras]`, and the top-level shape
/// sketch whose `[providers.X]` placeholders carry no keys at all. Neither is
/// a whole provider entry and neither can parse as one, so a table is checked
/// only when it is a direct `[providers.<name>]` header that declares `kind`.
fn declared_kinds(toml_src: &str) -> BTreeMap<String, String> {
    let Ok(value) = toml_src.parse::<toml::Table>() else {
        return BTreeMap::new();
    };
    let Some(toml::Value::Table(providers)) = value.get("providers") else {
        return BTreeMap::new();
    };
    providers
        .iter()
        .filter_map(|(name, entry)| {
            let kind = entry.as_table()?.get("kind")?.as_str()?;
            Some((name.clone(), kind.to_string()))
        })
        .collect()
}

/// Assert every kind-declaring provider table in `toml_src` deserializes into
/// the variant it names. `origin` identifies the block in the failure message.
fn assert_blocks_match_schema(origin: &str, toml_src: &str) -> usize {
    let declared = declared_kinds(toml_src);
    if declared.is_empty() {
        return 0;
    }

    let cfg: routectl_router::Config = toml::from_str(toml_src)
        .unwrap_or_else(|e| panic!("{origin} must parse as a config: {e}\n---\n{toml_src}\n---"));

    for (name, kind) in &declared {
        let entry = cfg
            .providers
            .get(name)
            .unwrap_or_else(|| panic!("{origin} must define provider `{name}`"));
        assert_eq!(
            entry.kind_str(),
            kind,
            "{origin}: provider `{name}` declares kind `{kind}` but parsed as \
             `{}`",
            entry.kind_str()
        );
    }
    declared.len()
}

#[test]
fn documented_provider_blocks_parse_as_the_kind_they_declare() {
    let mut checked_blocks = 0usize;
    let mut checked_providers = 0usize;

    for (line, body) in toml_blocks(CONFIGURATION_MD) {
        let origin = format!("docs/CONFIGURATION.md fenced toml block at line {line}");
        let n = assert_blocks_match_schema(&origin, &body);
        if n > 0 {
            checked_blocks += 1;
            checked_providers += n;
        }
    }

    // A refactor that breaks the fence scanner or the provider filter would
    // otherwise leave this test vacuously green over zero blocks.
    assert!(
        checked_blocks >= 20,
        "expected the docs to carry many provider blocks, checked only \
         {checked_blocks}"
    );
    assert!(checked_providers >= checked_blocks);
}

#[test]
fn the_example_config_provider_blocks_parse_as_the_kind_they_declare() {
    let checked = assert_blocks_match_schema("examples/config.toml", EXAMPLE_CONFIG);
    assert!(
        checked >= 4,
        "expected several provider entries in examples/config.toml, found {checked}"
    );
}

/// The README quickstart is the FIRST config anyone copies, and it was the
/// last one nothing checked: it shipped a `reasoning_dialect` on a provider
/// entry (a `[models.X]` key -- `ProviderEntry` is `deny_unknown_fields`,
/// so that block never parsed) behind a promise that it was a working
/// minimal example. Held to the same bar as the reference docs here.
#[test]
fn readme_provider_blocks_parse_as_the_kind_they_declare() {
    let mut checked_blocks = 0usize;

    for (line, body) in toml_blocks(README_MD) {
        let origin = format!("README.md fenced toml block at line {line}");
        if assert_blocks_match_schema(&origin, &body) > 0 {
            checked_blocks += 1;
        }
    }

    // The README carries exactly one config block today. Asserting it is
    // non-zero keeps a fence-scanner or filter regression from leaving this
    // vacuously green over zero blocks.
    assert!(
        checked_blocks >= 1,
        "expected the README quickstart to carry a provider block, checked \
         {checked_blocks}"
    );
}
