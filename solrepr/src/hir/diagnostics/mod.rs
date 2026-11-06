//! Diagnostics: the unrendered form the analyses emit, the reportable form,
//! and the translation between them.
//!
//! `semantic` holds what a check produces — a code plus an id anchor, so no
//! position enters a Salsa output and a reflow cannot invalidate it. `render`
//! turns those into the `Diagnostic` below, resolving anchors against the
//! current source.

mod render;
mod semantic;

pub(crate) use render::{collect_diagnostics, normalize_diagnostics};
pub use semantic::{DiagnosticAnchor, DiagnosticCode, DiagnosticSink, SemanticDiagnostic};

use crate::ast::NodeRange;
use crate::salsa::File;

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Severity {
    Error,
    Warning,
    Information,
    Hint,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelatedDiagnostic {
    pub file: File,
    pub range: NodeRange,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: DiagnosticCode,
    pub severity: Severity,
    pub range: NodeRange,
    pub message: String,
    pub related: Vec<RelatedDiagnostic>,
}
