use la_arena::Arena;

use crate::ast::{ContractId, EnumId, ErrorId, EventId, FunctionId, ModifierId, VarId};
use crate::hir::body_map::{self, LocalId};
use crate::hir::builtins::{
    Builtin, BuiltinField, BuiltinFn, BuiltinTy, ParamSchema,
};
use crate::hir::exprs::{Literal, Name};
use crate::hir::item_data::{FieldId, SyntaxStore, VariantId};
use crate::hir::types::{
    CallKind, EXTERNAL_CALL_OPTIONS, Fn, Type, ViaTypeKind, Visibility, def_to_type,
};
use crate::ir::def_map::DefId;
use crate::salsa::{FileId, HirDb};

// ── Callable ──────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallableOrigin {
    Function(FunctionId),
    Getter(VarId),
    Constructor(ContractId),
    Error(ErrorId),
    Event(EventId),
    Modifier(ModifierId),
    Allocation(Type),
    Builtin(BuiltinFn),
}

impl CallableOrigin {
    fn from_def(def: DefId) -> Self {
        match def {
            DefId::Function(id) => Self::Function(id),
            DefId::Error(id) => Self::Error(id),
            DefId::Event(id) => Self::Event(id),
            DefId::Modifier(id) => Self::Modifier(id),
            DefId::Var(id) => Self::Getter(id),
            DefId::Contract(id) => Self::Constructor(id),
            other => unreachable!("{other:?} does not name a callable declaration"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Callable {
    origin: CallableOrigin,
    kind: CallKind,
}

impl Callable {
    pub fn new_direct(def: DefId) -> Self {
        Self {
            origin: CallableOrigin::from_def(def),
            kind: CallKind::Direct,
        }
    }

    pub fn new_builtin(builtin: BuiltinFn) -> Self {
        let kind = builtin.callkind();
        Self {
            origin: CallableOrigin::Builtin(builtin),
            kind,
        }
    }

    /// An externally-accessed function value — `this.f`, `c.f`.
    pub fn new_external(id: FunctionId) -> Self {
        Self {
            origin: CallableOrigin::Function(id),
            kind: CallKind::External,
        }
    }

    /// A public state-variable getter (`c.x()`).
    pub fn new_getter(id: VarId) -> Self {
        Self {
            origin: CallableOrigin::Getter(id),
            kind: CallKind::External,
        }
    }

    pub fn new_constructor(id: ContractId) -> Self {
        Self {
            origin: CallableOrigin::Constructor(id),
            kind: CallKind::Direct,
        }
    }

    pub fn new_allocation(ty: Type) -> Self {
        Self {
            origin: CallableOrigin::Allocation(ty),
            kind: CallKind::Direct,
        }
    }

    pub fn new_bound(def: DefId) -> Self {
        Self {
            origin: CallableOrigin::from_def(def),
            kind: CallKind::Bound,
        }
    }

    /// Re-tag with the kind the resolver computed for the access path this
    /// callable was reached on.
    pub fn with_kind(&self, kind: CallKind) -> Self {
        Self {
            origin: self.origin,
            kind,
        }
    }

    pub fn kind(&self) -> CallKind {
        self.kind
    }

    pub fn is_external(&self) -> bool {
        self.kind.is_external()
    }

    pub fn options(&self) -> Option<&'static [&'static str]> {
        match &self.origin {
            CallableOrigin::Constructor(_) => Some(&["value", "salt"]),
            _ if self.is_external() => Some(match &self.origin {
                CallableOrigin::Builtin(b) => b.options(),
                _ => EXTERNAL_CALL_OPTIONS,
            }),
            _ => None,
        }
    }

    /// Whether arguments are ABI-encoded to reach this callable — message
    /// calls and public-library delegatecalls normalize calldata -> memory so located args conversion works .
    pub fn encodes_args(&self, sig: &Fn<Type>) -> bool {
        match self.kind {
            CallKind::External => true,
            CallKind::ViaType(ViaTypeKind::Library) => sig.visibility != Visibility::Internal,
            _ => false,
        }
    }

    pub fn is_invocable(&self, db: &dyn HirDb) -> bool {
        match self.kind {
            CallKind::ViaType(ViaTypeKind::NonLibrary) => {
                !matches!(self.origin, CallableOrigin::Function(_))
            }
            CallKind::ViaType(ViaTypeKind::Library) => {
                !matches!(self.origin, CallableOrigin::Modifier(_))
            }
            CallKind::Base(_) => match self.origin {
                CallableOrigin::Function(id) => {
                    let data = db.function_data(id);
                    matches!(data.vis, Visibility::Internal | Visibility::Public) && data.has_body
                }
                _ => false,
            },
            _ => true,
        }
    }

    pub fn is_getter(&self) -> bool {
        matches!(self.origin, CallableOrigin::Getter(_))
    }

    /// The declaration this callable names.
    pub fn def(&self) -> Option<DefId> {
        Some(match &self.origin {
            CallableOrigin::Function(id) => DefId::Function(*id),
            CallableOrigin::Getter(id) => DefId::Var(*id),
            CallableOrigin::Constructor(id) => DefId::Contract(*id),
            // An allocation names no declaration.
            CallableOrigin::Allocation(_) => return None,
            CallableOrigin::Error(id) => DefId::Error(*id),
            CallableOrigin::Event(id) => DefId::Event(*id),
            CallableOrigin::Modifier(id) => DefId::Modifier(*id),
            CallableOrigin::Builtin(_) => return None,
        })
    }

    pub fn builtin(&self) -> Option<BuiltinFn> {
        match &self.origin {
            CallableOrigin::Builtin(b) => Some(*b),
            _ => None,
        }
    }

    pub fn origin(&self) -> &CallableOrigin {
        &self.origin
    }

    /// Number of implicit receiver arguments (0 or 1).
    pub fn bound(&self) -> u8 {
        self.kind.bound_args()
    }

    pub fn schema(&self) -> ParamSchema {
        match &self.origin {
            CallableOrigin::Builtin(b) => b.schema(),
            _ => ParamSchema::Fixed,
        }
    }

    pub fn is_intrinsic(&self) -> bool {
        self.schema() == ParamSchema::Intrinsic
    }

    /// The parameters the callable is named by, as `(name, type-as-written)`
    /// in declaration order — the keys and value types of an `f({ … })` group.
    ///
    /// A `using`-attached function's implicit receiver parameters are skipped:
    pub fn named_parameters(&self, db: &dyn HirDb) -> Option<Box<[(Name, String)]>> {
        let skip = self.bound() as usize;
        fn pairs(store: &SyntaxStore, parameters: &Arena<body_map::Local>, locals: impl Iterator<Item = LocalId>) -> Vec<(Name, String)> {
            locals
                .map(|id| {
                    let local = &parameters[id];
                    (
                        local.name().clone(),
                        store.types[*local.ty_ref()].to_string(&store.types),
                    )
                })
                .collect()
        }
        let pairs = match &self.origin {
            CallableOrigin::Function(id) => {
                let data = db.function_data(*id);
                pairs(
                    &data.syntax_store,
                    &data.parameters,
                    data.args.iter().copied().skip(skip),
                )
            }
            CallableOrigin::Constructor(id) => match db.contract_data(*id).constructor {
                Some(ctor) => {
                    let data = db.function_data(ctor);
                    pairs(
                        &data.syntax_store,
                        &data.parameters,
                        data.args.iter().copied().skip(skip),
                    )
                }
                // The implicit default constructor takes no arguments.
                None => Vec::new(),
            },
            CallableOrigin::Event(id) => {
                let data = db.event_data(*id);
                pairs(
                    &data.syntax_store,
                    &data.parameters,
                    data.parameters.iter().map(|(id, _)| id),
                )
            }
            CallableOrigin::Error(id) => {
                let data = db.error_data(*id);
                pairs(
                    &data.syntax_store,
                    &data.parameters,
                    data.parameters.iter().map(|(id, _)| id),
                )
            }
            CallableOrigin::Modifier(id) => {
                let data = db.modifier_data(*id);
                pairs(
                    &data.syntax_store,
                    &data.parameters,
                    data.parameters.iter().map(|(id, _)| id),
                )
            }
            CallableOrigin::Getter(_)
            | CallableOrigin::Builtin(_)
            | CallableOrigin::Allocation(_) => return None,
        };
        Some(pairs.into_boxed_slice())
    }
}


// ── Local / Field / Variant ────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Local {
    pub owner: DefId,
    pub id: LocalId,
}

impl Local {
    pub fn new(owner: DefId, id: LocalId) -> Self {
        Self { owner, id }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Variant {
    pub enum_id: EnumId,
    pub id: VariantId,
}

impl Variant {
    pub fn new(enum_id: EnumId, id: VariantId) -> Self {
        Self { enum_id, id }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Struct(DefId, FieldId), //@TODO use structId directly
    Builtin(BuiltinField),
}

impl Field {
    pub fn new_struct(owner: DefId, id: FieldId) -> Self {
        Self::Struct(owner, id)
    }

    pub fn new_builtin(b: BuiltinField) -> Self {
        Self::Builtin(b)
    }

    pub fn struct_field(&self) -> Option<(DefId, FieldId)> {
        match self {
            Self::Struct(owner, field) => Some((*owner, *field)),
            Self::Builtin(_) => None,
        }
    }

    pub fn builtin_field(&self) -> Option<BuiltinField> {
        match self {
            Self::Builtin(field) => Some(*field),
            Self::Struct(_, _) => None,
        }
    }
}

// ── Resolution ────────────────────────────────────────────────────────────

/// What an identifier denotes — a name's resolution.
#[derive(Clone, PartialEq, Eq)]
pub enum Resolution {
    Var(DefId),
    Callable(Callable),
    Callables(Box<[Callable]>),
    Type(Type),
    Module(FileId),
    Literal(Literal),
    Local(Local),
    Field(Field),
    Variant(Variant),
    Builtin(BuiltinTy),
    MetaType(Type),
    This(DefId),
    Super(DefId),
}

impl Resolution {
    /// The resolution for a declaration used as a type name: a file is a
    /// module namespace, a def that denotes a type is a `Type`.
    pub fn from_type(db: &dyn HirDb, def: DefId) -> Resolution {
        if let DefId::File(file) = def {
            return Resolution::Module(file);
        }
        match def_to_type(db, def) {
            Some(ty) => Resolution::Type(ty),
            None => Resolution::Type(Type::invalid(db)),
        }
    }

}

impl From<Builtin> for Resolution {
    fn from(builtin: Builtin) -> Self {
        match builtin {
            Builtin::Obj(object) => Resolution::Builtin(object),
            Builtin::Field(field) => Resolution::Field(Field::new_builtin(field)),
            Builtin::Fn(function) => Resolution::Callable(Callable::new_builtin(function)),
        }
    }
}
