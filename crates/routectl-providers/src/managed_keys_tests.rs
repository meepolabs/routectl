//! Membership weld for the managed-key predicates every extras merge consults.
//!
//! Each (egress, source) pair that layers client `provider_extras` or operator
//! extras onto an assembled body drops the keys its predicate names. The
//! expected sets below are the exact membership of each pair over `UNIVERSE`;
//! a predicate that gains or loses a key fails here with the pair named.
//!
//! The universe is closed against the sources it summarizes: every string
//! literal in the canonical list and in each predicate's own arms must appear
//! in it, so a key added to a list without a universe entry cannot escape the
//! equality check.

use std::collections::BTreeSet;

use super::{Dialect, is_managed_key};

use syn::buffer::{Cursor, TokenBuffer};
use syn::{Expr, Item, Stmt};

/// Keys every pair is checked against: each old list's literals, the
/// canonical list, and unmanaged controls (long-tail knobs, spelling variants
/// of managed keys, and other wire dialects' names for the same knob).
const UNIVERSE: &[&str] = &[
    // Canonical `ChatRequest` wire names and aliases.
    "model",
    "messages",
    "system",
    "temperature",
    "top_p",
    "max_tokens",
    "max_completion_tokens",
    "stop",
    "stop_sequences",
    "stream",
    "n",
    "seed",
    "logprobs",
    "top_logprobs",
    "logit_bias",
    "presence_penalty",
    "frequency_penalty",
    "user",
    "tools",
    "tool_choice",
    "response_format",
    "cache_control",
    "anthropic_beta",
    "reasoning",
    "reasoning_details",
    "provider_extras",
    "chat_template_kwargs",
    // Anthropic-shape keys an egress composes itself.
    "thinking",
    "parallel_tool_calls",
    "anthropic_version",
    "output_config",
    // Bedrock Converse body fields.
    "inferenceConfig",
    "toolConfig",
    "additionalModelResponseFieldPaths",
    // Gemini body fields.
    "contents",
    "systemInstruction",
    "generationConfig",
    // Anthropic-only fields the ingress sweep forwards.
    "metadata",
    "context_management",
    "context_hint",
    "mcp_servers",
    "container",
    "inference_geo",
    "speed",
    "diagnostics",
    // Unmanaged controls.
    "top_k",
    "service_tier",
    "safetySettings",
    "safety_settings",
    "generation_config",
    "system_instruction",
    "tool_config",
    "maxTokens",
    "topP",
    "Temperature",
    "MAX_TOKENS",
    "",
];

#[cfg(feature = "anthropic-api")]
const ANTHROPIC_API_MANAGED: &[&str] = &[
    "anthropic_beta",
    "cache_control",
    "chat_template_kwargs",
    "frequency_penalty",
    "logit_bias",
    "logprobs",
    "max_completion_tokens",
    "max_tokens",
    "messages",
    "model",
    "n",
    "parallel_tool_calls",
    "presence_penalty",
    "provider_extras",
    "reasoning",
    "reasoning_details",
    "response_format",
    "seed",
    "stop",
    "stop_sequences",
    "stream",
    "system",
    "temperature",
    "thinking",
    "tool_choice",
    "tools",
    "top_logprobs",
    "top_p",
    "user",
];

/// The Invoke operator set before it composed the Anthropic Messages set: a
/// closed literal list.
#[cfg(feature = "bedrock")]
const BEDROCK_INVOKE_OPERATOR_MANAGED_BEFORE: &[&str] = &[
    "anthropic_beta",
    "anthropic_version",
    "cache_control",
    "max_tokens",
    "messages",
    "model",
    "stop_sequences",
    "stream",
    "system",
    "temperature",
    "thinking",
    "tool_choice",
    "tools",
    "top_p",
];

/// The keys composing the Anthropic Messages set added to the Invoke operator
/// set. Operator `additional_model_request_fields` naming one of these no
/// longer reach the wire.
#[cfg(feature = "bedrock")]
const BEDROCK_INVOKE_OPERATOR_WIDENING: &[&str] = &[
    "chat_template_kwargs",
    "frequency_penalty",
    "logit_bias",
    "logprobs",
    "max_completion_tokens",
    "n",
    "parallel_tool_calls",
    "presence_penalty",
    "provider_extras",
    "reasoning",
    "reasoning_details",
    "response_format",
    "seed",
    "stop",
    "top_logprobs",
    "user",
];

#[cfg(feature = "bedrock")]
const BEDROCK_INVOKE_OPERATOR_MANAGED: &[&str] = &[
    "anthropic_beta",
    "anthropic_version",
    "cache_control",
    "chat_template_kwargs",
    "frequency_penalty",
    "logit_bias",
    "logprobs",
    "max_completion_tokens",
    "max_tokens",
    "messages",
    "model",
    "n",
    "parallel_tool_calls",
    "presence_penalty",
    "provider_extras",
    "reasoning",
    "reasoning_details",
    "response_format",
    "seed",
    "stop",
    "stop_sequences",
    "stream",
    "system",
    "temperature",
    "thinking",
    "tool_choice",
    "tools",
    "top_logprobs",
    "top_p",
    "user",
];

#[cfg(feature = "bedrock")]
const BEDROCK_CONVERSE_MANAGED: &[&str] = &[
    "additionalModelResponseFieldPaths",
    "anthropic_beta",
    "cache_control",
    "chat_template_kwargs",
    "frequency_penalty",
    "inferenceConfig",
    "logit_bias",
    "logprobs",
    "max_completion_tokens",
    "max_tokens",
    "messages",
    "model",
    "n",
    "output_config",
    "presence_penalty",
    "provider_extras",
    "reasoning",
    "reasoning_details",
    "response_format",
    "seed",
    "stop",
    "stop_sequences",
    "stream",
    "system",
    "temperature",
    "thinking",
    "toolConfig",
    "tool_choice",
    "tools",
    "top_logprobs",
    "top_p",
    "user",
];

#[cfg(feature = "openai-compat")]
const OPENAI_COMPAT_MANAGED: &[&str] = &[
    "anthropic_beta",
    "cache_control",
    "chat_template_kwargs",
    "frequency_penalty",
    "logit_bias",
    "logprobs",
    "max_completion_tokens",
    "max_tokens",
    "messages",
    "model",
    "n",
    "presence_penalty",
    "provider_extras",
    "reasoning",
    "reasoning_details",
    "response_format",
    "seed",
    "stop",
    "stop_sequences",
    "stream",
    "system",
    "temperature",
    "tool_choice",
    "tools",
    "top_logprobs",
    "top_p",
    "user",
];

#[cfg(feature = "gemini")]
const GEMINI_MANAGED: &[&str] = &[
    "anthropic_beta",
    "cache_control",
    "chat_template_kwargs",
    "contents",
    "frequency_penalty",
    "generationConfig",
    "logit_bias",
    "logprobs",
    "max_completion_tokens",
    "max_tokens",
    "messages",
    "model",
    "n",
    "presence_penalty",
    "provider_extras",
    "reasoning",
    "reasoning_details",
    "response_format",
    "seed",
    "stop",
    "stop_sequences",
    "stream",
    "system",
    "systemInstruction",
    "temperature",
    "toolConfig",
    "tool_choice",
    "tools",
    "top_logprobs",
    "top_p",
    "user",
];

/// One (egress, source) pair: the predicate its merge consults and the keys
/// of `UNIVERSE` it must manage.
struct Pair {
    name: &'static str,
    is_managed: fn(&str) -> bool,
    expected: &'static [&'static str],
}

fn pairs() -> Vec<Pair> {
    vec![
        #[cfg(feature = "anthropic-api")]
        Pair {
            name: "anthropic-api / client provider_extras",
            is_managed: |k| is_managed_key(Dialect::AnthropicApi, k),
            expected: ANTHROPIC_API_MANAGED,
        },
        #[cfg(feature = "bedrock")]
        Pair {
            name: "bedrock-invoke / client provider_extras (anthropic merge)",
            is_managed: |k| is_managed_key(Dialect::AnthropicApi, k),
            expected: ANTHROPIC_API_MANAGED,
        },
        #[cfg(feature = "bedrock")]
        Pair {
            name: "bedrock-invoke / operator additional_model_request_fields",
            is_managed: |k| is_managed_key(Dialect::BedrockInvokeOperator, k),
            expected: BEDROCK_INVOKE_OPERATOR_MANAGED,
        },
        #[cfg(feature = "bedrock")]
        Pair {
            name: "bedrock-converse / client provider_extras",
            is_managed: |k| is_managed_key(Dialect::BedrockConverse, k),
            expected: BEDROCK_CONVERSE_MANAGED,
        },
        #[cfg(feature = "bedrock")]
        Pair {
            name: "bedrock-converse / operator additional_model_request_fields",
            is_managed: |k| is_managed_key(Dialect::BedrockConverse, k),
            expected: BEDROCK_CONVERSE_MANAGED,
        },
        #[cfg(feature = "openai-compat")]
        Pair {
            name: "openai-compat / provider_extras and default_extras",
            is_managed: |k| is_managed_key(Dialect::OpenAiCompat, k),
            expected: OPENAI_COMPAT_MANAGED,
        },
        #[cfg(feature = "gemini")]
        Pair {
            name: "gemini / payload_extras",
            is_managed: |k| is_managed_key(Dialect::Gemini, k),
            expected: GEMINI_MANAGED,
        },
    ]
}

fn managed_set(pair: &Pair) -> BTreeSet<&'static str> {
    UNIVERSE
        .iter()
        .copied()
        .filter(|k| (pair.is_managed)(k))
        .collect()
}

#[test]
fn every_pair_manages_exactly_its_pinned_key_set() {
    let mut mismatches = Vec::new();
    for pair in pairs() {
        let actual = managed_set(&pair);
        let expected: BTreeSet<&str> = pair.expected.iter().copied().collect();
        if actual != expected {
            let gained: Vec<_> = actual.difference(&expected).collect();
            let lost: Vec<_> = expected.difference(&actual).collect();
            mismatches.push(format!(
                "{}: newly managed {gained:?}, no longer managed {lost:?}",
                pair.name
            ));
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

#[cfg(all(feature = "bedrock", feature = "openai-compat", feature = "gemini"))]
#[test]
fn all_seven_egress_source_pairs_are_welded() {
    assert_eq!(pairs().len(), 7);
}

#[test]
fn sampling_and_token_limit_keys_are_managed_on_every_pair() {
    let mut unmanaged = Vec::new();
    for pair in pairs() {
        for key in ["temperature", "top_p", "max_tokens"] {
            if !(pair.is_managed)(key) {
                unmanaged.push(format!("{}: {key}", pair.name));
            }
        }
    }
    assert!(unmanaged.is_empty(), "{}", unmanaged.join("\n"));
}

#[test]
fn max_completion_tokens_is_managed_on_every_pair() {
    let unmanaged: Vec<&str> = pairs()
        .iter()
        .filter(|pair| !(pair.is_managed)("max_completion_tokens"))
        .map(|pair| pair.name)
        .collect();
    assert!(unmanaged.is_empty(), "{unmanaged:?}");
}

/// The Invoke operator set is its old closed list plus exactly the named
/// widening, so a key gained or lost beyond that list is a deliberate edit to
/// one of the two lists below rather than a side effect of the composition.
#[cfg(feature = "bedrock")]
#[test]
fn the_invoke_operator_set_is_the_old_set_plus_exactly_the_named_widening() {
    let before: BTreeSet<&str> = BEDROCK_INVOKE_OPERATOR_MANAGED_BEFORE
        .iter()
        .copied()
        .collect();
    let widening: BTreeSet<&str> = BEDROCK_INVOKE_OPERATOR_WIDENING.iter().copied().collect();
    assert_eq!(widening.len(), 16);
    assert!(
        before.is_disjoint(&widening),
        "the widening names a key the old set already managed: {:?}",
        before.intersection(&widening).collect::<Vec<_>>()
    );

    let actual: BTreeSet<&str> = UNIVERSE
        .iter()
        .copied()
        .filter(|k| is_managed_key(Dialect::BedrockInvokeOperator, k))
        .collect();

    let expected: BTreeSet<&str> = before.union(&widening).copied().collect();
    assert_eq!(actual, expected);
}

/// The Invoke seam writes `anthropic_version` before the operator merge, and
/// the Anthropic Messages set does not name it, so the Invoke operator arm
/// must add it on top of that set.
#[cfg(feature = "bedrock")]
#[test]
fn anthropic_version_is_managed_on_the_invoke_operator_pair_beyond_the_anthropic_set() {
    assert!(is_managed_key(
        Dialect::BedrockInvokeOperator,
        "anthropic_version"
    ));
    assert!(!is_managed_key(Dialect::AnthropicApi, "anthropic_version"));
}

#[test]
fn unmanaged_controls_pass_through_every_pair_that_does_not_name_them() {
    let controls = [
        "top_k",
        "service_tier",
        "safetySettings",
        "generation_config",
        "maxTokens",
        "Temperature",
    ];
    for pair in pairs() {
        for key in controls {
            assert!(!(pair.is_managed)(key), "{}: {key} managed", pair.name);
        }
        let metadata_expected = pair.expected.contains(&"metadata");
        assert_eq!(
            (pair.is_managed)("metadata"),
            metadata_expected,
            "{}: metadata",
            pair.name
        );
    }
}

/// The openai-compat merge refuses exactly the canonical keys: which client
/// extras reach that wire is decided by provenance, never by a list of
/// another dialect's field names kept here.
#[cfg(feature = "openai-compat")]
#[test]
fn the_openai_compat_set_is_the_canonical_set_only() {
    let canonical: BTreeSet<&str> = UNIVERSE
        .iter()
        .copied()
        .filter(|k| routectl_core::is_canonical_request_key(k))
        .collect();
    let compat: BTreeSet<&str> = UNIVERSE
        .iter()
        .copied()
        .filter(|k| is_managed_key(Dialect::OpenAiCompat, k))
        .collect();
    assert_eq!(compat, canonical);
}

/// String literals inside every macro invocation (`matches!`) in the body of
/// `fn <fn_name>` in `source`.
fn literals_in_fn(source: &str, fn_name: &str) -> BTreeSet<String> {
    let file = syn::parse_file(source).expect("source parses");
    let body = file
        .items
        .iter()
        .find_map(|item| match item {
            Item::Fn(f) if f.sig.ident == fn_name => Some(&f.block),
            _ => None,
        })
        .unwrap_or_else(|| panic!("fn {fn_name} not found"));
    let mut out = BTreeSet::new();
    for stmt in &body.stmts {
        match stmt {
            Stmt::Expr(expr, _) => literals_in_expr(expr, &mut out),
            Stmt::Macro(m) => literals_in_macro(&m.mac, &mut out),
            _ => {}
        }
    }
    out
}

fn literals_in_expr(expr: &Expr, out: &mut BTreeSet<String>) {
    match expr {
        Expr::Binary(b) => {
            literals_in_expr(&b.left, out);
            literals_in_expr(&b.right, out);
        }
        Expr::Paren(p) => literals_in_expr(&p.expr, out),
        Expr::Block(b) => {
            for stmt in &b.block.stmts {
                if let Stmt::Expr(expr, _) = stmt {
                    literals_in_expr(expr, out);
                }
            }
        }
        Expr::Macro(m) => literals_in_macro(&m.mac, out),
        Expr::Match(m) => {
            for arm in &m.arms {
                literals_in_expr(&arm.body, out);
            }
        }
        _ => {}
    }
}

fn literals_in_macro(mac: &syn::Macro, out: &mut BTreeSet<String>) {
    let buffer = TokenBuffer::new2(mac.tokens.clone());
    collect_literals(buffer.begin(), out);
}

fn collect_literals(mut cursor: Cursor<'_>, out: &mut BTreeSet<String>) {
    while !cursor.eof() {
        if let Some((inner, _, _, next)) = cursor.any_group() {
            collect_literals(inner, out);
            cursor = next;
        } else if let Some((lit, next)) = cursor.literal() {
            let lit = syn::Lit::new(lit);
            if let syn::Lit::Str(s) = lit {
                out.insert(s.value());
            }
            cursor = next;
        } else if let Some((_, next)) = cursor.token_tree() {
            cursor = next;
        } else {
            break;
        }
    }
}

const CANONICAL_SOURCE: &str = include_str!("../../routectl-core/src/reserved.rs");

#[test]
fn universe_covers_the_canonical_list() {
    let canonical = literals_in_fn(CANONICAL_SOURCE, "is_canonical_request_key");
    assert!(
        canonical.len() >= 27 && canonical.contains("max_completion_tokens"),
        "canonical extraction found {canonical:?}"
    );
    let missing: Vec<_> = canonical
        .iter()
        .filter(|k| !UNIVERSE.contains(&k.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "canonical keys outside the universe: {missing:?}"
    );
    for key in &canonical {
        assert!(
            routectl_core::is_canonical_request_key(key),
            "extracted {key:?} is not canonical"
        );
    }
}

const PREDICATE_SOURCE: &str = include_str!("managed_keys.rs");

#[test]
fn universe_covers_every_predicate_arm() {
    let literals = literals_in_fn(PREDICATE_SOURCE, "is_managed_key");
    assert!(
        literals.contains("anthropic_version") && literals.contains("generationConfig"),
        "arm extraction found {literals:?}"
    );
    let missing: Vec<_> = literals
        .iter()
        .filter(|k| !UNIVERSE.contains(&k.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "arm keys outside the universe: {missing:?}"
    );
}

/// One egress's `client_extras_for` configuration, with a key of each kind
/// its split treats differently.
#[cfg(any(
    feature = "anthropic-api",
    feature = "openai-compat",
    feature = "gemini"
))]
struct SplitRow {
    egress: &'static str,
    dialect: Dialect,
    handled_by_egress: &'static [&'static str],
    managed_key: &'static str,
}

#[cfg(any(
    feature = "anthropic-api",
    feature = "openai-compat",
    feature = "gemini"
))]
fn split_rows() -> Vec<SplitRow> {
    vec![
        #[cfg(feature = "anthropic-api")]
        SplitRow {
            egress: "anthropic-api",
            dialect: Dialect::AnthropicApi,
            handled_by_egress: &[],
            managed_key: "thinking",
        },
        #[cfg(feature = "openai-compat")]
        SplitRow {
            egress: "openai-compat",
            dialect: Dialect::OpenAiCompat,
            handled_by_egress: &["output_config"],
            managed_key: "temperature",
        },
        #[cfg(feature = "bedrock")]
        SplitRow {
            egress: "bedrock-converse",
            dialect: Dialect::BedrockConverse,
            handled_by_egress: &["metadata"],
            managed_key: "inferenceConfig",
        },
        #[cfg(feature = "gemini")]
        SplitRow {
            egress: "gemini",
            dialect: Dialect::Gemini,
            handled_by_egress: &["metadata"],
            managed_key: "generationConfig",
        },
    ]
}

#[cfg(any(
    feature = "anthropic-api",
    feature = "openai-compat",
    feature = "gemini"
))]
const HTTP_PROVENANCES: [routectl_core::RequestProvenance; 3] = [
    routectl_core::RequestProvenance::AnthropicIngress,
    routectl_core::RequestProvenance::OpenaiIngress,
    routectl_core::RequestProvenance::ResponsesIngress,
];

/// A request of `provenance` whose dispatched extras carry a managed key, a
/// handled key (when the row has one) and an unrelated client key.
#[cfg(any(
    feature = "anthropic-api",
    feature = "openai-compat",
    feature = "gemini"
))]
fn split_request(
    row: &SplitRow,
    provenance: routectl_core::RequestProvenance,
) -> routectl_core::ChatRequest {
    let mut extras = serde_json::Map::new();
    extras.insert(row.managed_key.into(), serde_json::json!({"zz": 1}));
    for key in row.handled_by_egress {
        extras.insert((*key).into(), serde_json::json!({"zz": 2}));
    }
    extras.insert("zz_client_key".into(), serde_json::json!(3));
    let mut req = routectl_core::ChatRequest {
        provider_extras: Some(serde_json::Value::Object(extras)),
        ..Default::default()
    };
    req.routectl_internal.provenance = provenance;
    req
}

#[cfg(any(
    feature = "anthropic-api",
    feature = "openai-compat",
    feature = "gemini"
))]
fn forwarded_keys(split: &super::ClientExtras<'_>) -> BTreeSet<String> {
    split
        .forwardable
        .as_deref()
        .and_then(serde_json::Value::as_object)
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

#[test]
#[cfg(feature = "gemini")]
fn with_no_matching_ingress_every_http_client_key_but_a_handled_one_is_foreign() {
    for row in split_rows() {
        for provenance in HTTP_PROVENANCES {
            // Arrange
            let label = format!("{} from {provenance:?}", row.egress);
            let req = split_request(&row, provenance);

            // Act
            let split = super::client_extras_for(
                &req,
                row.dialect,
                super::MatchingIngress::LibraryOnly,
                row.handled_by_egress,
            );

            // Assert
            let expected_kept: BTreeSet<String> = row
                .handled_by_egress
                .iter()
                .map(|k| (*k).to_string())
                .collect();
            assert_eq!(forwarded_keys(&split), expected_kept, "{label}");
            let mut expected_foreign = vec![row.managed_key, "zz_client_key"];
            expected_foreign.sort_unstable();
            assert_eq!(split.foreign_keys, expected_foreign, "{label}");
        }
    }
}

#[test]
#[cfg(any(
    feature = "anthropic-api",
    feature = "openai-compat",
    feature = "gemini"
))]
fn with_a_matching_ingress_another_dialects_managed_key_is_kept_for_the_merge_to_refuse() {
    for row in split_rows() {
        for matching in HTTP_PROVENANCES {
            for provenance in HTTP_PROVENANCES.into_iter().filter(|p| *p != matching) {
                // Arrange
                let label = format!("{} matching {matching:?} from {provenance:?}", row.egress);
                let req = split_request(&row, provenance);

                // Act
                let split = super::client_extras_for(
                    &req,
                    row.dialect,
                    super::MatchingIngress::Http(matching),
                    row.handled_by_egress,
                );

                // Assert
                let mut expected_kept: BTreeSet<String> = row
                    .handled_by_egress
                    .iter()
                    .map(|k| (*k).to_string())
                    .collect();
                expected_kept.insert(row.managed_key.to_string());
                assert_eq!(forwarded_keys(&split), expected_kept, "{label}");
                assert_eq!(split.foreign_keys, vec!["zz_client_key"], "{label}");
            }
        }
    }
}

#[test]
#[cfg(any(
    feature = "anthropic-api",
    feature = "openai-compat",
    feature = "gemini"
))]
fn a_library_caller_forwards_its_extras_whole_under_either_matching_input() {
    let matchings = [
        #[cfg(feature = "gemini")]
        super::MatchingIngress::LibraryOnly,
        super::MatchingIngress::Http(routectl_core::RequestProvenance::AnthropicIngress),
    ];
    for row in split_rows() {
        for matching in matchings {
            // Arrange
            let label = format!("{} with {matching:?}", row.egress);
            let req = split_request(&row, routectl_core::RequestProvenance::Library);

            // Act
            let split =
                super::client_extras_for(&req, row.dialect, matching, row.handled_by_egress);

            // Assert
            assert!(
                matches!(split.forwardable, Some(std::borrow::Cow::Borrowed(_))),
                "{label}"
            );
            assert_eq!(
                split.forwardable.as_deref(),
                req.provider_extras.as_ref(),
                "{label}"
            );
            assert!(split.foreign_keys.is_empty(), "{label}");
        }
    }
}
