//! Leak-guard for the canonical sampling knobs an egress cannot honor.
//!
//! `n`, `seed`, `logprobs`, `top_logprobs`, `logit_bias`,
//! `presence_penalty` and `frequency_penalty` are canonical `ChatRequest`
//! fields, so `is_canonical_request_key` gates them out of every egress's
//! `provider_extras` merge: they cannot ride through as extras either.
//! Whether a given knob survives is therefore decided entirely by whether
//! the egress translates it onto its own wire shape, and that varies per
//! egress: openai-compat forwards them under their canonical names (so it
//! never calls in here), the Gemini egress translates a documented subset
//! onto `generationConfig`, and the remaining egresses translate none.
//! What is not translated is lost semantically rather than cosmetically --
//! `n: 3` yields one completion, `seed` yields non-reproducible output
//! while the caller believes the sampler is pinned.
//!
//! So each caller passes the canonical names IT honors and this guard
//! reports the remainder: ONE structured WARN per egress invocation naming
//! the dropped fields the request carried (names only, never values). The
//! honored list stays with the code that builds the wire body, so the
//! guard never learns any dialect's wire shape.
//!
//! Why anthropic-api (and bedrock-invoke, which reaches the same wire
//! shape by delegation) honors none of the seven: none of them appears in
//! the Anthropic Messages API request surface, verified 2026-08-10 against
//! Anthropic's published parameter reference and corroborated against
//! `anthropic-sdk-python`'s `message_create_params`. That is an absence
//! from the documented request contract, not a demonstration that the
//! service rejects them -- the point is that there is no counterpart to
//! translate onto, so the honest answer is to drop and say so.
//! `docs/WIRE-GOTCHAS.md` carries the full parameter list.

use routectl_core::ChatRequest;

/// Canonical sampling fields the request carries that this egress cannot
/// honor, in schema declaration order. `honored` holds the canonical names
/// the caller does translate (not their wire spellings); they are filtered
/// out. Pure function of the inputs -- no logging, no mutation -- so the
/// detection is unit-testable directly.
fn dropped_sampling_fields(req: &ChatRequest, honored: &[&str]) -> Vec<&'static str> {
    let mut fields: Vec<&'static str> = Vec::new();
    if req.n.is_some() {
        fields.push("n");
    }
    if req.seed.is_some() {
        fields.push("seed");
    }
    if req.logprobs.is_some() {
        fields.push("logprobs");
    }
    if req.top_logprobs.is_some() {
        fields.push("top_logprobs");
    }
    if req.logit_bias.is_some() {
        fields.push("logit_bias");
    }
    if req.presence_penalty.is_some() {
        fields.push("presence_penalty");
    }
    if req.frequency_penalty.is_some() {
        fields.push("frequency_penalty");
    }
    fields.retain(|f| !honored.contains(f));
    fields
}

/// Emit a single WARN naming every canonical sampling field this egress
/// drops, excluding the canonical names in `honored` that it translates.
/// Silent when the request carries none of the dropped ones. Logs field
/// NAMES and a count only -- no values, since `logit_bias` and friends can
/// carry caller-shaped data.
pub fn warn_dropped_sampling_fields(provider_id: &str, req: &ChatRequest, honored: &[&str]) {
    let fields = dropped_sampling_fields(req, honored);
    if fields.is_empty() {
        return;
    }
    tracing::warn!(
        provider = %provider_id,
        dropped_fields = ?fields,
        dropped_count = fields.len(),
        "sampling fields dropped: not translated onto this egress's wire"
    );
}

/// Shared `logs_assert` predicate for the per-egress wiring tests, which
/// live in five sibling modules and must all pin the SAME contract.
#[cfg(test)]
pub mod test_support {
    /// The drop diagnostic's message text, as emitted by
    /// [`super::warn_dropped_sampling_fields`].
    const SAMPLING_WARN_NEEDLE: &str =
        "sampling fields dropped: not translated onto this egress's wire";

    /// `logs_assert` predicate requiring EXACTLY ONE WARN-level sampling
    /// drop diagnostic. At-least-one (`logs_contain`) would stay green if a
    /// second guard call or a second `warn!` were wired onto an egress,
    /// which is the failure this one-per-request contract exists to
    /// prevent; the level check keeps a downgrade to `debug!` -- invisible
    /// under production log filtering -- from passing either.
    pub fn exactly_one_sampling_warn(lines: &[&str]) -> Result<(), String> {
        let matches: Vec<&&str> = lines
            .iter()
            .filter(|l| l.contains(SAMPLING_WARN_NEEDLE))
            .collect();
        let warns = matches.iter().filter(|l| l.contains("WARN")).count();
        if warns == 1 && matches.len() == 1 {
            return Ok(());
        }
        Err(format!(
            "expected exactly one WARN naming dropped sampling fields; \
             got {} matching line(s), {warns} of them at WARN: {matches:?}",
            matches.len()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::exactly_one_sampling_warn;
    use super::{dropped_sampling_fields, warn_dropped_sampling_fields};
    use routectl_core::{ChatRequest, Message, MessageContent, Role};
    use serde_json::json;
    use tracing_test::traced_test;

    fn req() -> ChatRequest {
        ChatRequest {
            model: "m".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                reasoning: None,
                reasoning_details: Vec::new(),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }]
            .into(),
            ..Default::default()
        }
    }

    #[test]
    fn detects_no_dropped_fields_when_none_set() {
        // Arrange
        let r = req();

        // Act
        let fields = dropped_sampling_fields(&r, &[]);

        // Assert
        assert!(fields.is_empty());
    }

    #[test]
    fn detects_each_sampling_field_in_schema_order() {
        // Arrange
        let mut r = req();
        r.n = Some(3);
        r.seed = Some(7);
        r.logprobs = Some(true);
        r.top_logprobs = Some(2);
        r.logit_bias = Some(json!({"1": -100}));
        r.presence_penalty = Some(0.5);
        r.frequency_penalty = Some(0.25);

        // Act
        let fields = dropped_sampling_fields(&r, &[]);

        // Assert
        assert_eq!(
            fields,
            vec![
                "n",
                "seed",
                "logprobs",
                "top_logprobs",
                "logit_bias",
                "presence_penalty",
                "frequency_penalty",
            ]
        );
    }

    #[test]
    fn detects_only_the_fields_actually_set() {
        // Arrange
        let mut r = req();
        r.seed = Some(11);
        r.frequency_penalty = Some(1.0);

        // Act
        let fields = dropped_sampling_fields(&r, &[]);

        // Assert
        assert_eq!(fields, vec!["seed", "frequency_penalty"]);
    }

    #[test]
    fn omits_honored_fields_from_the_dropped_list() {
        // Arrange
        let mut r = req();
        r.n = Some(3);
        r.seed = Some(7);
        r.logprobs = Some(true);
        r.top_logprobs = Some(2);
        r.logit_bias = Some(json!({"1": -100}));
        r.presence_penalty = Some(0.5);
        r.frequency_penalty = Some(0.25);

        // Act
        let fields =
            dropped_sampling_fields(&r, &["seed", "presence_penalty", "frequency_penalty"]);

        // Assert
        assert_eq!(fields, vec!["n", "logprobs", "top_logprobs", "logit_bias"]);
    }

    #[traced_test]
    #[test]
    fn does_not_warn_when_every_carried_field_is_honored() {
        // Arrange
        let mut r = req();
        r.seed = Some(7);
        r.presence_penalty = Some(0.5);

        // Act
        warn_dropped_sampling_fields("prov-test", &r, &["seed", "presence_penalty"]);

        // Assert
        assert!(!logs_contain("sampling fields dropped"));
    }

    #[traced_test]
    #[test]
    fn warns_naming_dropped_fields_without_values() {
        // Arrange
        let mut r = req();
        r.n = Some(3);
        r.logit_bias = Some(json!({"1": -100}));

        // Act
        warn_dropped_sampling_fields("prov-test", &r, &[]);

        // Assert
        logs_assert(exactly_one_sampling_warn);
        assert!(logs_contain("\"n\""));
        assert!(logs_contain("logit_bias"));
        assert!(!logs_contain("-100"));
    }

    #[traced_test]
    #[test]
    fn does_not_warn_when_no_sampling_field_set() {
        // Arrange
        let r = req();

        // Act
        warn_dropped_sampling_fields("prov-test", &r, &[]);

        // Assert
        assert!(!logs_contain("sampling fields dropped"));
    }
}
