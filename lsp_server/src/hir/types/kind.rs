use la_arena::Arena;
use num_bigint::BigInt;

use crate::ast::{ContractId, EnumId, ErrorId, EventId, InterfaceId, LibraryId, ModifierId, StructId, UdvtId};
use crate::hir::body_map::Local;
use crate::hir::builtins::{BuiltinTy, MemberNamespace};
use crate::hir::types::func::{Callee, CallKind, Fn};
use crate::hir::types::literal::LiteralKind;
use crate::hir::types::primitive::Primitive;
use crate::hir::types::syntax::{Location, RefKind, TyName};
use crate::hir::types::ty::{Ty, Type};
use crate::ir::def_map::DefId;
use crate::salsa::{FileId, HirDb};

/// N = nameable/declarable subset that makeup [`Type`]. Concrete Ty's that can be written/assigned programatically
/// E = non-nameable subset everything else in [`Ty`]. intermediate/hidden/infered Ty's everything else that an expr can be typed as
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum TyKind {
    // ── N ────────────
    Primitive(Primitive),
    Contract(ContractId),
    Interface(InterfaceId),
    Library(LibraryId),
    Struct(StructId),
    Enum(EnumId),
    Udvt(UdvtId),
    FnPtr(Fn<Type>),
    Ref(Type, Location),
    Array {
        elem: Type,
        size: Option<BigInt>,
    },
    Mapping {
        key: Type,
        value: Type,
    },
    Invalid,
    // --- E -------------------------------------------------
    Literal(LiteralKind),
    Tuple(Tuple),
    /// The result of `a[from:to]` — solc's `ArraySliceType`. The `Type` is
    /// the located array base (`Ref`-wrapped: `bytes calldata`, `T[] memory`).
    Slice(Type),
    /// A type name in expression position (`uint` in `uint(x)`.
    Type(Type),
    /// The `type(T)` metatype — `.name`/`.min`/`.interfaceId`/`.Code`
    Meta(Type),
    /// The type of a file-module namespace reference — `import "…"`.
    Module(FileId),
    Super(ContractId),
    Error(ErrorId),
    Event(EventId),
    Modifier(ModifierId),
    Function {
        callee: Callee,
        kind: CallKind,
    },
    Builtin(BuiltinTy),
    Void,
}

impl TyKind {
    pub fn namespace(&self) -> MemberNamespace {
        match self {
            TyKind::Builtin(_)
            | TyKind::Super(_)
            | TyKind::Type(_)
            | TyKind::Module(_)
            | TyKind::Meta(_) => MemberNamespace::Types,
            _ => MemberNamespace::Values,
        }
    }
    pub fn is_nameable(&self) -> bool {
        matches!(
            self,
            TyKind::Primitive(_)
                | TyKind::Contract(_)
                | TyKind::Interface(_)
                | TyKind::Library(_)
                | TyKind::Struct(_)
                | TyKind::Enum(_)
                | TyKind::Udvt(_)
                | TyKind::FnPtr(_)
                | TyKind::Ref(..)
                | TyKind::Array { .. }
                | TyKind::Mapping { .. }
                | TyKind::Invalid
        )
    }

    pub fn is_dynamically_sized(&self) -> bool {
        matches!(
            self,
            TyKind::Array { size: None, .. }
                | TyKind::Primitive(Primitive::Bytes | Primitive::String)
        )
    }

    /// Kinds that take a data location
    pub fn is_reference_type(&self) -> bool {
        matches!(
            self,
            TyKind::Primitive(Primitive::Bytes | Primitive::String)
                | TyKind::Struct(_)
                | TyKind::Array { .. }
                | TyKind::Mapping { .. }
        )
    }

    pub fn default_loc(&self) -> Option<Location> {
        match self {
            TyKind::Primitive(Primitive::Bytes | Primitive::String)
            | TyKind::Struct(_)
            | TyKind::Array { .. } => Some(Location::Memory),
            TyKind::Mapping { .. } => Some(Location::Storage(RefKind::Reference)),
            _ => None,
        }
    }

    pub fn def_id(&self) -> Option<DefId> {
        match self {
            TyKind::Contract(id) => Some(DefId::Contract(*id)),
            TyKind::Interface(id) => Some(DefId::Interface(*id)),
            TyKind::Library(id) => Some(DefId::Library(*id)),
            TyKind::Struct(id) => Some(DefId::Struct(*id)),
            TyKind::Enum(id) => Some(DefId::Enum(*id)),
            TyKind::Udvt(id) => Some(DefId::Udvt(*id)),
            TyKind::Error(id) => Some(DefId::Error(*id)),
            TyKind::Event(id) => Some(DefId::Event(*id)),
            TyKind::Modifier(id) => Some(DefId::Modifier(*id)),
            TyKind::Function { callee, .. } => match callee {
                Callee::Fn(id) => Some(DefId::Function(*id)),
                Callee::Getter(id) => Some(DefId::Var(*id)),
                Callee::Constructor(id) => Some(DefId::Contract(*id)),
                Callee::Allocation(_) => None,
                // A builtin has no declaration to navigate to.
                Callee::Builtin(_) => None,
            },
            TyKind::Module(file) => Some(DefId::File(*file)),
            TyKind::Super(id) => Some(DefId::Contract(*id)),
            _ => None,
        }
    }

    /// Implicit-conversion cost: `None` means no conversion; `0` identical, higher = lossier.
    ///
    /// **Only a nameable/declarable type — or a tuple — can be a conversion target.**
    pub fn implicitly_converts_to(&self, target: &TyKind, db: &dyn HirDb) -> Option<u8> {
        if !target.is_nameable() && !matches!(target, TyKind::Tuple(_)) {
            return None;
        }
        match (self, target) {
            (TyKind::Invalid, _) | (_, TyKind::Invalid) => None,
            (TyKind::Literal(literal), TyKind::Ref(inner, loc)) => literal
                .implicitly_converts_to(&inner.kind(db))
                .filter(|_| loc.accepts(literal.default_loc())),
            (TyKind::Literal(literal), target) => literal.implicitly_converts_to(target),
            (TyKind::Primitive(a), TyKind::Primitive(b)) => a.implicitly_converts_to(b),
            (TyKind::Ref(a, from), TyKind::Ref(b, to)) if to.accepts(*from) => {
                a.implicitly_converts_to(*b, db)
            }
            // the slice converts exactly when its base was a dynamic calldata array —
            // and then *through* the base, so `bytes calldata slice` reaches
            // `bytes memory` too. A slice of anything else converts nowhere.
            (TyKind::Slice(base), target) => (base.loc(db) == Some(Location::Calldata)
                && base.peel_loc(db).kind(db).is_dynamically_sized())
            .then(|| base.kind(db).implicitly_converts_to(target, db)).flatten(),
            // bytesN -> bytes
            (from, TyKind::Ref(inner, loc))
                if !matches!(from, TyKind::Ref(..)) && loc.accepts(Location::Stack) =>
            {
                from.implicitly_converts_to(&inner.kind(db), db)
            }
            (_, TyKind::Ref(..)) => None,

            (TyKind::Array { elem: a, size: sa }, TyKind::Array { elem: b, size: sb })
                if sa == sb =>
            {
                a.implicitly_converts_to(*b, db)
            }
            (TyKind::Mapping { key: ka, value: va }, TyKind::Mapping { key: kb, value: vb }) => {
                let key = ka.implicitly_converts_to(*kb, db)?;
                let value = va.implicitly_converts_to(*vb, db)?;
                Some(key.saturating_add(value))
            }
            (TyKind::Tuple(from), TyKind::Tuple(to)) => Self::tuple_conversion(from, to, db),
            (TyKind::FnPtr(a), TyKind::FnPtr(b)) => Self::fn_ptr_to_fn_ptr_conversion(a, b),
            (TyKind::Function { callee, kind }, TyKind::FnPtr(sig)) => {
                Self::fn_to_fn_ptr_conversion(*callee, *kind, sig, db)
            }

            (a, b) if a.is_nameable() => match (a.def_id(), b.def_id()) {
                (Some(a), Some(b)) => Self::def_conversion(a, b, db),
                _ => None,
            },
            _ => None,
        }
    }

    /// Explicit-conversion cost — solc's `isExplicitlyConvertibleTo`.
    ///
    /// It **defaults to the implicit relation** and is widened per kind, so a
    /// cast accepts everything an assignment does plus whatever a cast adds.
    /// `None` means "Explicit type conversion not allowed".
    pub fn explicitly_converts_to(&self, target: &TyKind, db: &dyn HirDb) -> Option<u8> {
        self.implicitly_converts_to(target, db)
            .or_else(|| self.explicit_widening(target, db))
    }

    fn explicit_widening(&self, target: &TyKind, db: &dyn HirDb) -> Option<u8> {
        match (self, target) {
            (TyKind::Invalid, _) | (_, TyKind::Invalid) => None,

            (TyKind::Ref(inner, _), target) => inner.kind(db).explicit_widening(target, db),
            (from, TyKind::Ref(inner, _)) => from.explicit_widening(&inner.kind(db), db),

            (TyKind::Slice(base), target) => base
                .kind(db)
                .explicitly_converts_to(target, db),
            (TyKind::Literal(literal), target) => literal.explicit_widening(target, db),
            (TyKind::Primitive(from), TyKind::Primitive(to)) => from.explicit_widening(to),
            (TyKind::Primitive(Primitive::Uint(_) | Primitive::Int(_)), TyKind::Enum(_)) => Some(1),
            
            (TyKind::Enum(_), TyKind::Primitive(Primitive::Uint(_))) => Some(1),
            (TyKind::Primitive(Primitive::Address | Primitive::AddressPayable), target)
                if target.is_contract_like() && !target.is_payable(db) =>
            {
                Some(1)
            }
            (from, TyKind::Primitive(Primitive::Address)) if from.is_contract_like() => Some(1),
            (from, TyKind::Primitive(Primitive::AddressPayable))
                if from.is_payable(db) =>
            {
                Some(1)
            },
            (TyKind::Type(inner), TyKind::Primitive(Primitive::Address))
                if matches!(inner.kind(db), TyKind::Library(_)) =>
            {
                Some(1)
            }
            _ => None,
        }
    }

    /// Element-wise with matching arity. A hole in the *target* is an ignored
    /// lvalue slot and accepts any source element — that is what lets
    /// `(a, , b) = f()` discard the middle value; a hole in the source has no
    /// value to give.
    fn tuple_conversion(from: &Tuple, to: &Tuple, db: &dyn HirDb) -> Option<u8> {
        if from.elements.len() != to.elements.len() {
            return None;
        }
        from.elements
            .iter()
            .zip(to.elements.iter())
            .try_fold(0u8, |cost, (from, to)| match (from, to) {
                (_, None) => Some(cost),
                (None, Some(_)) => None,
                (Some(from), Some(to)) => from
                    .implicitly_converts_to(*to, db)
                    .map(|step| cost.saturating_add(step)),
            })
    }

    fn def_conversion(a: DefId, b: DefId, db: &dyn HirDb) -> Option<u8> {
        (a == b).then_some(0).or_else(|| match a {
            DefId::Contract(_) | DefId::Interface(_) => db
                .bases(a)
                .iter()
                .find_map(|&base| (base == b).then_some(0)),
            _ => None,
        })
    }

    fn fn_ptr_to_fn_ptr_conversion(source: &Fn<Type>, target: &Fn<Type>) -> Option<u8> {
        (source.visibility == target.visibility
            && source.mutability.implicitly_converts_to(target.mutability)
            && source.params == target.params
            && source.returns == target.returns)
            .then_some(0)
    }

    fn fn_to_fn_ptr_conversion(
        callee: Callee,
        kind: CallKind,
        sig: &Fn<Type>,
        db: &dyn HirDb,
    ) -> Option<u8> {
        let fnty = match callee {
            Callee::Fn(id) => {
                if matches!(kind, CallKind::Base(_)) && !db.function_data(id).has_body {
                    return None;
                }
                db.function_signature(id)?
            }
            // A getter's pointer is always external, it is reached through a message call
            Callee::Getter(id) => db.getter_signature(id)?,
            Callee::Builtin(id) => id.pointer_sig(db)?,
            Callee::Constructor(_) | Callee::Allocation(_) => return None,
        };
        let pointer = fnty.apply_kind(kind, db)?;
        Self::fn_ptr_to_fn_ptr_conversion(&pointer, sig)
    }

    pub fn display(&self, db: &dyn HirDb) -> String {
        match self {
            TyKind::Primitive(p) => p.to_string(),
            TyKind::Literal(literal) => literal.display(),
            TyKind::Contract(id) => db.contract_data(*id).name.to_string(),
            TyKind::Interface(id) => db.interface_data(*id).name.to_string(),
            TyKind::Library(id) => db.library_data(*id).name.to_string(),
            TyKind::Struct(id) => db.struct_data(*id).name.to_string(),
            TyKind::Enum(id) => db.enum_data(*id).name.to_string(),
            TyKind::Udvt(id) => db.udvt_data(*id).name.to_string(),
            TyKind::Function { callee, .. } => match callee {
                Callee::Fn(id) => db.function_data(*id).name.to_string(),
                Callee::Getter(id) => db.var_data(*id).name.to_string(),
                Callee::Constructor(id) => db.contract_data(*id).name.to_string(),
                Callee::Allocation(ty) => ty.display(db),
                Callee::Builtin(id) => id.name().to_string(),
            },
            TyKind::Error(id) => {
                let data = db.error_data(*id);
                format!(
                    "error {}({})",
                    data.name,
                    display_decl_parameters(&data.parameters, &data.syntax_store.types)
                )
            }
            TyKind::Event(id) => {
                let data = db.event_data(*id);
                format!(
                    "event {}({})",
                    data.name,
                    display_decl_parameters(&data.parameters, &data.syntax_store.types)
                )
            }
            TyKind::Modifier(id) => format!("modifier {}", db.modifier_data(*id).name),
            TyKind::Module(file) => file.path(db).file_name().unwrap_or("<file>").to_string(),
            TyKind::Super(id) => format!("super {}", db.contract_data(*id).name),
            TyKind::Builtin(id) => id.name().to_string(),
            TyKind::Ref(inner, loc) => format!("{} {}", inner.display(db), loc),
            TyKind::Array {
                elem,
                size: Some(n),
            } => format!("{}[{n}]", elem.display(db)),
            TyKind::Array { elem, size: None } => format!("{}[]", elem.display(db)),
            TyKind::Mapping { key, value } => {
                format!("mapping({} => {})", key.display(db), value.display(db))
            }
            TyKind::FnPtr(sig) => sig.display(db),
            TyKind::Type(inner) => inner.display(db),
            TyKind::Meta(inner) => format!("type({})", inner.display(db)),
            TyKind::Tuple(tuple) => {
                let parts: Vec<String> = tuple
                    .elements
                    .iter()
                    .map(|elem| match elem {
                        Some(ty) => ty.display(db),
                        None => String::new(),
                    })
                    .collect();
                format!("({})", parts.join(", "))
            }
            TyKind::Slice(base) => format!("{} slice", base.display(db)),
            TyKind::Void => "void".into(),
            TyKind::Invalid => "<invalid>".into(),
        }
    }


    
    fn is_contract_like(&self) -> bool {
        matches!(
            self,
            TyKind::Contract(_) | TyKind::Interface(_) | TyKind::Library(_)
        )
    }

    fn is_payable(&self, db: &dyn HirDb) -> bool {
        match self {
            TyKind::Contract(id) => db
                .member_map(DefId::Contract(*id))
                .data
                .is_payable(db),
            _ => false,
        }
    }
}

/// A tuple type: one entry per component, `None` for an omitted one (a hole),
/// which still counts toward the arity to differentiate — `(a, , b)` and `(a, b)`
///
/// Components are `Ty`, not `Type`, because a tuple can either contain value-types or type-types
/// e.g (x, y, z) or (uint, string, bool)
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Tuple {
    pub elements: Box<[Option<Ty>]>,
}

fn display_decl_parameters(parameters: &Arena<Local>, types: &Arena<TyName>) -> String {
    parameters
        .iter()
        .map(|(_, parameter)| {
            let ty = types[*parameter.ty_ref()].to_string(types);
            if parameter.name().is_empty() {
                ty
            } else {
                format!("{ty} {}", parameter.name())
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn def_to_type(db: &dyn HirDb, def: DefId) -> Option<Type> {
    let kind = match def {
        DefId::Contract(id) => TyKind::Contract(id),
        DefId::Interface(id) => TyKind::Interface(id),
        DefId::Library(id) => TyKind::Library(id),
        DefId::Struct(id) => TyKind::Struct(id),
        DefId::Enum(id) => TyKind::Enum(id),
        DefId::Udvt(id) => TyKind::Udvt(id),
        _ => return None,
    };
    Some(Type::new(db, kind).expect("every type-position def kind is nameable"))
}

