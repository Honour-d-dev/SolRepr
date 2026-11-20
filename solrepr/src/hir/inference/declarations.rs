//! Self-shape checks: a declaration's own body must be well-formed.
//!
//! Whether a declaration *belongs* in its container is a membership question
//! and lives in `ir::membership`; this namespace only answers questions that
//! need the declaration's own body lowered.

use crate::ast::ModifierId;
use crate::hir::diagnostics::DiagnosticAnchor;
use crate::ir::def_map::DefId;

use super::Inferer;

impl<'db, 'store> Inferer<'db, 'store> {
    pub(super) fn check_definition(&mut self) {
        if let DefId::Modifier(id) = self.owner {
            self.check_modifier_body(id);
        }
    }

    /// solc's `_` rule: a modifier body must contain the placeholder as a
    /// standalone statement.
    fn check_modifier_body(&mut self, id: ModifierId) {
        let data = self.db.modifier_data(id);
        if data.has_body && !data.has_placeholder {
            self.fail(
                DiagnosticAnchor::Body(self.owner),
                "modifier body does not contain `_`",
                (),
            );
        }
    }
}
