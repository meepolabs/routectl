//! The withheld-beta decision shared by the two anthropic-vocabulary egresses.
//!
//! The router hands each attempt the client `anthropic-beta` flags its lane
//! must not send (`routectl_internal.withheld_betas`). Both egresses drop
//! those flags from the client set before the wire, except a flag the
//! operator-asserted floor also carries: the floor always ships. What counts
//! as the floor is each egress's own call; the decision itself lives here so
//! the two lanes cannot drift on it.
//!
//! Each carrier also merges its beta lists into one before the wire; the
//! order-preserving merge they share lives here too, as does the set of
//! flags the shipped body itself requires.

use serde_json::{Map, Value};

use routectl_core::identity::anthropic::{EFFORT_BETA, STRUCTURED_OUTPUTS_BETA};

/// The `anthropic-beta` flag gating `thinking.display: "updates"`.
pub const THINKING_DISPLAY_UPDATES_BETA: &str = "thinking-display-updates-2026-08-18";

/// The client flags split by the withhold decision, each side in its
/// original order.
#[derive(Debug, PartialEq, Eq)]
pub struct WithholdSplit<T> {
    /// Entries that ship.
    pub kept: Vec<T>,
    /// Entries withheld from this lane.
    pub dropped: Vec<T>,
}

/// Split `flags` into the entries that ship and the ones withheld. An entry
/// is withheld when `flag_of` reads a flag from it that `withheld` names and
/// `floor` does not; every other entry (including one `flag_of` cannot read)
/// is kept in place.
pub fn split_withheld<T: Clone>(
    flags: &[T],
    flag_of: impl Fn(&T) -> Option<&str>,
    withheld: &[String],
    floor: &[String],
) -> WithholdSplit<T> {
    let mut split = WithholdSplit {
        kept: Vec::with_capacity(flags.len()),
        dropped: Vec::new(),
    };
    for item in flags {
        let is_withheld = flag_of(item).is_some_and(|flag| {
            withheld.iter().any(|w| w == flag) && !floor.iter().any(|f| f == flag)
        });
        if is_withheld {
            split.dropped.push(item.clone());
        } else {
            split.kept.push(item.clone());
        }
    }
    split
}

/// `entries` in order, minus every entry whose flag an earlier entry already
/// carries: the first occurrence of a flag keeps its position. An entry
/// `flag_of` cannot read a flag from is never a repeat, so it is kept in
/// place.
pub fn merge_unique<T>(
    entries: impl IntoIterator<Item = T>,
    flag_of: impl Fn(&T) -> Option<&str>,
) -> Vec<T> {
    let mut merged: Vec<T> = Vec::new();
    for entry in entries {
        let is_repeat = flag_of(&entry)
            .is_some_and(|flag| merged.iter().any(|kept| flag_of(kept) == Some(flag)));
        if !is_repeat {
            merged.push(entry);
        }
    }
    merged
}

/// The beta carrier a request egresses on, as far as body-implied flags care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(feature = "bedrock"), allow(dead_code))]
pub enum BetaLane {
    /// The anthropic-api header on routectl's own OAuth seat at
    /// api.anthropic.com.
    AnthropicApiCloak,
    /// The anthropic-api header on any other credential that is not the
    /// forwarded client bearer.
    AnthropicApi,
    /// The anthropic-api header on the forwarded client bearer: the client's
    /// beta set ships verbatim, so the body implies nothing routectl adds.
    AnthropicApiForwarded,
    /// The Bedrock Invoke body's `anthropic_beta`.
    BedrockInvoke,
    /// The Bedrock Converse `additionalModelRequestFields.anthropic_beta`.
    BedrockConverse,
}

/// The flags `body` requires on `lane`, in union order. `body` is the final
/// carrier as it ships: the anthropic-api wire body, the Invoke body, or the
/// Converse bag. Each flag is implied by a body field rather than opted into
/// by the client, so the carrier unions it after the withhold and every strip.
///
/// - `thinking.display: "updates"` implies `THINKING_DISPLAY_UPDATES_BETA`
///   on Converse only.
/// - `output_config.format` implies `STRUCTURED_OUTPUTS_BETA` on every lane
///   but the forwarded one. Retained as belt-and-braces, not a proven hard
///   requirement: api.anthropic.com accepted the field with and without the
///   flag on one measured lane (2026-08-11), and Bedrock accepted it without
///   the flag on one model (2026-10-21), but acceptance is not proof the
///   format is honored ungated, and an older account or model tier may still
///   gate it.
/// - `output_config.effort` implies `EFFORT_BETA` on the cloak lane only,
///   where routectl composes the directive. Bedrock Invoke and Converse
///   accepted the field without the flag (2026-10-21), so neither gains it.
pub fn body_implied_betas(body: &Map<String, Value>, lane: BetaLane) -> Vec<&'static str> {
    let output_config = body.get("output_config");
    let has_format = output_config.and_then(|oc| oc.get("format")).is_some();
    let has_effort = output_config.and_then(|oc| oc.get("effort")).is_some();
    let display_updates = body
        .get("thinking")
        .and_then(|t| t.get("display"))
        .and_then(Value::as_str)
        == Some("updates");

    let mut implied = Vec::new();
    match lane {
        BetaLane::AnthropicApiForwarded => {}
        BetaLane::AnthropicApi | BetaLane::BedrockInvoke => {
            if has_format {
                implied.push(STRUCTURED_OUTPUTS_BETA);
            }
        }
        BetaLane::AnthropicApiCloak => {
            if has_format {
                implied.push(STRUCTURED_OUTPUTS_BETA);
            }
            if has_effort {
                implied.push(EFFORT_BETA);
            }
        }
        BetaLane::BedrockConverse => {
            if display_updates {
                implied.push(THINKING_DISPLAY_UPDATES_BETA);
            }
            if has_format {
                implied.push(STRUCTURED_OUTPUTS_BETA);
            }
        }
    }
    implied
}

/// Append each [`body_implied_betas`] flag `betas` lacks, after every
/// existing entry. Idempotent: a present flag is neither duplicated nor
/// reordered, and a flag the body does not imply is never removed. Returns
/// the implied set, whether or not each flag was already present.
pub fn union_body_implied_betas(
    body: &Map<String, Value>,
    lane: BetaLane,
    betas: &mut Vec<String>,
) -> Vec<&'static str> {
    let implied = body_implied_betas(body, lane);
    for flag in &implied {
        if !betas.iter().any(|b| b == flag) {
            betas.push((*flag).to_string());
        }
    }
    implied
}

/// Record `floor` and `body_implied` on the attempt's beta assembly slot, when
/// the router installed one. `floor` keeps only the flags `ships` says the
/// final carrier holds, so a floor flag a later strip removed is not claimed
/// as shipped. Records no flag anywhere else.
pub fn deposit_beta_assembly(
    req: &routectl_core::ChatRequest,
    floor: Vec<String>,
    body_implied: Vec<String>,
    ships: impl Fn(&str) -> bool,
) {
    let Some(slot) = req.routectl_internal.beta_assembly.as_ref() else {
        return;
    };
    let floor = floor.into_iter().filter(|flag| ships(flag)).collect();
    slot.record(routectl_core::BetaAssembly::new(floor, body_implied));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(flags: &[&str]) -> Vec<String> {
        flags.iter().map(|f| (*f).to_string()).collect()
    }

    fn split(flags: &[&str], withheld: &[&str], floor: &[&str]) -> WithholdSplit<String> {
        split_withheld(
            &owned(flags),
            |f| Some(f.as_str()),
            &owned(withheld),
            &owned(floor),
        )
    }

    #[test]
    fn withheld_flag_outside_the_floor_is_dropped_and_order_is_kept() {
        let got = split(&["a", "x", "b", "x"], &["x"], &[]);

        assert_eq!(got.kept, owned(&["a", "b"]));
        assert_eq!(got.dropped, owned(&["x", "x"]));
    }

    #[test]
    fn withheld_flag_the_floor_asserts_is_kept_in_place() {
        let got = split(&["x", "a"], &["x"], &["x"]);

        assert_eq!(got.kept, owned(&["x", "a"]));
        assert!(got.dropped.is_empty());
    }

    #[test]
    fn empty_withheld_set_keeps_everything() {
        let got = split(&["a", "b"], &[], &[]);

        assert_eq!(got.kept, owned(&["a", "b"]));
        assert!(got.dropped.is_empty());
    }

    #[test]
    fn merge_unique_keeps_the_first_occurrence_in_order() {
        let rows: [(&str, &[&str], &[&str]); 5] = [
            ("empty in, empty out", &[], &[]),
            (
                "distinct flags keep their order",
                &["b", "a", "c"],
                &["b", "a", "c"],
            ),
            (
                "a repeat keeps the first position",
                &["a", "b", "a"],
                &["a", "b"],
            ),
            (
                "every repeat collapses",
                &["c", "c", "b", "c", "b"],
                &["c", "b"],
            ),
            (
                "flags compare exactly",
                &["a", " a", "A"],
                &["a", " a", "A"],
            ),
        ];

        for (name, input, want) in rows {
            let got = merge_unique(owned(input), |f| Some(f.as_str()));

            assert_eq!(got, owned(want), "{name}");
        }
    }

    #[test]
    fn merge_unique_keeps_every_entry_without_a_readable_flag() {
        let items = vec![None, Some("x"), None, Some("x"), Some("a")];

        let got = merge_unique(items, |i| *i);

        assert_eq!(got, vec![None, Some("x"), None, Some("a")]);
    }

    fn body(value: serde_json::Value) -> Map<String, Value> {
        value.as_object().expect("an object body").clone()
    }

    #[test]
    fn body_implied_betas_follow_each_lanes_gates() {
        use BetaLane::{
            AnthropicApi, AnthropicApiCloak, AnthropicApiForwarded, BedrockConverse, BedrockInvoke,
        };
        let format = serde_json::json!({"output_config": {"format": {"type": "json_schema"}}});
        let effort = serde_json::json!({"output_config": {"effort": "high"}});
        let display = serde_json::json!({"thinking": {"type": "adaptive", "display": "updates"}});
        let summarized = serde_json::json!({"thinking": {"display": "summarized"}});
        let all = serde_json::json!({
            "thinking": {"display": "updates"},
            "output_config": {"effort": "high", "format": {"type": "json_schema"}},
        });
        let plain = serde_json::json!({"model": "m"});
        let rows: [(&str, BetaLane, &serde_json::Value, &[&str]); 21] = [
            (
                "format on cloak",
                AnthropicApiCloak,
                &format,
                &[STRUCTURED_OUTPUTS_BETA],
            ),
            (
                "format on api key",
                AnthropicApi,
                &format,
                &[STRUCTURED_OUTPUTS_BETA],
            ),
            ("format on forwarded", AnthropicApiForwarded, &format, &[]),
            (
                "format on invoke",
                BedrockInvoke,
                &format,
                &[STRUCTURED_OUTPUTS_BETA],
            ),
            (
                "format on converse",
                BedrockConverse,
                &format,
                &[STRUCTURED_OUTPUTS_BETA],
            ),
            (
                "effort on cloak",
                AnthropicApiCloak,
                &effort,
                &[EFFORT_BETA],
            ),
            ("effort on api key", AnthropicApi, &effort, &[]),
            ("effort on forwarded", AnthropicApiForwarded, &effort, &[]),
            ("effort on invoke", BedrockInvoke, &effort, &[]),
            ("effort on converse", BedrockConverse, &effort, &[]),
            (
                "display updates on converse",
                BedrockConverse,
                &display,
                &[THINKING_DISPLAY_UPDATES_BETA],
            ),
            ("display updates on invoke", BedrockInvoke, &display, &[]),
            ("display updates on cloak", AnthropicApiCloak, &display, &[]),
            ("display updates on api key", AnthropicApi, &display, &[]),
            (
                "other display on converse",
                BedrockConverse,
                &summarized,
                &[],
            ),
            (
                "every field on cloak",
                AnthropicApiCloak,
                &all,
                &[STRUCTURED_OUTPUTS_BETA, EFFORT_BETA],
            ),
            ("every field on forwarded", AnthropicApiForwarded, &all, &[]),
            (
                "every field on invoke",
                BedrockInvoke,
                &all,
                &[STRUCTURED_OUTPUTS_BETA],
            ),
            (
                "every field on converse",
                BedrockConverse,
                &all,
                &[THINKING_DISPLAY_UPDATES_BETA, STRUCTURED_OUTPUTS_BETA],
            ),
            ("plain body on cloak", AnthropicApiCloak, &plain, &[]),
            ("plain body on converse", BedrockConverse, &plain, &[]),
        ];

        for (name, lane, value, want) in rows {
            let got = body_implied_betas(&body(value.clone()), lane);

            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn union_appends_only_the_missing_implied_flags_and_returns_the_whole_set() {
        let all = body(serde_json::json!({
            "output_config": {"effort": "high", "format": {"type": "json_schema"}},
        }));
        let mut betas = owned(&["client", EFFORT_BETA, "client"]);

        let implied = union_body_implied_betas(&all, BetaLane::AnthropicApiCloak, &mut betas);

        assert_eq!(implied, vec![STRUCTURED_OUTPUTS_BETA, EFFORT_BETA]);
        assert_eq!(
            betas,
            owned(&["client", EFFORT_BETA, "client", STRUCTURED_OUTPUTS_BETA])
        );
    }

    #[test]
    fn entry_without_a_readable_flag_is_kept() {
        let items = vec![Some("x"), None, Some("a")];

        let got = split_withheld(&items, |i| *i, &owned(&["x"]), &[]);

        assert_eq!(got.kept, vec![None, Some("a")]);
        assert_eq!(got.dropped, vec![Some("x")]);
    }
}
