//! Layered header/payload overlay merge (anthropic-beta comma-union, reasoning passthrough).

use std::collections::BTreeMap;

use routectl_core::{ChatRequest, RoutectlInternal};
use serde_json::Value;

use crate::config::{Config, ProviderEntry};

use super::{AUTH_HEADERS, DispatchTarget, LIST_VALUED_HEADERS, MANAGED_HEADERS};

/// Compose the layered configuration overlays into the per-attempt
/// request. v0.6.0 introduces three knobs that ride from operator
/// TOML through the dispatch layer onto the egress:
///
///   - `header_extras` (provider + model, with list-valued
///     `anthropic-beta` unioned)
///   - `payload_extras` (provider + model, deep-merged with model
///     winning on leaf collision)
///   - `routectl_internal` (per-model reasoning dialect + history
///     reasoning policy that the openai-compat egress reads)
///
/// All three are no-ops when neither the provider nor the model
/// configured them.
pub(super) fn apply_layered_overlays(
    config: &Config,
    target: &DispatchTarget,
    req: &mut ChatRequest,
) {
    let provider_entry = config.providers.get(&target.provider_name);
    let provider_headers = provider_entry.map(ProviderEntry::header_extras);
    let provider_payload = provider_entry.and_then(|e| e.payload_extras());

    merge_header_extras(
        &target.provider_name,
        provider_headers,
        &target.model.header_extras,
        req,
    );
    merge_payload_extras(
        &target.provider_name,
        provider_payload,
        target.model.payload_extras.as_ref(),
        req,
    );

    // Transport-internal carrier: the egress reads dialect +
    // history-reasoning from `req.routectl_internal` so the
    // `Provider` trait surface stays stable. Use struct-update on
    // Default so adding a new field on `RoutectlInternal` later
    // doesn't break this construction site (the type is
    // `#[non_exhaustive]`).
    //
    // Preserve `claude_code_headers` captured by the ingress: those
    // are inbound-request data, not per-model knobs, and the
    // Anthropic-API egress reads them downstream to forward
    // X-Claude-Code-* headers for gateway cost attribution.
    let captured_claude_code_headers =
        std::mem::take(&mut req.routectl_internal.claude_code_headers);
    // Preserve the ingress-set provenance: like `claude_code_headers`,
    // it is inbound-request data (which dialect produced the request),
    // not a per-model knob, so the per-attempt rebuild from
    // `Default::default()` must carry it across or it resets to
    // `Library`. `RequestProvenance` is `Copy`, so a plain read suffices.
    let captured_provenance = req.routectl_internal.provenance;
    // Preserve the header_extras map that `merge_header_extras` composed
    // onto the request above. The struct rebuild starts from
    // `Default::default()`, so without this take the merged provider +
    // model header_extras would be dropped before the egress reads them.
    let composed_header_extras = req.routectl_internal.header_extras.take();
    // Preserve the ingress-captured inbound per-conversation session key:
    // like `claude_code_headers`, it is inbound-request data, not a
    // per-model knob, so the per-attempt rebuild from `Default::default()`
    // must carry it across or it resets to `None` on the 2nd chain attempt.
    let captured_inbound_session_key = req.routectl_internal.inbound_session_key.take();
    // Preserve the ingress-forwarded bearer token: like
    // `inbound_session_key`, it is inbound-request data, not a per-model
    // knob, so the per-attempt rebuild from `Default::default()` must
    // carry it across or it resets to `None` on the 2nd chain attempt.
    let captured_forwarded_bearer = req.routectl_internal.forwarded_bearer.take();
    // Preserve the ingress-captured forwarded `x-stainless-*` headers:
    // like `forwarded_bearer`, they are inbound-request data (the client's
    // SDK fingerprint captured on the forwarded leg), not a per-model knob,
    // so the per-attempt rebuild from `Default::default()` must carry them
    // across or they reset to empty on the 2nd chain attempt -- which would
    // let routectl's minted fingerprint win on a retry.
    let captured_stainless_headers = std::mem::take(&mut req.routectl_internal.stainless_headers);
    // Preserve the ingress-captured unmodeled Responses input items: like
    // `stainless_headers`, they are inbound-request data (verbatim
    // codex-only `input[]` kinds the Responses egress replays), not a
    // per-model knob, so the per-attempt rebuild from `Default::default()`
    // must carry them across or they reset to empty on the 2nd chain
    // attempt -- silently reintroducing the drop this field exists to fix.
    let captured_responses_input_passthrough =
        std::mem::take(&mut req.routectl_internal.responses_input_passthrough);
    let mut internal = RoutectlInternal::default();
    internal.reasoning_dialect = target.reasoning_dialect.map(std::convert::Into::into);
    internal.history_reasoning = target.history_reasoning.map(std::convert::Into::into);
    internal.claude_code_headers = captured_claude_code_headers;
    internal.provenance = captured_provenance;
    internal.header_extras = composed_header_extras;
    internal.inbound_session_key = captured_inbound_session_key;
    internal.forwarded_bearer = captured_forwarded_bearer;
    internal.stainless_headers = captured_stainless_headers;
    internal.responses_input_passthrough = captured_responses_input_passthrough;
    internal.supports_adaptive_thinking = target.supports_adaptive_thinking;
    internal.effort_levels = target.effort_levels.clone();
    internal.max_thinking_budget = target.max_thinking_budget;
    // Per-model `max_tokens` ceiling. Zero means no per-model override;
    // Anthropic-shape egresses (anthropic-api, bedrock-invoke) read this
    // and fall through to their hardcoded 64000 baseline when zero.
    // Other egresses (openai-compat, openai-responses, bedrock-converse)
    // ignore this field and forward `req.max_tokens` omission cleanly.
    internal.max_output_tokens = target.max_output_tokens;
    // Operator-configured beta floor: the provider + model
    // `header_extras["anthropic-beta"]` betas, EXCLUDING the
    // client/ingress betas already on `req.anthropic_beta`. The
    // Anthropic-API egress re-adds these unconditionally after applying
    // the per-provider `allowed_betas` allowlist, so an operator's
    // model-pinned beta bypasses a filter meant only for client betas.
    // `req.anthropic_beta` itself stays the full union (composed by
    // `merge_header_extras`) so Bedrock's `filter_bedrock_betas` and the
    // log-safe summary still see the complete set.
    internal.operator_betas = operator_betas(provider_headers, &target.model.header_extras);
    req.routectl_internal = internal;
}

/// Collect the operator-configured `anthropic-beta` floor: the union of
/// the provider and model `header_extras["anthropic-beta"]` values
/// (comma-split, trimmed, deduplicated, visit order preserved). Client/
/// ingress betas are deliberately excluded -- those ride on
/// `req.anthropic_beta` and stay subject to the per-provider
/// `allowed_betas` allowlist.
pub(super) fn operator_betas(
    provider_extras: Option<&BTreeMap<String, String>>,
    model_extras: &BTreeMap<String, String>,
) -> Vec<String> {
    let provider_val = provider_extras
        .and_then(|m| {
            m.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("anthropic-beta"))
                .map(|(_, v)| v.as_str())
        })
        .unwrap_or("");
    let model_val = model_extras
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("anthropic-beta"))
        .map_or("", |(_, v)| v.as_str());

    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut out: Vec<String> = Vec::new();
    for raw in [provider_val, model_val] {
        for piece in raw.split(',') {
            let t = piece.trim();
            if !t.is_empty() && seen.insert(t.to_string()) {
                out.push(t.to_string());
            }
        }
    }
    out
}

/// Merge per-provider and per-model `header_extras` into the
/// per-attempt request. Three-source compose:
///
///   1. Clone the provider entry's `header_extras` into a working map.
///   2. Iterate the model's `header_extras`. Auth-reserved keys
///      (`authorization`, `x-api-key`, `anthropic-version`) WARN +
///      drop; managed-reserved keys (`host`, `content-type`,
///      `content-length`) DEBUG + drop. Other keys overwrite the
///      provider's value on collision (model wins).
///   3. For every list-valued header in `LIST_VALUED_HEADERS` (today
///      just `anthropic-beta`), run a comma-split-union-rejoin post-
///      pass over the three sources in visit order: `req.anthropic_beta`
///      (ingress lift) -> provider value -> model value. The unioned
///      string lands back on the merged map AND on `req.anthropic_beta`
///      so downstream readers (e.g. Bedrock's `filter_bedrock_betas`)
///      see the same fully-composed list.
///
/// The merged headers are published via `req.routectl_internal.header_extras`
/// and consumed by all four egresses (anthropic-api, openai-compat, bedrock,
/// openai-responses) at request-build time through
/// `crate::http_client::effective_header_extras`. The `anthropic-beta`
/// list-valued header is additionally written back to `req.anthropic_beta` so
/// the Anthropic-API egress (canonical field read) and Bedrock's beta filter
/// both see the fully-unioned set. Library consumers that construct a
/// `ChatRequest` without the router leave `header_extras` as `None`; the
/// egresses fall back to their construction-time `self.cfg.header_extras`
/// snapshot in that case.
pub fn merge_header_extras(
    provider_name: &str,
    provider_extras: Option<&BTreeMap<String, String>>,
    model_extras: &BTreeMap<String, String>,
    req: &mut ChatRequest,
) {
    // Start with a clone of the provider's headers.
    let mut merged: BTreeMap<String, String> = provider_extras.cloned().unwrap_or_default();

    // Layer the model's headers on top, gating against reserved
    // buckets. Model wins on plain-key collision.
    for (k, v) in model_extras {
        if is_auth_reserved(k) {
            tracing::warn!(
                provider = %routectl_core::sanitize_for_log(provider_name),
                header = %routectl_core::sanitize_for_log(k),
                "ignoring auth-reserved header from [models.X] header_extras",
            );
            continue;
        }
        if is_managed_reserved(k) {
            tracing::debug!(
                provider = %routectl_core::sanitize_for_log(provider_name),
                header = %routectl_core::sanitize_for_log(k),
                "dropping managed-reserved header from [models.X] header_extras",
            );
            continue;
        }
        merged.insert(k.clone(), v.clone());
    }

    // List-valued post-pass. For `anthropic-beta`, comma-split-union-
    // rejoin in visit order: req.anthropic_beta (ingress) -> provider
    // value -> model value. The unioned string lands back on the
    // merged map AND on req.anthropic_beta.
    for list_key in LIST_VALUED_HEADERS {
        let provider_val = provider_extras
            .and_then(|m| {
                m.iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(list_key))
                    .map(|(_, v)| v.as_str())
            })
            .unwrap_or("");
        let model_val = model_extras
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(list_key))
            .map_or("", |(_, v)| v.as_str());

        // Visit order: ingress (req.anthropic_beta) -> provider -> model.
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut unioned: Vec<String> = Vec::new();
        if list_key.eq_ignore_ascii_case("anthropic-beta") {
            for entry in &req.anthropic_beta {
                let t = entry.trim();
                if !t.is_empty() && seen.insert(t.to_string()) {
                    unioned.push(t.to_string());
                }
            }
        }
        for raw in [provider_val, model_val] {
            for piece in raw.split(',') {
                let t = piece.trim();
                if !t.is_empty() && seen.insert(t.to_string()) {
                    unioned.push(t.to_string());
                }
            }
        }

        if unioned.is_empty() {
            // Nothing to write; remove any inherited blank entry on
            // the merged map to keep the dump clean.
            let keys_to_drop: Vec<String> = merged
                .keys()
                .filter(|k| k.eq_ignore_ascii_case(list_key))
                .cloned()
                .collect();
            for k in keys_to_drop {
                merged.remove(&k);
            }
            continue;
        }

        let joined = unioned.join(",");
        // Drop any case-variant of the key already present, then
        // insert under the canonical lowercase name.
        let keys_to_drop: Vec<String> = merged
            .keys()
            .filter(|k| k.eq_ignore_ascii_case(list_key))
            .cloned()
            .collect();
        for k in keys_to_drop {
            merged.remove(&k);
        }
        merged.insert((*list_key).to_string(), joined);

        if list_key.eq_ignore_ascii_case("anthropic-beta") {
            req.anthropic_beta = unioned;
        }
    }

    // Strip `anthropic-beta` from the merged map before publishing it
    // to the egress -- it rides on `req.anthropic_beta` instead and
    // double-handling would cause the Anthropic-API egress to emit
    // duplicate values. The list-valued post-pass above already
    // wrote the unioned set there.
    let keys_to_strip: Vec<String> = merged
        .keys()
        .filter(|k| k.eq_ignore_ascii_case("anthropic-beta"))
        .cloned()
        .collect();
    for k in keys_to_strip {
        merged.remove(&k);
    }

    if !merged.is_empty() {
        tracing::debug!(
            provider = %routectl_core::sanitize_for_log(provider_name),
            header_keys = ?merged.keys().collect::<Vec<_>>(),
            "composed header_extras (provider + model + list-valued union)",
        );
    }

    // Publish the merged map to the egress via the transport-internal
    // carrier. Egresses read this in `build_headers` and union it with
    // their construction-time `self.cfg.header_extras` snapshot (model
    // wins on key collision). Library consumers that construct a
    // ChatRequest without the router leave this `None`, and the egress
    // falls back to its `self.cfg.header_extras` alone.
    req.routectl_internal.header_extras = Some(merged);
}

/// Merge per-provider and per-model `payload_extras` into the
/// per-attempt request. Deep recursive merge with model winning on
/// leaf collision; the result lands on `req.provider_extras` so each
/// egress's existing `provider_extras` reader picks it up.
///
/// Layer order: `req.provider_extras` (ingress forward-compat sweep,
/// pre-existing on the request) -> provider `payload_extras` ->
/// model `payload_extras`. The provider's payload IS deep-merged
/// over the ingress sweep on key collision, and the model's payload
/// then deep-merges over both. Net precedence: model > provider >
/// ingress sweep on shared leaf keys; ingress-only keys survive
/// untouched because no other source set them.
pub fn merge_payload_extras(
    provider_name: &str,
    provider_extras: Option<&Value>,
    model_extras: Option<&Value>,
    req: &mut ChatRequest,
) {
    if provider_extras.is_none() && model_extras.is_none() {
        return;
    }

    // Start with the request's existing provider_extras (if any),
    // then layer provider, then model.
    let mut accumulated: Value = req
        .provider_extras
        .clone()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

    if let Some(p) = provider_extras {
        deep_merge_value(&mut accumulated, p, provider_name, "provider");
    }
    if let Some(m) = model_extras {
        deep_merge_value(&mut accumulated, m, provider_name, "model");
    }

    // If nothing landed (both were empty objects), don't synthesize
    // an empty provider_extras on the request.
    let is_empty_object = accumulated
        .as_object()
        .is_some_and(serde_json::Map::is_empty);
    if is_empty_object && req.provider_extras.is_none() {
        return;
    }
    req.provider_extras = Some(accumulated);
}

/// Deep recursive merge of `src` into `dst`. Same-key object values
/// merge recursively; scalar / array collisions take the `src` value
/// with a DEBUG log naming the key (so an operator who shadowed a
/// provider scalar with a model value can correlate at triage).
fn deep_merge_value(dst: &mut Value, src: &Value, provider_name: &str, src_layer: &str) {
    match (dst, src) {
        (Value::Object(d), Value::Object(s)) => {
            for (k, v) in s {
                match d.get_mut(k) {
                    Some(existing) if existing.is_object() && v.is_object() => {
                        deep_merge_value(existing, v, provider_name, src_layer);
                    }
                    Some(_) => {
                        tracing::debug!(
                            provider = %routectl_core::sanitize_for_log(provider_name),
                            layer = %src_layer,
                            key = %routectl_core::sanitize_for_log(k),
                            "payload_extras: leaf collision; {src_layer} wins",
                        );
                        d.insert(k.clone(), v.clone());
                    }
                    None => {
                        d.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (dst, src) => {
            *dst = src.clone();
        }
    }
}

fn is_auth_reserved(name: &str) -> bool {
    let lc = name.to_ascii_lowercase();
    AUTH_HEADERS.contains(&lc.as_str())
}

fn is_managed_reserved(name: &str) -> bool {
    let lc = name.to_ascii_lowercase();
    MANAGED_HEADERS.contains(&lc.as_str())
}

#[cfg(test)]
#[path = "merge_header_extras_tests.rs"]
mod merge_header_extras_tests;

#[cfg(test)]
#[path = "merge_payload_extras_tests.rs"]
mod merge_payload_extras_tests;

#[cfg(test)]
#[path = "three_source_anthropic_beta_lift_tests.rs"]
mod three_source_anthropic_beta_lift_tests;

#[cfg(test)]
#[path = "reasoning_passthrough_tests.rs"]
mod reasoning_passthrough_tests;
