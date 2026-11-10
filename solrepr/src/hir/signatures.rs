use crate::ast::FunctionKind;
use crate::hir::exprs::Name;
use crate::hir::types::{
    Fn, Location, Mutability, Primitive, TyKind, Type, VariableKind, Visibility};
use crate::ir::def_map::{DefId, TypeDefId};
use crate::salsa::HirDb;


/// The override slot a declaration occupies.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum OverrideSignature {
    Function {
        name: Name,
        kind: FunctionKind,
        params: Box<[Type]>,
    },
    Modifier(Name),
}

impl OverrideSignature {
    pub fn of(db: &dyn HirDb, def: DefId) -> Option<Self> {
        Shape::of(db, def).map(|shape| shape.signature(db))
    }

    pub fn name(&self) -> &Name {
        match self {
            Self::Function { name, .. } | Self::Modifier(name) => name,
        }
    }

    pub fn sort_key(&self, db: &dyn HirDb) -> (Name, String, Box<[String]>) {
        match self {
            Self::Function { name, kind, params } => (
                name.clone(),
                format!("{kind:?}"),
                params.iter().map(|ty| ty.display(db)).collect(),
            ),
            Self::Modifier(name) => (name.clone(), "Modifier".to_string(), Box::new([])),
        }
    }

    pub fn compatibility(
        db: &dyn HirDb,
        derived: DefId,
        base: DefId,
    ) -> Option<Box<[OverrideMismatch]>> {
        Shape::of(db, derived)?.compatibility(db, &Shape::of(db, base)?)
    }
}

/// A reason a derived declaration cannot replace its base in the same slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverrideMismatch {
    /// Normalized return types differ.
    ReturnTypes,
    /// Parameter data locations differ where the base is not external.
    ParameterLocations,
    /// Return data locations differ where the base is not external.
    ReturnLocations,
    /// Visibility changed other than `external` → `public`.
    Visibility,
    /// Mutability became less strict, or the base is `payable`.
    Mutability,
    /// Modifier parameter lists differ.
    ModifierParameters,
    /// A public state-variable getter overrides a non-external function.
    GetterRequiresExternalBase,
    /// A public state variable is the base; solc does not allow overriding it.
    GetterCannotBeOverridden,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ShapeKind {
    Function,
    Getter,
    Modifier,
}

/// The data a comparison needs about one declaration.
struct Shape {
    name: Name,
    fn_kind: FunctionKind,
    shape_kind: ShapeKind,
    sig: Fn<Type>,
}

impl Shape {
    fn of(db: &dyn HirDb, def: DefId) -> Option<Self> {
        match def {
            DefId::Function(id) => {
                let data = db.function_data(id);
                if data.kind == FunctionKind::Constructor {
                    return None;
                }
                Some(Self {
                    name: data.name.clone(),
                    fn_kind: data.kind,
                    shape_kind: ShapeKind::Function,
                    sig: signature_of(db, def)?,
                })
            }
            DefId::Var(id) => {
                let data = db.var_data(id);
                if data.vis != Visibility::Public {
                    return None;
                }
                let mut sig = signature_of(db, def)?;
                // solc: a constant getter is `pure`, any other getter is `view`.
                sig.mutability = if data.kind == VariableKind::Const {
                    Mutability::Pure
                } else {
                    Mutability::View
                };
                Some(Self {
                    name: data.name.clone(),
                    fn_kind: FunctionKind::Regular,
                    shape_kind: ShapeKind::Getter,
                    sig,
                })
            }
            DefId::Modifier(id) => {
                let data = db.modifier_data(id);
                Some(Self {
                    name: data.name.clone(),
                    fn_kind: FunctionKind::Regular,
                    shape_kind: ShapeKind::Modifier,
                    sig: signature_of(db, def)?,
                })
            }
            _ => None,
        }
    }

    fn signature(&self, db: &dyn HirDb) -> OverrideSignature {
        match self.shape_kind {
            ShapeKind::Function | ShapeKind::Getter => OverrideSignature::Function {
                name: self.name.clone(),
                kind: self.fn_kind,
                params: Fn::to_external_abi_types(db, &self.sig.params),
            },
            ShapeKind::Modifier => OverrideSignature::Modifier(self.name.clone()),
        }
    }

    fn compatibility(&self, db: &dyn HirDb, base: &Self) -> Option<Box<[OverrideMismatch]>> {
        if self.signature(db) != base.signature(db) {
            return None;
        }
        let mut mismatches = Vec::new();
        match base.shape_kind {
            ShapeKind::Getter => mismatches.push(OverrideMismatch::GetterCannotBeOverridden),
            ShapeKind::Modifier => {
                if Fn::to_external_abi_types(db, &self.sig.params) != Fn::to_external_abi_types(db, &base.sig.params) {
                    mismatches.push(OverrideMismatch::ModifierParameters);
                }
            }
            ShapeKind::Function => self.function_compatibility(db, base, &mut mismatches),
        }
        Some(mismatches.into_boxed_slice())
    }

    fn function_compatibility(
        &self,
        db: &dyn HirDb,
        base: &Self,
        mismatches: &mut Vec<OverrideMismatch>,
    ) {
        if self.shape_kind == ShapeKind::Getter && base.sig.visibility != Visibility::External {
            mismatches.push(OverrideMismatch::GetterRequiresExternalBase);
        }
        if self.shape_kind == ShapeKind::Function
            && !visibility_allows(self.sig.visibility, base.sig.visibility)
        {
            mismatches.push(OverrideMismatch::Visibility);
        }
        if mutability_violation(self.sig.mutability, base.sig.mutability) {
            mismatches.push(OverrideMismatch::Mutability);
        }

        // solc skips the return and location checks for fallback: its signature is
        // fixed by the grammar, so only the slot identity matters.
        if self.fn_kind == FunctionKind::Fallback {
            return;
        }
        if Fn::to_external_abi_types(db, &self.sig.returns) != Fn::to_external_abi_types(db, &base.sig.returns) {
            mismatches.push(OverrideMismatch::ReturnTypes);
        } else if self.shape_kind == ShapeKind::Function && base.sig.visibility != Visibility::External {
            // Locations only matter when the base is internally callable. Storage
            // differences never reach here: they change the slot itself.
            if self.sig.params != base.sig.params {
                mismatches.push(OverrideMismatch::ParameterLocations);
            }
            if self.sig.returns != base.sig.returns {
                mismatches.push(OverrideMismatch::ReturnLocations);
            }
        }
    }
}

/// An external base may be overridden by a `public` declaration; otherwise the
/// visibility must not change.
fn visibility_allows(derived: Visibility, base: Visibility) -> bool {
    derived == base || (base == Visibility::External && derived == Visibility::Public)
}

/// Mutability may only become stricter (`payable` > `nonpayable` > `view` >
/// `pure`). `implicitly_converts_to` covers the strictness ordering; the
/// payable axis is separate — a `payable` declaration is not a *stricter*
/// `nonpayable` one for override purposes, and `payable` may never be lost.
fn mutability_violation(derived: Mutability, base: Mutability) -> bool {
    derived != base
        && (!derived.implicitly_converts_to(base)
            || derived == Mutability::Payable
            || base == Mutability::Payable)
}

/// The syntactic signature of `def`, when the declaration kind has one —
/// functions, getters, modifiers, errors and events
pub fn signature_of(db: &dyn HirDb, def: DefId) -> Option<Fn<Type>> {
    match def {
        DefId::Function(id) => db.function_signature(id),
        DefId::Modifier(id) => db.modifier_signature(id),
        DefId::Error(id) => db.error_signature(id),
        DefId::Event(id) => db.event_signature(id),
        DefId::Var(id) => db.getter_signature(id),
        _ => None,
    }
}

// ── Getter shape ─────────────────────────────────────────────────────────

pub(crate) fn getter_signature(db: &dyn HirDb, ty: Type) -> Fn<Type> {
    let mut ty = ty.peel_loc(db);
    let mut params: Vec<Type> = Vec::new();
    loop {
        match ty.kind(db) {
            TyKind::Mapping { key, value } => {
                params.push(key.upcast(db));
                ty = value;
            }
            TyKind::Array { elem, .. } => {
                params.push(Type::primitive(db, Primitive::Uint(256)));
                ty = elem;
            }
            _ => break,
        }
    }

    let returns: Vec<Type> = match ty.kind(db) {
        TyKind::TypeDef(TypeDefId::Struct(id)) => {
            let data = db.struct_data(id);
            let strukt = db.inference(DefId::Struct(id));
            data.fields
                .iter()
                .filter_map(|(field_id, _)| {
                    let field = strukt.field_ty(field_id);
                    // peel, field types are located.
                    (!matches!(
                        field.peel_loc(db).kind(db),
                        TyKind::Mapping { .. } | TyKind::Array { .. }
                    ))
                    .then(|| field.at_loc(db, Location::Memory))
                })
                .collect()
        }
        _ => vec![ty.at_loc(db, Location::Memory)],
    };

    Fn {
        // A getter is external by construction and reads state.
        visibility: Visibility::External,
        mutability: Mutability::View,
        params: params.into_boxed_slice(),
        returns: returns.into_boxed_slice(),
    }
}

