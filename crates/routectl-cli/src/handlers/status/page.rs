//! The embedded-asset dashboard page.
//!
//! Serves a single self-contained HTML document at `GET /`. The document is
//! ASSEMBLED AT COMPILE TIME from its authoring sources -- `dashboard.html`
//! (markup), the [`STYLE_PARTS`] style sources, and the [`SCRIPT_PARTS`] script
//! sources -- each embedded via `include_str!` and spliced into the markup's
//! two asset slots as one inline `<style>` and one inline `<script>`. The split
//! is an authoring convenience only: the runtime artifact is still ONE offline
//! page with zero external requests (asserted by the
//! `assembled_page_has_no_external_refs` test),
//! and the whole assembly is a `const`, so the served bytes are still static
//! and there is no per-request work.
//!
//! The response is static bytes only: this handler carries no state and
//! imports nothing mutating, so it is structurally read-only, and the
//! forbidden-import scan in [`super`] covers this source alongside the panels.
//!
//! [`page_router`] is a stateless [`AxumRouter<()>`]. It is merged into the
//! serve process under the same `Host` allowlist as the `/status` JSON, and
//! under the same listener auth gate (applied whenever tokens are configured
//! or the bind is non-loopback), but deliberately OUTSIDE the JSON load-shed
//! budget (see [`crate::server`] wiring): a zero-I/O `&'static str` response
//! cannot stall or hold a shed permit, so an overload sheds status DATA while
//! the page shell still loads.

use axum::Router as AxumRouter;
use axum::http::header;
use axum::response::Html;
use axum::routing::get;

/// The dashboard's sources. Only [`PAGE`] is ever served; these are the
/// authoring inputs the tests read directly (a scan of the JS is a scan of the
/// script, with none of the markup around it).
///
/// The style and script bodies may each be authored as SEVERAL files:
/// `STYLE_PARTS` and `SCRIPT_PARTS` hold them in concatenation order and are
/// each spliced into [`PARTS`] as one run, so the served page still carries a
/// single `<style>` and a single `<script>` block. For the style that order IS
/// the cascade. Guards must scan the whole concatenation (`tests::style`,
/// `tests::script`), never one part -- a guard reading one element would
/// silently stop covering the others.
const MARKUP: &str = include_str!("dashboard.html");
const STYLE_PARTS: &[&str] = &[
    include_str!("dash_base.css"),
    include_str!("dash_components.css"),
    include_str!("dash_tabs.css"),
];
const SCRIPT_PARTS: &[&str] = &[
    include_str!("dash_00_state.js"),
    include_str!("dash_05_idiomorph.js"),
    include_str!("dash_10_format.js"),
    include_str!("dash_20_query_vocab.js"),
    include_str!("dash_30_transport.js"),
    include_str!("dash_40_render.js"),
    include_str!("dash_50_dom.js"),
    include_str!("dash_60_tab_overview.js"),
    include_str!("dash_61_view_traffic.js"),
    include_str!("dash_62_view_accounts.js"),
    include_str!("dash_63_view_models.js"),
    include_str!("dash_72_tab_config.js"),
    include_str!("dash_73_tab_doctor.js"),
    include_str!("dash_80_views.js"),
    include_str!("dash_90_chrome.js"),
];

/// The markup's asset slots. Each appears EXACTLY once and is replaced by the
/// corresponding inline block; a missing slot is a compile error, not a
/// silently style-less page.
const STYLE_SLOT: &str = "@@DASHBOARD_STYLE@@";
const SCRIPT_SLOT: &str = "@@DASHBOARD_SCRIPT@@";

/// Byte offset of `needle` in `haystack`, panicking at COMPILE time when it is
/// absent or repeated. A slot that moved or got duplicated must break the
/// build rather than produce a half-assembled page.
const fn slot_offset(haystack: &str, needle: &str) -> usize {
    let (hay, need) = (haystack.as_bytes(), needle.as_bytes());
    let mut found = usize::MAX;
    let mut i = 0;
    while i + need.len() <= hay.len() {
        let mut j = 0;
        while j < need.len() && hay[i + j] == need[j] {
            j += 1;
        }
        if j == need.len() {
            assert!(found == usize::MAX, "dashboard asset slot appears twice");
            found = i;
        }
        i += 1;
    }
    assert!(found != usize::MAX, "dashboard asset slot is missing");
    found
}

/// `s[from..to]` as a `&str`, in const context.
const fn slice(s: &str, from: usize, to: usize) -> &str {
    let (_, rest) = s.as_bytes().split_at(from);
    let (mid, _) = rest.split_at(to - from);
    match core::str::from_utf8(mid) {
        Ok(text) => text,
        Err(_) => panic!("dashboard markup slot lands mid-character"),
    }
}

const STYLE_AT: usize = slot_offset(MARKUP, STYLE_SLOT);
const SCRIPT_AT: usize = slot_offset(MARKUP, SCRIPT_SLOT);

/// The three markup segments around the two slots. The style slot must precede
/// the script slot (style in `<head>`, script at the end of `<body>`).
const HEAD: &str = slice(MARKUP, 0, STYLE_AT);
const BETWEEN: &str = slice(MARKUP, STYLE_AT + STYLE_SLOT.len(), SCRIPT_AT);
const TAIL: &str = slice(MARKUP, SCRIPT_AT + SCRIPT_SLOT.len(), MARKUP.len());

/// The assembled document in render order. Each entry is a GROUP of adjacent
/// fragments, so a multi-file style or script body splices in as one run
/// without the order of the surrounding markup being stated anywhere else.
const PARTS: &[&[&str]] = &[
    &[HEAD],
    &["<style>\n"],
    STYLE_PARTS,
    &["</style>"],
    &[BETWEEN],
    &["<script>\n"],
    SCRIPT_PARTS,
    &["</script>"],
    &[TAIL],
];

const fn joined_len(groups: &[&[&str]]) -> usize {
    let mut total = 0;
    let mut g = 0;
    while g < groups.len() {
        let group = groups[g];
        let mut i = 0;
        while i < group.len() {
            total += group[i].len();
            i += 1;
        }
        g += 1;
    }
    total
}

const PAGE_LEN: usize = joined_len(PARTS);

const fn join<const N: usize>(groups: &[&[&str]]) -> [u8; N] {
    let mut out = [0u8; N];
    let mut written = 0;
    let mut g = 0;
    while g < groups.len() {
        let group = groups[g];
        let mut i = 0;
        while i < group.len() {
            let part = group[i].as_bytes();
            let mut j = 0;
            while j < part.len() {
                out[written] = part[j];
                written += 1;
                j += 1;
            }
            i += 1;
        }
        g += 1;
    }
    out
}

/// The assembled bytes. A `static` rather than a `const` so the page exists
/// once in the binary instead of being copied into each use site (the array is
/// the whole document, and clippy rejects a const this large for exactly that
/// reason).
static PAGE_BYTES: [u8; PAGE_LEN] = join::<PAGE_LEN>(PARTS);

/// The self-contained dashboard document, assembled at build time.
static PAGE: &str = match core::str::from_utf8(&PAGE_BYTES) {
    Ok(text) => text,
    Err(_) => panic!("assembled dashboard page is not UTF-8"),
};

/// GET-only router serving the dashboard shell at `/`. Stateless
/// ([`AxumRouter<()>`]) so it merges into the state-erased serve router without
/// a `.with_state` call; a non-GET request to `/` gets a 405 from axum's method
/// router.
pub fn page_router() -> AxumRouter<()> {
    AxumRouter::new().route("/", get(serve_page))
}

/// Serve the embedded page with a `Cache-Control: no-store` header so a browser
/// never caches the shell (the panel data it polls is always live).
async fn serve_page() -> ([(header::HeaderName, &'static str); 1], Html<&'static str>) {
    ([(header::CACHE_CONTROL, "no-store")], Html(PAGE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// The whole style body as one string, in cascade order: guards scan THIS,
    /// not a single element of [`STYLE_PARTS`], so a style split across more
    /// sources keeps every guard covering all of it.
    fn style() -> String {
        STYLE_PARTS.concat()
    }

    /// The whole script body as one string: every guard scans THIS, not a
    /// single element of [`SCRIPT_PARTS`], so a script split across more files
    /// keeps every guard covering all of it. Allocating is fine in tests; a
    /// `const &str` cannot span several sources.
    fn script() -> String {
        SCRIPT_PARTS.concat()
    }

    /// Whitespace- and case-normalized view of a source, so `method : 'Post'`
    /// and `method:"POST"` collapse to the same needle. Backticks are kept.
    fn compact(src: &str) -> String {
        src.chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_ascii_lowercase()
    }

    /// Strip `//` and block comments from a JS source, leaving string literals
    /// intact. The positive path/verb scans below are assertions about what the
    /// CODE reaches, so prose that happens to contain a slash or an apostrophe
    /// must not read as a path literal or an unterminated string. Quote-aware
    /// so a `//` inside a string literal is not mistaken for a comment.
    fn strip_js_comments(src: &str) -> String {
        let bytes = src.as_bytes();
        let mut out = String::with_capacity(src.len());
        let mut i = 0;
        let mut quote: Option<u8> = None;
        while i < bytes.len() {
            let c = bytes[i];
            if let Some(q) = quote {
                out.push(c as char);
                if c == b'\\' && i + 1 < bytes.len() {
                    out.push(bytes[i + 1] as char);
                    i += 2;
                    continue;
                }
                if c == q {
                    quote = None;
                }
                i += 1;
                continue;
            }
            if c == b'"' || c == b'\'' || c == b'`' {
                quote = Some(c);
                out.push(c as char);
                i += 1;
                continue;
            }
            if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
                continue;
            }
            out.push(c as char);
            i += 1;
        }
        out
    }

    /// Strip CSS comments, so prose in the `dash_*.css` sources cannot read
    /// as a rule.
    fn strip_css_comments(src: &str) -> String {
        let mut out = String::with_capacity(src.len());
        let mut rest = src;
        while let Some(open) = rest.find("/*") {
            out.push_str(&rest[..open]);
            let after = &rest[open + 2..];
            match after.find("*/") {
                Some(close) => rest = &after[close + 2..],
                None => return out,
            }
        }
        out.push_str(rest);
        out
    }

    /// Strip HTML comments, so the markup's authoring notes are not scanned as
    /// markup.
    fn strip_html_comments(src: &str) -> String {
        let mut out = String::with_capacity(src.len());
        let mut rest = src;
        while let Some(open) = rest.find("<!--") {
            out.push_str(&rest[..open]);
            let after = &rest[open + 4..];
            match after.find("-->") {
                Some(close) => rest = &after[close + 3..],
                None => return out,
            }
        }
        out.push_str(rest);
        out
    }

    /// The page route is GET-only: `GET /` succeeds, every mutating method is
    /// a 405. Mirrors the status-router assertion so a non-GET route added to
    /// the page trips the read-only security floor.
    #[tokio::test]
    async fn get_succeeds_and_non_get_returns_405() {
        let app = page_router();
        let get_resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(get_resp.status(), StatusCode::OK, "GET /");

        for method in ["POST", "PUT", "DELETE", "PATCH"] {
            let app = page_router();
            let resp = app
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri("/")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} / must be 405 (GET-only route)"
            );
        }
    }

    /// Read-only client-surface guard: the dashboard script carries no
    /// mutation channel and reaches no daemon path other than the read-only
    /// status surface. Read-only is a hard milestone invariant, so this pins
    /// it at the client layer.
    ///
    /// The verb check is BOTH directions. The deny-list rejects the mutating
    /// verbs in every spelling (quoted, unquoted, computed property, form
    /// attribute); the positive check then extracts EVERY quoted `method:`
    /// value the script sets and asserts the set is a subset of the one verb
    /// this page is allowed to speak. The positive form is strictly stronger:
    /// a verb nobody thought to deny-list still fails closed.
    #[test]
    fn dashboard_js_carries_no_mutation_channel() {
        let script = compact(&strip_js_comments(&script()));

        for verb in ["post", "put", "delete", "patch"] {
            for quote in ['"', '\'', '`'] {
                let needle = format!("method:{quote}{verb}{quote}");
                assert!(
                    !script.contains(&needle),
                    "dashboard JS must not set a `{needle}` fetch option (read-only)"
                );
            }
            // Unquoted fetch option (`method:post`) and form-post attribute
            // (`method=post`), both after whitespace/quote normalization.
            assert!(
                !script.contains(&format!("method:{verb}")),
                "dashboard JS must not set an unquoted `method:{verb}` (read-only)"
            );
            assert!(
                !script.contains(&format!("method={verb}")),
                "dashboard JS must not carry a `method={verb}` form attribute (read-only)"
            );
        }

        // A computed `method` property (`obj['method'] = 'POST'`) would smuggle
        // a mutating verb past the literal-`method:` checks above.
        for quote in ['"', '\'', '`'] {
            let needle = format!("[{quote}method{quote}]");
            assert!(
                !script.contains(&needle),
                "dashboard JS must not use a computed `method` property (read-only)"
            );
        }

        // Positive verb allowlist. `/status/query` answers the QUERY method
        // and nothing else on this surface carries a verb at all, so the set
        // of `method:` values the script sets must be exactly that or empty.
        let verbs = quoted_method_values(&script);
        for verb in &verbs {
            assert!(
                verb == "query",
                "dashboard JS sets fetch method `{verb}`; only `query` is permitted (read-only)"
            );
        }
        assert!(
            verbs.contains(&"query".to_string()),
            "dashboard JS is expected to issue the QUERY aggregate; the positive \
             method scan found no `method:` at all, so it is no longer guarding anything"
        );

        // Form elements are browser-native mutation affordances -- markup,
        // dynamic construction, and programmatic submission all breach the
        // pure-read surface, so none may appear in EITHER source.
        let markup = compact(&strip_html_comments(MARKUP));
        assert!(
            !markup.contains("<form") && !script.contains("<form"),
            "dashboard must carry no <form> element (read-only)"
        );
        for quote in ['"', '\'', '`'] {
            let needle = format!("createelement({quote}form{quote}");
            assert!(
                !script.contains(&needle),
                "dashboard JS must not construct a form element (read-only)"
            );
        }
        assert!(
            !script.contains(".submit("),
            "dashboard JS must not submit a form (read-only)"
        );
        assert!(
            !script.contains("document.forms"),
            "dashboard JS must not reach the document forms collection (read-only)"
        );

        // Positive path allowlist: every path-like string literal in the
        // script must target the read-only status surface. A GET to `/v1/...`
        // or any other daemon path is still a breach of the read-only
        // contract, so pin the set of reachable paths to the `/status` family.
        for quote in ['"', '\'', '`'] {
            let opener = format!("{quote}/");
            let mut rest = script.as_str();
            while let Some(pos) = rest.find(&opener) {
                let from_slash = &rest[pos + quote.len_utf8()..];
                let close = from_slash[1..]
                    .find(quote)
                    .expect("path literal in dashboard JS is terminated");
                let path = &from_slash[..=close];
                assert!(
                    path.starts_with("/status"),
                    "dashboard JS may only fetch the /status family, found `{path}`"
                );
                rest = &from_slash[close + 1..];
            }
        }
    }

    /// Every quoted value assigned to a `method:` key in a compacted source.
    fn quoted_method_values(compacted: &str) -> Vec<String> {
        let mut found = Vec::new();
        let mut rest = compacted;
        while let Some(pos) = rest.find("method:") {
            rest = &rest[pos + "method:".len()..];
            let mut chars = rest.chars();
            let Some(quote) = chars.next() else { break };
            if quote != '"' && quote != '\'' && quote != '`' {
                continue;
            }
            let body = &rest[quote.len_utf8()..];
            let Some(close) = body.find(quote) else { break };
            found.push(body[..close].to_string());
            rest = &body[close..];
        }
        found
    }

    /// Slice the body of a `var <name> = <open> ... <close>` literal out of the
    /// dashboard script (the whole concatenation, so a declaration in any part
    /// is found).
    ///
    /// Anchored on the DECLARATION, not on the first textual occurrence of the
    /// name: a comment mentioning `QUERY_METRICS = [...]` precedes the real
    /// `var QUERY_METRICS =` in this file, and matching it would let the drift
    /// tests below validate prose while the live declaration drifted freely.
    /// An absent declaration still fails.
    fn literal_body(name: &str, open: char, close: char) -> String {
        let script = script();
        let decl_needle = format!("var {name} =");
        let decl = script
            .find(&decl_needle)
            .unwrap_or_else(|| panic!("dashboard JS declares `{decl_needle}`"));
        let decl = decl + decl_needle.len();
        let start = script[decl..]
            .find(open)
            .unwrap_or_else(|| panic!("{name} has a literal body"))
            + decl;
        let end = script[start..]
            .find(close)
            .unwrap_or_else(|| panic!("{name} literal is closed"))
            + start;
        script[start + 1..end].to_string()
    }

    /// Drift guard between client and server: the dashboard's `EXPECTED`
    /// schema-version map (the per-source wire versions the JS renders
    /// against) must equal the Rust `SCHEMA_VERSION` consts exactly. They ship
    /// in the same binary, so a mismatch here would silently degrade a LIVE
    /// source to the client's "incompatible" fallback. `query` is in the map
    /// alongside the four GET panels because the QUERY aggregate is a source
    /// of its own with its own version.
    #[test]
    fn dashboard_expected_map_matches_panel_schema_versions() {
        let body = literal_body("EXPECTED", '{', '}');

        // Parse the `key: value` pairs (keys may be bare or quoted).
        let mut parsed: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
        for entry in body.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let (key, val) = entry
                .split_once(':')
                .expect("EXPECTED entry is `key: value`");
            let key = key
                .trim()
                .trim_matches(|c| c == '\'' || c == '"')
                .to_string();
            let val: u32 = val.trim().parse().expect("EXPECTED value is a u32");
            parsed.insert(key, val);
        }

        let expected: std::collections::BTreeMap<String, u32> = [
            (
                "health".to_string(),
                crate::handlers::status::health::SCHEMA_VERSION,
            ),
            (
                "config".to_string(),
                crate::handlers::status::config::SCHEMA_VERSION,
            ),
            (
                "doctor".to_string(),
                crate::handlers::status::doctor::DOCTOR_SCHEMA_VERSION,
            ),
            (
                "query".to_string(),
                crate::handlers::status::query::SCHEMA_VERSION,
            ),
            (
                "accounts".to_string(),
                crate::handlers::status::accounts::SCHEMA_VERSION,
            ),
            (
                "attention".to_string(),
                crate::handlers::status::attention::SCHEMA_VERSION,
            ),
            (
                "models".to_string(),
                crate::handlers::status::models::SCHEMA_VERSION,
            ),
        ]
        .into_iter()
        .collect();

        assert_eq!(
            parsed, expected,
            "dashboard EXPECTED map must match the panel SCHEMA_VERSION consts"
        );
    }

    /// Drift guard on the query VOCABULARY, both halves of it.
    ///
    /// `QUERY_METRICS` + `QUERY_TOKENS` are the only places a raw query field
    /// name lives in the dashboard, so a server-side rename would otherwise
    /// fail every query payload's shape check (each listed field is required)
    /// or carry a cost token as `undefined`. Both
    /// arrays are checked, so a field moved between them stays covered. The
    /// Rust side is DERIVED from serde -- a serialized `QueryMetrics` value --
    /// rather than a second hardcoded list, so this test cannot rot into
    /// agreeing with itself. Subset, not equality: the dashboard is free to read
    /// only part of the vocabulary.
    ///
    /// `QUERY_SHAPES` is the COMPLETE set of request bodies the dashboard can
    /// issue: the client copies an entry verbatim rather than patching fields
    /// at runtime, so validating every entry through the route's own parser
    /// validates every request that can leave the page. Completeness is checked
    /// too -- every selectable window must carry a shape for every
    /// (group_by, series-mode) pair any window declares, and no pair may be
    /// declared twice for one window -- so the client cannot resolve a
    /// selection to a shape this test never saw. A body may also carry the
    /// selection's `provider` and `model` scopes (bodyFromShape adds them on
    /// top of the shape), so every shape is validated with both added too.
    #[test]
    fn dashboard_metric_tokens_are_in_the_query_vocabulary() {
        let vocabulary = serde_json::to_value(routectl_usage::QueryMetrics::default())
            .expect("QueryMetrics serializes");
        let vocabulary = vocabulary
            .as_object()
            .expect("QueryMetrics serializes to an object");

        for name in [
            "QUERY_METRICS",
            "QUERY_TOKENS",
            "QUERY_MISSING_PRICE_LISTS",
            "QUERY_COUNT_MAPS",
        ] {
            let tokens = string_array(name);
            assert!(
                !tokens.is_empty(),
                "{name} must not be empty (it is part of the dashboard's query field vocabulary)"
            );
            for token in &tokens {
                assert!(
                    vocabulary.contains_key(token.as_str()),
                    "dashboard query field `{token}` (in {name}) is not a field of the \
                     server's QueryMetrics"
                );
            }
        }

        // Every declared request shape must parse as a valid query body. The
        // shapes are written as strict JSON in the dashboard script sources
        // precisely so they can be fed to the server's parser verbatim.
        let now = time::OffsetDateTime::now_utc();
        let shapes = json_object_literals(&literal_body("QUERY_SHAPES", '[', ']'));
        assert!(
            !shapes.is_empty(),
            "QUERY_SHAPES must declare at least one request body"
        );
        // The keys bodyFromShape writes on top of the copied shape: each one
        // is added to every shape below, so a key the server does not know
        // fails its parser here.
        let body_from_shape = compact(&function_body("bodyFromShape"));
        assert!(
            body_from_shape.contains("body[k]=shape[k]"),
            "bodyFromShape no longer copies the shape; the key scan below sees nothing of it"
        );
        let added: std::collections::BTreeSet<&str> = body_from_shape
            .match_indices("body.")
            .map(|(at, _)| {
                let rest = &body_from_shape[at + "body.".len()..];
                &rest[..rest.find('=').unwrap_or(rest.len())]
            })
            .collect();
        assert_eq!(
            added,
            ["model", "provider"].into_iter().collect(),
            "bodyFromShape adds the provider and model scopes"
        );
        for shape in &shapes {
            assert!(
                crate::handlers::status::query::spec_from_body(
                    shape.as_bytes(),
                    &crate::local_time::LocalZone::utc(),
                    now
                )
                .is_ok(),
                "dashboard query shape `{shape}` is not in the server's request vocabulary"
            );
            let mut scoped: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(shape).expect("a shape is a JSON object");
            for key in &added {
                scoped.insert((*key).to_owned(), "x".into());
            }
            let scoped = serde_json::to_vec(&scoped).expect("a scoped body serializes");
            assert!(
                crate::handlers::status::query::spec_from_body(
                    &scoped,
                    &crate::local_time::LocalZone::utc(),
                    now
                )
                .is_ok(),
                "dashboard query shape `{shape}` with the scopes bodyFromShape adds ({added:?}) \
                 is not in the server's request vocabulary"
            );
        }

        // Completeness: the window is chosen independently of the tab, so every
        // selectable window needs every (group_by, series-mode) pair the page
        // asks for anywhere. A missing cell would leave the client resolving a
        // live selection to no shape at all.
        let mut declared: std::collections::BTreeSet<(String, String, bool)> =
            std::collections::BTreeSet::new();
        for shape in &shapes {
            let parsed: std::collections::BTreeMap<String, String> =
                serde_json::from_str(shape).expect("QUERY_SHAPES entry is a flat JSON object");
            let window = parsed
                .get("window")
                .expect("query shape declares a window")
                .clone();
            let group_by = parsed
                .get("group_by")
                .expect("query shape declares a group_by")
                .clone();
            let bucketed = parsed.contains_key("bucket");
            assert!(
                declared.insert((window.clone(), group_by.clone(), bucketed)),
                "QUERY_SHAPES declares `{window}`/`{group_by}` twice for the same series mode; \
                 the client would resolve one selection to two bodies"
            );
        }
        let pairs: std::collections::BTreeSet<(String, bool)> = declared
            .iter()
            .map(|(_, group_by, bucketed)| (group_by.clone(), *bucketed))
            .collect();
        for window in string_array("WINDOWS") {
            for (group_by, bucketed) in &pairs {
                assert!(
                    declared.contains(&(window.clone(), group_by.clone(), *bucketed)),
                    "QUERY_SHAPES has no `{window}` shape for `{group_by}` \
                     (series: {bucketed}), but the page can select that combination"
                );
            }
        }
    }

    /// The fields the page checks on each `missing_prices` entry are exactly
    /// the keys a serialized `MissingPrice` carries, split by whether the
    /// value may be null: derived from serde, so a renamed or added entry
    /// field fails here instead of failing every live payload's shape check.
    #[test]
    fn missing_price_entry_fields_are_the_dto_fields() {
        let entry = serde_json::to_value(routectl_usage::MissingPrice {
            model: "m".to_string(),
            dimension: "d".to_string(),
            config_key: None,
        })
        .expect("MissingPrice serializes");
        let entry = entry.as_object().expect("MissingPrice is an object");
        let server_nullable: std::collections::BTreeSet<String> = entry
            .iter()
            .filter(|(_, value)| value.is_null())
            .map(|(key, _)| key.clone())
            .collect();
        let server_text: std::collections::BTreeSet<String> = entry
            .iter()
            .filter(|(_, value)| value.is_string())
            .map(|(key, _)| key.clone())
            .collect();

        let page_text: std::collections::BTreeSet<String> =
            string_array("MISSING_PRICE_TEXT").into_iter().collect();
        let page_nullable: std::collections::BTreeSet<String> =
            string_array("MISSING_PRICE_NULLABLE_TEXT")
                .into_iter()
                .collect();

        assert_eq!(server_text.len() + server_nullable.len(), entry.len());
        assert_eq!(page_text, server_text);
        assert_eq!(page_nullable, server_nullable);
    }

    /// The `[token, label]` pairs of the page's `GROUP_BYS`, tokens only, in
    /// declaration order. One pair per line, as the page writes it.
    fn group_by_tokens() -> Vec<String> {
        let script = script();
        let needle = "var GROUP_BYS = [";
        let start = script
            .find(needle)
            .expect("dashboard JS declares `var GROUP_BYS = [`")
            + needle.len();
        let end = script[start..]
            .find("\n  ];")
            .expect("GROUP_BYS literal is closed")
            + start;
        script[start..end]
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| {
                let pair = line
                    .trim_end_matches(',')
                    .strip_prefix('[')
                    .and_then(|rest| rest.strip_suffix(']'))
                    .unwrap_or_else(|| panic!("GROUP_BYS line `{line}` is not one pair"));
                let token = pair.split(',').next().unwrap_or_default();
                token.trim().trim_matches('\'').to_string()
            })
            .collect()
    }

    /// The page's groupings are the server's: the same tokens in the same
    /// order as `GroupDim::ALL`, so the picker, the URL codec and the
    /// transport offer every grouping the query route answers and nothing
    /// else.
    ///
    /// Mutation check: drop the `seat` pair from GROUP_BYS -> red.
    #[test]
    fn group_bys_are_the_server_groupings_in_order() {
        let server: Vec<String> = routectl_usage::GroupDim::ALL
            .iter()
            .map(|dim| dim.as_str().to_string())
            .collect();

        let page = group_by_tokens();

        assert_eq!(page, server);
    }

    /// The string entries of a `var <name> = [ ... ]` array literal in the
    /// dashboard script.
    fn string_array(name: &str) -> Vec<String> {
        literal_body(name, '[', ']')
            .split(',')
            .map(|entry| {
                entry
                    .trim()
                    .trim_matches(|c| c == '\'' || c == '"')
                    .to_string()
            })
            .filter(|entry| !entry.is_empty())
            .collect()
    }

    /// Each `{...}` object literal in an array body, as its own JSON text.
    fn json_object_literals(body: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = body;
        while let Some(open) = rest.find('{') {
            let close = rest[open..].find('}').expect("shape entry is closed") + open;
            out.push(rest[open..=close].to_string());
            rest = &rest[close + 1..];
        }
        out
    }

    /// Whether `c` may appear in a JS identifier (ASCII subset: the dashboard
    /// sources are ASCII-only).
    fn is_ident_char(c: char) -> bool {
        c.is_ascii_alphanumeric() || c == '_' || c == '$'
    }

    /// The identifier or keyword starting at `chars[at]`, empty when that
    /// position does not begin one.
    fn read_word(chars: &[char], at: usize) -> String {
        chars[at.min(chars.len())..]
            .iter()
            .take_while(|c| is_ident_char(**c))
            .collect()
    }

    /// Index of the first non-whitespace character at or after `at`.
    fn skip_whitespace(chars: &[char], at: usize) -> usize {
        let mut i = at;
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        i
    }

    /// Index just past the string literal opened at `chars[at]`, honoring
    /// backslash escapes.
    fn skip_string_literal(chars: &[char], at: usize) -> usize {
        let quote = chars[at];
        let mut i = at + 1;
        while i < chars.len() {
            if chars[i] == '\\' {
                i += 2;
                continue;
            }
            if chars[i] == quote {
                return i + 1;
            }
            i += 1;
        }
        i
    }

    /// Every top-level declaration in a script concatenation, paired with the
    /// part that declares it.
    ///
    /// The parts share ONE function scope: `page.rs` splices them into a single
    /// `<script>` inside one IIFE, so a declaration is visible to every part and
    /// two parts declaring the same name is a redeclaration, not two locals.
    ///
    /// Scope is read from BRACE DEPTH carried across the concatenation -- the
    /// IIFE body is depth 1 -- not from indentation, so a reindented part is
    /// still scanned and a nested declaration is still recognized as genuinely
    /// local. Parenthesis nesting is tracked relative to the innermost brace, so
    /// the IIFE's own unclosed `(` does not hide its body's statements.
    ///
    /// Every declaration form that can appear at that level is recognized:
    /// `function`, `async function`, generator `function*`, and `var`/`let`/
    /// `const` with any number of declarators. A declaring form the scan cannot
    /// classify (a class, a module declaration, a destructuring pattern) PANICS
    /// rather than being skipped: an unrecognized declaration is an unguarded
    /// top-level name, which is the exact hazard this scan exists to catch, so
    /// it fails the build instead of passing silently.
    fn top_level_declarations(parts: &[&str]) -> Vec<(String, usize)> {
        const UNSUPPORTED: [&str; 3] = ["class", "import", "export"];

        let mut found = Vec::new();
        let mut saved_nesting: Vec<i32> = Vec::new();
        let mut nesting = 0i32;
        let mut prev: Option<char> = None;
        for (part, src) in parts.iter().enumerate() {
            let stripped = strip_js_comments(src);
            let chars: Vec<char> = stripped.chars().collect();
            let mut i = 0;
            while i < chars.len() {
                let c = chars[i];
                if c.is_whitespace() {
                    i += 1;
                    continue;
                }
                if c == '\'' || c == '"' || c == '`' {
                    i = skip_string_literal(&chars, i);
                    prev = Some(c);
                    continue;
                }
                let at_statement = saved_nesting.len() == 1
                    && nesting == 0
                    && matches!(prev, None | Some('{' | '}' | ';'));
                if at_statement && is_ident_char(c) && !c.is_ascii_digit() {
                    let word = read_word(&chars, i);
                    match word.as_str() {
                        "function" | "async" => {
                            let mut j = i + word.len();
                            if word == "async" {
                                j = skip_whitespace(&chars, j);
                                let next = read_word(&chars, j);
                                assert_eq!(
                                    next, "function",
                                    "the dashboard script has a top-level `async {next}`, which \
                                     the declaration scan cannot classify; an unrecognized \
                                     declaration would leave its name unguarded against collision"
                                );
                                j += next.len();
                            }
                            j = skip_whitespace(&chars, j);
                            if chars.get(j) == Some(&'*') {
                                j = skip_whitespace(&chars, j + 1);
                            }
                            let name = read_word(&chars, j);
                            assert!(
                                !name.is_empty(),
                                "the dashboard script has an unnamed top-level `{word}` \
                                 declaration, which is not valid as a statement"
                            );
                            i = j + name.len();
                            prev = name.chars().last();
                            found.push((name, part));
                            continue;
                        }
                        "var" | "let" | "const" => {
                            let mut j = i + word.len();
                            loop {
                                j = skip_whitespace(&chars, j);
                                let name = read_word(&chars, j);
                                assert!(
                                    !name.is_empty(),
                                    "the dashboard script has a top-level `{word}` declarator the \
                                     scan cannot classify (a destructuring pattern binds names it \
                                     would not see); an unrecognized declaration would leave its \
                                     names unguarded against collision"
                                );
                                j += name.len();
                                found.push((name, part));
                                // Skip this declarator's initializer, which may
                                // carry balanced brackets and strings of its own;
                                // only a separator at the declaration's own level
                                // ends it.
                                let mut inner = 0i32;
                                let mut ended = false;
                                while j < chars.len() {
                                    let d = chars[j];
                                    if d == '\'' || d == '"' || d == '`' {
                                        j = skip_string_literal(&chars, j);
                                        continue;
                                    }
                                    match d {
                                        '(' | '[' | '{' => inner += 1,
                                        ')' | ']' | '}' => inner -= 1,
                                        ',' if inner == 0 => {
                                            j += 1;
                                            break;
                                        }
                                        ';' if inner == 0 => {
                                            j += 1;
                                            ended = true;
                                            break;
                                        }
                                        _ => {}
                                    }
                                    j += 1;
                                }
                                if ended || j >= chars.len() {
                                    break;
                                }
                            }
                            i = j;
                            prev = Some(';');
                            continue;
                        }
                        other if UNSUPPORTED.contains(&other) => panic!(
                            "the dashboard script has a top-level `{other}` declaration, which \
                             the declaration scan cannot classify; an unrecognized declaration \
                             would leave its name unguarded against collision"
                        ),
                        _ => {
                            // An expression or control statement: no name enters
                            // the shared scope, so skip the whole word rather
                            // than re-testing each of its characters.
                            i += word.len();
                            prev = word.chars().last();
                            continue;
                        }
                    }
                }
                match c {
                    '{' => {
                        saved_nesting.push(nesting);
                        nesting = 0;
                    }
                    '}' => nesting = saved_nesting.pop().unwrap_or(0),
                    '(' | '[' => nesting += 1,
                    ')' | ']' => nesting -= 1,
                    _ => {}
                }
                prev = Some(c);
                i += 1;
            }
        }
        found
    }

    /// The first top-level name declared twice in a script concatenation, with
    /// the parts that declare it.
    fn first_colliding_declaration(parts: &[&str]) -> Option<(String, usize, usize)> {
        let mut first_seen: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for (name, part) in top_level_declarations(parts) {
            if let Some(earlier) = first_seen.insert(name.clone(), part) {
                return Some((name, earlier, part));
            }
        }
        None
    }

    /// The dashboard parts share one function scope, so no two of them may
    /// declare the same top-level name.
    ///
    /// A collision is SILENT and total: hoisting makes the last `function`
    /// declaration win for every call site in every part, so one tab's builder
    /// starts running another tab's -- which throws on the payload it is handed
    /// and degrades that tab to its `invalid_payload` containment card forever,
    /// with a healthy transport and a well-formed payload. Nothing else on this
    /// page reports it: the source is `live`, the fetch is right, and only the
    /// rendered section is wrong. That makes this the one structural hazard of
    /// the multi-part authoring split worth a mechanical guard.
    ///
    /// That the scan still RECOGNIZES each declaration form is a separate
    /// assertion (`top_level_declaration_scan_recognizes_every_declaration_form`),
    /// per form, against synthetic sources.
    #[test]
    fn dashboard_script_parts_declare_no_colliding_top_level_names() {
        if let Some((name, first, second)) = first_colliding_declaration(SCRIPT_PARTS) {
            panic!(
                "the dashboard script declares a top-level `{name}` in both script part {first} \
                 and script part {second}; the parts share ONE function scope, so the later \
                 declaration silently replaces the earlier one for every caller of both"
            );
        }
    }

    /// A script part body wrapped the way `page.rs` assembles the real ones, so
    /// a synthetic declaration sits at the IIFE body's own depth.
    fn synthetic_script(body: &str) -> String {
        format!("'use strict';\n(function () {{\n{body}\n}})();\n")
    }

    /// Whether the declaration scan REFUSES a source rather than skipping past
    /// a form it cannot classify. The panic message is the assertion's payload,
    /// so the hook is silenced to keep a passing run's output clean.
    fn declaration_scan_refuses(src: &str) -> bool {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome = std::panic::catch_unwind(|| top_level_declarations(&[src]));
        std::panic::set_hook(previous);
        outcome.is_err()
    }

    /// Recognition guard on the declaration scan itself, one case per FORM.
    ///
    /// The collision test above is only as strong as what the scan sees, and a
    /// count-based self-check cannot establish that: the ~300 ordinary `function`
    /// declarations satisfy any plausible threshold on their own, so a scan that
    /// silently stopped recognizing every OTHER form would still look healthy.
    /// Each supported form is therefore fed in twice and must be reported as a
    /// collision, with a paired negative for the scope rule (nested same-name
    /// declarations ARE locals) and a paired refusal for the forms the scan
    /// cannot classify.
    #[test]
    fn top_level_declaration_scan_recognizes_every_declaration_form() {
        // Every body but the last is written at the two-space indentation the
        // real parts use, so a scan re-anchored on indentation fails ONLY the
        // indentation case and each other case indicts its own form alone.
        for (form, body) in [
            (
                "async function",
                "  async function dup() {}\n  async function dup() {}",
            ),
            (
                "generator function",
                "  function* dup() {}\n  function *dup() {}",
            ),
            (
                "the later declarator of a multi-declarator var",
                "  var ignored = 0, dup = function () { return { a: 1, b: 2 }; };\n\
                 \x20 function dup() {}",
            ),
            ("let", "  let dup = 1;\n  let dup = 2;"),
            ("const", "  const dup = 1;\n  const dup = 2;"),
            (
                "a declaration at non-standard indentation",
                "\tfunction dup() {}\n        function dup() {}",
            ),
        ] {
            let src = synthetic_script(body);
            let collision = first_colliding_declaration(&[src.as_str()]);
            assert_eq!(
                collision.map(|(name, ..)| name),
                Some("dup".to_string()),
                "the declaration scan does not recognize {form}, so a collision in that form \
                 would pass the dashboard collision guard unnoticed"
            );
        }

        // Scope, the other half of the recognition contract: same-name
        // declarations inside separate BLOCKS are locals, not a collision.
        let nested =
            synthetic_script("  if (1) { function dup() {} }\n  if (2) { function dup() {} }");
        assert_eq!(
            first_colliding_declaration(&[nested.as_str()]),
            None,
            "the declaration scan reports nested declarations as a top-level collision; it \
             would fail the build on code that is correct"
        );

        for unsupported in ["  class Dup {}", "  var { dup } = obj;", "  async dup = 1;"] {
            let src = synthetic_script(unsupported);
            assert!(
                declaration_scan_refuses(&src),
                "the declaration scan silently skips `{}`, so the name it binds \
                 enters the shared scope unguarded",
                unsupported.trim()
            );
        }
    }

    /// One registered QUERY read as written in `QUERY_SOURCE_DEFS`: the
    /// identifier its `name` is written as, the source key that identifier is
    /// declared to, and the compacted body of its `shape` function.
    struct QuerySourceDef {
        ident: String,
        key: String,
        shape: String,
    }

    /// The `QUERY_SOURCE_DEFS` registry, in order, each name resolved through
    /// its own `var <IDENT> = '<key>'` declaration.
    fn query_source_defs() -> Vec<QuerySourceDef> {
        let script = script();
        let code = strip_js_comments(&script);
        let at = code
            .find("var QUERY_SOURCE_DEFS = [")
            .expect("the dashboard script declares `var QUERY_SOURCE_DEFS = [`");
        let chars: Vec<char> = code[at..].chars().collect();
        let open = chars
            .iter()
            .position(|c| *c == '[')
            .expect("QUERY_SOURCE_DEFS body");
        let end = views::balanced_end(&chars, open);
        views::top_level_objects(&chars[open + 1..end - 1])
            .iter()
            .map(|object| {
                let ident = views::field_text(object, "name")
                    .expect("every query source declares a `name`");
                let shape = views::field_text(object, "shape")
                    .expect("every query source declares a `shape`");
                let needle = format!("var {ident} =");
                let decl = script
                    .find(&needle)
                    .unwrap_or_else(|| panic!("dashboard JS declares `{needle}`"));
                let key = views::unquote(
                    script[decl + needle.len()..]
                        .split(';')
                        .next()
                        .expect("declaration is terminated"),
                );
                QuerySourceDef {
                    ident,
                    key,
                    shape: compact(&shape),
                }
            })
            .collect()
    }

    /// The registry entry whose name is written as `ident`.
    fn query_source_def(ident: &str) -> QuerySourceDef {
        query_source_defs()
            .into_iter()
            .find(|def| def.ident == ident)
            .unwrap_or_else(|| panic!("QUERY_SOURCE_DEFS registers `{ident}`"))
    }

    /// Drift guard on the per-shape QUERY source split and its one registry.
    ///
    /// Now reads `/status/query` bucketed, Traffic reads it series-less, the
    /// per-seat read groups it by seat and the per-model read asks for the
    /// last seven days by model; the payloads are not interchangeable, since
    /// Now's sparklines cannot be drawn from a payload whose `series` is null.
    /// They therefore get their OWN source records, all registered in
    /// `QUERY_SOURCE_DEFS`, and this pins both halves of the split: the
    /// records EXIST (each declared, distinct, and every membership list --
    /// QUERY_SOURCES, SOURCE_PANEL, FETCH_SOURCES, shapeFor -- derived from the
    /// registry rather than written by hand), and the transport is WIRED to
    /// them (each view routed to its own record through its registered query
    /// source, each record asking for its own shape, and the outcome written
    /// to the record the round was issued for rather than to a hardcoded
    /// one).
    ///
    /// Mutation check: point Now's QUERY source in VIEWS at `QUERY_SOURCE`
    /// -> red.
    #[test]
    fn dashboard_query_shapes_have_their_own_source_records() {
        let script = script();
        let code = compact(&strip_js_comments(&script));
        let sources = string_array("GET_SOURCES");
        let defs = query_source_defs();

        let idents: Vec<&str> = defs.iter().map(|def| def.ident.as_str()).collect();
        assert_eq!(
            idents,
            [
                "QUERY_SOURCE",
                "QUERY_SERIES_SOURCE",
                "QUERY_SEATS_SOURCE",
                "QUERY_MODELS_SOURCE"
            ],
            "QUERY_SOURCE_DEFS registers every QUERY read the views name"
        );
        for def in &defs {
            assert!(
                !sources.contains(&def.key),
                "`{}` names `{}`, which is already a GET source; a QUERY read sharing a GET \
                 panel's record would let each overwrite the other",
                def.ident,
                def.key
            );
        }
        let keys: std::collections::BTreeSet<&String> = defs.iter().map(|def| &def.key).collect();
        assert_eq!(
            keys.len(),
            defs.len(),
            "the QUERY reads must name DIFFERENT source records, or whichever view polled \
             last defines what another renders from"
        );
        assert_eq!(
            defs[0].key, "query",
            "the series-less QUERY source must BE the query panel's own name"
        );

        // DERIVED: every membership list comes from the registry, so a read
        // registered once is a record, an epoch and a query-panel envelope,
        // and no hand-written list can leave one out.
        for (what, derivation) in [
            (
                "QUERY_SOURCES",
                "varquery_sources=query_source_defs.map(function(def){returndef.name;});",
            ),
            (
                "SOURCE_PANEL",
                "query_sources.foreach(function(name){source_panel[name]='query';});",
            ),
            (
                "FETCH_SOURCES",
                "varfetch_sources=[aggregate_fetch].concat(query_sources);",
            ),
            (
                "ALL_SOURCES",
                "varall_sources=get_sources.concat(query_sources);",
            ),
        ] {
            assert!(
                code.contains(derivation),
                "{what} must be derived from QUERY_SOURCE_DEFS as `{derivation}`"
            );
        }
        let panel_map = literal_body("SOURCE_PANEL", '{', '}');
        for def in &defs {
            assert_eq!(
                object_entry(&panel_map, &def.key),
                None,
                "SOURCE_PANEL lists `{}` by hand; the registry maps every QUERY read",
                def.key
            );
        }
        let shape_for = compact(&function_body("shapeFor"));
        let shape_at = compact(&function_body("shapeAt"));
        assert!(
            shape_for == "returnshapeat(source,{window:selectedwindow,groupby:selectedgroupby});"
                && shape_at.contains("query_source_defs[i].name===source")
                && shape_at.contains("returnquery_source_defs[i].shape(sel);")
                && shape_at.ends_with("returnnull;"),
            "shapeFor must resolve a source through its registry entry under the live \
             selection, and null otherwise"
        );
        assert!(
            !code.contains("query_source,query_series_source"),
            "a hand-written QUERY source membership list is back"
        );

        // Each record asks for ITS OWN shape: the bucketed record a series at
        // provider grain, the series-less one none, at the group-by grain, the
        // per-seat one at seat grain and the per-model one the last seven days
        // by model, whatever the range says.
        for (ident, shape) in [
            (
                "QUERY_SERIES_SOURCE",
                "returnfindshape(sel.window,'provider',true);",
            ),
            (
                "QUERY_SOURCE",
                "returnfindshape(sel.window,sel.groupby,false);",
            ),
            (
                "QUERY_SEATS_SOURCE",
                "returnfindshape(sel.window,'seat',false);",
            ),
            (
                "QUERY_MODELS_SOURCE",
                "returnfindshape('last7d','model',false);",
            ),
        ] {
            let def = query_source_def(ident);
            assert!(
                def.shape.contains(shape),
                "`{ident}` must ask for `{shape}`: {}",
                def.shape
            );
        }
        assert!(
            compact(&function_body("queryBodyFor"))
                .contains("varsource=querysourcefor(tab);returnbodyfromshape(shapefor(source),"),
            "queryBodyFor must build the body from the view's own query source"
        );

        // Each query-backed view's QUERY source (the first QUERY read in its
        // `sources`) is the read that view actually issues.
        assert_eq!(
            view_query_source("now").as_deref(),
            Some("QUERY_SERIES_SOURCE"),
            "Now must read the bucketed QUERY source; it is the only one whose payload \
             carries the series its sparklines are drawn from"
        );
        assert_eq!(
            view_query_source("traffic").as_deref(),
            Some("QUERY_SOURCE"),
            "Traffic must read the series-less QUERY source, the shape its usage cards request"
        );

        // WIRING. `querySourceFor` is what actually routes a round's response:
        // it must resolve the view's registered query source, and that must be
        // the first QUERY read the view lists, primary or not.
        let routing = compact(&function_body("querySourceFor"));
        assert!(
            routing.contains("viewfor(tab)") && routing.contains("viewquerysource(view)"),
            "querySourceFor must route a view's round through its registered query source"
        );
        let query_source = compact(&function_body("viewQuerySource"));
        assert!(
            query_source.contains("for(vari=0;i<view.sources.length;i++)")
                && query_source.contains(
                    "if(query_sources.indexof(view.sources[i])>=0){returnview.sources[i];}"
                )
                && query_source.ends_with("returnnull;"),
            "viewQuerySource must return the first QUERY read in the view's sources, in order, \
             and null when it lists none"
        );

        // The round must ASK which record it is writing, and hand it on.
        let round = function_body("queryRound");
        assert!(
            round.contains("querySourceFor(activeTab)") && round.contains("applyQueryOutcome("),
            "the QUERY round must resolve its own source record through querySourceFor and pass \
             it to applyQueryOutcome; a round that does not is writing a record it did not ask for"
        );

        // And the outcome must land on the record it was PASSED, never on a
        // constant.
        let outcome = function_body("applyQueryOutcome");
        assert!(
            outcome.contains("renderPanelGuarded(source,") && outcome.contains("SOURCES[source]"),
            "applyQueryOutcome must render and read through its `source` parameter"
        );
        for def in &defs {
            assert!(
                !outcome.contains(&def.ident),
                "applyQueryOutcome names `{}` directly; it must write only the record it was \
                 passed, or one view's outcome lands on another view's source",
                def.ident
            );
        }
    }

    /// Every request a registered QUERY read can resolve to is a declared
    /// `QUERY_SHAPES` body: each `findShape` argument the registry writes is
    /// either a literal or the live selection, and every value the selection
    /// can take (WINDOWS, GROUP_BYS) has its shape declared, so the parser
    /// drift test above has validated every body any read can send. The
    /// per-model read's fixed `last7d`/`model` body is one of them.
    ///
    /// Mutation check: change the per-model read to
    /// `findShape('last7d', 'model', true)` -> red (no such shape).
    #[test]
    fn every_registered_query_read_resolves_to_a_declared_shape() {
        let declared: Vec<serde_json::Map<String, serde_json::Value>> =
            json_object_literals(&literal_body("QUERY_SHAPES", '[', ']'))
                .iter()
                .map(|shape| serde_json::from_str(shape).expect("QUERY_SHAPES entry is JSON"))
                .collect();
        let windows = string_array("WINDOWS");
        let groups = group_by_tokens();
        let mut checked = 0;
        for def in query_source_defs() {
            let call = def
                .shape
                .split("findshape(")
                .nth(1)
                .and_then(|rest| rest.split(')').next())
                .unwrap_or_else(|| panic!("`{}` resolves through findShape", def.ident));
            let args: Vec<&str> = call.split(',').collect();
            let [window, group, bucketed] = args.as_slice() else {
                panic!(
                    "`{}` calls findShape with three arguments: {call}",
                    def.ident
                );
            };
            let pick = |arg: &str, live: &str, all: &[String]| -> Vec<String> {
                if arg == live {
                    all.to_vec()
                } else {
                    vec![arg.trim_matches('\'').to_string()]
                }
            };
            let bucketed = *bucketed == "true";
            for w in pick(window, "sel.window", &windows) {
                for g in pick(group, "sel.groupby", &groups) {
                    assert!(
                        declared.iter().any(|shape| {
                            shape.get("window").and_then(|v| v.as_str()) == Some(w.as_str())
                                && shape.get("group_by").and_then(|v| v.as_str())
                                    == Some(g.as_str())
                                && shape.contains_key("bucket") == bucketed
                        }),
                        "`{}` can ask for {w}/{g} (series: {bucketed}), which QUERY_SHAPES \
                         does not declare",
                        def.ident
                    );
                    checked += 1;
                }
            }
        }
        let models = query_source_def("QUERY_MODELS_SOURCE");
        assert!(
            models
                .shape
                .contains("returnfindshape('last7d','model',false);"),
            "positive control: the per-model read is fixed at last7d by model: {}",
            models.shape
        );
        assert!(
            checked > windows.len(),
            "the registry resolved {checked} bodies"
        );
    }

    /// The deterministic-refusal marker (`queryRejectedKeyBySource`) must be
    /// keyed per query source, mirroring the source-record split above: a
    /// 400/405 on Now's shape and a 400/405 on Traffic's shape are
    /// unrelated deterministic refusals, so recording one must never
    /// suppress retries of the other.
    ///
    /// Mutation check: point Now's QUERY source in VIEWS at `QUERY_SOURCE`
    /// -> red (two views would share one marker).
    #[test]
    fn dashboard_query_rejection_marker_is_keyed_per_source() {
        let script = script();
        assert!(
            !script.contains("var queryRejectedKey ="),
            "a page-wide `queryRejectedKey` scalar must not return; a refusal \
             of one query shape would silently suppress the other shape's \
             retries while the selection stands"
        );
        assert!(
            script.contains("var queryRejectedKeyBySource ="),
            "the deterministic-refusal marker must be declared as a per-source \
             map, not a single page-wide value"
        );

        // POSITIVE CONTROL: a round must still gate on ITS OWN source's
        // rejection key, or the marker's entire purpose -- never repeating a
        // request the server has already deterministically refused -- is lost.
        let round = function_body("queryRound");
        assert!(
            round.contains("queryRejectedKeyBySource[source] === key"),
            "queryRound must skip a round whose OWN source already holds this \
             exact key rejected, or a refused shape retries forever"
        );

        // NEGATIVE ASSERTION: the outcome handler must record a refusal
        // against the `source` parameter it was passed, never a hardcoded
        // constant -- a fixed key would tie both query shapes back to one
        // marker, recreating the page-wide bug the split source records fixed.
        let outcome = function_body("applyQueryOutcome");
        assert!(
            outcome.contains("queryRejectedKeyBySource[source] = key"),
            "applyQueryOutcome must record a refusal against its own `source` \
             parameter; a rejection on one query shape must never key onto the \
             other shape's marker"
        );
        for constant in ["QUERY_SOURCE", "QUERY_SERIES_SOURCE"] {
            let hardcoded = format!("queryRejectedKeyBySource[{constant}]");
            assert!(
                !outcome.contains(&hardcoded),
                "applyQueryOutcome must not hardcode `{constant}` when recording \
                 a refusal; that reintroduces a marker shared across both shapes"
            );
        }

        // A marker per source is a marker per view only while each query-backed
        // view issues its OWN source: two views registering one query source
        // would share one marker, so a refusal of one view's shape would
        // suppress the other's retries.
        let mut issued: Vec<String> = views::view_entries(&script)
            .into_iter()
            .filter_map(|view| first_query_source(&view.sources))
            .collect();
        issued.sort();
        let mut distinct = issued.clone();
        distinct.dedup();
        assert_eq!(
            issued, distinct,
            "each QUERY source must be issued by at most one view, or two views share one \
             rejection marker"
        );
        assert!(
            issued.contains(&"QUERY_SERIES_SOURCE".to_string())
                && issued.contains(&"QUERY_SOURCE".to_string()),
            "positive control: Now and Traffic each issue a QUERY: {issued:?}"
        );

        // DRIFT GUARD: the reset path that clears a stale marker must derive
        // the view set from `VIEWS`, not from a literal pair of query-backed
        // view ids. A hardcoded pair silently drops a future third
        // query-backed view from the reset, leaving its source suppressed
        // under a key its own next round will never produce again -- the
        // same class of bug this whole marker split exists to fix, one view
        // wider.
        let reset = function_body("queryInputChanged");
        assert!(
            !reset.contains("['now', 'traffic']") && !reset.contains("[\"now\", \"traffic\"]"),
            "queryInputChanged must not hardcode the query-backed view pair; \
             iterate `VIEWS` and let queryBodyFor/querySourceFor filter out \
             non-query views, so a new query-backed view is covered automatically"
        );
        assert!(
            reset.contains("VIEWS.forEach"),
            "queryInputChanged's marker reset must iterate `VIEWS` (the \
             canonical, self-maintaining view list), not a locally hardcoded set"
        );
    }

    /// The value of `key` in an object-literal body, unquoted, or `None` when
    /// the key is absent.
    fn object_entry(body: &str, key: &str) -> Option<String> {
        body.split(',')
            .filter_map(|entry| entry.split_once(':'))
            .find(|(name, _)| name.trim().trim_matches(|c| c == '\'' || c == '"') == key)
            .map(|(_, value)| {
                value
                    .trim()
                    .trim_matches(|c| c == '\'' || c == '"')
                    .to_string()
            })
    }

    /// The body of `function <name>(...) { ... }` in the dashboard script,
    /// comments stripped and braces balanced (so a nested block or object
    /// literal does not truncate it). An absent function fails.
    fn function_body(name: &str) -> String {
        let src = strip_js_comments(&script());
        let needle = format!("function {name}(");
        let at = src
            .find(&needle)
            .unwrap_or_else(|| panic!("dashboard JS declares `function {name}(`"));
        let chars: Vec<char> = src[at..].chars().collect();
        let mut i = chars
            .iter()
            .position(|c| *c == '{')
            .unwrap_or_else(|| panic!("`{name}` has a body"));
        let start = i + 1;
        let mut depth = 0i32;
        while i < chars.len() {
            match chars[i] {
                '\'' | '"' | '`' => {
                    i = skip_string_literal(&chars, i);
                    continue;
                }
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return chars[start..i].iter().collect();
                    }
                }
                _ => {}
            }
            i += 1;
        }
        panic!("`{name}` body is not closed");
    }

    /// The first QUERY source in `sources`, as viewQuerySource picks it: an
    /// identifier QUERY_SOURCE_DEFS registers.
    fn first_query_source(sources: &[String]) -> Option<String> {
        let idents: Vec<String> = query_source_defs()
            .into_iter()
            .map(|def| def.ident)
            .collect();
        sources
            .iter()
            .find(|source| idents.contains(source))
            .cloned()
    }

    /// The QUERY source a registered view issues, as the identifier it is
    /// written as in the script.
    fn view_query_source(id: &str) -> Option<String> {
        first_query_source(&views::view_entry(id).sources)
    }

    /// Drift guard on the QUERY retry contract: which codes back off, and the
    /// ladder they back off on.
    ///
    /// The code set is checked against the server's OWN `vocabulary::codes`
    /// consts rather than a second hardcoded list of spellings, so this test
    /// cannot rot into agreeing with itself: renaming or dropping a code in
    /// `types.rs` moves the server's value and fails here instead of leaving
    /// the dashboard silently no longer backing off on that failure.
    ///
    /// The ladder is pinned by its SEMANTIC properties only -- non-empty,
    /// positive, strictly increasing -- because the cadence is a tuning
    /// decision but "a failing source waits longer each time" is the contract.
    /// QUERY and GET keep separate indexes over this one shared array, so a
    /// non-monotonic or empty ladder would break both schedules at once.
    #[test]
    fn dashboard_query_retry_codes_and_backoff_ladder_match_the_server() {
        use crate::handlers::status::types::vocabulary::codes;

        let server_codes: std::collections::BTreeSet<&str> = [
            codes::NO_DATA,
            codes::SCHEMA_MISMATCH,
            codes::DB_BUSY,
            codes::DB_UNAVAILABLE,
            codes::CONFIG_UNAVAILABLE,
            codes::DOCTOR_UNAVAILABLE,
            codes::NO_CONFIG_PATH,
            codes::QUERY_TIMEOUT,
        ]
        .into_iter()
        .collect();

        let body = literal_body("QUERY_RETRY_CODES", '{', '}');
        let retry_codes: Vec<String> = body
            .split(',')
            .filter_map(|entry| entry.split_once(':'))
            .map(|(key, _)| {
                key.trim()
                    .trim_matches(|c| c == '\'' || c == '"')
                    .to_string()
            })
            .filter(|key| !key.is_empty())
            .collect();
        assert!(
            !retry_codes.is_empty(),
            "QUERY_RETRY_CODES must name at least one code, or no 200-carried \
             failure ever engages QUERY backoff"
        );
        for code in &retry_codes {
            assert!(
                server_codes.contains(code.as_str()),
                "dashboard QUERY retry code `{code}` is not a server unavailable code; a code \
                 was renamed or removed in types.rs and the dashboard will now silently stop \
                 backing off on that failure"
            );
        }

        let ladder: Vec<u64> = literal_body("BACKOFF_STEPS_MS", '[', ']')
            .split(',')
            .map(str::trim)
            .filter(|step| !step.is_empty())
            .map(|step| {
                step.parse()
                    .unwrap_or_else(|_| panic!("BACKOFF_STEPS_MS entry `{step}` is a u64 ms delay"))
            })
            .collect();
        assert!(
            !ladder.is_empty(),
            "BACKOFF_STEPS_MS must have at least one step, or a failing source retries \
             at the healthy cadence forever"
        );
        for pair in ladder.windows(2) {
            assert!(
                pair[0] < pair[1],
                "BACKOFF_STEPS_MS must increase strictly ({} then {}); a flat or falling \
                 step means a failing source stops backing off",
                pair[0],
                pair[1]
            );
        }
        assert!(
            ladder[0] > 0,
            "BACKOFF_STEPS_MS steps must be positive delays"
        );
    }

    /// The checklist beside these sources records what the browser tests
    /// cover and what is still verified by hand.
    const MANUAL_CHECKLIST: &str = "dashboard-manual-checklist.md";

    /// What is wrong with the pointer from the script to the manual checklist
    /// and from the checklist to the browser tests; empty when both hold: the
    /// script names the checklist, and the checklist names every browser test
    /// module it points the reader at.
    fn checklist_pointer_gaps(script: &str, checklist: &str, suites: &[&str]) -> Vec<String> {
        let mut gaps = Vec::new();
        if !script.contains(MANUAL_CHECKLIST) {
            gaps.push(format!("the script does not point at `{MANUAL_CHECKLIST}`"));
        }
        for suite in suites {
            if !checklist.contains(suite) {
                gaps.push(format!("the checklist does not name `{suite}`"));
            }
        }
        gaps
    }

    /// The browser test modules the checklist names as the automated
    /// coverage, each one a real sidecar file.
    const CHECKLIST_BROWSER_SUITES: &[(&str, &str)] = &[
        (
            "page_browser_tests.rs",
            include_str!("page_browser_tests.rs"),
        ),
        (
            "page_browser_epoch_tests.rs",
            include_str!("page_browser_epoch_tests.rs"),
        ),
        (
            "page_browser_states_tests.rs",
            include_str!("page_browser_states_tests.rs"),
        ),
        (
            "page_browser_accounts_tests.rs",
            include_str!("page_browser_accounts_tests.rs"),
        ),
        (
            "page_browser_verdict_tests.rs",
            include_str!("page_browser_verdict_tests.rs"),
        ),
        (
            "page_browser_locks_tests.rs",
            include_str!("page_browser_locks_tests.rs"),
        ),
        (
            "page_browser_motion_tests.rs",
            include_str!("page_browser_motion_tests.rs"),
        ),
        (
            "page_browser_host_tests.rs",
            include_str!("page_browser_host_tests.rs"),
        ),
        (
            "page_browser_layout_tests.rs",
            include_str!("page_browser_layout_tests.rs"),
        ),
        (
            "page_browser_cases_tests.rs",
            include_str!("page_browser_cases_tests.rs"),
        ),
    ];

    /// The script points at the manual checklist, and the checklist points
    /// at the browser tests that now carry the generation guard, fault
    /// reconciliation, focus and the changed-value mark. A part rewritten
    /// without the pointer, or a checklist that drops a suite, fails here.
    ///
    /// Negative case: the same check over the script with every pointer
    /// removed, and over a checklist without the motion suite, reports each.
    #[test]
    fn dashboard_script_points_at_the_manual_checklist() {
        const CHECKLIST: &str = include_str!("dashboard-manual-checklist.md");
        let suites: Vec<&str> = CHECKLIST_BROWSER_SUITES
            .iter()
            .map(|(name, src)| {
                assert!(src.contains("page_browser"), "`{name}` is a browser suite");
                *name
            })
            .collect();
        let script = script();

        assert_eq!(
            checklist_pointer_gaps(&script, CHECKLIST, &suites),
            Vec::<String>::new()
        );
        assert!(
            CHECKLIST.contains("generation guard") && CHECKLIST.contains("BY HAND"),
            "the checklist says what is automated and what is still verified by hand"
        );

        let unpointed = script.replace(MANUAL_CHECKLIST, "the checklist");
        let no_motion = CHECKLIST.replace("page_browser_motion_tests.rs", "the motion tests");
        assert_eq!(
            checklist_pointer_gaps(&unpointed, CHECKLIST, &suites),
            vec![format!("the script does not point at `{MANUAL_CHECKLIST}`")]
        );
        assert_eq!(
            checklist_pointer_gaps(&script, &no_motion, &suites),
            vec!["the checklist does not name `page_browser_motion_tests.rs`".to_owned()]
        );
    }

    /// The Models view's refusal chip reads a learned row's `value` in its
    /// tooltip. A text scan only: that the tooltip renders is on the manual
    /// checklist.
    ///
    /// Mutation check: drop the `value` clause from the builder -> red.
    #[test]
    fn the_learned_tooltip_reads_the_value_field() {
        assert!(
            function_body("modelsRefusalChip").contains("refusal.value"),
            "`modelsRefusalChip` must render `refusal.value`"
        );
    }

    /// The config tab reads the model facts the panel sends and nothing from
    /// the catalog fields the panel no longer carries.
    ///
    /// Mutation check: drop the `prices` read from `modelRow`, or read
    /// `m.economics`, `m.source` or `m.verified_at` again -> red.
    #[test]
    fn config_model_row_reads_the_model_facts() {
        let body = function_body("modelRow");
        for field in ["m.context_window", "m.fill", "m.prices"] {
            assert!(body.contains(field), "`modelRow` must render `{field}`");
        }
        for retired in ["m.source", "m.verified_at"] {
            assert!(
                !body.contains(retired),
                "`modelRow` must not read the retired `{retired}`"
            );
        }
        assert!(
            !strip_js_comments(&script()).contains("economics"),
            "the dashboard must not read the retired economics block"
        );
        let price = function_body("pricePair");
        for field in ["p.per_mtok", "p.source", "p.table_date"] {
            assert!(price.contains(field), "`pricePair` must render `{field}`");
        }
    }

    /// The written cache volume the Now and Traffic views show is the
    /// both-era write total. The per-lifetime query fields cover only rows
    /// written before the ledger recorded a total, so reading them would drop
    /// every current write from the figure.
    ///
    /// Mutation check: make `cacheWritten` sum `m.cache_write_5m` and
    /// `m.cache_write_1h` again -> red.
    #[test]
    fn cache_written_reads_the_write_total() {
        let body = function_body("cacheWritten");
        assert!(
            body.contains("m.cache_write_tokens"),
            "`cacheWritten` must read `cache_write_tokens`: {body}"
        );
        let code = strip_js_comments(&script());
        for legacy in ["cache_write_5m", "cache_write_1h"] {
            assert!(
                !code.contains(legacy),
                "the dashboard must not read the older-rows-only `{legacy}`"
            );
        }
    }

    /// An unreported cache write and an observed zero stay distinct on the
    /// page: `cacheWritten` yields null when no row in the group reported a
    /// write (which `magSpan` draws as the faint "-") and the total otherwise
    /// (an observed zero draws as "0"). The figure needs the presence count in
    /// the adapter's vocabulary, and no render path may read the write total
    /// around `cacheWritten`. A text scan only: the rendered "-" against "0" is
    /// on the manual checklist.
    ///
    /// Mutation check: drop `cache_write_present` from `QUERY_METRICS`, make
    /// `cacheWritten` return the total unconditionally, or render
    /// `m.cache_write_tokens` directly in a tab -> red.
    #[test]
    fn cache_written_is_absent_without_a_reported_write() {
        assert!(
            string_array("QUERY_METRICS").contains(&"cache_write_present".to_string()),
            "QUERY_METRICS must carry `cache_write_present`"
        );
        let body = function_body("cacheWritten");
        for needle in ["m.cache_write_present", "null"] {
            assert!(
                body.contains(needle),
                "`cacheWritten` must gate the total on presence (`{needle}`): {body}"
            );
        }
        let absent = function_body("magSpan");
        assert!(
            absent.contains("n === null") && absent.contains("'-'"),
            "`magSpan` must draw a null figure as \"-\": {absent}"
        );
        let code = strip_js_comments(&script());
        let reads = code.matches(".cache_write_tokens").count();
        assert_eq!(
            reads, 1,
            "the write total is read only inside `cacheWritten`, found {reads} reads"
        );
    }

    /// Self-containment guard on the ASSEMBLED page (what the handler serves,
    /// not the three authoring sources). The single-file, renders-offline
    /// constraint is what the compile-time assembly must preserve: no
    /// `src`/`href` pointing anywhere but an inline `data:` URI, and no CSS
    /// `url(...)` at all. A stylesheet link, a script src, or a web font
    /// would make the page depend on the network.
    #[test]
    fn assembled_page_has_no_external_refs() {
        assert!(
            PAGE.contains("<style>") && PAGE.contains("<script>"),
            "the assembled page must carry the inlined style and script blocks"
        );
        assert!(
            !PAGE.contains(STYLE_SLOT) && !PAGE.contains(SCRIPT_SLOT),
            "an asset slot survived assembly unreplaced"
        );

        // Every `src` / `href` ATTRIBUTE value must be an inline data: URI --
        // the favicon is the only one on this page. Matched on the RAW page
        // (case-folded for the attribute NAME only, which preserves byte
        // offsets) and only where whitespace precedes the name and an `=`
        // follows it, so this is an HTML attribute and not a JS identifier or a
        // CSS class that happens to contain the same letters. Both the name's
        // case and the whitespace HTML permits around the `=` are tolerated:
        // `<script SRC = "https://...">` is as external as the lowercase,
        // tight-binding spelling.
        let folded = PAGE.to_ascii_lowercase();
        for attr in ["src", "href"] {
            let mut offset = 0;
            while let Some(pos) = folded[offset..].find(attr) {
                let at = offset + pos;
                offset = at + attr.len();
                let preceded_by_space = folded[..at]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_whitespace);
                if !preceded_by_space {
                    continue;
                }
                let after_name = folded[offset..].trim_start_matches(char::is_whitespace);
                let Some(after_eq) = after_name.strip_prefix('=') else {
                    continue;
                };
                let value = after_eq.trim_start_matches(char::is_whitespace);
                let quote = value.chars().next().expect("attribute has a value");
                assert!(
                    quote == '"' || quote == '\'',
                    "dashboard `{attr}` attribute value must be quoted"
                );
                assert!(
                    value[quote.len_utf8()..].starts_with("data:"),
                    "assembled page carries an external `{attr}` reference (must be a data: URI)"
                );
            }
        }
        // The scan below reads each source's CODE (comments stripped, so prose
        // about `url()` does not read as a rule). Asserting the assembled page
        // carries each source verbatim is what makes that scan a statement
        // about the SERVED bytes rather than about the authoring inputs.
        let script = script();
        let style = style();
        assert!(
            PAGE.contains(&style) && PAGE.contains(&script),
            "the assembled page must carry both asset sources verbatim"
        );
        let code = [
            compact(&strip_html_comments(MARKUP)),
            compact(&strip_css_comments(&style)),
            compact(&strip_js_comments(&script)),
        ];
        for source in &code {
            assert!(
                !source.contains("url("),
                "assembled page carries a CSS url(...) reference (no external asset may be fetched)"
            );
            assert!(
                !source.contains("@import"),
                "assembled page carries a CSS @import (no external stylesheet may be fetched)"
            );
        }
    }

    /// The integer a `var <name> = <int>;` declaration assigns in the dashboard
    /// script. Anchored on the declaration, which must occur exactly once in
    /// the code, for the same reason as [`literal_body`]: a comment naming the
    /// variable must not stand in for the live value.
    fn script_ms(name: &str) -> u64 {
        declared_int(&script(), name)
    }

    /// The integer `var <name> = <int>;` assigns in `src`, read from the code
    /// with comments stripped so prose naming the declaration, or a block
    /// comment before the `;`, cannot change the value read.
    fn declared_int(src: &str, name: &str) -> u64 {
        let script = strip_js_comments(src);
        let decl_needle = format!("var {name} =");
        assert_eq!(
            script.matches(&decl_needle).count(),
            1,
            "dashboard JS declares `{decl_needle}` exactly once"
        );
        let decl = script.find(&decl_needle).expect("declaration present") + decl_needle.len();
        let end = script[decl..]
            .find(';')
            .unwrap_or_else(|| panic!("{name} declaration ends in `;`"))
            + decl;
        script[decl..end]
            .trim()
            .parse()
            .unwrap_or_else(|e| panic!("{name} is an integer literal: {e}"))
    }

    /// The sum identity in `crate::server::status_gate`'s module docs for the
    /// `/status` aggregate: its budgeted stretch must land inside the
    /// browser's per-GET abort, or the browser aborts a round the server
    /// would have answered with a shed panel. The abort must also stay under
    /// the poll cadence. The sum's terms are pinned in `status_gate`.
    #[test]
    fn the_aggregate_budget_fits_inside_the_browser_get_abort() {
        let timeout_ms = script_ms("TIMEOUT_MS");
        let cadence_ms = script_ms("BASE_MS");

        let server_ms = crate::server::status_gate::AGGREGATE_BUDGET_MS;

        assert!(
            server_ms <= timeout_ms,
            "AGGREGATE_BUDGET_MS = {server_ms}ms exceeds the browser GET abort \
             TIMEOUT_MS = {timeout_ms}ms"
        );
        assert!(
            timeout_ms < cadence_ms,
            "TIMEOUT_MS = {timeout_ms}ms must stay under the BASE_MS = {cadence_ms}ms cadence"
        );
    }

    /// The `/status/query` sum identity in `crate::server::status_gate`'s
    /// module docs: the body read and the query budget run serially in one
    /// handler, so together they must land inside the browser's QUERY abort.
    #[test]
    fn the_query_route_budgets_sum_inside_the_browser_query_abort() {
        let timeout_ms = script_ms("QUERY_TIMEOUT_MS");

        let body_read_ms = u64::try_from(super::super::query::BODY_READ_TIMEOUT.as_millis())
            .expect("body read timeout fits in u64 ms");
        let server_ms = body_read_ms + crate::server::status_gate::QUERY_BUDGET_MS;

        assert!(
            server_ms <= timeout_ms,
            "BODY_READ_TIMEOUT + QUERY_BUDGET_MS = {server_ms}ms exceeds the browser QUERY \
             abort QUERY_TIMEOUT_MS = {timeout_ms}ms"
        );
    }

    /// The condition of the `if (...) {` each `queryRound()` call in `body`
    /// opens, whitespace removed. A call that is not the first statement of an
    /// `if` block yields an empty condition, so an unguarded call reads as
    /// missing the guard rather than borrowing an earlier, unrelated `if`.
    fn query_round_call_conditions(body: &str) -> Vec<String> {
        let code: String = body.chars().filter(|c| !c.is_whitespace()).collect();
        code.match_indices("queryRound();")
            .map(|(at, _)| {
                let Some(before) = code[..at].strip_suffix("){") else {
                    return String::new();
                };
                before
                    .rfind("if(")
                    .map(|open| before[open + "if(".len()..].to_string())
                    .filter(|cond| !cond.contains('{') && !cond.contains(';'))
                    .unwrap_or_default()
            })
            .collect()
    }

    /// The QUERY abort outlasts the GET cadence (see
    /// `crate::server::status_gate`'s module docs), so the per-round nudge in
    /// `runRound` must skip while a query is in flight; otherwise every round
    /// would supersede a slow read before it could answer.
    #[test]
    fn the_cadence_nudge_never_supersedes_an_in_flight_query() {
        let conditions = query_round_call_conditions(&function_body("runRound"));

        assert!(
            !conditions.is_empty(),
            "runRound must nudge the QUERY with a `queryRound()` call"
        );
        for cond in &conditions {
            let conjuncts: Vec<&str> = cond.split("&&").collect();
            assert!(
                !cond.contains("||") && conjuncts.contains(&"!queryInFlight()"),
                "runRound's nudge `if ({cond})` must require `!queryInFlight()` as a conjunct"
            );
        }
    }

    /// A `db_busy` answer to a QUERY that superseded the page's own aborted
    /// read is the server still releasing the permit that read held, not an
    /// outage: the round must take the short retry schedule and never reach
    /// the outcome renderer or the backoff ladder. Read from the wiring: the
    /// round must hand its answer to `retrySupersededBusy` and return before
    /// `applyQueryOutcome`, and that function must re-arm `queryRound` from
    /// its own short schedule, gated on the page's own abort and the server's
    /// own busy code. That the panel stays in Loading is on the manual
    /// checklist.
    #[test]
    fn a_busy_answer_after_the_pages_own_abort_retries_briefly_outside_the_backoff_ladder() {
        use crate::handlers::status::types::vocabulary::codes;

        let compact_code =
            |body: String| -> String { body.chars().filter(|c| !c.is_whitespace()).collect() };
        let round = compact_code(function_body("queryRound"));
        let diverted = round
            .find("if(retrySupersededBusy(out,revision)){return;}")
            .expect("queryRound must return early when retrySupersededBusy takes the answer");
        let rendered = round
            .find("applyQueryOutcome(")
            .expect("queryRound renders through applyQueryOutcome");
        assert!(
            diverted < rendered,
            "queryRound must divert a superseded busy answer BEFORE applyQueryOutcome, or it \
             renders the red unavailable panel and enters the backoff ladder"
        );

        let retry = compact_code(function_body("retrySupersededBusy"));
        assert!(
            retry.contains("setTimeout(queryRound,SUPERSEDED_BUSY_RETRY_MS["),
            "retrySupersededBusy must re-arm queryRound on SUPERSEDED_BUSY_RETRY_MS: {retry}"
        );
        for backoff in ["scheduleNextQuery", "BACKOFF_STEPS_MS", "queryBackoffIndex"] {
            assert!(
                !retry.contains(backoff),
                "retrySupersededBusy names `{backoff}`; a superseded busy answer must not \
                 enter the backoff ladder"
            );
        }
        assert!(
            retry.contains("queryOwnAbortRevision!==revision")
                && retry.contains(&format!("'{}'", codes::DB_BUSY)),
            "retrySupersededBusy must be gated on a page abort at the round's own selection \
             revision AND the server's `{}` code; any other failure keeps the error and \
             backoff path",
            codes::DB_BUSY
        );
        let status = compact_code(function_body("queryStatus"));
        assert!(
            status.contains("queryCtrl.abort();queryOwnAbortRevision=querySelectionRevision;"),
            "queryStatus must record the selection revision it aborted its own read at"
        );
        assert!(
            round.contains("varrevision=querySelectionRevision;")
                && round.contains("retrySupersededBusy(out,revision)"),
            "queryRound must hand retrySupersededBusy the revision the round was issued at"
        );
        assert!(
            compact_code(function_body("queryInputChanged"))
                .starts_with("querySelectionRevision+=1;"),
            "every selection change must move the selection revision first"
        );

        let schedule: Vec<u64> = literal_body("SUPERSEDED_BUSY_RETRY_MS", '[', ']')
            .split(',')
            .map(str::trim)
            .filter(|step| !step.is_empty())
            .map(|step| {
                step.parse()
                    .expect("SUPERSEDED_BUSY_RETRY_MS entry is a u64 ms delay")
            })
            .collect();
        let first_backoff = literal_body("BACKOFF_STEPS_MS", '[', ']')
            .split(',')
            .next()
            .and_then(|step| step.trim().parse::<u64>().ok())
            .expect("BACKOFF_STEPS_MS has a first step");
        assert!(
            (1..=4).contains(&schedule.len()),
            "SUPERSEDED_BUSY_RETRY_MS must be a short bounded schedule: {schedule:?}"
        );
        assert!(
            schedule.iter().sum::<u64>() < first_backoff,
            "the whole short busy schedule {schedule:?} must finish before the first \
             backoff step ({first_backoff}ms), or it is no faster than backing off"
        );
    }

    #[test]
    fn a_query_round_call_condition_is_read_from_its_enclosing_if() {
        let cases = [
            (
                "guarded",
                "if (a < 0 && !queryInFlight() && b()) { queryRound(); }",
                "a<0&&!queryInFlight()&&b()",
            ),
            ("unguarded", "x(); queryRound();", ""),
            (
                "after an unrelated if",
                "if (!queryInFlight()) { y(); } queryRound();",
                "",
            ),
        ];
        for (label, body, expected) in cases {
            assert_eq!(
                query_round_call_conditions(body),
                vec![expected.to_string()],
                "{label}"
            );
        }
    }

    #[test]
    fn a_declared_int_is_read_from_code_not_comments() {
        let src = "// var TIMEOUT_MS = 1;\n\
                   var TIMEOUT_MS = 3500 /* ms */;\n\
                   /* var TIMEOUT_MS = 2; */\n";

        assert_eq!(declared_int(src, "TIMEOUT_MS"), 3500);
    }

    mod view_contract {
        use super::*;
        include!("page_view_contract_tests.rs");
    }

    mod style {
        use super::*;
        include!("page_style_tests.rs");
    }

    mod morph {
        use super::*;
        include!("page_morph_tests.rs");
    }

    mod views {
        use super::*;
        include!("page_views_tests.rs");
    }

    mod browser {
        use super::*;
        include!("page_browser_tests.rs");
    }

    mod browser_views {
        use super::*;
        include!("page_browser_views_tests.rs");
    }

    mod browser_url {
        use super::*;
        include!("page_browser_url_tests.rs");
    }

    mod browser_sort {
        use super::*;
        include!("page_browser_sort_tests.rs");
    }

    mod browser_epoch {
        use super::*;
        include!("page_browser_epoch_tests.rs");
    }

    mod browser_locks {
        use super::*;
        include!("page_browser_locks_tests.rs");
    }

    mod browser_states {
        use super::*;
        include!("page_browser_states_tests.rs");
    }

    mod browser_verdict {
        use super::*;
        include!("page_browser_verdict_tests.rs");
    }

    mod browser_motion {
        use super::*;
        include!("page_browser_motion_tests.rs");
    }

    mod browser_host {
        use super::*;
        include!("page_browser_host_tests.rs");
    }

    mod browser_layout {
        use super::*;
        include!("page_browser_layout_tests.rs");
    }

    mod browser_tiles {
        use super::*;
        include!("page_browser_tiles_tests.rs");
    }

    mod browser_sources {
        use super::*;
        include!("page_browser_sources_tests.rs");
    }

    mod browser_now {
        use super::*;
        include!("page_browser_now_tests.rs");
    }

    mod browser_accounts {
        use super::*;
        include!("page_browser_accounts_tests.rs");
    }

    mod browser_models {
        use super::*;
        include!("page_browser_models_tests.rs");
    }

    mod browser_traffic {
        use super::*;
        include!("page_browser_traffic_tests.rs");
    }

    mod browser_cases {
        use super::*;
        include!("page_browser_cases_tests.rs");
    }

    mod payload_keys {
        use super::*;
        include!("page_payload_keys_tests.rs");
    }
}
