//! The embedded-asset dashboard page.
//!
//! Serves a single self-contained HTML document at `GET /`. The document is
//! ASSEMBLED AT COMPILE TIME from its authoring sources -- `dashboard.html`
//! (markup), the [`STYLE_PARTS`] style sources, and the [`SCRIPT_PARTS`] script
//! sources -- each embedded via `include_str!` and spliced into the markup's
//! two asset slots as one inline `<style>` and one inline `<script>`. The split
//! is an authoring
//! convenience only: the runtime artifact is still ONE offline page with zero
//! external requests (asserted by [`tests::assembled_page_has_no_external_refs`]),
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
    include_str!("dash_10_format.js"),
    include_str!("dash_20_query_vocab.js"),
    include_str!("dash_30_transport.js"),
    include_str!("dash_40_render.js"),
    include_str!("dash_50_dom.js"),
    include_str!("dash_60_tab_overview.js"),
    include_str!("dash_61_tab_usage.js"),
    include_str!("dash_70_tab_routing.js"),
    include_str!("dash_71_tab_health.js"),
    include_str!("dash_72_tab_config.js"),
    include_str!("dash_73_tab_doctor.js"),
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
                "usage".to_string(),
                crate::handlers::status::usage::SCHEMA_VERSION,
            ),
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
    /// silently turn a column into zeroes (the adapter coerces a missing
    /// numeric field through `num0`) or a cost token into `undefined`. Both
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
    /// selection to a shape this test never saw.
    #[test]
    fn dashboard_metric_tokens_are_in_the_query_vocabulary() {
        let vocabulary = serde_json::to_value(routectl_usage::QueryMetrics::default())
            .expect("QueryMetrics serializes");
        let vocabulary = vocabulary
            .as_object()
            .expect("QueryMetrics serializes to an object");

        for name in ["QUERY_METRICS", "QUERY_TOKENS"] {
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
        let now = chrono::Local::now();
        let shapes = json_object_literals(&literal_body("QUERY_SHAPES", '[', ']'));
        assert!(
            !shapes.is_empty(),
            "QUERY_SHAPES must declare at least one request body"
        );
        for shape in &shapes {
            assert!(
                crate::handlers::status::query::spec_from_body(shape.as_bytes(), now).is_ok(),
                "dashboard query shape `{shape}` is not in the server's request vocabulary"
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

    /// The client JS has no runtime harness, so the manual checklist beside
    /// these sources IS the coverage for the transport, render, and DOM
    /// surfaces. This pins the pointer to it: the checklist file must exist,
    /// and the concatenated script must still name it. A part rewritten
    /// without carrying the pointer forward, or a deleted checklist, fails the
    /// build rather than quietly leaving the untested surfaces unannounced.
    #[test]
    fn dashboard_script_points_at_the_manual_checklist() {
        const CHECKLIST: &str = include_str!("dashboard-manual-checklist.md");
        const MARKER: &str = "dashboard-manual-checklist.md";

        assert!(
            CHECKLIST.contains("generation guard"),
            "the manual checklist must still name the untested single-flight generation \
             guard; it is the coverage gap this file exists to record"
        );
        assert!(
            script().contains(MARKER),
            "the dashboard script must point at `{MARKER}`: the client JS has no runtime \
             harness, so that checklist is the only record of what is verified by hand"
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
}
