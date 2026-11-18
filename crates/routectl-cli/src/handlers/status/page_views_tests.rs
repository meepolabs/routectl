// The view registry contract: `VIEWS` in the page script is read as data, and
// the markup's per-view elements follow it. `include!`d into a child module of
// `page::tests`, so the script helpers there (`script`, `strip_js_comments`,
// `skip_string_literal`, ...) are in scope; do not add `use` lines here.

/// One registered view as written in the script: its id and its `sources`
/// entries as the identifiers or literals they are written as.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ViewEntry {
    pub(super) id: String,
    pub(super) sources: Vec<String>,
}

/// The index just past the bracket that closes the one opened at
/// `chars[open]`, skipping string literals.
fn balanced_end(chars: &[char], open: usize) -> usize {
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
    panic!("unbalanced literal in the view registry");
}

/// The top-level `{...}` objects of an array body, each as its own text.
fn top_level_objects(body: &[char]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < body.len() {
        match body[i] {
            '\'' | '"' | '`' => i = skip_string_literal(body, i),
            '{' => {
                let end = balanced_end(body, i);
                out.push(body[i..end].iter().collect());
                i = end;
            }
            _ => i += 1,
        }
    }
    out
}

/// The text after `key:` in an object literal, up to the next top-level comma
/// or the object's end.
fn field_text(object: &str, key: &str) -> Option<String> {
    let at = object.find(&format!("{key}:"))? + key.len() + 1;
    let chars: Vec<char> = object[at..].chars().collect();
    let mut i = 0;
    let mut depth = 0i32;
    while i < chars.len() {
        match chars[i] {
            '\'' | '"' | '`' => {
                i = skip_string_literal(&chars, i);
                continue;
            }
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' if depth == 0 => break,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => break,
            _ => {}
        }
        i += 1;
    }
    Some(chars[..i].iter().collect::<String>().trim().to_string())
}

fn unquote(text: &str) -> String {
    text.trim()
        .trim_matches(|c| c == '\'' || c == '"')
        .to_string()
}

/// The `var VIEWS = [ ... ]` registry in `src` (comments stripped), in order.
pub(super) fn view_entries(src: &str) -> Vec<ViewEntry> {
    let code = strip_js_comments(src);
    let at = code
        .find("var VIEWS = [")
        .expect("the dashboard script declares `var VIEWS = [`");
    let chars: Vec<char> = code[at..].chars().collect();
    let open = chars.iter().position(|c| *c == '[').expect("VIEWS body");
    let end = balanced_end(&chars, open);
    top_level_objects(&chars[open + 1..end - 1])
        .iter()
        .map(|object| {
            let id = field_text(object, "id").expect("every view declares an `id`");
            let sources = field_text(object, "sources").expect("every view declares `sources`");
            ViewEntry {
                id: unquote(&id),
                sources: sources
                    .trim_matches(|c| c == '[' || c == ']')
                    .split(',')
                    .map(unquote)
                    .filter(|s| !s.is_empty())
                    .collect(),
            }
        })
        .collect()
}

/// The registered view with `id` in the page script.
pub(super) fn view_entry(id: &str) -> ViewEntry {
    view_entries(&script())
        .into_iter()
        .find(|view| view.id == id)
        .unwrap_or_else(|| panic!("VIEWS registers `{id}`"))
}

/// The per-view id prefixes in the markup: every one must name the views in
/// VIEWS order, since the chrome resolves each view's elements by id.
const VIEW_ID_PREFIXES: &[&str] = &["tab-", "badge-", "pane-", "status-", "body-"];

/// The suffixes of every `id="<prefix>..."` attribute in `markup`, in order.
fn markup_ids_with_prefix(markup: &str, prefix: &str) -> Vec<String> {
    let needle = format!("id=\"{prefix}");
    markup
        .match_indices(&needle)
        .map(|(at, _)| {
            markup[at + needle.len()..]
                .chars()
                .take_while(|c| *c != '"')
                .collect()
        })
        .collect()
}

/// The prefixes whose markup ids differ from `view_ids` (in order).
fn markup_view_drift(markup: &str, view_ids: &[String]) -> Vec<&'static str> {
    let markup = strip_html_comments(markup);
    VIEW_ID_PREFIXES
        .iter()
        .copied()
        .filter(|prefix| markup_ids_with_prefix(&markup, prefix) != view_ids)
        .collect()
}

fn registered_view_ids() -> Vec<String> {
    view_entries(&script()).into_iter().map(|v| v.id).collect()
}

/// The markup carries one tab, badge, pane, status line and body per view,
/// in VIEWS order.
///
/// Mutation check: swap the `health` and `config` entries in VIEWS -> red.
#[test]
fn markup_view_elements_follow_the_view_registry_in_order() {
    let ids = registered_view_ids();

    assert_eq!(ids.len(), 6, "the registry must parse into its six views");
    assert_eq!(markup_view_drift(MARKUP, &ids), Vec::<&str>::new());
}

#[test]
fn reordered_markup_is_reported_against_the_view_registry() {
    let ids: Vec<String> = ["one", "two"].iter().map(|s| (*s).to_string()).collect();
    let block = |id: &str| {
        format!(
            "<button id=\"tab-{id}\"><span id=\"badge-{id}\"></span></button>\
             <section id=\"pane-{id}\"><div id=\"status-{id}\"></div><div id=\"body-{id}\"></div></section>"
        )
    };
    let in_order = format!("<div id=\"tabbar\">{}{}</div>", block("one"), block("two"));
    let swapped_panes = in_order
        .replace("pane-one", "pane-TMP")
        .replace("pane-two", "pane-one")
        .replace("pane-TMP", "pane-two");
    let commented = format!("<!-- id=\"tab-zero\" -->{in_order}");

    assert_eq!(markup_view_drift(&in_order, &ids), Vec::<&str>::new());
    assert_eq!(markup_view_drift(&commented, &ids), Vec::<&str>::new());
    assert_eq!(markup_view_drift(&swapped_panes, &ids), vec!["pane-"]);
}

#[test]
fn the_view_registry_parser_reads_ids_and_sources_in_order() {
    let src = "var VIEWS = [\n  // { id: 'ghost', sources: ['x'] },\n  \
               { id: 'a', label: 'A', sources: [Q_SRC, 'usage'], span: null, build: buildA },\n  \
               { id: 'b', label: 'B, b', sources: ['config'], span: 'all history', build: buildB }\n];";

    assert_eq!(
        view_entries(src),
        vec![
            ViewEntry {
                id: "a".to_string(),
                sources: vec!["Q_SRC".to_string(), "usage".to_string()],
            },
            ViewEntry {
                id: "b".to_string(),
                sources: vec!["config".to_string()],
            },
        ]
    );
}

/// The old per-view maps are gone: VIEWS is the one registry.
#[test]
fn the_per_tab_maps_are_retired() {
    let code = strip_js_comments(&script());

    assert!(
        code.contains("var VIEWS = ["),
        "positive control: the registry is declared"
    );
    for name in [
        "TABS",
        "TAB_LABELS",
        "TAB_SOURCES",
        "WINDOWLESS_TABS",
        "WINDOWLESS_SPAN",
        "WINDOWLESS_LABEL",
        "BUILDERS",
    ] {
        let used = code.match_indices(name).any(|(at, _)| {
            let before = code[..at].chars().next_back();
            let after = code[at + name.len()..].chars().next();
            !before.is_some_and(is_ident_char) && !after.is_some_and(is_ident_char)
        });
        assert!(!used, "`{name}` must be gone; read VIEWS instead");
    }
}
