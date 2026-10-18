//! Egress-side reporting into the per-attempt wire tap: the body bytes a
//! provider sends and the body edits it declares on the way.
//!
//! Every entry point is a no-op when the request carries no tap, so an
//! attempt nobody checks pays one `Option` test per site.

#[cfg(any(
    feature = "openai-compat",
    feature = "openai-responses",
    feature = "anthropic-api"
))]
use std::borrow::Cow;

use bytes::Bytes;
use routectl_core::ChatRequest;
use routectl_core::wire_check::WireTap;
#[cfg(any(
    feature = "openai-compat",
    feature = "openai-responses",
    feature = "anthropic-api"
))]
use routectl_core::wire_check::{DeclaredEdit, EditKind, EditSite};

/// The tap riding `req`, if a caller is listening.
pub const fn tap(req: &ChatRequest) -> Option<&WireTap> {
    req.routectl_internal.wire_tap.as_ref()
}

/// Serialize the outgoing body once and record those exact bytes in the
/// tap. The returned bytes are what the caller sends: the tap holds a
/// refcount on the same buffer, never a second serialization.
#[cfg(any(
    feature = "openai-compat",
    feature = "openai-responses",
    feature = "gemini"
))]
pub fn serialize_and_deposit(
    req: &ChatRequest,
    body: &serde_json::Value,
) -> serde_json::Result<Bytes> {
    let bytes = Bytes::from(serde_json::to_vec(body)?);
    if let Some(tap) = tap(req) {
        tap.deposit(bytes.clone());
    }
    Ok(bytes)
}

/// Record the complete upstream response body when the attempt checks its
/// response, and hand the same buffer back for parsing: the tap holds a
/// refcount on it, never a copy.
#[cfg(any(
    feature = "openai-compat",
    feature = "anthropic-api",
    feature = "bedrock"
))]
pub fn deposit_response(req: &ChatRequest, body: Vec<u8>) -> Bytes {
    let body = Bytes::from(body);
    if let Some(tap) = tap(req).filter(|tap| tap.checks_response()) {
        tap.deposit_response(body.clone());
    }
    body
}

/// A handle on the tap for a stream reader to fold upstream frames into,
/// present only when the attempt folds its response. A stream nobody
/// folds gets `None` and pays one `Option` test per frame.
#[cfg(any(
    feature = "openai-compat",
    feature = "openai-responses",
    feature = "anthropic-api",
    feature = "bedrock"
))]
pub fn response_tap(req: &ChatRequest) -> Option<WireTap> {
    tap(req).filter(|tap| tap.folds_response()).cloned()
}

/// Fold one upstream server-sent event into `tap`'s response assembler.
#[cfg(any(
    feature = "openai-compat",
    feature = "openai-responses",
    feature = "anthropic-api"
))]
pub fn push_frame(tap: Option<&WireTap>, event: &str, data: &str) {
    if let Some(tap) = tap {
        tap.push_response_frame(routectl_core::wire_check::SseFrame::new(Some(event), data));
    }
}

/// Declare one request-body edit. A pattern the edit register refuses is
/// dropped with a DEBUG naming only the site: the unexplained difference
/// then surfaces as a finding, which is the safe direction.
#[cfg(any(
    feature = "openai-compat",
    feature = "openai-responses",
    feature = "anthropic-api"
))]
pub fn declare(tap: &WireTap, site: EditSite, path: impl Into<Cow<'static, str>>, kind: EditKind) {
    match DeclaredEdit::new(site, path, kind) {
        Ok(edit) => tap.declare(edit),
        Err(e) => tracing::debug!(
            site = site.label(),
            error = %e,
            "wire-check edit declaration refused"
        ),
    }
}

/// Declare one response-body edit on a tap that checks the response; see
/// [`declare`] for how a refused pattern is handled.
#[cfg(feature = "openai-compat")]
pub fn declare_response(tap: Option<&WireTap>, site: EditSite, path: &'static str, kind: EditKind) {
    let Some(tap) = tap.filter(|tap| tap.checks_response()) else {
        return;
    };
    match DeclaredEdit::new(site, path, kind) {
        Ok(edit) => tap.declare_response(edit),
        Err(e) => tracing::debug!(
            site = site.label(),
            error = %e,
            "wire-check response edit declaration refused"
        ),
    }
}

/// RFC 6901 pointer for member `key` under `parent` (`""` for the top
/// level), or `None` when the key is not token-shaped: such a key renders
/// as a wildcard in a finding, and a top-level wildcard pattern is refused
/// as a blanket exemption.
#[cfg(feature = "openai-compat")]
pub fn member_pointer(parent: &str, key: &str) -> Option<String> {
    routectl_core::is_safe_token(key)
        .then(|| format!("{parent}/{}", key.replace('~', "~0").replace('/', "~1")))
}

/// Declare the body `stream` flag the provider forces to `forced`: an Add
/// when the canonical request left it unset, a Rewrite when it asked for
/// the other mode.
#[cfg(any(
    feature = "openai-compat",
    feature = "openai-responses",
    feature = "anthropic-api"
))]
pub fn declare_stream_flag(req: &ChatRequest, forced: bool) {
    let Some(tap) = tap(req) else {
        return;
    };
    match req.stream {
        None => declare(tap, EditSite::StreamFlagForce, "/stream", EditKind::Add),
        Some(requested) if requested != forced => {
            declare(tap, EditSite::StreamFlagForce, "/stream", EditKind::Rewrite);
        }
        Some(_) => {}
    }
}

#[cfg(all(test, feature = "openai-compat"))]
mod tests {
    use super::member_pointer;

    #[test]
    fn member_pointer_escapes_token_keys_and_refuses_the_rest() {
        let rows: [(&str, &str, &str, Option<&str>); 4] = [
            ("plain key", "", "metadata", Some("/metadata")),
            (
                "nested key",
                "/client_metadata",
                "a~b",
                Some("/client_metadata/a~0b"),
            ),
            ("slash key", "", "a/b", Some("/a~1b")),
            ("whitespace key", "", "has space", None),
        ];
        for (name, parent, key, want) in rows {
            assert_eq!(member_pointer(parent, key).as_deref(), want, "{name}");
        }
    }
}
