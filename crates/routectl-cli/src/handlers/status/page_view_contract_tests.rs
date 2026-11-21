// The view-builder contract: builders attach no listeners, every control they
// mark names a delegated action that exists, and every table carries a unique
// literal id. `include!`d into a child module of `page::tests`, so the script
// helpers there (`script`, `strip_js_comments`, `function_body`, ...) are in
// scope; do not add `use` lines here.

/// Every script part by file name, in `SCRIPT_PARTS` order. Welded to
/// `SCRIPT_PARTS` by `named_parts_are_the_assembled_script_parts`, so a part
/// added to the page without a name here fails rather than escaping the scans.
pub(super) const NAMED_PARTS: &[(&str, &str)] = &[
    ("dash_00_state.js", include_str!("dash_00_state.js")),
    ("dash_05_idiomorph.js", include_str!("dash_05_idiomorph.js")),
    ("dash_10_format.js", include_str!("dash_10_format.js")),
    (
        "dash_20_query_vocab.js",
        include_str!("dash_20_query_vocab.js"),
    ),
    ("dash_30_transport.js", include_str!("dash_30_transport.js")),
    ("dash_40_render.js", include_str!("dash_40_render.js")),
    ("dash_50_dom.js", include_str!("dash_50_dom.js")),
    (
        "dash_60_tab_overview.js",
        include_str!("dash_60_tab_overview.js"),
    ),
    (
        "dash_61_view_traffic.js",
        include_str!("dash_61_view_traffic.js"),
    ),
    (
        "dash_62_view_accounts.js",
        include_str!("dash_62_view_accounts.js"),
    ),
    (
        "dash_63_view_models.js",
        include_str!("dash_63_view_models.js"),
    ),
    (
        "dash_72_tab_config.js",
        include_str!("dash_72_tab_config.js"),
    ),
    (
        "dash_73_tab_doctor.js",
        include_str!("dash_73_tab_doctor.js"),
    ),
    ("dash_80_views.js", include_str!("dash_80_views.js")),
    ("dash_90_chrome.js", include_str!("dash_90_chrome.js")),
];

/// Whether a part builds view DOM: the shared DOM helpers and every view part.
fn is_builder_part(name: &str) -> bool {
    name == "dash_50_dom.js" || name.starts_with("dash_6") || name.starts_with("dash_7")
}

fn builder_parts() -> Vec<(&'static str, &'static str)> {
    NAMED_PARTS
        .iter()
        .copied()
        .filter(|(name, _)| is_builder_part(name))
        .collect()
}

/// The parts (by name) whose code, comments stripped, attaches a listener.
fn parts_attaching_listeners(parts: &[(&str, &str)]) -> Vec<String> {
    parts
        .iter()
        .filter(|(_, src)| strip_js_comments(src).contains("addEventListener"))
        .map(|(name, _)| (*name).to_string())
        .collect()
}

/// The character index just past the call's closing parenthesis, given the
/// index of its opening one.
pub(super) fn call_end(chars: &[char], open: usize) -> usize {
    let mut depth = 0i32;
    let mut i = open;
    while i < chars.len() {
        match chars[i] {
            '\'' | '"' | '`' => {
                i = skip_string_literal(chars, i);
                continue;
            }
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    chars.len()
}

/// The argument texts of every CALL of `name` in `src` (comments stripped),
/// split at the call's own top-level commas. The function's declaration is
/// not a call and is skipped.
pub(super) fn call_args(src: &str, name: &str) -> Vec<Vec<String>> {
    let src = strip_js_comments(src);
    let chars: Vec<char> = src.chars().collect();
    let needle: Vec<char> = format!("{name}(").chars().collect();
    let mut calls = Vec::new();
    let mut i = 0;
    while i + needle.len() <= chars.len() {
        if chars[i] == '\'' || chars[i] == '"' || chars[i] == '`' {
            i = skip_string_literal(&chars, i);
            continue;
        }
        let starts_word = i == 0 || !is_ident_char(chars[i - 1]);
        if !starts_word || chars[i..i + needle.len()] != needle[..] {
            i += 1;
            continue;
        }
        let before: String = chars[..i].iter().collect();
        let open = i + needle.len() - 1;
        let end = call_end(&chars, open);
        if !before.trim_end().ends_with("function") {
            calls.push(split_args(&chars[open + 1..end - 1]));
        }
        i = end;
    }
    calls
}

pub(super) fn split_args(body: &[char]) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    let mut i = 0;
    while i < body.len() {
        let c = body[i];
        if c == '\'' || c == '"' || c == '`' {
            let end = skip_string_literal(body, i);
            current.extend(&body[i..end]);
            i = end;
            continue;
        }
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                args.push(current.trim().to_string());
                current.clear();
                i += 1;
                continue;
            }
            _ => {}
        }
        current.push(c);
        i += 1;
    }
    if !current.trim().is_empty() {
        args.push(current.trim().to_string());
    }
    args
}

/// The value of a single-quoted string literal, or `None` for any other
/// expression.
pub(super) fn quoted_literal(arg: &str) -> Option<String> {
    let inner = arg.strip_prefix('\'')?.strip_suffix('\'')?;
    (!inner.contains('\'')).then(|| inner.to_string())
}

/// The literal action names `setAction` calls in `parts` mark nodes with.
/// A call whose action is not a string literal panics: an action computed at
/// runtime is one this check cannot see.
fn marked_actions(parts: &[(&str, &str)]) -> std::collections::BTreeSet<String> {
    let mut found = std::collections::BTreeSet::new();
    for (name, src) in parts {
        for args in call_args(src, "setAction") {
            let action = args
                .get(1)
                .unwrap_or_else(|| panic!("{name}: a `setAction` call has no action argument"));
            let literal = quoted_literal(action).unwrap_or_else(|| {
                panic!("{name}: `setAction` action `{action}` is not a string literal")
            });
            found.insert(literal);
        }
    }
    found
}

/// The keys of the `var ACTIONS = { ... }` object literal in `src`.
fn action_keys(src: &str) -> std::collections::BTreeSet<String> {
    let src = strip_js_comments(src);
    let at = src
        .find("var ACTIONS = {")
        .expect("the dashboard script declares `var ACTIONS = {`");
    let chars: Vec<char> = src[at..].chars().collect();
    let open = chars.iter().position(|c| *c == '{').expect("ACTIONS body");
    let end = call_end(&chars, open);
    let mut keys = std::collections::BTreeSet::new();
    let mut depth = 0i32;
    let mut i = open + 1;
    while i < end - 1 {
        match chars[i] {
            '\'' | '"' => {
                let close = skip_string_literal(&chars, i);
                let after = skip_whitespace(&chars, close);
                if depth == 0 && chars.get(after) == Some(&':') {
                    keys.insert(chars[i + 1..close - 1].iter().collect());
                }
                i = close;
                continue;
            }
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    keys
}

/// Builder actions without an ACTIONS entry, and ACTIONS entries no builder
/// marks a node with.
fn action_drift(builders: &[(&str, &str)], actions_src: &str) -> (Vec<String>, Vec<String>) {
    let marked = marked_actions(builders);
    let keys = action_keys(actions_src);
    (
        marked.difference(&keys).cloned().collect(),
        keys.difference(&marked).cloned().collect(),
    )
}

/// The first `mkTable` id literal used twice in `src`. A call whose id is not
/// a string literal panics: its uniqueness could not be checked.
fn duplicate_table_id(src: &str) -> Option<String> {
    let mut seen = std::collections::BTreeSet::new();
    for args in call_args(src, "mkTable") {
        let first = args.first().map_or("", String::as_str);
        let id = quoted_literal(first)
            .unwrap_or_else(|| panic!("`mkTable` id `{first}` is not a string literal"));
        if !seen.insert(id.clone()) {
            return Some(id);
        }
    }
    None
}

/// The number of places in `parts` (comments stripped) that name the
/// `data-action` attribute in any quote style, or reach it through `dataset`.
fn data_action_writes(parts: &[(&str, &str)]) -> usize {
    parts
        .iter()
        .map(|(_, src)| {
            let code = strip_js_comments(src);
            code.matches("data-action").count() + code.matches("dataset").count()
        })
        .sum()
}

#[test]
fn named_parts_are_the_assembled_script_parts() {
    let named: Vec<&str> = NAMED_PARTS.iter().map(|(_, src)| *src).collect();

    assert_eq!(
        named, SCRIPT_PARTS,
        "NAMED_PARTS must list every SCRIPT_PARTS entry, in order"
    );
}

/// No view builder attaches a listener: interaction goes through
/// `data-action` and the delegated dispatch in the chrome part.
///
/// Mutation check: restore `tr.addEventListener('click', toggle);` in xrow ->
/// red.
#[test]
fn view_builders_attach_no_listeners() {
    let parts = builder_parts();
    assert!(
        parts.len() >= 7,
        "the builder filter must select the DOM part and every view part"
    );

    assert_eq!(parts_attaching_listeners(&parts), Vec::<String>::new());
}

#[test]
fn the_listener_scan_flags_code_and_ignores_comments() {
    let flagged = [(
        "dash_62_synthetic.js",
        "function b() { var x = mk(); x.addEventListener('click', go); }",
    )];
    let commented = [(
        "dash_62_synthetic.js",
        "// never x.addEventListener here\nfunction b() { return mk(); }",
    )];

    assert_eq!(
        parts_attaching_listeners(&flagged),
        vec!["dash_62_synthetic.js"]
    );
    assert!(parts_attaching_listeners(&commented).is_empty());
}

/// Every action a builder marks a node with has a handler, and every handler
/// is reachable from some builder.
///
/// Mutation check: rename the `'provider-overflow'` key in ACTIONS -> red
/// (one marked action unhandled, one entry unused).
#[test]
fn every_marked_action_has_a_handler_and_every_handler_is_marked() {
    let builders = builder_parts();
    let chrome = include_str!("dash_90_chrome.js");

    let (unhandled, unused) = action_drift(&builders, chrome);

    assert!(
        !marked_actions(&builders).is_empty(),
        "the builders mark no action at all; the scan is not seeing setAction"
    );
    assert_eq!(
        unhandled,
        Vec::<String>::new(),
        "marked actions with no ACTIONS entry"
    );
    assert_eq!(
        unused,
        Vec::<String>::new(),
        "ACTIONS entries no builder marks"
    );
}

#[test]
fn action_drift_reports_unhandled_and_unused_actions() {
    let builders = [(
        "dash_62_synthetic.js",
        "function b() { setAction(x, 'shared', 1); setAction(y, 'missing', null); }",
    )];
    let actions = "var ACTIONS = {\n  'shared': function (arg) { go(arg, { a: 1 }); },\n  \
                   'spare': function () { stop(); }\n};";

    let (unhandled, unused) = action_drift(&builders, actions);

    assert_eq!(unhandled, vec!["missing"]);
    assert_eq!(unused, vec!["spare"]);
}

/// Builders reach `data-action` only through `setAction`, in any quote style
/// or through `dataset`, so the literal scan of `setAction` calls sees every
/// action name.
///
/// Mutation check: add `btn.setAttribute("data-action", "x");` to
/// groupByButton -> red.
#[test]
fn builders_write_data_action_only_inside_set_action() {
    let builders = builder_parts();

    assert_eq!(
        data_action_writes(&[("setAction", &function_body("setAction"))]),
        1,
        "setAction must be the writer of `data-action`"
    );
    assert_eq!(
        data_action_writes(&builders),
        1,
        "only setAction may name `data-action` in a builder part"
    );
}

#[test]
fn the_data_action_scan_counts_every_spelling_and_ignores_comments() {
    let setter = "function setAction(n, a) { n.setAttribute('data-action', a); }\n\
                  // a note about data-action in prose\n";
    let double =
        format!("{setter}function b(x) {{ x.setAttribute(\"data-action\", \"missing\"); }}");
    let template = format!("{setter}function b(x) {{ x.setAttribute(`data-action`, 'm'); }}");
    let dataset = format!("{setter}function b(x) {{ x.dataset.action = 'missing'; }}");

    assert_eq!(data_action_writes(&[("dash_62_synthetic.js", setter)]), 1);
    for src in [&double, &template, &dataset] {
        assert_eq!(
            data_action_writes(&[("dash_62_synthetic.js", src.as_str())]),
            2,
            "a second writer must be counted: {src}"
        );
    }
}

/// Table ids are literals and unique across the whole script, so a table's
/// rows, detail rows and scroll container never share an id with another's.
///
/// Mutation check: rename `'config-classes'` to `'config-models'` -> red.
#[test]
fn table_ids_are_unique_literals() {
    let script = script();

    assert!(
        call_args(&script, "mkTable").len() >= 7,
        "the scan must see every mkTable call"
    );
    assert_eq!(duplicate_table_id(&script), None);
}

#[test]
fn a_duplicate_table_id_is_reported() {
    let src = "function mkTable(id, caption, cols, opts) { return 1; }\n\
               var a = mkTable('t-one', 'A', [C('x')]);\n\
               var b = mkTable('t-two', 'B', [C('x')], { expandable: true });\n\
               var c = mkTable('t-one', 'C', [C('x')]);";
    let unique = "var a = mkTable('t-one', 'A', []); var b = mkTable('t-two', 'B', []);";

    assert_eq!(duplicate_table_id(src), Some("t-one".to_string()));
    assert_eq!(duplicate_table_id(unique), None);
}

/// Expandable-row ids come from (table id, row key) alone: no render counter
/// survives anywhere in the script.
///
/// Mutation check: put `'detail-' + (uid++)` back as the detail-row id ->
/// red.
#[test]
fn row_ids_derive_only_from_table_id_and_row_key() {
    let code = strip_js_comments(&script());
    let xrow = function_body("xrow");
    let row_id = function_body("rowDomId");

    assert!(!code.contains("uid++"), "the render counter must be gone");
    assert!(
        !code.contains("var uid"),
        "the render counter declaration must be gone"
    );
    assert!(xrow.contains("var rid = rowDomId(tbl.id, rowKey);"));
    for id in [
        "tr.id = rid;",
        "detailRow.id = rid + '-detail';",
        "btn.id = rid + '-toggle';",
    ] {
        assert!(xrow.contains(id), "xrow must assign `{id}`");
    }
    assert_eq!(
        xrow.matches(".id = ").count(),
        3,
        "xrow assigns exactly the three row-derived ids"
    );
    assert_eq!(row_id.trim(), "return tableId + '-' + idKey(rowKey);");
}

/// Items of a list that reorders carry an id from their own identity, so the
/// morph matches a card to the same record rather than to whatever now sits
/// at its position: Doctor checks (section, name, detail, remediation), Usage
/// groups (label, as a sortable table's row key) and the Models cause
/// lines (their attention item's key).
///
/// Mutation check: drop `cardEl.id = id;` from doctorFinding -> red.
#[test]
fn reordering_list_items_carry_an_identity_id() {
    let finding = function_body("doctorFinding");
    let pass = function_body("doctorPass");
    let item_id = compact(&function_body("doctorItemId"));
    let findings = function_body("doctorFindings");
    let passes = function_body("doctorPasses");
    let traffic = function_body("trafficTable");

    assert!(finding.contains("cardEl.id = id;"));
    assert!(pass.contains("row.id = id;"));
    assert_eq!(
        item_id,
        "varbase=prefix+'-'+idkey(f.section)+'-'+idkey(f.name)+'-'+idkey(f.detail);\
         if(f.remediation){base+='-'+idkey(f.remediation);}\
         returnuniqueitemid(base,seen);"
    );
    assert!(findings.contains("doctorFinding(f, doctorItemId('doctor-finding', f, seen))"));
    assert!(passes.contains("doctorPass(p, doctorItemId('doctor-pass', p, seen))"));
    assert!(traffic.contains("rowKey: function (g) { return g.label; }"));
    assert!(
        function_body("modelsCauseLine").contains("tr.id = 'models-cause-' + idKey(group.key);")
    );
}
