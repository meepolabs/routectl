// The in-place update contract: the vendored morph library is the pinned
// upstream file, `morphPanel` is the only code that reaches it, and no page
// script or markup parses a string as HTML. `include!`d into a child module of
// `page::tests`, so the script helpers there (`strip_js_comments`,
// `skip_string_literal`, `function_body`, ...) are in scope, and the named
// part list is read from the sibling view-contract module; do not add `use`
// lines here.

/// The vendored library's file name; every scan below excludes it, since it
/// is upstream code reviewed as a whole, not page code.
const MORPH_LIBRARY: &str = "dash_05_idiomorph.js";

/// SHA-256 of idiomorph v0.8.0 `dist/idiomorph.js` as published on npm.
const MORPH_LIBRARY_SHA256: &str =
    "4cda2ad0d39c630100957eeb5ec3115f84d57351c6c313579d2441cf48e9880d";

/// The DOM entry points that turn a string into markup. None may appear in
/// page code: every text the page shows is wire data.
const HTML_PARSING_SINKS: &[&str] = &[
    "innerHTML",
    "outerHTML",
    "insertAdjacentHTML",
    "DOMParser",
    "createContextualFragment",
    "document.write",
];

/// The positional updater `morphPanel` replaced. One updater only.
const RETIRED_UPDATER: &[&str] = &[
    "patchChildren",
    "patchNode",
    "sameAttributes",
    "PATCHED_TABS",
];

/// Every script part except the vendored library, by name.
fn page_parts() -> Vec<(&'static str, &'static str)> {
    super::view_contract::NAMED_PARTS
        .iter()
        .copied()
        .filter(|(name, _)| *name != MORPH_LIBRARY)
        .collect()
}

/// Every `function NAME(` declaration in `src` (comments already stripped),
/// with the character range of its body.
fn function_spans(chars: &[char]) -> Vec<(String, usize, usize)> {
    let mut spans = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if matches!(chars[i], '\'' | '"' | '`') {
            i = skip_string_literal(chars, i);
            continue;
        }
        let at_word = i == 0 || !is_ident_char(chars[i - 1]);
        if at_word && read_word(chars, i) == "function" {
            let name_at = skip_whitespace(chars, i + "function".len());
            let name = read_word(chars, name_at);
            if let Some(open) = chars[name_at..].iter().position(|c| *c == '{') {
                let open = name_at + open;
                spans.push((name, open, call_end_of_block(chars, open)));
            }
        }
        i += 1;
    }
    spans
}

/// The index just past the `}` closing the block opened at `open`.
fn call_end_of_block(chars: &[char], open: usize) -> usize {
    let mut depth = 0i32;
    let mut i = open;
    while i < chars.len() {
        match chars[i] {
            '\'' | '"' | '`' => {
                i = skip_string_literal(chars, i);
                continue;
            }
            '{' => depth += 1,
            '}' => {
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

/// For every code (non-string, non-comment) reference to the `Idiomorph`
/// identifier in `parts`, the part and the innermost named function enclosing
/// it (`<top level>` when none does).
fn morph_library_references(parts: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for (name, src) in parts {
        let chars: Vec<char> = strip_js_comments(src).chars().collect();
        let spans = function_spans(&chars);
        let mut i = 0;
        while i < chars.len() {
            if matches!(chars[i], '\'' | '"' | '`') {
                i = skip_string_literal(&chars, i);
                continue;
            }
            let at_word = i == 0 || !is_ident_char(chars[i - 1]);
            if at_word && read_word(&chars, i) == "Idiomorph" {
                let enclosing = spans
                    .iter()
                    .filter(|(_, start, end)| *start < i && i < *end)
                    .min_by_key(|(_, start, end)| end - start)
                    .map_or("<top level>", |(f, _, _)| f.as_str());
                found.push(((*name).to_string(), enclosing.to_string()));
            }
            i += 1;
        }
    }
    found
}

/// The morph library's option selecting a children-only morph. Its value
/// spells a sink name but names a library mode, not the DOM property, so the
/// sink scan skips exactly this text and
/// `morph_panel_refuses_a_non_node_before_morphing` pins where it may appear.
const MORPH_STYLE_OPTION: &str = "morphStyle: 'innerHTML'";

/// Every (part, sink) pair whose code, comments and the one morph option
/// stripped, names an HTML-parsing sink in any spelling.
fn html_sink_hits(parts: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut hits = Vec::new();
    for (name, src) in parts {
        let code = strip_js_comments(src).replace(MORPH_STYLE_OPTION, "");
        for sink in HTML_PARSING_SINKS {
            if code.contains(sink) {
                hits.push(((*name).to_string(), (*sink).to_string()));
            }
        }
    }
    hits
}

/// The named functions in `parts` whose code calls `replaceChildren`.
fn replace_children_callers(parts: &[(&str, &str)]) -> Vec<String> {
    let mut callers = Vec::new();
    for (_, src) in parts {
        let chars: Vec<char> = strip_js_comments(src).chars().collect();
        let spans = function_spans(&chars);
        let code: String = chars.iter().collect();
        for (at, _) in code.match_indices(".replaceChildren(") {
            let at = code[..at].chars().count();
            let enclosing = spans
                .iter()
                .filter(|(_, start, end)| *start < at && at < *end)
                .min_by_key(|(_, start, end)| end - start)
                .map_or("<top level>", |(f, _, _)| f.as_str());
            callers.push(enclosing.to_string());
        }
    }
    callers
}

#[test]
fn the_vendored_morph_library_is_the_pinned_upstream_file() {
    use sha2::Digest;
    let (_, src) = super::view_contract::NAMED_PARTS
        .iter()
        .find(|(name, _)| *name == MORPH_LIBRARY)
        .expect("the morph library is an assembled script part");

    let digest = sha2::Sha256::digest(src.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();

    assert_eq!(hex, MORPH_LIBRARY_SHA256);
}

#[test]
fn the_morph_library_is_assembled_right_after_the_state_part() {
    let names: Vec<&str> = super::view_contract::NAMED_PARTS
        .iter()
        .map(|(name, _)| *name)
        .collect();

    assert_eq!(&names[..2], &["dash_00_state.js", MORPH_LIBRARY]);
}

/// Page code reaches the morph library at exactly one place, inside
/// `morphPanel`, so every update goes through its node-only check.
///
/// Mutation check: call `Idiomorph.morph(body, ...)` directly from
/// renderActiveTab -> red.
#[test]
fn page_code_reaches_the_morph_library_only_inside_morph_panel() {
    let refs = morph_library_references(&page_parts());

    assert_eq!(
        refs,
        vec![("dash_40_render.js".to_string(), "morphPanel".to_string())]
    );
}

#[test]
fn the_morph_reference_scan_attributes_each_call_and_ignores_comments() {
    let parts = [(
        "dash_62_synthetic.js",
        "function morphPanel(p, n) { Idiomorph.morph(p, n, {}); }\n\
         // Idiomorph.morph(p, '<b>') in prose only\n\
         function other(p) { var s = 'Idiomorph'; Idiomorph.morph(p, '<b>'); }\n\
         var alias = Idiomorph;",
    )];

    let refs = morph_library_references(&parts);

    let named: Vec<(&str, &str)> = refs
        .iter()
        .map(|(part, f)| (part.as_str(), f.as_str()))
        .collect();
    assert_eq!(
        named,
        vec![
            ("dash_62_synthetic.js", "morphPanel"),
            ("dash_62_synthetic.js", "other"),
            ("dash_62_synthetic.js", "<top level>"),
        ]
    );
}

/// `morphPanel` refuses anything but a DOM node before it calls the library:
/// the library parses a string argument as HTML.
///
/// Mutation check: delete the `instanceof Node` throw -> red.
#[test]
fn morph_panel_refuses_a_non_node_before_morphing() {
    let body = function_body("morphPanel");
    let guard = body
        .find("if (!(node instanceof Node))")
        .expect("morphPanel checks `node instanceof Node`");
    let throw = body[guard..].find("throw ").map(|at| guard + at);
    let call = body
        .find("Idiomorph.morph(parent, node, { morphStyle: 'innerHTML' });")
        .expect("morphPanel morphs the parent's children to the node");

    assert!(
        throw.is_some_and(|at| at < call),
        "the non-node check must throw before the morph call"
    );
    let code: String = page_parts()
        .iter()
        .map(|(_, src)| strip_js_comments(src))
        .collect();
    assert_eq!(
        code.matches(MORPH_STYLE_OPTION).count(),
        1,
        "the morph option the sink scan skips appears only in morphPanel"
    );
}

/// No page script or markup parses a string as HTML.
///
/// Mutation check: set a pane body with `body.innerHTML = ''` -> red.
#[test]
fn page_code_and_markup_name_no_html_parsing_sink() {
    let parts = page_parts();
    assert!(parts.len() >= 13, "the scan must see every page part");
    let markup = strip_html_comments(MARKUP);

    assert_eq!(html_sink_hits(&parts), Vec::<(String, String)>::new());
    for sink in HTML_PARSING_SINKS {
        assert!(!markup.contains(sink), "the markup names `{sink}`");
    }
}

#[test]
fn the_html_sink_scan_flags_each_sink_in_code_and_ignores_comments() {
    for sink in HTML_PARSING_SINKS {
        let flagged_src = format!("function b(n, s) {{ var x = n.{sink}; return x; }}");
        let keyed_src = format!("function b(n, s) {{ n['{sink}'] = s; }}");
        let keyed = [("dash_62_synthetic.js", keyed_src.as_str())];
        let commented_src = format!("// never {sink} here\nfunction b() {{ return 1; }}");
        let flagged = [("dash_62_synthetic.js", flagged_src.as_str())];
        let commented = [("dash_62_synthetic.js", commented_src.as_str())];

        assert_eq!(
            html_sink_hits(&flagged),
            vec![("dash_62_synthetic.js".to_string(), (*sink).to_string())],
            "`{sink}` in code must be flagged"
        );
        assert_eq!(
            html_sink_hits(&keyed).len(),
            1,
            "`{sink}` as a quoted key must be flagged"
        );
        assert!(
            html_sink_hits(&commented).is_empty(),
            "`{sink}` in a comment must not be flagged"
        );
    }
}

/// Pane bodies and the modal host are morphed, never replaced wholesale; the
/// only `replaceChildren` left is the pane status line. The positional updater
/// is gone.
///
/// Mutation check: put `body.replaceChildren(` back in renderActiveTab -> red.
#[test]
fn pane_bodies_and_the_modal_host_are_only_morphed() {
    let code = strip_js_comments(&page_parts().iter().map(|(_, s)| *s).collect::<String>());
    let render = function_body("renderActiveTab");
    let overlay = function_body("paintOverlay");

    assert_eq!(
        replace_children_callers(&page_parts()),
        vec!["renderSectionStatus"]
    );
    assert_eq!(render.matches("morphPanel(body, ").count(), 2);
    assert!(overlay.contains("morphPanel(host, "));
    for symbol in RETIRED_UPDATER {
        assert!(!code.contains(symbol), "`{symbol}` must be gone");
    }
}

#[test]
fn the_replace_children_scan_names_the_enclosing_function() {
    let parts = [(
        "dash_62_synthetic.js",
        "function paint(b) { b.replaceChildren(x()); }\n\
         // host.replaceChildren() in prose only\n\
         function status(s) { s.replaceChildren(); }",
    )];

    assert_eq!(replace_children_callers(&parts), vec!["paint", "status"]);
}

/// The seat modal is built as a node and handed to the render pass, which
/// morphs it into the host; it never writes the host itself.
#[test]
fn the_seat_modal_is_offered_as_a_node_not_written_to_the_host() {
    let modal = function_body("renderSeatModal");

    assert!(modal.contains("offerOverlay(seatModalNode("));
    assert!(
        !modal.contains("modal-host"),
        "renderSeatModal must not reach the host itself"
    );
}

#[test]
fn the_html_sink_scan_skips_only_the_morph_option() {
    let option = [(
        "dash_62_synthetic.js",
        "function m(p, n) { lib.morph(p, n, { morphStyle: 'innerHTML' }); }",
    )];
    let option_beside_sink = [(
        "dash_62_synthetic.js",
        "function m(p, n) { lib.morph(p, n, { morphStyle: 'innerHTML' }); p.innerHTML = n; }",
    )];
    let other_option = [(
        "dash_62_synthetic.js",
        "function m(p, n) { lib.morph(p, n, { morphStyle: 'outerHTML' }); }",
    )];

    assert!(html_sink_hits(&option).is_empty());
    assert_eq!(html_sink_hits(&option_beside_sink).len(), 1);
    assert_eq!(html_sink_hits(&other_option).len(), 1);
}
