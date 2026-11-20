// The view registry contract: `VIEWS` in the page script is read as data, and
// the markup's per-view elements follow it. `include!`d into a child module of
// `page::tests`, so the script helpers there (`script`, `strip_js_comments`,
// `skip_string_literal`, ...) are in scope; do not add `use` lines here.

/// One registered view as written in the script: its id, its tab label, its
/// `sources` entries as the identifiers or literals they are written as, and
/// the `badge` source it declares, if any.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ViewEntry {
    pub(super) id: String,
    pub(super) label: String,
    pub(super) sources: Vec<String>,
    pub(super) badge: Option<String>,
}

impl ViewEntry {
    /// The source the tab badge reads: the declared `badge`, else the
    /// primary.
    pub(super) fn badge_source(&self) -> &str {
        self.badge.as_deref().unwrap_or(&self.sources[0])
    }
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
            let label = field_text(object, "label").expect("every view declares a `label`");
            let sources = field_text(object, "sources").expect("every view declares `sources`");
            ViewEntry {
                id: unquote(&id),
                label: unquote(&label),
                sources: sources
                    .trim_matches(|c| c == '[' || c == ']')
                    .split(',')
                    .map(unquote)
                    .filter(|s| !s.is_empty())
                    .collect(),
                badge: field_text(object, "badge").map(|badge| unquote(&badge)),
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
/// Mutation check: swap the `models` and `config` entries in VIEWS -> red.
#[test]
fn markup_view_elements_follow_the_view_registry_in_order() {
    let ids = registered_view_ids();

    assert_eq!(ids.len(), 6, "the registry must parse into its six views");
    assert_eq!(markup_view_drift(MARKUP, &ids), Vec::<&str>::new());
}

/// The visible text of each tab button in `markup`, in order: the text
/// between the button's opening tag and its badge.
fn markup_tab_labels(markup: &str) -> Vec<String> {
    let markup = strip_html_comments(markup);
    markup
        .match_indices("role=\"tab\"")
        .map(|(at, _)| {
            let rest = &markup[at..];
            let open_end = rest.find('>').expect("the tab tag closes") + 1;
            let text_end = rest.find("<span").expect("every tab carries its badge");
            rest[open_end..text_end].trim().to_string()
        })
        .collect()
}

/// The tab bar reads the six views in order, and each tab's text is its
/// view's registered label.
///
/// Mutation check: relabel the Accounts tab in the markup -> red.
#[test]
fn tab_labels_read_the_six_views_in_order() {
    let registered: Vec<String> = view_entries(&script())
        .into_iter()
        .map(|v| v.label)
        .collect();

    let shown = markup_tab_labels(MARKUP);

    assert_eq!(
        shown,
        ["Now", "Accounts", "Traffic", "Models", "Config", "Doctor"]
    );
    assert_eq!(registered, shown);
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
fn the_view_registry_parser_reads_ids_sources_and_badge_in_order() {
    let src = "var VIEWS = [\n  // { id: 'ghost', sources: ['x'] },\n  \
               { id: 'a', label: 'A', sources: [Q_SRC, 'usage'], span: null, build: buildA },\n  \
               { id: 'b', label: 'B, b', sources: ['config'], badge: 'other', span: 'all history', build: buildB }\n];";

    assert_eq!(
        view_entries(src),
        vec![
            ViewEntry {
                id: "a".to_string(),
                label: "A".to_string(),
                sources: vec!["Q_SRC".to_string(), "usage".to_string()],
                badge: None,
            },
            ViewEntry {
                id: "b".to_string(),
                label: "B, b".to_string(),
                sources: vec!["config".to_string()],
                badge: Some("other".to_string()),
            },
        ]
    );
}

/// The tab badges count what needs attention from one list: Doctor declares
/// the attention source as its badge and Now's primary is that source, so the
/// two tabs and the verdict strip read the same items. A declared badge names
/// a source the page fetches.
///
/// Mutation check: drop `badge: 'attention'` from the Doctor entry -> red.
#[test]
fn doctor_and_now_badge_from_the_attention_list() {
    let views = view_entries(&script());
    let gets = string_array("GET_SOURCES");

    assert_eq!(view_entry("doctor").badge.as_deref(), Some("attention"));
    assert_eq!(view_entry("doctor").badge_source(), "attention");
    assert_eq!(view_entry("now").badge_source(), "attention");
    for view in &views {
        if let Some(badge) = &view.badge {
            assert!(
                gets.contains(badge),
                "`{}` badges from `{badge}`, which the page never fetches",
                view.id
            );
        }
    }
    assert!(
        views.iter().any(|view| view.badge.is_some()),
        "positive control: some view declares a badge"
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
