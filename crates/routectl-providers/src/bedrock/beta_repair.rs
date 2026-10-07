//! Reactive repair for a Bedrock 400 that names the `anthropic-beta` flags it
//! rejected.
//!
//! AWS answers an unsupported client beta with a `ValidationException` whose
//! message lists exactly the rejected flags, backtick-quoted, in request
//! order:
//!
//! ```text
//! Unexpected value(s) `a`, `b` for the `anthropic-beta` header. Please consult our documentation at platform.claude.com/docs or try again without the header.
//! ```
//!
//! Converse wraps the same text in `The model returned the following errors: `.
//! A never-issued flag gets the same message, so this is the only signal for a
//! flag the request's withheld set does not already name.
//!
//! The provider strips exactly the named flags and retries once. Only a
//! successful inference retry records them, into the request's
//! `routectl_internal.beta_repair_report` slot when the caller installed one,
//! so the caller can withhold them from later requests; a CountTokens retry
//! and a failed retry record nothing. The floor is
//! [`super::betas::operator_floor`]. A flag is repairable only when the
//! message is the exact envelope, every token is token-shaped, every token was
//! lifted from the client, and none is an operator-floor flag: the floor is the
//! operator's explicit override and is never second-guessed here. Nor is a
//! flag repaired when the body built without it would still carry it, because
//! the request's own features re-add it.

use futures::future::BoxFuture;
use serde_json::Value;

use routectl_core::{
    ChatRequest, Error, Result, is_safe_token, sanitize_for_log, strip_converse_errors_prefix,
};

use super::BedrockProvider;
use crate::aws_error::{VALIDATION_EXCEPTION_TYPE, aws_exception_type_is};

const ENVELOPE_HEAD: &str = "Unexpected value(s) ";
const ENVELOPE_TAIL: &str = " for the `anthropic-beta` header. Please consult our documentation at platform.claude.com/docs or try again without the header.";
const LIST_SEPARATOR: &str = ", ";

/// Parse the flags out of an exact beta-rejection envelope, bare or with the
/// Converse prefix. `None` when the message deviates from the envelope in any
/// byte, or when any listed token is not token-shaped.
pub(super) fn parse_rejected_beta_envelope(message: &str) -> Option<Vec<&str>> {
    let envelope = strip_converse_errors_prefix(message);
    let list = envelope
        .strip_prefix(ENVELOPE_HEAD)?
        .strip_suffix(ENVELOPE_TAIL)?;
    list.split(LIST_SEPARATOR)
        .map(|quoted| {
            let token = quoted.strip_prefix('`')?.strip_suffix('`')?;
            (!token.contains('`') && is_safe_token(token)).then_some(token)
        })
        .collect()
}

/// The flags to strip for a retry, or `None` when `err` is not a repairable
/// beta rejection for this request: not a 400 `ValidationException`, not the
/// exact naming envelope, a named flag the client did not send, or a named
/// flag the operator floor asserts. Duplicates collapse, first-seen order kept.
pub(super) fn repairable_rejected_betas(
    provider_id: &str,
    carrier: &str,
    err: &Error,
    client_betas: &[String],
    floor_betas: &[String],
) -> Option<Vec<String>> {
    let message = validation_400_message(err)?;
    let named = parse_rejected_beta_envelope(&message)?;
    let sent = |flag: &str| client_betas.iter().any(|b| b == flag);
    let floor = |flag: &str| floor_betas.iter().any(|b| b == flag);
    if let Some(refused) = named.iter().find(|flag| !sent(flag) || floor(flag)) {
        tracing::debug!(
            provider = %provider_id,
            carrier,
            flag = %sanitize_for_log(refused),
            in_request = sent(refused),
            operator_floor = floor(refused),
            "bedrock named beta rejection not repairable",
        );
        return None;
    }
    let mut flags: Vec<String> = Vec::with_capacity(named.len());
    for flag in named {
        if !flags.iter().any(|f| f == flag) {
            flags.push(flag.to_string());
        }
    }
    Some(flags)
}

/// The flat-envelope `message` of a 400 `ValidationException`, or `None` for
/// any other error. The body was already bounded when the error was built.
fn validation_400_message(err: &Error) -> Option<String> {
    let Error::Upstream {
        status: 400,
        body,
        upstream_type: Some(upstream_type),
        ..
    } = err
    else {
        return None;
    };
    if !aws_exception_type_is(upstream_type, VALIDATION_EXCEPTION_TYPE) {
        return None;
    }
    let parsed: Value = serde_json::from_str(body).ok()?;
    parsed.get("message")?.as_str().map(str::to_string)
}

/// `req` with every client-lifted beta in `flags` removed.
pub(super) fn without_client_betas(mut req: ChatRequest, flags: &[String]) -> ChatRequest {
    req.anthropic_beta.retain(|beta| !flags.contains(beta));
    req
}

impl BedrockProvider {
    /// Run `send` and repair one named beta rejection: when the upstream 400
    /// names client flags it rejects, retry once without exactly those flags.
    /// With `record`, a successful retry records them into the request's
    /// repair report, when one is installed. A retry failure is returned as
    /// is and records nothing.
    ///
    /// Each attempt is normalized exactly once, here, and `send` ships that
    /// body: normalization records translation telemetry, so a body that is
    /// never sent must never be built.
    pub(super) async fn with_beta_repair<T>(
        &self,
        req: ChatRequest,
        record: bool,
        send: for<'a> fn(&'a Self, &'a ChatRequest, Value) -> BoxFuture<'a, Result<T>>,
    ) -> Result<T> {
        use routectl_core::Provider;
        let carrier = self.cfg.api_shape.provider_kind_str();
        let floor = super::betas::operator_floor(&self.cfg, &req);
        let floor = floor.as_slice();
        let body = self.normalize_request(&req)?;
        let implied = feature_implied_betas_of(self.cfg.api_shape, &body);
        let err = match send(self, &req, body).await {
            Err(err) => err,
            ok => return ok,
        };
        let Some(flags) =
            repairable_rejected_betas(&self.cfg.id, carrier, &err, &req.anthropic_beta, floor)
        else {
            return Err(err);
        };
        if let Some(readded) = flags.iter().find(|flag| implied.contains(&flag.as_str())) {
            tracing::debug!(
                provider = %self.cfg.id,
                carrier,
                flag = %sanitize_for_log(readded),
                "bedrock named beta rejection not repairable: the request's own features re-add the flag",
            );
            return Err(err);
        }
        tracing::warn!(
            provider = %self.cfg.id,
            carrier,
            count = flags.len(),
            flags = %sanitize_for_log(&flags.join(",")),
            "bedrock rejected named beta flags; retrying once without them",
        );
        let report = record
            .then(|| req.routectl_internal.beta_repair_report.clone())
            .flatten();
        let retry = without_client_betas(req, &flags);
        let outcome = match self.normalize_request(&retry) {
            Ok(body) => send(self, &retry, body).await,
            Err(e) => Err(e),
        };
        match (&outcome, report) {
            (Ok(_), Some(report)) => {
                report.record(&flags);
                tracing::debug!(
                    provider = %self.cfg.id,
                    carrier,
                    count = flags.len(),
                    "bedrock beta retry succeeded; stripped flags reported",
                );
            }
            (Ok(_), None) => {}
            (Err(_), _) => tracing::warn!(
                provider = %self.cfg.id,
                carrier,
                count = flags.len(),
                "bedrock beta retry failed; nothing reported",
            ),
        }
        outcome
    }
}

/// The betas the builder unioned into `body` from its own fields. Stripping
/// client betas changes only `anthropic_beta`, so a retry body implies the
/// same set: any of these the upstream named would ship again.
fn feature_implied_betas_of(shape: super::BedrockApiShape, body: &Value) -> Vec<&'static str> {
    let fields = match shape {
        super::BedrockApiShape::Invoke => body.as_object(),
        super::BedrockApiShape::Converse => body["additionalModelRequestFields"].as_object(),
    };
    fields.map_or_else(Vec::new, |fields| {
        super::betas::feature_implied_betas(shape, fields)
    })
}

#[cfg(test)]
#[path = "beta_repair_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "beta_repair_wire_tests.rs"]
mod wire_tests;
