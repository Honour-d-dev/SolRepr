//! Definition-shape checks: a bodyless declaration must be `virtual`, and
//! library/free functions must have an implementation.

use crate::ast::{FunctionId, FunctionKind, ModifierId};
use crate::hir::diagnostics::{DiagnosticAnchor, SemanticDiagnostic};
use crate::ir::def_map::DefId;

use super::Inferer;

impl<'db, 'store> Inferer<'db, 'store> {
    pub(super) fn check_definition(&mut self) {
        match self.owner {
            DefId::Function(id) => self.check_function_definition(id),
            DefId::Modifier(id) => self.check_modifier_definition(id),
            _ => {}
        }
    }

    fn check_function_definition(&mut self, id: FunctionId) {
        let data = self.db.function_data(id);
        let owner = self.db.owner(self.owner);

        // `virtual` is never allowed in a library, with or without a body.
        if data.is_virtual && matches!(owner, Some(DefId::Library(_))) {
            self.report(
                SemanticDiagnostic::VirtualInLibrary {
                    declaration: self.owner,
                },
                (),
            );
        }

        if data.has_body || data.kind == FunctionKind::Constructor {
            return;
        }
        match owner {
            // An interface function is implicitly virtual; a library or free
            // function needs a body outright, `virtual` does not rescue it.
            Some(DefId::Contract(_)) if !data.is_virtual => {
                self.report(
                    SemanticDiagnostic::MissingVirtual {
                        declaration: self.owner,
                    },
                    (),
                );
            }
            Some(DefId::Library(_) | DefId::File(_)) => {
                self.report(
                    SemanticDiagnostic::MissingImplementation {
                        declaration: self.owner,
                    },
                    (),
                );
            }
            _ => {}
        }
    }

    fn check_modifier_definition(&mut self, id: ModifierId) {
        let data = self.db.modifier_data(id);
        if data.has_body && !data.has_placeholder {
            self.fail(
                DiagnosticAnchor::Body(self.owner),
                "modifier body does not contain `_`",
                (),
            );
        }
        match self.db.owner(self.owner) {
            // An interface can't declare modifiers at all — body or not.
            Some(DefId::Interface(_)) => {
                self.fail(
                    DiagnosticAnchor::Def(self.owner),
                    "modifiers cannot be defined or declared in interfaces",
                    (),
                );
            }
            Some(DefId::Library(_)) if data.is_virtual => {
                self.report(
                    SemanticDiagnostic::VirtualInLibrary {
                        declaration: self.owner,
                    },
                    (),
                );
            }
            _ => {}
        }
        if !data.has_body && !data.is_virtual {
            self.report(
                SemanticDiagnostic::MissingVirtual {
                    declaration: self.owner,
                },
                (),
            );
        }
    }
}
