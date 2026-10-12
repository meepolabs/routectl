//! The edits an ingress renderer makes to the response on purpose, declared
//! at the site that makes them so the response leg of the wire check can
//! tell them from a loss. Recording is off unless the attempt's response is
//! checked, so an unchecked render pays one bool test per site.

use std::borrow::Cow;

use routectl_core::wire_check::{DeclaredEdit, EditKind, EditSite, WirePath};

/// Collector a renderer declares its response edits into.
#[derive(Debug, Default)]
pub struct ResponseEdits {
    recording: bool,
    edits: Vec<DeclaredEdit>,
}

impl ResponseEdits {
    /// A collector that keeps what is declared.
    pub const fn recording() -> Self {
        Self {
            recording: true,
            edits: Vec::new(),
        }
    }

    /// Declare one edit. Ignored unless recording; a repeat of an edit
    /// already declared is kept once. A pattern the edit register refuses
    /// is dropped with a DEBUG naming only the site, so the unexplained
    /// difference surfaces as a finding.
    pub fn declare(&mut self, site: EditSite, path: &'static str, kind: EditKind) {
        if self.recording {
            self.record(site, Cow::Borrowed(path), kind);
        }
    }

    /// [`Self::declare`] at a pointer built only when recording, for a path
    /// that carries an array index known at render time.
    pub fn declare_at(&mut self, site: EditSite, path: impl FnOnce() -> WirePath, kind: EditKind) {
        if self.recording {
            self.record(site, Cow::Owned(path().as_str().to_owned()), kind);
        }
    }

    fn record(&mut self, site: EditSite, path: Cow<'static, str>, kind: EditKind) {
        match DeclaredEdit::new(site, path, kind) {
            Ok(edit) if !self.edits.contains(&edit) => self.edits.push(edit),
            Ok(_) => {}
            Err(e) => tracing::debug!(
                site = site.label(),
                error = %e,
                "wire-check response edit declaration refused"
            ),
        }
    }

    /// Every edit declared so far, in declaration order.
    pub fn as_slice(&self) -> &[DeclaredEdit] {
        &self.edits
    }
}
