//! Per-attempt carriage of the bodies a wire check compares.
//!
//! The ingress body rides the canonical request; the egress body, the
//! upstream response and the edits a provider makes on either direction come
//! back through a [`WireTap`] whose clones share one slot. None of these types render body bytes through `Debug`.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;

use super::edits::DeclaredEdit;
use super::lane::{IngressDialect, Leg};
use crate::error::Error;
use crate::schema::ChatRequest;

/// The body exactly as the client sent it, captured once after parsing.
#[derive(Clone)]
pub struct IngressWire {
    /// The ingress dialect that parsed the body.
    pub dialect: IngressDialect,
    /// The client's version token, when the client identified one.
    pub client_version: Option<String>,
    /// The raw request body.
    pub bytes: Bytes,
}

impl fmt::Debug for IngressWire {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IngressWire")
            .field("dialect", &self.dialect)
            .field("client_version", &self.client_version)
            .field("bytes_len", &self.bytes.len())
            .finish()
    }
}

/// What a provider reported through a [`WireTap`] on one attempt, kept per
/// direction.
///
/// The response side holds a complete upstream body today. A streamed
/// response folds frame by frame instead; its fold state takes a slot of its
/// own beside the complete one, which the private fields leave room for.
#[derive(Clone, Default)]
pub struct TapContents {
    request: Option<Bytes>,
    response: Option<Bytes>,
    request_edits: Vec<DeclaredEdit>,
    response_edits: Vec<DeclaredEdit>,
}

impl TapContents {
    /// The request body the provider sent upstream, if it deposited one.
    pub const fn request(&self) -> Option<&Bytes> {
        self.request.as_ref()
    }

    /// The complete upstream response body, if the provider deposited one.
    pub const fn response(&self) -> Option<&Bytes> {
        self.response.as_ref()
    }

    /// The edits declared on `leg`, in declaration order.
    pub fn edits(&self, leg: Leg) -> &[DeclaredEdit] {
        match leg {
            Leg::Request => &self.request_edits,
            Leg::Response => &self.response_edits,
        }
    }
}

impl fmt::Debug for TapContents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TapContents")
            .field("request_len", &self.request.as_ref().map(Bytes::len))
            .field("response_len", &self.response.as_ref().map(Bytes::len))
            .field("request_edits", &self.request_edits)
            .field("response_edits", &self.response_edits)
            .finish()
    }
}

/// Shared, clone-cheap slot through which a provider reports, per
/// direction, the body that crossed the wire and the edits it made on one
/// attempt. Every clone shares one slot, so the copy the router keeps
/// observes what the provider records on the copy that rode the request.
///
/// A tap also carries whether its attempt was admitted for a full check, so
/// a provider can skip work only an admitted attempt reads. Admission is
/// fixed when the tap is created and shared by every clone.
#[derive(Clone, Default)]
pub struct WireTap {
    slot: Arc<Mutex<TapContents>>,
    admitted: bool,
    #[cfg(any(test, feature = "test-utils"))]
    drop_on_deposit: Option<&'static str>,
}

impl WireTap {
    /// A tap whose attempt is admitted for a full check.
    pub fn admitted() -> Self {
        Self {
            admitted: true,
            ..Self::default()
        }
    }

    /// Whether this tap's attempt is admitted for a full check.
    pub const fn is_admitted(&self) -> bool {
        self.admitted
    }

    /// Record the request body as sent. A later deposit on the same attempt
    /// replaces an earlier one, so the slot holds the last body sent.
    pub fn deposit(&self, egress: Bytes) {
        #[cfg(any(test, feature = "test-utils"))]
        let egress = match self.drop_on_deposit {
            Some(pointer) => remove_pointer(&egress, pointer).unwrap_or(egress),
            None => egress,
        };
        self.lock().request = Some(egress);
    }

    /// Record the complete upstream response body as received. A later
    /// deposit on the same attempt replaces an earlier one.
    pub fn deposit_response(&self, upstream: Bytes) {
        self.lock().response = Some(upstream);
    }

    /// Append an edit made to the request body.
    pub fn declare(&self, edit: DeclaredEdit) {
        self.lock().request_edits.push(edit);
    }

    /// Append an edit made to the response body.
    pub fn declare_response(&self, edit: DeclaredEdit) {
        self.lock().response_edits.push(edit);
    }

    /// Return the request body and request edits reported so far and empty
    /// that side of the slot. The response side stays for its own check.
    pub fn take_request(&self) -> TapContents {
        let mut slot = self.lock();
        TapContents {
            request: slot.request.take(),
            request_edits: std::mem::take(&mut slot.request_edits),
            ..TapContents::default()
        }
    }

    /// Return the response body and response edits reported so far and
    /// empty that side of the slot. The request side stays for its own check.
    pub fn take_response(&self) -> TapContents {
        let mut slot = self.lock();
        TapContents {
            response: slot.response.take(),
            response_edits: std::mem::take(&mut slot.response_edits),
            ..TapContents::default()
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TapContents> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl WireTap {
    /// A tap that removes the member at `pointer` (an RFC 6901 pointer to an
    /// object member) from every request body deposited into it, simulating
    /// an egress that silently lost that key. A body that is not JSON, or has
    /// no such member, is stored unchanged.
    pub fn dropping_on_deposit(pointer: &'static str) -> Self {
        Self {
            drop_on_deposit: Some(pointer),
            ..Self::default()
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
fn remove_pointer(body: &Bytes, pointer: &str) -> Option<Bytes> {
    let (parent, member) = pointer.rsplit_once('/')?;
    let member = member.replace("~1", "/").replace("~0", "~");
    let mut doc: serde_json::Value = serde_json::from_slice(body).ok()?;
    doc.pointer_mut(parent)?.as_object_mut()?.remove(&member)?;
    serde_json::to_vec(&doc).ok().map(Bytes::from)
}

impl fmt::Debug for WireTap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WireTap")
            .field("admitted", &self.admitted)
            .field("contents", &*self.lock())
            .finish()
    }
}

/// Borrowed view of one upstream attempt: what the client sent, the
/// canonical request, what routectl sent and the edits it declared on each
/// direction, the upstream response and the upstream rejection, if any.
/// Lives only as long as the attempt.
#[derive(Clone, Copy)]
pub struct AttemptRecord<'a> {
    ingress: &'a IngressWire,
    canonical: &'a ChatRequest,
    egress: Option<&'a Bytes>,
    request_edits: &'a [DeclaredEdit],
    upstream_response: Option<&'a Bytes>,
    response_edits: &'a [DeclaredEdit],
    rejection: Option<&'a Error>,
}

impl<'a> AttemptRecord<'a> {
    /// A record of the request direction: the client's body, the canonical
    /// request, the body routectl sent (`None` when the provider deposited
    /// nothing) and every request edit declared, router and provider alike.
    /// No response and no rejection until added.
    pub const fn new(
        ingress: &'a IngressWire,
        canonical: &'a ChatRequest,
        egress: Option<&'a Bytes>,
        request_edits: &'a [DeclaredEdit],
    ) -> Self {
        Self {
            ingress,
            canonical,
            egress,
            request_edits,
            upstream_response: None,
            response_edits: &[],
            rejection: None,
        }
    }

    /// This record with the response direction: the upstream response body
    /// (`None` when the provider deposited nothing) and every response edit
    /// declared.
    #[must_use]
    pub const fn with_response(
        self,
        upstream_response: Option<&'a Bytes>,
        response_edits: &'a [DeclaredEdit],
    ) -> Self {
        Self {
            upstream_response,
            response_edits,
            ..self
        }
    }

    /// This record with the upstream's rejection of the attempt.
    #[must_use]
    pub const fn with_rejection(self, rejection: &'a Error) -> Self {
        Self {
            rejection: Some(rejection),
            ..self
        }
    }

    /// The body as the client sent it.
    pub const fn ingress(&self) -> &'a IngressWire {
        self.ingress
    }

    /// The canonical request this attempt was built from.
    pub const fn canonical(&self) -> &'a ChatRequest {
        self.canonical
    }

    /// The request body as routectl sent it.
    pub const fn egress(&self) -> Option<&'a Bytes> {
        self.egress
    }

    /// The response body as the upstream sent it.
    pub const fn upstream_response(&self) -> Option<&'a Bytes> {
        self.upstream_response
    }

    /// Every edit declared on `leg`.
    pub const fn edits(&self, leg: Leg) -> &'a [DeclaredEdit] {
        match leg {
            Leg::Request => self.request_edits,
            Leg::Response => self.response_edits,
        }
    }

    /// The upstream's rejection of this attempt, if it rejected it.
    pub const fn rejection(&self) -> Option<&'a Error> {
        self.rejection
    }
}

impl fmt::Debug for AttemptRecord<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AttemptRecord")
            .field("ingress", self.ingress)
            .field("egress_len", &self.egress.map(Bytes::len))
            .field("request_edits", &self.request_edits)
            .field(
                "upstream_response_len",
                &self.upstream_response.map(Bytes::len),
            )
            .field("response_edits", &self.response_edits)
            .field("rejected", &self.rejection.is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
#[allow(clippy::panic)]
#[path = "carriage_tests.rs"]
mod tests;
