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

#[cfg(feature = "bedrock")]
const BEDROCK_INVOKE_OPERATOR_MANAGED: &[&str] = &[
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
fn max_completion_tokens_is_managed_on_every_pair_but_the_invoke_operator_one() {
    for pair in pairs() {
        let is_invoke_operator = pair.name.starts_with("bedrock-invoke / operator");
        assert_eq!(
            (pair.is_managed)("max_completion_tokens"),
            !is_invoke_operator,
            "{}",
            pair.name
        );
    }
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
