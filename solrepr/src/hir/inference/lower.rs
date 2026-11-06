use crate::hir::body_map::LocalId;
use crate::hir::item_data::FieldId;
use crate::hir::resolution::Resolution;
use crate::hir::diagnostics::{DiagnosticAnchor, SemanticDiagnostic};
use crate::hir::types::{Fn, TyKind, TyName, TyNameId, Type};
use crate::ir::def_map::DefId;

use super::Inferer;

impl<'db, 'store> Inferer<'db, 'store> {
    pub(super) fn infer_local(&mut self, local: LocalId) -> Type {
        let Some(local_arena) = self.input.locals else {
            return self.fail(
                DiagnosticAnchor::Local(local),
                "local resolution has no local arena",
                Type::invalid(self.db),
            );
        };
        let local = &local_arena[local];
        let ty_ref_id = *local.ty_ref();
        let located = !matches!(self.owner, DefId::Event(_) | DefId::Error(_));
        self.lower_type(ty_ref_id, located)
    }

    pub(super) fn infer_field(&mut self, field: FieldId) -> Type {
        let Some(fields) = self.input.fields else {
            return self.fail(
                DiagnosticAnchor::Def(self.owner),
                "struct field inferred outside a struct",
                Type::invalid(self.db),
            );
        };
        self.lower_type(fields[field].ty_ref, false)
    }

    pub(super) fn infer_variant(&mut self) -> Type {
        let DefId::Enum(enum_id) = self.owner else {
            return self.fail(
                DiagnosticAnchor::Def(self.owner),
                "variant inferred outside an enum",
                Type::invalid(self.db),
            );
        };
        Type::new(self.db, TyKind::Enum(enum_id)).expect("an enum is nameable")
    }

    // ── Type name lowering ───────────────────────────────────────────────

    /// Memoized `TyNameId` lowering; `tables.types` is the memo.
    pub(super) fn lower_type(&mut self, ty_ref_id: TyNameId, located: bool) -> Type {
        if let Some(cached) = self.tables.try_ty(ty_ref_id) {
            return cached;
        }
        let ty = self.lower_ty_ref_uncached(ty_ref_id, located);
        
        let written = matches!(self.input.store.types[ty_ref_id], TyName::Located { .. });
        if located && !written && ty.default_loc(self.db).is_some() {
            self.report(
                SemanticDiagnostic::DataLocationRequired { ty_ref_id, ty },
                (),
            );
        }
        self.tables.set_ty(ty_ref_id, ty);
        ty
    }

    fn lower_ty_ref_uncached(&mut self, ty_ref_id: TyNameId, located: bool) -> Type {
        match &self.input.store.types[ty_ref_id] {
            TyName::Primitive(primitive) => Type::primitive(self.db, *primitive),
            TyName::UserDefined { base, name } => {
                let base_ty = match base {
                    None => None,
                    Some(base) => {
                        let ty = self.lower_type(*base, false);
                        if ty.is_invalid(self.db) {
                            return Type::invalid(self.db);
                        }
                        Some(ty)
                    }
                };
                let resolution = match base_ty {
                    None => self.resolver.resolve_path(std::slice::from_ref(name)),
                    Some(ty) => ty
                        .def_id(self.db)
                        .and_then(|def| self.resolver.lookup_type(&def, name)),
                };
                let Some(resolution) = resolution else {
                    return self.report(
                        SemanticDiagnostic::UnresolvedType {
                            ty_ref_id,
                            name: name.clone(),
                        },
                        Type::invalid(self.db),
                    );
                };
                match resolution {
                    Resolution::Type(ty) => ty,
                    _ => self.fail(
                        DiagnosticAnchor::TyRef(ty_ref_id),
                        format!("`{name}` does not resolve to a type"),
                        Type::invalid(self.db),
                    ),
                }
            }
            TyName::Array { ty, size } => {
                let elem = self.lower_type(*ty, false);
                if elem.is_invalid(self.db) {
                    return Type::invalid(self.db);
                }
                let size = match size {
                    Some(expr) => match self.const_size(*expr) {
                        Some(size) => Some(size),
                        None => return Type::invalid(self.db),
                    },
                    None => None,
                };
                Type::array(self.db, elem, size)
            }
            TyName::Mapping { key, value } => {
                let key = self.lower_type(*key, false);
                
                let value = self.lower_type(*value, false);
               
                if key.is_invalid(self.db)||value.is_invalid(self.db){
                    Type::invalid(self.db)
                } else {
                    Type::mapping(self.db, key, value)
                }
            }
            TyName::Located { ty, loc } => {
                let inner = self.lower_type(*ty, false);
                if inner.is_invalid(self.db) {
                    return Type::invalid(self.db);
                }
                // validate the written location
                let is_valid = match inner.default_loc(self.db) {
                    // if default location is storage writtten location must be storage in a located context
                    Some(default_loc) if default_loc.is_storage() => loc.is_storage() && located,
                    // If type has a default location, then its valid based on whether the context demands it
                    // so writing a location in a context that demands no location is invalid
                    Some(_) => located,
                    // location written but type has no default location. invalid
                    None => false,
                };
                if !is_valid {
                    return self.report(
                        SemanticDiagnostic::InvalidDataLocation {
                            ty_ref_id,
                            ty: inner,
                            location: *loc,
                        },
                        Type::invalid(self.db),
                    );
                }
                inner.at_loc(self.db, *loc)
            }
            TyName::Fn(function) => {
                let mut valid = true;
                let mut params = Vec::with_capacity(function.params.len());
                for &parameter in function.params.iter() {
                    let ty = self.lower_type(parameter, true);
                    if ty.is_invalid(self.db) {
                        valid = false;
                    }
                    params.push(ty);
                }
                let mut ret = Vec::with_capacity(function.returns.len());
                for &return_type in function.returns.iter() {
                    let ty = self.lower_type(return_type, true);
                    if ty.is_invalid(self.db) {
                        valid = false;
                    }
                    ret.push(ty);
                }
                if !valid {
                    return Type::invalid(self.db);
                }
                Type::fn_ptr(
                    self.db,
                    Fn {
                        visibility: function.visibility,
                        mutability: function.mutability,
                        params: params.into_boxed_slice(),
                        returns: ret.into_boxed_slice(),
                    },
                )
            }
        }
    }

}
