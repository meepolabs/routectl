use super::*;
use crate::config::{
    CURRENT_CONFIG_VERSION, Config, RawConfigPreflightError, preflight_raw_config,
};
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

/// Whether the schema has no key at `path`: an unknown segment, or a segment
/// that descends into a scalar (a retired leaf under what is now a scalar
/// map value, such as a retry class count).
fn is_absent_from_schema(verdict: &Result<crate::config_path::PathShape, PathError>) -> bool {
    matches!(
        verdict,
        Err(PathError::UnknownSegment { .. } | PathError::NotATable { .. })
    )
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
                is_absent_from_schema(&verdict),
                "`{path}` must be unknown to the schema: {verdict:?}"
            );
        }
    }
}

/// The absence check accepts both ways a path can miss the schema and
/// nothing else: a live path, a live scalar map value and a live table
/// stay present.
#[test]
fn the_schema_absence_check_rejects_live_paths() {
    let rows = [
        ("an unknown key", "retry.no_such_knob", true),
        (
            "a leaf under a scalar map value",
            "retry.classes.server-error.retry",
            true,
        ),
        ("a live scalar", "retry.max_attempts", false),
        (
            "a live scalar map value",
            "retry.classes.server-error",
            false,
        ),
        ("a live table", "retry.classes", false),
    ];

    for (row, path, absent) in rows {
        let verdict = validate_config_path(path);

        assert_eq!(
            is_absent_from_schema(&verdict),
            absent,
            "row `{row}`: {verdict:?}"
        );
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

/// A key nested under a retired table is reported once, as the table: one
/// line naming `fidelity`, never a second line for the nested key.
#[test]
fn a_key_under_a_retired_table_is_reported_once_as_the_table() {
    let rows = [
        (
            "the paid-probe caps sub-table",
            "[fidelity.paid_probe_daily_caps]\nfast = 5\n",
        ),
        (
            "the opt-in list beside the caps",
            "[fidelity]\nprefix_impact_opt_in = [\"fast\"]\npaid_probe_daily_caps = 1\n",
        ),
    ];
    let row = RETIRED_KEYS
        .iter()
        .find(|row| row.path == ["fidelity"])
        .expect("the table retires `fidelity` whole");
    let expected = format!("`fidelity` is retired: {}; delete it", row.replacement);

    for (name, body) in rows {
        let raw = format!("version = {CURRENT_CONFIG_VERSION}\n{body}");

        let msg = refusal_message(&raw);

        assert_eq!(msg, expected, "row `{name}`");
    }
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
        ("usage db path", "[usage]\ndb_path = \"/tmp/u.db\"\n"),
        ("usage retention", "[usage]\nretention_days = 30\n"),
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

/// The refused `seat_selection` literal is named by key path only: the line
/// carries neither the refused value nor any other value text from the file,
/// and the live values on the same key pass.
#[test]
fn round_robin_seat_selection_is_refused_by_key_path_alone() {
    // Arrange
    let pool = |selection: &str| {
        format!(
            "version = {CURRENT_CONFIG_VERSION}\n\
             [pools.team]\n\
             members = [\"{SENTINEL}\"]\n\
             seat_selection = \"{selection}\"\n"
        )
    };

    // Act
    let msg = refusal_message(&pool("round-robin"));

    // Assert
    assert_eq!(
        msg,
        "`pools.team.seat_selection` is retired: use sticky-least-loaded; delete it"
    );
    assert!(!msg.contains("round-robin"), "{msg}");
    assert!(!msg.contains(SENTINEL), "{msg}");
    for live in ["fill-first", "sticky-least-loaded"] {
        assert_eq!(preflight_raw_config(&pool(live)), Ok(()), "{live}");
    }
}

/// A file that sets no retired key, or any other key, loads with every
/// setting at its built-in default: retiring keys moved no other default.
#[test]
fn a_file_omitting_every_retired_key_keeps_the_built_in_defaults() {
    // Arrange
    let raw = format!("version = {CURRENT_CONFIG_VERSION}\n");
    let built_in = Config {
        version: CURRENT_CONFIG_VERSION,
        ..Config::default()
    };

    // Act
    preflight_raw_config(&raw).expect("no retired key to refuse");
    let parsed = crate::parse_config(&raw).expect("a version-only file parses");

    // Assert
    assert_eq!(
        serde_json::to_value(&parsed).expect("parsed config serializes"),
        serde_json::to_value(&built_in).expect("built-in config serializes"),
    );
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

/// An entry name is rendered without control bytes and capped, on the file
/// pass and on the edit-path lookup alike.
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
            "models.<quoted name>.knob".to_string(),
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
    assert_eq!(
        lookup,
        "`models.<quoted name>.knob` is retired: r; delete it"
    );
}

/// The `allowed_betas` row, the retired key under a provider entry.
fn allowed_betas_row() -> &'static RetiredKey {
    RETIRED_KEYS
        .iter()
        .find(|row| row.path == ["providers", ANY_ENTRY, "allowed_betas"])
        .expect("the table retires `providers.*.allowed_betas`")
}

/// A quoted entry name can be any string, a pasted credential included, so
/// a name that is not a bare key is shown as a placeholder on the file pass
/// and on the edit-path lookup, never echoed.
#[test]
fn a_quoted_entry_name_renders_as_a_placeholder() {
    let row = allowed_betas_row();
    let expected = format!(
        "`providers.<quoted name>.allowed_betas` is retired: {}; delete it",
        row.replacement
    );
    let names = [
        ("a credential-shaped name", format!("sk-ant:{SENTINEL}")),
        ("a name with a space", format!("my {SENTINEL}")),
        ("a non-ASCII name", format!("{SENTINEL}\u{e9}")),
        ("a name with a dot", format!("{SENTINEL}.x")),
    ];

    for (case, name) in &names {
        // Arrange
        let mut entry = toml::Table::new();
        entry.insert("allowed_betas".to_string(), toml::Value::Integer(1));
        let mut providers = toml::Table::new();
        providers.insert(name.clone(), toml::Value::Table(entry));
        let mut root = toml::Table::new();
        root.insert("providers".to_string(), toml::Value::Table(providers));
        let raw = at_current_version(&root);
        assert!(raw.contains(SENTINEL), "control `{case}`: {raw}");

        // Act
        let msg = refusal_message(&raw);

        // Assert
        assert_eq!(msg, expected, "file pass `{case}`");
        assert!(!msg.contains(SENTINEL), "file pass `{case}`: {msg}");
    }

    // A dotted path cannot carry a dot inside one segment, so the lookup
    // takes the other names only.
    for (case, name) in &names[..3] {
        let msg = retired_key_message(&format!("providers.{name}.allowed_betas"))
            .expect("a retired key under any entry name");

        assert_eq!(msg, expected, "lookup `{case}`");
        assert!(!msg.contains(SENTINEL), "lookup `{case}`: {msg}");
    }
}

/// A bare entry name is shown as written, on both passes.
#[test]
fn a_bare_entry_name_renders_verbatim() {
    let row = allowed_betas_row();
    let names = ["anthropic", "Work_2", "east-1"];

    for name in names {
        let raw =
            format!("version = {CURRENT_CONFIG_VERSION}\n[providers.{name}]\nallowed_betas = 1\n");
        let expected = format!(
            "`providers.{name}.allowed_betas` is retired: {}; delete it",
            row.replacement
        );

        let file_pass = refusal_message(&raw);
        let lookup = retired_key_message(&format!("providers.{name}.allowed_betas"));

        assert_eq!(file_pass, expected, "file pass `{name}`");
        assert_eq!(lookup, Some(expected), "lookup `{name}`");
    }
}

/// A provider entry on the current shape, the positive control for the
/// cloak rows below.
const CLOAK_PROVIDER: &str = "[providers.sub]\nformat = \"anthropic-messages\"\n\
                              base_url = \"https://api.anthropic.com\"\n\
                              credential = \"oauth://anthropic\"\nauth = \"bearer\"\n";

/// The cloak keys moved onto the entry itself, so a leftover `cloak` table
/// is retired whole: every spelling TOML allows for it, word list and
/// rename pairs included, is refused as one line naming the table, never a
/// second line for the nested key and never the value.
#[test]
fn a_leftover_cloak_table_is_refused_once_in_every_spelling() {
    let pair = format!("from = \"{SENTINEL}\"\nto = \"y\"\n");
    let rows = [
        (
            "word list sub-table",
            format!("[providers.sub.cloak]\nsensitive_words = [\"{SENTINEL}\"]\n"),
        ),
        (
            "word list inline table",
            format!("cloak = {{ sensitive_words = [\"{SENTINEL}\"] }}\n"),
        ),
        (
            "word list dotted key",
            format!("cloak.sensitive_words = [\"{SENTINEL}\"]\n"),
        ),
        (
            "rename array of tables",
            format!("[[providers.sub.cloak.tool_rename]]\n{pair}"),
        ),
        (
            "rename inline table",
            format!("cloak = {{ tool_rename = [{{ from = \"{SENTINEL}\", to = \"y\" }}] }}\n"),
        ),
        (
            "rename dotted key",
            format!("cloak.tool_rename = [{{ from = \"{SENTINEL}\", to = \"y\" }}]\n"),
        ),
        (
            "mode sub-table",
            "[providers.sub.cloak]\nmode = \"always\"\n".to_string(),
        ),
    ];
    let row = RETIRED_KEYS
        .iter()
        .find(|row| row.path == ["providers", ANY_ENTRY, "cloak"])
        .expect("the table retires the provider cloak table whole");
    let expected = format!(
        "`providers.sub.cloak` is retired: {}; delete it",
        row.replacement
    );
    let clean = format!("version = {CURRENT_CONFIG_VERSION}\n{CLOAK_PROVIDER}");
    assert!(
        crate::parse_config(&clean).is_ok(),
        "control: the entry without a cloak table parses"
    );

    for (row, body) in rows {
        // Arrange
        let raw = format!("version = {CURRENT_CONFIG_VERSION}\n{CLOAK_PROVIDER}{body}");

        // Act
        let msg = refusal_message(&raw);

        // Assert
        assert_eq!(msg, expected, "row `{row}`");
        assert!(
            !msg.contains(SENTINEL),
            "row `{row}` echoes the value: {msg}"
        );
    }
}
