//! Forwarded-mode (pure-proxy) ingress admission gate.
//!
//! Runs the forwarded-mode rejection matrix at the shared ingress
//! driver, before body parse and dispatch: a forwarded-mode request must
//! arrive on the Anthropic dialect, through the MITM proxy, carrying an
//! inbound `Authorization` bearer and a client session id, or it is
//! rejected with a dialect-correct error envelope and a
//! `pure_proxy_rejections_total{reason}` bump. A no-op in own mode -- every
//! non-forwarded request is byte-identical to the pre-passthrough path.

use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use routectl_router::config::CredentialSource;

use crate::handlers::ingress_handle::{
    error_response, extract_authorization_bearer, session_id_of,
};
use crate::handlers::pure_proxy_metrics::{PureProxyRejectionReason, record_rejection};
use crate::ingress::ErrorEnvelopeShape;

/// Forwarded-mode (pure-proxy) ingress admission gate. Runs the
/// forwarded-mode rejection matrix and, on a rejection, records the
/// `pure_proxy_rejections_total{reason}` counter + the structured rejection
/// log, then returns the dialect-correct error `Response`. Returns `None`
/// (admit) for own mode and for a well-formed forwarded request.
///
/// The dialect is read straight off `envelope`: the Anthropic ingress uses
/// `ErrorEnvelopeShape::Anthropic`; the OpenAI chat-completions and Responses
/// ingresses both use `ErrorEnvelopeShape::OpenAi`. That is the signal the
/// non-Anthropic-dialect check keys on, so gating here (the shared driver all
/// three handlers funnel through) covers every dialect at one point.
pub(crate) fn enforce_pure_proxy_admission(
    headers: &HeaderMap,
    router: &routectl_router::Router,
    envelope: ErrorEnvelopeShape,
) -> Option<Response> {
    let forwarded = matches!(
        router.config.mitm.as_ref().map(|m| m.credential_source),
        Some(CredentialSource::Forwarded)
    );
    let is_anthropic_dialect = envelope == ErrorEnvelopeShape::Anthropic;
    let seam_present = headers.contains_key(crate::ingress::MITM_PROXIED_HEADER);
    let has_bearer = extract_authorization_bearer(headers).is_some();
    let has_session_id = session_id_of(headers).is_some();

    let reason = classify_pure_proxy_rejection(PureProxyAdmissionInputs {
        forwarded,
        is_anthropic_dialect,
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
    /// `[mitm] credential_source == Forwarded` (config-is-the-capability).
    pub(crate) forwarded: bool,
    /// The request arrived on the Anthropic dialect (`/v1/messages`); false
    /// for the OpenAI chat-completions and Responses dialects.
    pub(crate) is_anthropic_dialect: bool,
    /// The `x-routectl-mitm-proxied` seam header is present
    /// (header-is-a-hint: it arrived through the MITM inference path).
    pub(crate) seam_present: bool,
    /// A usable inbound `Authorization` bearer is present.
    pub(crate) has_bearer: bool,
    /// The `x-claude-code-session-id` identity header is present.
    pub(crate) has_session_id: bool,
}

/// Pure decision core for the forwarded-mode admission matrix. Returns the
/// rejection reason, or `None` to admit. Own mode (`!forwarded`) ALWAYS
/// admits -- none of these checks fire -- so a non-forwarded request stays
/// byte-identical to the pre-passthrough path.
///
/// Precedence (only when `forwarded`):
/// 1. non-Anthropic dialect -> `NonAnthropicDialect`. The dialect itself is
///    disqualifying, independent of the seam header.
/// 2. Anthropic dialect, seam header ABSENT -> `NotMitm` (a direct :9100
///    loopback client -- not a valid pure-proxy path).
/// 3. Anthropic dialect, seam header PRESENT, no bearer -> `TokenMissing`
///    (CC not logged into claude.ai). Checked BEFORE the session id so a
///    request missing both surfaces the more fundamental missing credential.
/// 4. Anthropic dialect, seam header PRESENT, bearer present, no session
///    id -> `IdentityMissing` (fail before minting identity).
pub(crate) const fn classify_pure_proxy_rejection(
    inputs: PureProxyAdmissionInputs,
) -> Option<PureProxyRejectionReason> {
    if !inputs.forwarded {
        return None;
    }
    if !inputs.is_anthropic_dialect {
        return Some(PureProxyRejectionReason::NonAnthropicDialect);
    }
    if !inputs.seam_present {
        return Some(PureProxyRejectionReason::NotMitm);
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
        PureProxyRejectionReason::NotMitm => {
            "forwarded (pure-proxy) mode accepts Anthropic-dialect requests \
             only through the MITM proxy"
        }
        PureProxyRejectionReason::IdentityMissing => {
            "forwarded (pure-proxy) mode requires the x-claude-code-session-id \
             identity header"
        }
        PureProxyRejectionReason::NonAnthropicDialect => {
            "forwarded (pure-proxy) mode supports the Anthropic dialect only"
        }
    };
    format!("{detail} (reason={})", reason.as_str())
}
