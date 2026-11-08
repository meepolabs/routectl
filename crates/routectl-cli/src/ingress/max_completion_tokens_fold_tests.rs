//! Both ingresses that fold the `max_completion_tokens` alias declare
//! exactly the edit the fold makes, so the wire check reads the fold as an
//! edit rather than a loss.

use axum::http::HeaderMap;
use routectl_core::wire_check::EditKind::{Relocate, Remove};
use routectl_core::wire_check::{EditKind, EditSite};
use serde_json::{Value, json};

use crate::ingress::IngressAdapter;
use crate::ingress::anthropic::AnthropicIngress;
use crate::ingress::openai::OpenAiIngress;

/// One body row: its name, the token-cap members it carries, and the fold
/// declarations it must draw.
type FoldRow = (&'static str, Value, Vec<(&'static str, EditKind)>);

const TOKEN_CAP_PATHS: [&str; 2] = ["/max_completion_tokens", "/max_tokens"];

/// The token-cap fold declarations `ingress` left on the parsed `body`.
fn fold_edits(ingress: &dyn IngressAdapter, body: Value) -> Vec<(String, EditKind)> {
    ingress
        .parse_request_value(&HeaderMap::new(), body)
        .expect("parses")
        .routectl_internal
        .declared_edits
        .iter()
        .filter(|e| e.site() == EditSite::LegacyFieldFold)
        .filter(|e| TOKEN_CAP_PATHS.contains(&e.path()))
        .map(|e| (e.path().to_string(), e.kind()))
        .collect()
}

fn body_with(caps: Value) -> Value {
    let mut body = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
    body.as_object_mut()
        .expect("object")
        .extend(caps.as_object().expect("object").clone());
    body
}

#[test]
fn the_max_completion_tokens_fold_declares_exactly_the_edit_it_makes() {
    let ingresses: [(&str, &dyn IngressAdapter); 2] =
        [("chat", &OpenAiIngress), ("anthropic", &AnthropicIngress)];
    let rows: [FoldRow; 3] = [
        (
            "only the alias: renamed to max_tokens",
            json!({"max_completion_tokens": 64}),
            vec![
                ("/max_completion_tokens", Relocate),
                ("/max_tokens", Relocate),
            ],
        ),
        (
            "both: the alias dropped beside the client's max_tokens",
            json!({"max_tokens": 32, "max_completion_tokens": 64}),
            vec![("/max_completion_tokens", Remove)],
        ),
        (
            "only max_tokens: nothing folded",
            json!({"max_tokens": 32}),
            vec![],
        ),
    ];
    for (dialect, ingress) in ingresses {
        for (name, caps, expected) in &rows {
            let expected: Vec<(String, EditKind)> = expected
                .iter()
                .map(|(path, kind)| ((*path).to_string(), *kind))
                .collect();

            let got = fold_edits(ingress, body_with(caps.clone()));

            assert_eq!(got, expected, "{dialect}: {name}");
        }
    }
}
