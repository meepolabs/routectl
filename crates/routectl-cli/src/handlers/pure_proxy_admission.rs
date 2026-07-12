//! Forwarded-mode (pure-proxy) ingress admission gate.
//!
//! Runs the forwarded-mode rejection matrix at the shared ingress
//! driver, before body parse and dispatch: a request that arrived through
//! the MITM inference path (the `x-routectl-mitm-proxied` seam header,
//! carrying the process's [`crate::ingress::MitmSeamNonce`] value) must
//! carry an inbound `Authorization` bearer and a client session id, or it is
//! rejected with a dialect-correct error envelope and a
//! `pure_proxy_rejections_total{reason}` bump. A no-op for every request
//! that did NOT arrive through that path -- own-provider and non-Anthropic-
//! dialect traffic is admitted untouched even while a forwarded provider is
//! configured.

use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;

use crate::handlers::ingress_handle::{
    error_response, extract_authorization_bearer, session_id_of,
};
use crate::handlers::pure_proxy_metrics::{PureProxyRejectionReason, record_rejection};
use crate::ingress::{ErrorEnvelopeShape, MitmSeamNonce};

/// Forwarded-mode (pure-proxy) ingress admission gate. Runs the
/// forwarded-mode rejection matrix and, on a rejection, records the
/// `pure_proxy_rejections_total{reason}` counter + the structured rejection
/// log, then returns the dialect-correct error `Response`. Returns `None`
/// (admit) for every request that did not arrive through the MITM
/// inference path, and for a well-formed one that did.
///
/// The dialect is read straight off `envelope`: the Anthropic ingress uses
/// `ErrorEnvelopeShape::Anthropic`; the OpenAI chat-completions and Responses
/// ingresses both use `ErrorEnvelopeShape::OpenAi`. It only affects how a
/// rejection is RENDERED here -- the seam header this gate keys on is
/// stamped exclusively on the Anthropic MITM inference leg, so it is never
/// present on the other two dialects in practice.
///
/// `seam_nonce` is the process's [`MitmSeamNonce`] (from `AppState`): the
/// header must carry ITS value, not merely be present, or this gate treats
/// the request as seam-absent (see `MitmSeamNonce::is_present_in`).
pub(crate) fn enforce_pure_proxy_admission(
    headers: &HeaderMap,
    envelope: ErrorEnvelopeShape,
    seam_nonce: &MitmSeamNonce,
) -> Option<Response> {
    let seam_present = seam_nonce.is_present_in(headers);
    let has_bearer = extract_authorization_bearer(headers).is_some();
    let has_session_id = session_id_of(headers).is_some();

    let reason = classify_pure_proxy_rejection(PureProxyAdmissionInputs {
        seam_present,
        has_bearer,
        has_session_id,
    })?;
    // SAFE dimensions only: `has_session_id` is the boolean the log carries;
    // the token itself is never touched here (or captured, on this path).
    record_rejection(reason, has_session_id);
    Some(render_pure_proxy_rejection(envelope, reason))
}

/// The SAFE, request-derived facts the forwarded-mode admission matrix
/// decides on. Booleans only -- never a token, header, or body value -- so
/// the decision core cannot depend on (or leak) request content.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PureProxyAdmissionInputs {
    /// The `x-routectl-mitm-proxied` seam header carries the process's
    /// nonce value (not merely present) -- the request arrived through the
    /// MITM inference path. Every other check in this matrix is gated on
    /// this being true; a request without a matching value is admitted
    /// untouched regardless of dialect or config.
    pub(crate) seam_present: bool,
    /// A usable inbound `Authorization` bearer is present.
    pub(crate) has_bearer: bool,
    /// The `x-claude-code-session-id` identity header is present.
    pub(crate) has_session_id: bool,
}

/// Pure decision core for the forwarded-mode admission matrix. Returns the
/// rejection reason, or `None` to admit. A request with the seam header
/// ABSENT ALWAYS admits -- none of these checks fire -- so own-provider and
/// non-Anthropic-dialect traffic is untouched even while a forwarded
/// provider is configured.
///
/// Precedence (only when `seam_present`):
/// 1. no bearer -> `TokenMissing` (CC not logged into claude.ai). Checked
///    BEFORE the session id so a request missing both surfaces the more
///    fundamental missing credential.
/// 2. bearer present, no session id -> `IdentityMissing` (fail before
///    minting identity).
pub(crate) const fn classify_pure_proxy_rejection(
    inputs: PureProxyAdmissionInputs,
) -> Option<PureProxyRejectionReason> {
    if !inputs.seam_present {
        return None;
    }
    if !inputs.has_bearer {
        return Some(PureProxyRejectionReason::TokenMissing);
    }
    if !inputs.has_session_id {
        return Some(PureProxyRejectionReason::IdentityMissing);
    }
    None
}

/// Build the dialect-correct error envelope for a forwarded-mode admission
/// rejection, reusing the shared `error_response` / `anthropic_error_type`
/// mapping (Anthropic envelope for the Anthropic path, OpenAI-shaped for the
/// OpenAI / Responses path). The client message carries the safe `reason=`
/// tag -- never the token or any request-derived value.
pub(crate) fn render_pure_proxy_rejection(
    shape: ErrorEnvelopeShape,
    reason: PureProxyRejectionReason,
) -> Response {
    let status = reason.status();
    // Route the internal err_type through the same status -> vocab table the
    // rest of the ingress uses: a 401 becomes `authentication_error`, a 400
    // becomes `invalid_request_error` on the Anthropic path; the OpenAI shape
    // surfaces the tag verbatim.
    let err_type = if status == StatusCode::UNAUTHORIZED {
        "authentication_error"
    } else {
        "bad_request"
    };
    let message = pure_proxy_rejection_message(reason);
    error_response(shape, status, err_type, &message, err_type, None, None)
}

/// Operator-actionable, token-free client message per rejection reason. Each
/// carries the safe `reason=<...>` tag so an SDK / operator can branch on it
/// without parsing prose.
fn pure_proxy_rejection_message(reason: PureProxyRejectionReason) -> String {
    let detail = match reason {
        PureProxyRejectionReason::TokenMissing => {
            "forwarded (pure-proxy) mode requires an inbound Authorization \
             bearer; log Claude Code into claude.ai"
        }
        PureProxyRejectionReason::IdentityMissing => {
            "forwarded (pure-proxy) mode requires the x-claude-code-session-id \
             identity header"
        }
    };
    format!("{detail} (reason={})", reason.as_str())
}
