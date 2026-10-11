//! Per-attempt carriage of the bodies a wire check compares.
//!
//! The ingress body rides the canonical request; the egress body and the
//! edits a provider makes come back through a [`WireTap`] whose clones share
//! one slot. None of these types render body bytes through `Debug`.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;

use super::edits::DeclaredEdit;
use super::lane::IngressDialect;
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

/// What a provider reported through a [`WireTap`] on one attempt.
#[derive(Clone, Default)]
pub struct TapContents {
    /// The serialized body the provider sent, if it deposited one.
    pub egress: Option<Bytes>,
    /// The edits the provider declared, in declaration order.
    pub edits: Vec<DeclaredEdit>,
}

impl fmt::Debug for TapContents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TapContents")
            .field("egress_len", &self.egress.as_ref().map(Bytes::len))
            .field("edits", &self.edits)
            .finish()
    }
}

/// Shared, clone-cheap slot through which a provider reports the body it
/// sent and the edits it made on one attempt. Every clone shares one slot,
/// so the copy the router keeps observes what the provider records on the
/// copy that rode the request.
#[derive(Clone, Default)]
pub struct WireTap {
    slot: Arc<Mutex<TapContents>>,
    #[cfg(any(test, feature = "test-utils"))]
    drop_on_deposit: Option<&'static str>,
}

impl WireTap {
    /// Record the body as sent. A later deposit on the same attempt replaces
    /// an earlier one, so the slot holds the last body sent.
    pub fn deposit(&self, egress: Bytes) {
        #[cfg(any(test, feature = "test-utils"))]
        let egress = match self.drop_on_deposit {
            Some(pointer) => remove_pointer(&egress, pointer).unwrap_or(egress),
            None => egress,
        };
        self.lock().egress = Some(egress);
    }

    /// Append a declared edit.
    pub fn declare(&self, edit: DeclaredEdit) {
        self.lock().edits.push(edit);
    }

    /// Return everything reported so far and empty the slot.
    pub fn take(&self) -> TapContents {
        std::mem::take(&mut *self.lock())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TapContents> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl WireTap {
    /// A tap that removes the member at `pointer` (an RFC 6901 pointer to an
    /// object member) from every body deposited into it, simulating an
    /// egress that silently lost that key. A body that is not JSON, or has
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
            .field("contents", &*self.lock())
            .finish()
    }
}

/// Borrowed view of one upstream attempt: what the client sent, the
/// canonical request, what routectl sent, the edits it declared and the
/// upstream rejection, if any. Lives only as long as the attempt.
#[derive(Clone, Copy)]
pub struct AttemptRecord<'a> {
    /// The body as the client sent it.
    pub ingress: &'a IngressWire,
    /// The canonical request this attempt was built from.
    pub canonical: &'a ChatRequest,
    /// The body as routectl sent it; `None` when the provider deposited
    /// nothing.
    pub egress: Option<&'a Bytes>,
    /// Every edit declared on this attempt, router and provider alike.
    pub edits: &'a [DeclaredEdit],
    /// The upstream's rejection of this attempt, if it rejected it.
    pub rejection: Option<&'a Error>,
}

impl fmt::Debug for AttemptRecord<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AttemptRecord")
            .field("ingress", self.ingress)
            .field("egress_len", &self.egress.map(Bytes::len))
            .field("edits", &self.edits)
            .field("rejected", &self.rejection.is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
#[allow(clippy::panic)]
#[path = "carriage_tests.rs"]
mod tests;
