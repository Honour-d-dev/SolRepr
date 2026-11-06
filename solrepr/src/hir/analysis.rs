use triomphe::Arc;

use crate::hir::diagnostics::SemanticDiagnostic;

/// A query-level result that may contain usable partial data and diagnostics.
#[derive(PartialEq, Eq)]
pub struct Analysis<T: ?Sized> {
    pub data: Arc<T>,
    pub diagnostics: Arc<[SemanticDiagnostic]>,
}

impl<T: ?Sized> Clone for Analysis<T> {
    fn clone(&self) -> Self {
        Self {
            data: self.data.clone(),
            diagnostics: self.diagnostics.clone(),
        }
    }
}

impl<T: ?Sized> Analysis<T> {
    pub fn new(data: impl Into<Arc<T>>, diagnostics: Arc<[SemanticDiagnostic]>) -> Self {
        Self {
            data: data.into(),
            diagnostics,
        }
    }
}


