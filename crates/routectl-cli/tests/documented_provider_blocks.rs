//! Every config table published in the operator-facing docs must parse as
//! the schema type it names, and every published WHOLE config must also
//! survive the semantic validators an operator's `config check` runs.
//!
//! `Config` and `ProviderEntry` are `deny_unknown_fields` (and
//! `ProviderEntry` is `kind`-tagged), so a key written on a table that has
//! none -- a `[models.X]` knob copied onto a provider entry, an auth
//! selector copied between provider kinds, a renamed field left behind --
//! is a parse error. Parsing is not enough on its own though: a config can
//! deserialize perfectly and still be unloadable (an alias whose key equals
//! the nickname it points at is a one-hop cycle the loader refuses), so a
//! block carrying a `version` key -- i.e. one an operator can save as their
//! whole `config.toml` -- additionally runs the version preflight and the
//! `config check` validator suite.
//!
//! Without this test both classes surface at an operator's next startup,
//! after they copied the block out of the docs; with it, the docs and the
//! schema drift apart at commit time instead.
//!
//! Sources: `docs/CONFIGURATION.md` and `README.md` (every fenced ```toml
//! block) and `examples/config.toml` (whole file). All three are
//! `include_str!`d, so moving or renaming any of them is a compile error
//! rather than a silently skipped check.

use std::collections::BTreeMap;

use routectl_cli::commands::config::validation_report;
use routectl_router::{Config, preflight_config_version};

const CONFIGURATION_MD: &str = include_str!("../../../docs/CONFIGURATION.md");
const README_MD: &str = include_str!("../../../README.md");
const EXAMPLE_CONFIG: &str = include_str!("../../../examples/config.toml");

/// Top-level `Config` tables that are MAPS of typed entries, paired with
/// the keys an entry must carry to be a whole entry rather than a
/// documentation fragment. An empty list means any non-empty table counts.
const ENTRY_SECTIONS: &[(&str, &[&str])] = &[
    ("providers", &["kind"]),
    ("pools", &["members"]),
    ("models", &["provider", "upstream"]),
    ("registry", &[]),
    ("cache_pricing", &[]),
];

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

/// What one fenced block is worth checking for.
struct BlockShape {
    /// Schema-typed things the block populates: one per complete entry in
    /// an [`ENTRY_SECTIONS`] map plus one per populated singleton section
    /// (`[server]`, `[retry]`, `[bedrock]`, `[log]`, ...).
    units: usize,
    /// The block sketches a shape rather than configuring anything: it
    /// carries a placeholder that cannot deserialize (see the skip rule on
    /// [`classify`]).
    fragment: bool,
    /// The block carries a `version` key, so an operator can save it as a
    /// whole `config.toml` -- it is held to the loader's semantic bar too.
    whole_config: bool,
}

/// The `kind` string each complete `[providers.<name>]` table declares.
fn declared_kinds(table: &toml::Table) -> BTreeMap<String, String> {
    let Some(toml::Value::Table(providers)) = table.get("providers") else {
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

/// Classify one block: how much of the schema it populates, and whether it
/// is a fragment to skip.
///
/// SKIP RULE (deliberate, do not "fix" this into flagging fragments): the
/// docs also publish FRAGMENTS, and a fragment cannot deserialize as a
/// `Config` because the placeholder tables it contains are missing their
/// required fields. Two shapes occur -- a lone sub-table such as
/// `[providers.anthropic-default.header_extras]` (a provider entry with no
/// `kind`), and the top-level shape sketch whose `[providers.X]`,
/// `[models.X]`, `[server.auth]` and `[registry."<glob>"]` placeholders
/// carry no keys at all. Both are detected the same way: an entry in an
/// [`ENTRY_SECTIONS`] map that lacks its required keys, or a singleton
/// section that is an empty table, marks the whole block a fragment. The
/// block is checked only when every table it opens is populated.
fn classify(table: &toml::Table) -> BlockShape {
    let mut units = 0usize;
    let mut fragment = false;

    for (key, value) in table {
        if key == "version" {
            continue;
        }
        match ENTRY_SECTIONS.iter().find(|(name, _)| name == key) {
            Some((_, required)) => {
                let Some(entries) = value.as_table() else {
                    fragment = true;
                    continue;
                };
                for entry in entries.values() {
                    let complete = entry.as_table().is_some_and(|entry| {
                        if required.is_empty() {
                            !entry.is_empty()
                        } else {
                            required.iter().all(|key| entry.contains_key(*key))
                        }
                    });
                    if complete {
                        units += 1;
                    } else {
                        fragment = true;
                    }
                }
            }
            None => match value.as_table() {
                Some(section) if section.is_empty() => fragment = true,
                _ => units += 1,
            },
        }
    }

    BlockShape {
        units,
        fragment,
        whole_config: table.contains_key("version"),
    }
}

/// One block's contribution to a source file's coverage totals.
#[derive(Default)]
struct Checked {
    blocks: usize,
    units: usize,
    whole_configs: usize,
}

impl Checked {
    const fn add(&mut self, shape: &BlockShape) {
        self.blocks += 1;
        self.units += shape.units;
        if shape.whole_config {
            self.whole_configs += 1;
        }
    }
}

/// Assert `toml_src` deserializes into the schema types it names -- and,
/// when it is a whole config, that it also loads. `origin` identifies the
/// block in every failure message.
fn assert_block_matches_schema(origin: &str, toml_src: &str) -> Option<BlockShape> {
    // Every fenced ```toml block is TOML, including the ones this harness
    // then skips as fragments: a block that does not even parse as TOML is
    // a doc bug regardless of which tables it opens.
    let table: toml::Table = toml_src
        .parse()
        .unwrap_or_else(|e| panic!("{origin} must be valid TOML: {e}\n---\n{toml_src}\n---"));

    let shape = classify(&table);
    if shape.units == 0 || shape.fragment {
        return None;
    }

    let cfg: Config = toml::from_str(toml_src)
        .unwrap_or_else(|e| panic!("{origin} must parse as a config: {e}\n---\n{toml_src}\n---"));

    for (name, kind) in &declared_kinds(&table) {
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

    if shape.whole_config {
        assert_whole_config_loads(origin, toml_src, &cfg);
    }

    Some(shape)
}

/// The semantic half, for a block an operator can save verbatim as their
/// `config.toml`: the version preflight the loader runs off the raw text,
/// then the `config check` validator suite (alias cycles and patterns,
/// model-to-provider references, pool membership, float and base-URL
/// sanity, the Bedrock allowlists). Secret RESOLUTION is deliberately not
/// exercised -- every published credential is a reference to an env var or
/// a login the test machine does not have, and an unresolved ref is a
/// warning rather than an error even for an operator.
fn assert_whole_config_loads(origin: &str, toml_src: &str, cfg: &Config) {
    if let Err(e) = preflight_config_version(toml_src) {
        panic!("{origin} is a whole config that the version preflight rejects: {e}");
    }

    let report = validation_report(cfg, Some(toml_src));
    assert!(
        report.errors.is_empty(),
        "{origin} is a whole config an operator can save, but it fails validation:\n  - {}",
        report.errors.join("\n  - "),
    );
}

#[test]
fn documented_config_blocks_parse_and_whole_configs_validate() {
    let mut checked = Checked::default();

    for (line, body) in toml_blocks(CONFIGURATION_MD) {
        let origin = format!("docs/CONFIGURATION.md fenced toml block at line {line}");
        if let Some(shape) = assert_block_matches_schema(&origin, &body) {
            checked.add(&shape);
        }
    }

    // A refactor that breaks the fence scanner or the block classifier
    // would otherwise leave this test vacuously green over zero blocks.
    // Floors sit below today's counts (55 blocks, 80 units, 1 whole
    // config) so ordinary doc edits do not trip them.
    assert!(
        checked.blocks >= 50,
        "expected the docs to carry many config blocks, checked only {}",
        checked.blocks
    );
    assert!(
        checked.units >= 70,
        "expected the docs to populate many config tables, checked only {}",
        checked.units
    );
    assert!(
        checked.whole_configs >= 1,
        "expected the docs to publish at least one whole config"
    );
}

#[test]
fn the_example_config_parses_and_validates() {
    let shape = assert_block_matches_schema("examples/config.toml", EXAMPLE_CONFIG)
        .expect("examples/config.toml is a whole config, never a fragment");
    assert!(
        shape.units >= 15,
        "expected many populated tables in examples/config.toml, found {}",
        shape.units
    );
    assert!(
        shape.whole_config,
        "examples/config.toml must carry a `version` key"
    );
}

/// The README quickstart is the FIRST config anyone copies, and it was the
/// last one nothing checked: it shipped a `reasoning_dialect` on a provider
/// entry (a `[models.X]` key -- `ProviderEntry` is `deny_unknown_fields`,
/// so that block never parsed) behind a promise that it was a working
/// minimal example, and later an alias self-cycle that parsed fine and
/// still would not load. Held to the same bar as the reference docs here.
#[test]
fn readme_config_blocks_parse_and_whole_configs_validate() {
    let mut checked = Checked::default();

    for (line, body) in toml_blocks(README_MD) {
        let origin = format!("README.md fenced toml block at line {line}");
        if let Some(shape) = assert_block_matches_schema(&origin, &body) {
            checked.add(&shape);
        }
    }

    // The README carries exactly one config block today, and it is a whole
    // config. Asserting both are non-zero keeps a fence-scanner or
    // classifier regression from leaving this vacuously green.
    assert!(
        checked.blocks >= 1,
        "expected the README quickstart to carry a config block, checked {}",
        checked.blocks
    );
    assert!(
        checked.whole_configs >= 1,
        "expected the README quickstart to publish a whole config"
    );
}
