// The payload key check: the fields the page declares it reads from each GET
// panel are keys that panel's real DTOs serialize, and every lowercase member
// the script reads is declared, built by the script itself, or a JS/DOM
// built-in. `include!`d into a child module of `page::tests`, so the script
// helpers there (`strip_js_comments`, `skip_string_literal`, `read_word`,
// `string_array`, ...) are in scope, and the named part list is read from the
// sibling view-contract module; do not add `use` lines here.

/// The vendored morph library: upstream code, not page code, so its members
/// are not page reads.
const VENDORED_PART: &str = "dash_05_idiomorph.js";

/// Each GET panel's declared field list, by the `/status` aggregate key that
/// carries the panel.
const PANEL_FIELD_LISTS: &[(&str, &str)] = &[
    ("usage", "USAGE_FIELDS"),
    ("health", "HEALTH_FIELDS"),
    ("config", "CONFIG_FIELDS"),
    ("doctor", "DOCTOR_FIELDS"),
    ("attention", "ATTENTION_FIELDS"),
    ("accounts", "ACCOUNTS_FIELDS"),
];

/// The query vocabulary lists, already pinned to `QueryMetrics` by
/// `dashboard_metric_tokens_are_in_the_query_vocabulary`.
const QUERY_FIELD_LISTS: &[&str] = &["QUERY_METRICS", "QUERY_TOKENS"];

/// Lowercase members of the JS language and the DOM the page calls or reads.
/// Only built-ins belong here: a name the server sends goes in a panel list.
const BUILT_IN_MEMBERS: &[&str] = &[
    // Array, String, RegExp, Promise, Function
    "concat",
    "every",
    "filter",
    "join",
    "length",
    "map",
    "push",
    "reduce",
    "slice",
    "some",
    "replace",
    "split",
    "test",
    "then",
    "catch",
    "resolve",
    "apply",
    "call",
    // Object, Math, JSON, Date
    "create",
    "keys",
    "prototype",
    "abs",
    "floor",
    "max",
    "min",
    "round",
    "trunc",
    "stringify",
    "now",
    "parse",
    // DOM, events, location, URLSearchParams, AbortController
    "add",
    "append",
    "closest",
    "contains",
    "disabled",
    "focus",
    "hidden",
    "remove",
    "style",
    "toggle",
    "type",
    "hash",
    "get",
    "abort",
];

/// The vendored library's public API, the one non-built-in the page calls on
/// an object it did not build.
const VENDORED_MEMBERS: &[&str] = &["morph"];

/// The page script with the vendored library left out.
fn page_script() -> String {
    super::view_contract::NAMED_PARTS
        .iter()
        .filter(|(name, _)| *name != VENDORED_PART)
        .map(|(_, src)| *src)
        .collect()
}

/// Whether a member name is lowercase: letters, digits and underscores, led
/// by a letter or underscore. camelCase names are DOM API spellings, never a
/// `/status` key.
fn is_lowercase_member(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// What the scan finds in a script: every lowercase `.member` read, and every
/// key of an object literal the script builds (bare or quoted).
#[derive(Default)]
struct MemberScan {
    reads: std::collections::BTreeSet<String>,
    literal_keys: std::collections::BTreeSet<String>,
}

/// The last non-whitespace character before `at`, if any.
fn previous_significant(chars: &[char], at: usize) -> Option<char> {
    chars[..at]
        .iter()
        .rev()
        .copied()
        .find(|c| !c.is_whitespace())
}

/// Scan a script, comments stripped and string contents skipped, so prose and
/// label text never read as members or keys. A key is an identifier or string
/// literal that follows `{` or `,` and is followed by `:`.
fn scan_members(src: &str) -> MemberScan {
    let code = strip_js_comments(src);
    let chars: Vec<char> = code.chars().collect();
    let mut scan = MemberScan::default();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' || c == '"' || c == '`' {
            let end = skip_string_literal(&chars, i);
            let opens_key = matches!(previous_significant(&chars, i), Some('{' | ','));
            let after = skip_whitespace(&chars, end);
            if opens_key && chars.get(after) == Some(&':') {
                let inner: String = chars[i + 1..end.saturating_sub(1)].iter().collect();
                scan.literal_keys.insert(inner);
            }
            i = end;
            continue;
        }
        if c == '.' {
            let prev_is_digit = i > 0 && chars[i - 1].is_ascii_digit();
            let at = skip_whitespace(&chars, i + 1);
            let word = read_word(&chars, at);
            if !prev_is_digit && is_lowercase_member(&word) {
                scan.reads.insert(word.clone());
            }
            i = at + word.len().max(1);
            continue;
        }
        if is_ident_char(c) && !c.is_ascii_digit() && (i == 0 || !is_ident_char(chars[i - 1])) {
            let word = read_word(&chars, i);
            let opens_key = matches!(previous_significant(&chars, i), Some('{' | ','));
            let after = skip_whitespace(&chars, i + word.len());
            if opens_key && chars.get(after) == Some(&':') {
                scan.literal_keys.insert(word.clone());
            }
            i += word.len();
            continue;
        }
        i += 1;
    }
    scan
}

/// The union every member read must fall in: the declared panel lists, the
/// query lists, the script's own object-literal keys and the named
/// built-ins.
fn accounted_names(scan: &MemberScan) -> std::collections::BTreeSet<String> {
    let declared = PANEL_FIELD_LISTS
        .iter()
        .map(|(_, list)| *list)
        .chain(QUERY_FIELD_LISTS.iter().copied())
        .flat_map(string_array);
    let named = BUILT_IN_MEMBERS
        .iter()
        .chain(VENDORED_MEMBERS)
        .map(|s| (*s).to_string());
    declared
        .chain(named)
        .chain(scan.literal_keys.iter().cloned())
        .collect()
}

/// The member reads of `src` that nothing accounts for.
fn unaccounted_reads(src: &str) -> Vec<String> {
    let scan = scan_members(src);
    let accounted = accounted_names(&scan);
    scan.reads
        .iter()
        .filter(|read| !accounted.contains(*read))
        .cloned()
        .collect()
}

/// Every object key in `value`, at every depth.
fn keys_at_all_depths(value: &serde_json::Value, out: &mut std::collections::BTreeSet<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                out.insert(key.clone());
                keys_at_all_depths(child, out);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                keys_at_all_depths(item, out);
            }
        }
        _ => {}
    }
}

/// Serialized variants one populated panel cannot carry beside the variant it
/// does: the doctor matrix is available in the fixture, so its unavailable
/// `code` comes from the same DTO serialized in that state.
fn other_variants(panel: &str) -> Vec<serde_json::Value> {
    match panel {
        "doctor" => vec![
            serde_json::to_value(routectl_router::MatrixAvailability::Unavailable {
                code: "no_data",
            })
            .expect("the matrix availability serializes"),
        ],
        _ => Vec::new(),
    }
}

/// Every key the real DTOs serialize for one GET panel, read off the
/// populated `/status` aggregate.
fn panel_keys(panel: &str) -> std::collections::BTreeSet<String> {
    let aggregate = crate::handlers::status::panel_fixtures::aggregate_json();
    let envelope = &aggregate["panels"][panel];
    assert!(
        envelope.is_object(),
        "the aggregate carries the {panel} panel"
    );
    let mut keys = std::collections::BTreeSet::new();
    keys_at_all_depths(envelope, &mut keys);
    for variant in other_variants(panel) {
        keys_at_all_depths(&variant, &mut keys);
    }
    keys
}

/// The names of `fields` that `keys` does not hold.
fn fields_missing_from(
    fields: &[String],
    keys: &std::collections::BTreeSet<String>,
) -> Vec<String> {
    fields
        .iter()
        .filter(|field| !keys.contains(*field))
        .cloned()
        .collect()
}

/// The query metrics the page accepts as null are exactly the ones the
/// server's `QueryMetrics` serializes as an explicit null when nothing
/// contributed: read off a default value, where every `Option` is `None` and
/// every count is `0`. A count the page wrongly allowed as null could render
/// a malformed payload; a nullable metric it wrongly required would refuse a
/// real one.
#[test]
fn the_nullable_query_metrics_are_the_dto_options() {
    let default = serde_json::to_value(routectl_usage::QueryMetrics::default())
        .expect("QueryMetrics serializes");
    let server_nullable: Vec<String> = string_array("QUERY_METRICS")
        .into_iter()
        .filter(|name| default[name.as_str()].is_null())
        .collect();

    let page_nullable = string_array("QUERY_NULLABLE_METRICS");

    assert!(
        !server_nullable.is_empty(),
        "positive control: some listed metric is an Option"
    );
    assert_eq!(page_nullable, server_nullable);
}

#[test]
fn each_declared_panel_field_is_a_key_its_dto_serializes() {
    for (panel, list) in PANEL_FIELD_LISTS {
        let fields = string_array(list);
        let keys = panel_keys(panel);

        let missing = fields_missing_from(&fields, &keys);

        assert!(!fields.is_empty(), "{list} declares no field");
        assert!(
            missing.is_empty(),
            "{list} names fields the {panel} panel never serializes: {missing:?}"
        );
    }
}

/// A key only health serializes, declared for usage, is refused: the check is
/// per panel, not against the union of every panel's keys.
#[test]
fn a_field_from_another_panel_is_refused() {
    let health_only = "half_open_probe_in_flight";
    assert!(panel_keys("health").contains(health_only));
    let mut usage_fields = string_array("USAGE_FIELDS");
    usage_fields.push(health_only.to_string());

    let missing = fields_missing_from(&usage_fields, &panel_keys("usage"));

    assert_eq!(missing, vec![health_only.to_string()]);
}

#[test]
fn every_lowercase_member_read_is_declared_built_or_built_in() {
    let unaccounted = unaccounted_reads(&page_script());

    assert!(
        unaccounted.is_empty(),
        "the page reads members no field list, object literal or built-in accounts for \
         (a payload field goes in its panel's *_FIELDS list in the query vocabulary part): \
         {unaccounted:?}"
    );
}

/// The scan sees the page's real payload reads, and an undeclared read added
/// to the page is reported by name.
#[test]
fn the_member_scan_reports_an_undeclared_read() {
    let scan = scan_members(&page_script());
    assert!(scan.reads.contains("half_open_probe_in_flight"));
    assert!(scan.reads.contains("capability_matrix"));
    let planted = format!(
        "{}\n  var probe = rec.data.not_a_payload_key;\n",
        page_script()
    );

    let unaccounted = unaccounted_reads(&planted);

    assert_eq!(unaccounted, vec!["not_a_payload_key".to_string()]);
}

/// The scan reads code only: a member spelled in a comment or a string is not
/// a read, a number's decimal point is not a member, and both key spellings
/// of an object literal are keys.
#[test]
fn the_member_scan_reads_code_not_comments_strings_or_numbers() {
    let src = "// x.in_comment\n var a = 'y.in_string'; var b = 1.5;\n \
               var c = { bare_key: 1, 'quoted_key': 2 }; c.real_read;";

    let scan = scan_members(src);

    assert_eq!(
        scan.reads.iter().cloned().collect::<Vec<_>>(),
        vec!["real_read".to_string()]
    );
    assert_eq!(
        scan.literal_keys.iter().cloned().collect::<Vec<_>>(),
        vec!["bare_key".to_string(), "quoted_key".to_string()]
    );
}

/// The built-in allowlist holds no name any GET panel serializes, so it can
/// never stand in for a declared payload field.
#[test]
fn no_built_in_member_is_a_panel_key() {
    let mut all_keys = std::collections::BTreeSet::new();
    for (panel, _) in PANEL_FIELD_LISTS {
        all_keys.extend(panel_keys(panel));
    }

    let shadowing: Vec<&str> = BUILT_IN_MEMBERS
        .iter()
        .chain(VENDORED_MEMBERS)
        .copied()
        .filter(|name| all_keys.contains(*name))
        .collect();

    assert!(all_keys.contains("half_open_probe_in_flight"));
    assert!(
        shadowing.is_empty(),
        "allowlisted names are panel keys: {shadowing:?}"
    );
}
