use super::*;
use crate::config::{CURRENT_CONFIG_VERSION, RawConfigPreflightError, preflight_raw_config};
use crate::config_path::{PathError, validate_config_path};

/// A value no key path or replacement text contains; it must never reach a
/// message.
const SENTINEL: &str = "SENTINEL-VALUE-7f3a";

/// The entry name standing in for `*` in every generated fixture.
const ENTRY: &str = "entry";

/// How the operator docs write `*` in a retired key path.
const DOC_ENTRY: &str = "<name>";

/// One retired key with its concrete path: `*` is replaced by [`ENTRY`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Case {
    path: Vec<String>,
    refused_value: Option<String>,
    replacement: String,
}

impl Case {
    fn from_row(row: &RetiredKey) -> Self {
        Self {
            path: row
                .path
                .iter()
                .map(|segment| concrete_segment(segment))
                .collect(),
            refused_value: row.refused_value.map(str::to_string),
            replacement: row.replacement.to_string(),
        }
    }

    fn dotted(&self) -> String {
        self.path.join(".")
    }

    fn expected_line(&self) -> String {
        format!(
            "`{}` is retired: {}; delete it",
            self.dotted(),
            self.replacement
        )
    }
}

fn concrete_segment(segment: &str) -> String {
    if segment == ANY_ENTRY || segment == DOC_ENTRY {
        ENTRY.to_string()
    } else {
        segment.to_string()
    }
}

/// The rows of the CONFIGURATION.md "Retired config keys" table, the
/// operator-facing record of what is retired. Fixtures built from it, not
/// from [`RETIRED_KEYS`], so a row dropped from the code fails its case.
fn documented_cases() -> Vec<Case> {
    let doc = include_str!("../../../../docs/CONFIGURATION.md");
    let section = doc
        .split("\n## Retired config keys\n")
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .expect("CONFIGURATION.md has a Retired config keys section");
    section
        .lines()
        .filter_map(|line| line.strip_prefix("| `"))
        .map(|line| {
            let (key, rest) = line.split_once("` | ").expect("a key cell");
            let replacement = rest.strip_suffix(" |").expect("a replacement cell");
            let (path, refused_value) = match key.split_once(" = ") {
                Some((path, value)) => (path, Some(value.trim_matches('"').to_string())),
                None => (key, None),
            };
            Case {
                path: path.split('.').map(concrete_segment).collect(),
                refused_value,
                replacement: replacement.to_string(),
            }
        })
        .collect()
}

/// Plant `case`'s key into `root`: a value-retired key carries its refused
/// literal with the sentinel on a sibling key, any other key carries the
/// sentinel itself.
fn plant(root: &mut toml::Table, case: &Case) {
    let (last, parents) = case.path.split_last().expect("a path is never empty");
    let mut table = root;
    for segment in parents {
        table = table
            .entry(segment.clone())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .expect("parent segments are tables");
    }
    match &case.refused_value {
        Some(refused) => {
            table.insert(last.clone(), toml::Value::String(refused.clone()));
            table.insert(
                "sentinel_carrier".to_string(),
                toml::Value::String(SENTINEL.to_string()),
            );
        }
        None => {
            table.insert(last.clone(), toml::Value::String(SENTINEL.to_string()));
        }
    }
}

fn at_current_version(root: &toml::Table) -> String {
    let body = toml::to_string(root).expect("fixture serializes");
    format!("version = {CURRENT_CONFIG_VERSION}\n{body}")
}

fn fixture_for(case: &Case) -> String {
    let mut root = toml::Table::new();
    plant(&mut root, case);
    at_current_version(&root)
}

fn refusal_message(raw: &str) -> String {
    match preflight_raw_config(raw) {
        Err(RawConfigPreflightError::RetiredKeys(err)) => err.to_string(),
        other => panic!("expected a retired-key refusal, got {other:?} for:\n{raw}"),
    }
}

/// Each documented key, alone in a file, is refused by the loader's driver
/// with one line naming its concrete path and the documented replacement,
/// and never its value.
#[test]
fn every_documented_key_is_refused_naming_its_path_and_replacement_without_its_value() {
    let cases = documented_cases();
    assert!(!cases.is_empty(), "control: the doc table parses");

    for case in cases {
        // Arrange
        let path = case.dotted();
        let raw = fixture_for(&case);
        assert!(raw.contains(SENTINEL), "control: `{path}` plants it");

        // Act
        let msg = refusal_message(&raw);

        // Assert
        assert!(msg.contains(&format!("`{path}`")), "`{path}`: {msg}");
        assert!(!msg.contains(SENTINEL), "`{path}` leaked its value: {msg}");
        assert_eq!(msg, case.expected_line(), "`{path}`");
    }
}

/// One file carrying every documented key yields one line per table row:
/// the pass collects every hit rather than stopping at the first, and the
/// docs list exactly the rows the code holds.
#[test]
fn a_file_carrying_every_documented_key_reports_each_once() {
    // Arrange
    let mut documented = documented_cases();
    let mut root = toml::Table::new();
    for case in &documented {
        plant(&mut root, case);
    }
    let raw = at_current_version(&root);

    // Act
    let msg = refusal_message(&raw);

    // Assert
    let lines: Vec<&str> = msg.lines().collect();
    assert_eq!(lines.len(), RETIRED_KEYS.len(), "{msg}");
    let mut from_code: Vec<Case> = RETIRED_KEYS.iter().map(Case::from_row).collect();
    from_code.sort();
    documented.sort();
    assert_eq!(
        documented, from_code,
        "the doc table lists exactly the rows"
    );
    let expected: Vec<String> = from_code.iter().map(Case::expected_line).collect();
    assert_eq!(lines, expected, "sorted by concrete path");
    assert!(!msg.contains(SENTINEL), "{msg}");
}

#[test]
fn entries_under_a_wildcard_are_reported_sorted_by_name() {
    let table = [RetiredKey {
        path: &["models", ANY_ENTRY, "gone"],
        refused_value: None,
        replacement: "r",
    }];
    let raw = "[models.zeta]\ngone = 1\n[models.alpha]\ngone = 2\n[models.mid]\nkept = 3\n";

    let msg = preflight_retired_keys(raw, &table)
        .expect_err("two entries carry the key")
        .to_string();

    assert_eq!(
        msg,
        "`models.alpha.gone` is retired: r; delete it\n\
         `models.zeta.gone` is retired: r; delete it"
    );
}

#[test]
fn an_empty_entry_name_or_a_non_table_entry_is_skipped() {
    let table = [RetiredKey {
        path: &["models", ANY_ENTRY, "gone"],
        refused_value: None,
        replacement: "r",
    }];

    for raw in [
        "[models.\"\"]\ngone = 1\n",
        "[models]\nscalar = 1\n",
        "models = 1\n",
    ] {
        assert_eq!(preflight_retired_keys(raw, &table), Ok(()), "{raw}");
    }
    assert!(
        preflight_retired_keys("[models.m]\ngone = 1\n", &table).is_err(),
        "control: a named entry is reported"
    );
}

/// A value-retired key is refused only for its exact string literal: the
/// key itself stays live, so another string or a non-string passes, and the
/// edit-path lookup never names it.
#[test]
fn a_value_retired_row_hits_only_its_exact_string() {
    let table = [RetiredKey {
        path: &["pools", ANY_ENTRY, "mode"],
        refused_value: Some("old-mode"),
        replacement: "use another mode",
    }];
    let rows = [
        ("the refused string", "mode = \"old-mode\"\n", true),
        ("another string", "mode = \"new-mode\"\n", false),
        ("a prefix of the refused string", "mode = \"old\"\n", false),
        ("an integer", "mode = 1\n", false),
        ("a table", "mode = { old-mode = true }\n", false),
        ("an array of the string", "mode = [\"old-mode\"]\n", false),
    ];

    for (row, body, refused) in rows {
        let raw = format!("[pools.p]\n{body}");

        let result = preflight_retired_keys(&raw, &table);

        if refused {
            assert_eq!(
                result.expect_err(row).to_string(),
                "`pools.p.mode` is retired: use another mode; delete it",
                "{row}"
            );
        } else {
            assert_eq!(result, Ok(()), "{row}");
        }
    }
    assert_eq!(retired_key_message_in(&table, "pools.p.mode"), None);
}

/// Every pair of rows that can name the same concrete key: one path matches
/// the other's leading segments (equal length or a prefix), counting `*` as
/// matching any segment.
fn overlapping_rows(table: &[RetiredKey]) -> Vec<(String, String)> {
    let mut overlaps = Vec::new();
    for (i, a) in table.iter().enumerate() {
        for b in &table[i + 1..] {
            let compatible = a
                .path
                .iter()
                .zip(b.path)
                .all(|(x, y)| x == y || *x == ANY_ENTRY || *y == ANY_ENTRY);
            if compatible {
                overlaps.push((a.path.join("."), b.path.join(".")));
            }
        }
    }
    overlaps
}

/// No two rows can name the same key: the second would report it again,
/// and `retired_key_message` would depend on row order.
#[test]
fn no_two_rows_can_name_the_same_key() {
    assert_eq!(overlapping_rows(RETIRED_KEYS), Vec::new());
}

#[test]
fn overlapping_rows_flags_every_way_two_paths_can_name_one_key() {
    let row = |path: &'static [&'static str]| RetiredKey {
        path,
        refused_value: None,
        replacement: "r",
    };
    let cases: [(&str, [RetiredKey; 2], bool); 7] = [
        (
            "identical paths",
            [
                row(&["providers", "fast", "x"]),
                row(&["providers", "fast", "x"]),
            ],
            true,
        ),
        (
            "equal length, wildcard against literal",
            [
                row(&["providers", ANY_ENTRY, "allowed_betas"]),
                row(&["providers", "fast", "allowed_betas"]),
            ],
            true,
        ),
        (
            "equal length, literal against wildcard",
            [
                row(&["providers", "fast", "allowed_betas"]),
                row(&["providers", ANY_ENTRY, "allowed_betas"]),
            ],
            true,
        ),
        (
            "literal prefix",
            [row(&["bedrock"]), row(&["bedrock", "allowed_betas"])],
            true,
        ),
        (
            "prefix through a wildcard",
            [
                row(&["providers", ANY_ENTRY]),
                row(&["providers", "fast", "x"]),
            ],
            true,
        ),
        (
            "longer row first",
            [
                row(&["providers", "fast", "x"]),
                row(&["providers", ANY_ENTRY]),
            ],
            true,
        ),
        (
            "same shape, different leaf",
            [
                row(&["providers", ANY_ENTRY, "allowed_betas"]),
                row(&["providers", ANY_ENTRY, "unsupported_features"]),
            ],
            false,
        ),
    ];

    for (name, table, overlaps) in cases {
        let found = overlapping_rows(&table);

        assert_eq!(found.len(), usize::from(overlaps), "{name}: {found:?}");
    }
}

/// The table stays in step with the schema: every key-retired path is no
/// longer a config path, and a value-retired path still is one.
#[test]
fn every_retired_path_is_absent_from_the_config_schema() {
    for row in RETIRED_KEYS {
        let path = Case::from_row(row).dotted();

        let verdict = validate_config_path(&path);

        if row.refused_value.is_some() {
            assert!(
                verdict.is_ok(),
                "`{path}` must stay a live key: {verdict:?}"
            );
        } else {
            assert!(
                matches!(verdict, Err(PathError::UnknownSegment { .. })),
                "`{path}` must be unknown to the schema: {verdict:?}"
            );
        }
    }
}

/// The edit-path lookup words each row exactly as the file pass words the
/// same key, for the key itself and for any path beneath it.
#[test]
fn the_path_lookup_matches_the_file_pass_for_each_row() {
    for row in RETIRED_KEYS
        .iter()
        .filter(|row| row.refused_value.is_none())
    {
        let case = Case::from_row(row);
        let path = case.dotted();
        let line = refusal_message(&fixture_for(&case));

        assert_eq!(retired_key_message(&path), Some(line.clone()), "`{path}`");
        assert_eq!(
            retired_key_message(&format!("{path}.beneath")),
            Some(line),
            "`{path}.beneath`"
        );
    }
}

#[test]
fn the_path_lookup_ignores_every_other_path() {
    let table = [
        RetiredKey {
            path: &["gone"],
            refused_value: None,
            replacement: "r",
        },
        RetiredKey {
            path: &["models", ANY_ENTRY, "knob"],
            refused_value: None,
            replacement: "r",
        },
    ];
    let rows = [
        ("a live top-level key", "server.port"),
        ("a live entry key", "models.m.upstream"),
        ("the row's name outside its section", "providers.p.knob"),
        ("the row's name as the entry", "models.knob"),
        ("the row's name one level deeper", "models.m.pricing.knob"),
        ("an empty entry name", "models..knob"),
        ("a longer literal sharing a prefix", "gone_too"),
    ];

    for (row, path) in rows {
        assert_eq!(retired_key_message_in(&table, path), None, "row `{row}`");
    }
    assert_eq!(
        retired_key_message_in(&table, "gone.x.y"),
        Some("`gone` is retired: r; delete it".to_string()),
        "control: a path under a whole-table row"
    );
}

fn assert_accepted(row: &str, raw: &str) {
    assert_eq!(
        preflight_retired_keys(raw, RETIRED_KEYS),
        Ok(()),
        "row `{row}`"
    );
}

/// Live keys beside and inside the retired ones are never refused.
#[test]
fn live_keys_and_the_shipped_examples_produce_no_hits() {
    let rows = [
        (
            "global auto-emit switch",
            "[cache]\nauto_emit_top_level_breakpoint = true\n",
        ),
        ("capability enabled", "[capability]\nenabled = true\n"),
        (
            "usage db path and retention",
            "[usage]\ndb_path = \"/tmp/u.db\"\nretention_days = 30\n",
        ),
        (
            "fill-first pool",
            "[pools.p]\nmembers = [\"a\", \"b\"]\nseat_selection = \"fill-first\"\n",
        ),
        (
            "sticky-least-loaded pool",
            "[pools.p]\nmembers = [\"a\", \"b\"]\nseat_selection = \"sticky-least-loaded\"\n",
        ),
        ("lean example", crate::config::LEAN_EXAMPLE_CONFIG),
        (
            "shipped example",
            include_str!("../../../../examples/config.toml"),
        ),
    ];

    for (row, raw) in rows {
        assert_accepted(row, raw);
    }
}

/// A typo passes the table and still gets the typed unknown-field error; an
/// old version is reported ahead of a retired key; unparseable TOML passes
/// the table so the typed parse reports the syntax error.
#[test]
fn the_table_leaves_other_refusals_to_their_owners() {
    let current = CURRENT_CONFIG_VERSION;
    let typo = format!("version = {current}\n[server]\nstrct_translation = true\n");
    assert_eq!(preflight_raw_config(&typo), Ok(()));
    let typed = crate::parse_config(&typo).expect_err("a typo never parses");
    assert!(
        typed.contains("unknown field `strct_translation`"),
        "{typed}"
    );

    let old = format!("version = {}\n[bedrock]\n", current - 1);
    assert!(
        matches!(
            preflight_raw_config(&old),
            Err(RawConfigPreflightError::Version(_))
        ),
        "{:?}",
        preflight_raw_config(&old)
    );

    let broken = "version = 5\n[bedrock\nallowed_betas = = 1\n";
    assert_eq!(preflight_raw_config(broken), Ok(()));
    let typed = crate::parse_config(broken).expect_err("broken TOML never parses");
    assert!(typed.contains("TOML parse error"), "{typed}");
}

/// A quoted entry name is rendered without control bytes and capped, on the
/// file pass and on the edit-path lookup alike.
#[test]
fn a_hostile_entry_name_is_sanitized_and_capped() {
    let table = [RetiredKey {
        path: &["models", ANY_ENTRY, "knob"],
        refused_value: None,
        replacement: "r",
    }];
    let long = "n".repeat(400);
    let rows = [
        (
            "control bytes",
            "[models.\"m\\u001b[31m\\r\\n\"]\nknob = 1\n".to_string(),
            "models.m?[31m??.knob".to_string(),
        ),
        (
            "an overlong name",
            format!("[models.{long}]\nknob = 1\n"),
            format!("models.{}", "n".repeat(256 - "models.".len())),
        ),
    ];

    for (row, raw, rendered) in rows {
        let msg = preflight_retired_keys(&raw, &table)
            .expect_err(row)
            .to_string();

        for byte in ['\u{1b}', '\r', '\n'] {
            assert!(!msg.contains(byte), "{row}: {byte:?} survived in {msg:?}");
        }
        assert_eq!(
            msg,
            format!("`{rendered}` is retired: r; delete it"),
            "{row}"
        );
    }

    let lookup = retired_key_message_in(&table, "models.m\u{1b}\r\n.knob")
        .expect("a retired key under any entry name");
    assert_eq!(lookup, "`models.m???.knob` is retired: r; delete it");
}
