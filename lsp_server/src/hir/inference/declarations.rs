//! Definition-shape checks: a bodyless declaration must be `virtual`, and
//! library/free functions must have an implementation.

use crate::ast::{FunctionId, FunctionKind, ModifierId, NodeRange};
use crate::hir::semantic_errors::SemanticDiagnostic;
use crate::ir::def_map::DefId;
use crate::salsa::root_db::def_range;

use super::Inferer;

impl<'db, 'store> Inferer<'db, 'store> {
    pub(super) fn check_definition(&mut self) {
        let Some(range) = def_range(self.db, self.owner) else {
            return;
        };
        match self.owner {
            DefId::Function(id) => self.check_function_definition(id, range),
            DefId::Modifier(id) => self.check_modifier_definition(id, range),
            _ => {}
        }
    }

    fn check_function_definition(&mut self, id: FunctionId, range: NodeRange) {
        let data = self.db.function_data(id);
        let owner = self.db.owner(self.owner);

        // `virtual` is never allowed in a library, with or without a body.
        if data.is_virtual && matches!(owner, Some(DefId::Library(_))) {
            self.report(
                SemanticDiagnostic::VirtualInLibrary {
                    declaration: self.owner,
                    range,
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
                        range,
                    },
                    (),
                );
            }
            Some(DefId::Library(_) | DefId::File(_)) => {
                self.report(
                    SemanticDiagnostic::MissingImplementation {
                        declaration: self.owner,
                        range,
                    },
                    (),
                );
            }
            _ => {}
        }
    }

    fn check_modifier_definition(&mut self, id: ModifierId, range: NodeRange) {
        let data = self.db.modifier_data(id);
        // solc reports the missing placeholder at the body block, before the
        // placement rules (which point at the declaration).
        if let Some(body_range) = data.body_range
            && !data.has_placeholder
        {
            self.fail(body_range, "modifier body does not contain `_`", ());
        }
        match self.db.owner(self.owner) {
            // An interface can't declare modifiers at all — body or not.
            Some(DefId::Interface(_)) => {
                self.fail(range, "modifiers cannot be defined or declared in interfaces", ());
            }
            Some(DefId::Library(_)) if data.is_virtual => {
                self.report(
                    SemanticDiagnostic::VirtualInLibrary {
                        declaration: self.owner,
                        range,
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
                    range,
                },
                (),
            );
        }
    }
}
