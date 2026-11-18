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
    "setHTMLUnsafe",
    "parseHTMLUnsafe",
    "srcdoc",
];

/// Names a page script may reach only through a plain, dotted spelling the
/// scans above can see. As a string literal they are the key of a bracket
/// access (`x['replaceChildren'](`) or of an alias assembled at runtime.
const GUARDED_LITERALS: &[&str] = &["Idiomorph", "replaceChildren"];

/// Members that, written as a bracket key, reach a guarded call:
/// `document['write']`, `lib['morph']`. Only the bracket form is flagged,
/// since `'write'` is ordinary label text elsewhere.
const GUARDED_BRACKET_KEYS: &[&str] = &["write", "morph", "replaceChildren", "Idiomorph"];

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

/// The innermost named function in `spans` enclosing `at`.
fn enclosing_function(spans: &[(String, usize, usize)], at: usize) -> String {
    spans
        .iter()
        .filter(|(_, start, end)| *start < at && at < *end)
        .min_by_key(|(_, start, end)| end - start)
        .map_or("<top level>", |(f, _, _)| f.as_str())
        .to_string()
}

/// One code reference to the morph library: its part, its enclosing named
/// function, and whether it is the direct call `Idiomorph.morph(`. Anything
/// else (`var m = Idiomorph.morph;`, `.bind(`, passing `Idiomorph` on) hands
/// the library out of `morphPanel`, where its node-only check no longer runs.
#[derive(Debug, PartialEq, Eq)]
struct MorphReference {
    part: String,
    function: String,
    is_direct_call: bool,
}

/// Whether `chars[at..]` reads `Idiomorph.morph(`, whitespace allowed around
/// the dot and before the paren.
fn is_direct_morph_call(chars: &[char], at: usize) -> bool {
    let dot = skip_whitespace(chars, at + "Idiomorph".len());
    if chars.get(dot) != Some(&'.') {
        return false;
    }
    let member_at = skip_whitespace(chars, dot + 1);
    if read_word(chars, member_at) != "morph" {
        return false;
    }
    let paren = skip_whitespace(chars, member_at + "morph".len());
    chars.get(paren) == Some(&'(')
}

/// Every code (non-string, non-comment) reference to the `Idiomorph`
/// identifier in `parts`.
fn morph_library_references(parts: &[(&str, &str)]) -> Vec<MorphReference> {
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
                found.push(MorphReference {
                    part: (*name).to_string(),
                    function: enclosing_function(&spans, i),
                    is_direct_call: is_direct_morph_call(&chars, i),
                });
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
            callers.push(enclosing_function(&spans, at));
        }
    }
    callers
}

/// The content of every plain string literal in `chars` (comments already
/// stripped), with the index of its opening quote. A template literal with a
/// `${` substitution has no single content and is skipped.
fn string_literals(chars: &[char]) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if matches!(chars[i], '\'' | '"' | '`') {
            let end = skip_string_literal(chars, i);
            let content: String = chars[i + 1..end.saturating_sub(1).max(i + 1)]
                .iter()
                .collect();
            if !(chars[i] == '`' && content.contains("${")) {
                found.push((i, content));
            }
            i = end;
            continue;
        }
        i += 1;
    }
    found
}

/// Whether the `[` before the literal at `quote` opens a member access
/// (`x[`, `f()[`, `a[0][`) rather than an array literal (`= [`, `, [`).
fn is_bracket_key(chars: &[char], quote: usize) -> bool {
    let mut i = quote;
    while i > 0 && chars[i - 1].is_whitespace() {
        i -= 1;
    }
    if i == 0 || chars[i - 1] != '[' {
        return false;
    }
    let mut j = i - 1;
    while j > 0 && chars[j - 1].is_whitespace() {
        j -= 1;
    }
    j > 0 && (is_ident_char(chars[j - 1]) || matches!(chars[j - 1], ')' | ']'))
}

/// Every (part, literal) pair whose string literal spells a guarded name, or
/// keys a bracket access with a guarded member.
fn guarded_literal_hits(parts: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut hits = Vec::new();
    for (name, src) in parts {
        let chars: Vec<char> = strip_js_comments(src).chars().collect();
        for (quote, content) in string_literals(&chars) {
            let guarded = GUARDED_LITERALS.contains(&content.as_str())
                || (GUARDED_BRACKET_KEYS.contains(&content.as_str())
                    && is_bracket_key(&chars, quote));
            if guarded {
                hits.push(((*name).to_string(), content));
            }
        }
    }
    hits
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
        vec![MorphReference {
            part: "dash_40_render.js".to_string(),
            function: "morphPanel".to_string(),
            is_direct_call: true,
        }]
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
        .map(|r| (r.part.as_str(), r.function.as_str()))
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

/// An alias taken inside `morphPanel` and invoked elsewhere escapes the
/// node-only check while every call site still reads as `morphPanel`'s, so
/// any reference there other than the direct call is reported as one.
#[test]
fn the_morph_reference_scan_tells_a_direct_call_from_an_alias() {
    let cases = [
        ("direct call", "Idiomorph.morph(p, n, {});", true),
        ("spaced call", "Idiomorph . morph (p, n, {});", true),
        (
            "aliased member",
            "morphAlias = Idiomorph.morph; morphAlias(p, n);",
            false,
        ),
        (
            "bound member",
            "var m = Idiomorph.morph.bind(null); m(p, n);",
            false,
        ),
        ("library handed on", "keep(Idiomorph);", false),
        (
            "other member",
            "Idiomorph.defaults.morphStyle = 'outerHTML';",
            false,
        ),
    ];
    for (label, body, direct) in cases {
        let src = format!("function morphPanel(p, n) {{ {body} }}");
        let parts = [("dash_62_synthetic.js", src.as_str())];

        let refs = morph_library_references(&parts);

        assert_eq!(refs.len(), 1, "{label}: one reference");
        assert_eq!(refs[0].function, "morphPanel", "{label}");
        assert_eq!(refs[0].is_direct_call, direct, "{label}");
    }
}

/// Page code never spells a guarded name as a string: no bracket access to
/// the library or `replaceChildren`, and no runtime-assembled alias.
///
/// Mutation check: write `body['replaceChildren']()` in renderActiveTab ->
/// red.
#[test]
fn page_code_spells_no_guarded_name_as_a_string() {
    assert_eq!(
        guarded_literal_hits(&page_parts()),
        Vec::<(String, String)>::new()
    );
}

#[test]
fn the_guarded_literal_scan_flags_bracket_and_alias_spellings() {
    let flagged = [
        (
            "bracket replaceChildren",
            "function p(b) { b['replaceChildren'](x); }",
            "replaceChildren",
        ),
        (
            "double-quoted key",
            "function p(b) { b[\"replaceChildren\"](); }",
            "replaceChildren",
        ),
        (
            "window library",
            "function p() { return window['Idiomorph']; }",
            "Idiomorph",
        ),
        (
            "alias by name",
            "var k = 'Idiomorph'; function p() { return self[k]; }",
            "Idiomorph",
        ),
        (
            "template key",
            "function p(b) { b[`replaceChildren`](); }",
            "replaceChildren",
        ),
        (
            "document write",
            "function p(s) { document['write'](s); }",
            "write",
        ),
        (
            "library member",
            "function p(l, a, b) { l['morph'](a, b); }",
            "morph",
        ),
        (
            "chained key",
            "function p(b) { b.children[0]['replaceChildren'](); }",
            "replaceChildren",
        ),
    ];
    for (label, src, literal) in flagged {
        let parts = [("dash_62_synthetic.js", src)];

        assert_eq!(
            guarded_literal_hits(&parts),
            vec![("dash_62_synthetic.js".to_string(), literal.to_string())],
            "{label} must be flagged"
        );
    }
    let accepted = [
        ("label array", "var rows = [['read', 1], ['write', 2]];"),
        ("label text", "function p() { return 'write'; }"),
        ("prose word", "var t = 'replace children of Idiomorph';"),
        (
            "comment",
            "// b['replaceChildren']() in prose only\nvar a = 1;",
        ),
        ("substituted template", "var t = `${'x'}Idiomorph`;"),
    ];
    for (label, src) in accepted {
        let parts = [("dash_62_synthetic.js", src)];

        assert!(guarded_literal_hits(&parts).is_empty(), "{label} must pass");
    }
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
        .find("Idiomorph.morph(parent, node, {")
        .expect("morphPanel morphs the parent's children to the node");

    assert!(
        throw.is_some_and(|at| at < call),
        "the non-node check must throw before the morph call"
    );
    assert!(body[call..].contains(MORPH_STYLE_OPTION));
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

/// A disclosure the operator opened stays open through a refresh: the fresh
/// node is always built closed, and the morph would otherwise drop `open`.
/// The morph's one callback carries the open state before it decides whether
/// to hold a selected unit, so a held unit's disclosures keep their state too.
///
/// Mutation check: drop `keepOpenState(oldNode, freshNode);` from morphGate -> red.
#[test]
fn morph_panel_keeps_a_kept_disclosure_open() {
    let body = compact(&function_body("morphPanel"));
    let gate = compact(&function_body("morphGate"));
    let keep = compact(&function_body("keepOpenState"));

    assert!(
        body.contains("callbacks:{beforenodemorphed:morphgate(held)}"),
        "morphPanel must hand its composite callback to the morph"
    );
    assert!(
        gate.starts_with(
            "returnfunction(oldnode,freshnode){keepopenstate(oldnode,freshnode);return"
        ),
        "the callback carries the open state first, then decides"
    );
    assert!(
        keep.contains(
            "if(oldnodeinstanceofhtmldetailselement&&freshnodeinstanceofhtmldetailselement)\
             {freshnode.open=oldnode.open;}"
        ),
        "the callback copies the kept disclosure's live state onto the fresh one"
    );
}

#[test]
fn the_html_sink_scan_flags_the_newer_parsing_apis() {
    let cases = [
        ("setHTMLUnsafe", "function b(n, s) { n.setHTMLUnsafe(s); }"),
        (
            "parseHTMLUnsafe",
            "function b(s) { return Document.parseHTMLUnsafe(s); }",
        ),
        ("srcdoc", "function b(f, s) { f.srcdoc = s; }"),
    ];
    for (sink, src) in cases {
        let parts = [("dash_62_synthetic.js", src)];

        assert_eq!(
            html_sink_hits(&parts),
            vec![("dash_62_synthetic.js".to_string(), sink.to_string())],
            "`{sink}` must be flagged"
        );
    }
}
