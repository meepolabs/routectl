//! The embedded-asset dashboard page.
//!
//! Serves a single self-contained HTML document (embedded via `include_str!`,
//! the house pattern -- no `ServeDir`/`rust-embed`) at `GET /`. The response
//! is static bytes only: this handler carries no state and imports nothing
//! mutating, so it is structurally read-only, and the forbidden-import scan in
//! [`super`] covers this source alongside the panels.
//!
//! [`page_router`] is a stateless [`AxumRouter<()>`]. It is merged into the
//! serve process AUTH-EXEMPT and under the same `Host` allowlist as the
//! `/status` JSON, but deliberately OUTSIDE the JSON load-shed budget (see
//! [`crate::server`] wiring): a zero-I/O `&'static str` response cannot stall
//! or hold a shed permit, so an overload sheds status DATA while the page shell
//! still loads.

use axum::Router as AxumRouter;
use axum::http::header;
use axum::response::Html;
use axum::routing::get;

/// The self-contained dashboard document, embedded at build time.
const PAGE: &str = include_str!("dashboard.html");

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

    /// Read-only client-surface guard: the embedded dashboard JS carries no
    /// mutation channel and reaches no daemon path other than the read-only
    /// status surface. Read-only is a hard milestone invariant, so this pins
    /// it at the client layer: any mutating `fetch` method (quoted, backtick,
    /// unquoted, or a computed `method` property), any form affordance
    /// (`<form>`, `createElement('form')`, `.submit(`, `document.forms`), or
    /// any `fetch` to a path outside `/status` / `/status/usage` trips it.
    #[test]
    fn dashboard_js_carries_no_mutation_channel() {
        const DASHBOARD: &str = include_str!("dashboard.html");
        // Normalize away whitespace + case so `method : 'Post'` and
        // `method:"POST"` collapse to the same needle. Backticks are kept.
        let compact: String = DASHBOARD
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_ascii_lowercase();

        for verb in ["post", "put", "delete", "patch"] {
            for quote in ['"', '\'', '`'] {
                let needle = format!("method:{quote}{verb}{quote}");
                assert!(
                    !compact.contains(&needle),
                    "dashboard JS must not set a `{needle}` fetch option (read-only)"
                );
            }
            // Unquoted fetch option (`method:post`) and form-post attribute
            // (`method=post`), both after whitespace/quote normalization.
            assert!(
                !compact.contains(&format!("method:{verb}")),
                "dashboard JS must not set an unquoted `method:{verb}` (read-only)"
            );
            assert!(
                !compact.contains(&format!("method={verb}")),
                "dashboard must not carry a `method={verb}` form attribute (read-only)"
            );
        }

        // A computed `method` property (`obj['method'] = 'POST'`) would smuggle
        // a mutating verb past the literal-`method:` checks above.
        for quote in ['"', '\'', '`'] {
            let needle = format!("[{quote}method{quote}]");
            assert!(
                !compact.contains(&needle),
                "dashboard JS must not use a computed `method` property (read-only)"
            );
        }

        // Form elements are browser-native mutation affordances -- markup,
        // dynamic construction, and programmatic submission all breach the
        // pure-read surface, so none may appear.
        assert!(
            !compact.contains("<form"),
            "dashboard must carry no <form> element (read-only)"
        );
        for quote in ['"', '\'', '`'] {
            let needle = format!("createelement({quote}form{quote}");
            assert!(
                !compact.contains(&needle),
                "dashboard JS must not construct a form element (read-only)"
            );
        }
        assert!(
            !compact.contains(".submit("),
            "dashboard JS must not submit a form (read-only)"
        );
        assert!(
            !compact.contains("document.forms"),
            "dashboard JS must not reach the document forms collection (read-only)"
        );

        // Positive allowlist: every path-like string literal in the embedded
        // JS must target the read-only status surface. A GET to `/v1/...` or
        // any other daemon path is still a breach of the read-only contract,
        // so pin the set of reachable paths to `/status` / `/status/usage`.
        let script = {
            let start = DASHBOARD
                .find("<script>")
                .expect("dashboard has a <script> block");
            let end = DASHBOARD
                .find("</script>")
                .expect("dashboard <script> block is closed");
            &DASHBOARD[start..end]
        };
        for quote in ['"', '\'', '`'] {
            let opener = format!("{quote}/");
            let mut rest = script;
            while let Some(pos) = rest.find(&opener) {
                let from_slash = &rest[pos + quote.len_utf8()..];
                let close = from_slash[1..]
                    .find(quote)
                    .expect("path literal in dashboard JS is terminated");
                let path = &from_slash[..=close];
                assert!(
                    path.starts_with("/status"),
                    "dashboard JS may only fetch /status or /status/usage, found `{path}`"
                );
                rest = &from_slash[close + 1..];
            }
        }
    }
}
