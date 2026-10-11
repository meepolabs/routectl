//! The edits an ingress renderer makes to the response on purpose, declared
//! at the site that makes them so the response leg of the wire check can
//! tell them from a loss. Recording is off unless the attempt's response is
//! checked, so an unchecked render pays one bool test per site.

use routectl_core::wire_check::{DeclaredEdit, EditKind, EditSite};

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
        if !self.recording {
            return;
        }
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
