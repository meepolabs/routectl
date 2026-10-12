//! Per-attempt carriage of the bodies a wire check compares.
//!
//! The ingress body rides the canonical request; the egress body, the
//! upstream response and the edits a provider makes on either direction come
//! back through a [`WireTap`] whose clones share one slot. None of these
//! types render body bytes through `Debug`.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;

use super::assemble::{Assembled, SseFrame, StreamAssembler, StreamExit};
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
/// The response side holds a complete upstream body. A streamed response
/// folds frame by frame in the tap instead and is taken with
/// [`WireTap::finish_response_stream`].
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
/// A tap also carries whether its attempt was admitted for a full check,
/// and whether that check covers the response, so a provider can skip work
/// only a checked attempt reads. Both are fixed when the tap is created and
/// shared by every clone.
#[derive(Clone, Default)]
pub struct WireTap {
    slot: Arc<Mutex<Slot>>,
    admitted: bool,
    response_dialect: Option<IngressDialect>,
    #[cfg(any(test, feature = "test-utils"))]
    drop_on_deposit: Option<&'static str>,
}

impl WireTap {
    /// A tap whose attempt is admitted for a full request check.
    pub fn admitted() -> Self {
        Self {
            admitted: true,
            ..Self::default()
        }
    }

    /// A tap whose attempt is admitted for a full check of both directions,
    /// the response arriving in `dialect`.
    pub fn checking_response(dialect: IngressDialect) -> Self {
        Self {
            admitted: true,
            response_dialect: Some(dialect),
            ..Self::default()
        }
    }

    /// Whether this tap's attempt is admitted for a full check.
    pub const fn is_admitted(&self) -> bool {
        self.admitted
    }

    /// Whether this tap's attempt checks the response direction, so the
    /// provider should report what the upstream answered.
    pub const fn checks_response(&self) -> bool {
        self.response_dialect.is_some()
    }

    /// Record the request body as sent. A later deposit on the same attempt
    /// replaces an earlier one, so the slot holds the last body sent.
    pub fn deposit(&self, egress: Bytes) {
        #[cfg(any(test, feature = "test-utils"))]
        let egress = match self.drop_on_deposit {
            Some(pointer) => remove_pointer(&egress, pointer).unwrap_or(egress),
            None => egress,
        };
        self.lock().contents.request = Some(egress);
    }

    /// Record the complete upstream response body as received. A later
    /// deposit on the same attempt replaces an earlier one.
    pub fn deposit_response(&self, upstream: Bytes) {
        self.lock().contents.response = Some(upstream);
    }

    /// Fold one upstream stream frame into the tap's response assembler,
    /// creating it on the first frame. Does nothing on a tap that does not
    /// check the response, so an unchecked stream never builds an
    /// assembler.
    pub fn push_response_frame(&self, frame: SseFrame<'_>) {
        let Some(dialect) = self.response_dialect else {
            return;
        };
        let mut slot = self.lock();
        if slot.upstream_stream_taken {
            return;
        }
        slot.upstream_stream
            .get_or_insert_with(|| StreamAssembler::new(dialect))
            .push(frame);
    }

    /// Take the response assembler and finish it as the stream ended by
    /// `exit`. `None` when no frame was ever folded. Frames pushed after
    /// this are ignored, so a reader still running after the check ended
    /// builds nothing.
    pub fn finish_response_stream(&self, exit: StreamExit) -> Option<Assembled> {
        let assembler = {
            let mut slot = self.lock();
            slot.upstream_stream_taken = true;
            slot.upstream_stream.take()
        }?;
        Some(assembler.finish(exit))
    }

    /// Append an edit made to the request body.
    pub fn declare(&self, edit: DeclaredEdit) {
        self.lock().contents.request_edits.push(edit);
    }

    /// Append an edit made to the response body. An edit already declared
    /// is not appended again, so a site that declares once per stream frame
    /// keeps the list as long as the set of distinct edits. Nothing is
    /// appended once the response assembly has failed: that check can no
    /// longer complete, so no declaration can be consumed.
    pub fn declare_response(&self, edit: DeclaredEdit) {
        let mut slot = self.lock();
        if slot
            .upstream_stream
            .as_ref()
            .is_some_and(StreamAssembler::has_failed)
        {
            return;
        }
        let edits = &mut slot.contents.response_edits;
        if !edits.contains(&edit) {
            edits.push(edit);
        }
    }

    /// Return the request body and request edits reported so far and empty
    /// that side of the slot. The response side stays for its own check.
    pub fn take_request(&self) -> TapContents {
        let mut guard = self.lock();
        let slot = &mut guard.contents;
        TapContents {
            request: slot.request.take(),
            request_edits: std::mem::take(&mut slot.request_edits),
            ..TapContents::default()
        }
    }

    /// Return the response body and response edits reported so far and
    /// empty that side of the slot. The request side stays for its own check.
    pub fn take_response(&self) -> TapContents {
        let mut guard = self.lock();
        let slot = &mut guard.contents;
        TapContents {
            response: slot.response.take(),
            response_edits: std::mem::take(&mut slot.response_edits),
            ..TapContents::default()
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Slot> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The state every clone of one [`WireTap`] shares.
#[derive(Default)]
struct Slot {
    contents: TapContents,
    upstream_stream: Option<StreamAssembler>,
    upstream_stream_taken: bool,
}

#[cfg(any(test, feature = "test-utils"))]
impl WireTap {
    /// A tap that removes the member at `pointer` (an RFC 6901 pointer to an
    /// object member) from every request body deposited into it, simulating
    /// an egress that silently lost that key. A body that is not JSON, or has
    /// no such member, is stored unchanged.
    pub fn dropping_on_deposit(pointer: &'static str) -> Self {
        Self::default().with_drop_on_deposit(pointer)
    }

    /// This tap, also removing the member at `pointer` from every request
    /// body deposited into it, as [`Self::dropping_on_deposit`] does. The
    /// admission and response-check decisions are kept.
    #[must_use]
    pub const fn with_drop_on_deposit(mut self, pointer: &'static str) -> Self {
        self.drop_on_deposit = Some(pointer);
        self
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
        let slot = self.lock();
        f.debug_struct("WireTap")
            .field("admitted", &self.admitted)
            .field("response_dialect", &self.response_dialect)
            .field("contents", &slot.contents)
            .field("upstream_stream", &slot.upstream_stream)
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
