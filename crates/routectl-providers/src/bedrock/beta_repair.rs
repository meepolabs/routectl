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
//! flag that appears after [`super::betas::BEDROCK_REJECTED_BETAS`] was cut.
//!
//! The provider strips exactly the named flags and retries once. Only a
//! successful inference retry records them in the lane's [`RejectedBetaMemo`],
//! which later requests consult before egress; a CountTokens retry is never
//! recorded. The floor is [`super::betas::operator_floor`]. A flag is repairable only when the
//! message is the exact envelope, every token is token-shaped, every token was
//! lifted from the client, and none is an operator-floor flag: the floor is the
//! operator's explicit override and is never second-guessed here.

use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};

use futures::future::BoxFuture;
use serde_json::Value;

use routectl_core::{ChatRequest, Error, Result, is_safe_token, sanitize_for_log};

use super::BedrockProvider;
use crate::aws_error::{VALIDATION_EXCEPTION_TYPE, aws_exception_type_is};

/// The wrapper Converse puts in front of the upstream model's message.
const CONVERSE_ERRORS_PREFIX: &str = "The model returned the following errors: ";
const ENVELOPE_HEAD: &str = "Unexpected value(s) ";
const ENVELOPE_TAIL: &str = " for the `anthropic-beta` header. Please consult our documentation at platform.claude.com/docs or try again without the header.";
const LIST_SEPARATOR: &str = ", ";

/// Most distinct flags one lane remembers. Every entry is a client-sent flag
/// AWS named, so a real lane holds a handful; the cap bounds memory against an
/// upstream that keeps naming new ones, and the oldest entry is evicted so a
/// full set still learns.
pub(super) const MAX_REMEMBERED_REJECTED_BETAS: usize = 32;

/// Parse the flags out of an exact beta-rejection envelope, bare or with the
/// Converse prefix. `None` when the message deviates from the envelope in any
/// byte, or when any listed token is not token-shaped.
pub(super) fn parse_rejected_beta_envelope(message: &str) -> Option<Vec<&str>> {
    let envelope = message
        .strip_prefix(CONVERSE_ERRORS_PREFIX)
        .unwrap_or(message);
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

/// The per-lane set of flags a successful retry proved AWS rejects, oldest
/// first.
#[derive(Debug, Default)]
pub(super) struct RejectedBetaMemo {
    flags: Mutex<VecDeque<String>>,
}

impl RejectedBetaMemo {
    /// Record `flags`; returns how many were newly added. Past
    /// [`MAX_REMEMBERED_REJECTED_BETAS`] the oldest entry is evicted.
    pub(super) fn remember(&self, flags: &[String]) -> usize {
        let mut remembered = self.flags.lock().unwrap_or_else(PoisonError::into_inner);
        let mut added = 0;
        for flag in flags {
            if remembered.contains(flag) {
                continue;
            }
            if remembered.len() >= MAX_REMEMBERED_REJECTED_BETAS {
                remembered.pop_front();
            }
            remembered.push_back(flag.clone());
            added += 1;
        }
        added
    }

    /// `req` with every remembered flag removed from its client-lifted betas.
    /// Operator-floor flags are never stripped.
    pub(super) fn strip_remembered(
        &self,
        provider_id: &str,
        carrier: &str,
        req: ChatRequest,
        floor_betas: &[String],
    ) -> ChatRequest {
        if req.anthropic_beta.is_empty() {
            return req;
        }
        let strip: Vec<String> = {
            let remembered = self.flags.lock().unwrap_or_else(PoisonError::into_inner);
            req.anthropic_beta
                .iter()
                .filter(|beta| remembered.contains(*beta) && !floor_betas.contains(beta))
                .cloned()
                .collect()
        };
        if strip.is_empty() {
            return req;
        }
        tracing::debug!(
            provider = %provider_id,
            carrier,
            count = strip.len(),
            flags = %sanitize_for_log(&strip.join(",")),
            "withholding beta flags this lane previously confirmed Bedrock rejects",
        );
        without_client_betas(req, &strip)
    }
}

impl BedrockProvider {
    /// Run `send` with this lane's remembered rejected betas withheld, and
    /// repair one named beta rejection: when the upstream 400 names client
    /// flags it rejects, retry once without exactly those flags. With
    /// `remember_on_success`, a successful retry records them for the lane.
    /// A retry failure is returned as is.
    pub(super) async fn with_beta_repair<T>(
        &self,
        req: ChatRequest,
        remember_on_success: bool,
        send: for<'a> fn(&'a Self, &'a ChatRequest) -> BoxFuture<'a, Result<T>>,
    ) -> Result<T> {
        let carrier = self.cfg.api_shape.provider_kind_str();
        let floor = super::betas::operator_floor(&self.cfg, &req);
        let floor = floor.as_slice();
        let req = self
            .rejected_betas
            .strip_remembered(&self.cfg.id, carrier, req, floor);
        let err = match send(self, &req).await {
            Err(err) => err,
            ok => return ok,
        };
        let Some(flags) =
            repairable_rejected_betas(&self.cfg.id, carrier, &err, &req.anthropic_beta, floor)
        else {
            return Err(err);
        };
        tracing::warn!(
            provider = %self.cfg.id,
            carrier,
            count = flags.len(),
            flags = %sanitize_for_log(&flags.join(",")),
            "bedrock rejected named beta flags; retrying once without them",
        );
        let outcome = send(self, &without_client_betas(req, &flags)).await;
        if outcome.is_ok() && remember_on_success {
            let added = self.rejected_betas.remember(&flags);
            tracing::debug!(
                provider = %self.cfg.id,
                carrier,
                count = flags.len(),
                added,
                "bedrock beta retry succeeded; flags withheld on this lane from now on",
            );
        } else if outcome.is_err() {
            tracing::warn!(
                provider = %self.cfg.id,
                carrier,
                count = flags.len(),
                "bedrock beta retry failed; nothing remembered",
            );
        }
        outcome
    }
}

#[cfg(test)]
#[path = "beta_repair_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "beta_repair_wire_tests.rs"]
mod wire_tests;
