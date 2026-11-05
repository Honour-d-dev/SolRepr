use crate::ast::{ContractId, FunctionId, VarId};
use crate::hir::builtins::{BuiltinFn, StaticName};
use crate::hir::types::kind::TyKind;
use crate::hir::types::primitive::Primitive;
use crate::hir::types::syntax::{Location, Mutability, Visibility};
use crate::hir::types::ty::{Ty, Type};
use crate::ir::def_map::DefId;
use crate::salsa::HirDb;

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Fn<T> {
    pub visibility: Visibility,
    pub mutability: Mutability,
    pub params: Box<[T]>,
    pub returns: Box<[T]>,
}

impl<T> Default for Fn<T> {
    fn default() -> Self {
        Self {
            visibility: Visibility::default(),
            mutability: Mutability::default(),
            params: Box::default(),
            returns: Box::default(),
        }
    }
}

/// `{value, gas}` — the option set of an external message call.
pub const EXTERNAL_CALL_OPTIONS: &[StaticName] = &["value", "gas"];

/// The value type a call option expects; `None` for an unknown option.
pub fn option_type(name: &str) -> Option<Primitive> {
    match name {
        "value" | "gas" => Some(Primitive::Uint(256)),
        "salt" => Some(Primitive::FixedBytes(32)),
        _ => None,
    }
}



impl Fn<Type> {
    pub fn options(&self) -> Option<&'static [StaticName]> {
        (self.visibility == Visibility::External).then_some(EXTERNAL_CALL_OPTIONS)
    }

    pub fn result(&self, db: &dyn HirDb) -> Ty {
        Ty::from_types(db, &self.returns)
    }

    /// The signature an external caller sees — a written `calldata` param
    /// decodes to its default location; `storage` keeps its location.
    pub fn external_abi(&self, db: &dyn HirDb) -> Fn<Type> {
        Fn {
            params: Fn::to_external_abi_types(db, &self.params),
            returns: Fn::to_external_abi_types(db, &self.returns),
            ..self.clone()
        }
    }

    /// The signature form of a type list: a written `calldata` decodes to its
    /// default location; `storage` keeps its location.
    pub fn to_external_abi_types(db: &dyn HirDb, types: &[Type]) -> Box<[Type]> {
        types
            .iter()
            .map(|&ty| match ty.kind(db) {
                TyKind::Ref(inner, Location::Calldata) => inner.upcast(db),
                _ => ty,
            })
            .collect()
    }

    /// Fn to FnPtr conversion helper.
    /// see [`CallKind`]
    pub(crate) fn apply_kind(&self, kind: CallKind, db: &dyn HirDb) -> Option<Fn<Type>> {
        let visibility = match kind {
            // `this.f` — a message call, encoded as an address plus a selector.
            CallKind::External => {
                return Some(Fn {
                    visibility: Visibility::External,
                    ..self.external_abi(db)
                })
            }
            // A bare name is a jump, so an `external` declaration has no internal reference.
            CallKind::Direct => {
                if self.visibility == Visibility::External {
                    return None;
                }
                Visibility::Internal
            }
            // Qualified self/base/super references select an internal
            // implementation. An `external` declaration may still resolve
            // as a declaration reference through an explicit contract name,
            // but it is not an internal function pointer.
            CallKind::Base(_) if self.visibility == Visibility::External => return None,
            CallKind::Base(_) => Visibility::Internal,
            // `L.f` is a jump only for an *internal* library function:
            CallKind::ViaType(ViaTypeKind::Library) if self.visibility == Visibility::Internal => {
                Visibility::Internal
            }
            // A declaration-only reference, a `using`-attached function, or a
            // static builtin is not a value.
            CallKind::ViaType(_) | CallKind::Bound | CallKind::Static => return None,
        };
        Some(Fn {
            visibility,
            ..self.clone()
        })
    }

    pub fn display(&self, db: &dyn HirDb) -> String {
        let params = self
            .params
            .iter()
            .map(|p| p.display(db))
            .collect::<Vec<_>>()
            .join(", ");
        let mut out = format!("function({params}) {}", self.visibility.as_str());
        if self.mutability != Mutability::NonPayable {
            out.push(' ');
            out.push_str(self.mutability.as_str());
        }
        if !self.returns.is_empty() {
            let ret = self
                .returns
                .iter()
                .map(|r| r.display(db))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!(" returns ({ret})"));
        }
        out
    }
}

/// CallKind carries information on how a call/callable was reached.
/// This is useful for simultaneously encoding 2 behaviours.
/// - visible member properties e.g address, selector etc
/// - Call semantics, how the fn is called or if it's even callable.
///
/// **Call-semantics** refers to how the call is invoked. There are 3:
/// - **jump** — an inlined call inside the current code unit (`f()`, `super.f()`, and `L.f()` when the library function is `internal`).
/// - **call** — a message call to another account (`CALL`), encoded as an address plus a selector (`this.f()`, `c.f()`).
/// - **delegatecall** — a message call that keeps the caller's context: storage, balance, `msg.sender`/`msg.value` and `address(this)` (`L.f()` when the library function is `public`/`external`).
///
/// Call-semantics might appear linked to visibility, but it is infact a property of "visibility + callKind"
/// for example - a "Direct" call of a public function is a "jump" while an "External" call of same function is a "call"
/// "ViaType/Bound" + "Visibility" decides whether reaching a library fn is a "jump" or "delegatecall".
///
/// This is particularly useful during fn -> fn-pointer conversions.
/// The visibility of the resulting fn-pointer is a function of the call-semantics of the fn (not just its visibility)
/// i.e A "Direct" "piblic" function converts to an internal fn-pointer, due to its call-semantics.
/// A delegatecall does not convert to pointer.
///
/// see [`TyKind::fn_to_fn_ptr_conversion`] [`Fn::apply_kind`]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallKind {
    /// A direct/internal call/reference: `f()`/`f`, errors, events and
    /// modifier calls reached through unqualified names.
    Direct,
    /// An externally-accessed fn (`this.f`, `c.f`, public getter). Calls via an instance that invoke a message-call
    External,
    /// A call reached through an unrelated contract/interface type name
    /// or a library type name — `X.f` or `L.f`.
    ViaType(ViaTypeKind),
    /// A contract-qualified internal reference or base access path.
    Base(BaseKind),
    /// A `using`-attached fn (`x.extension`).
    Bound,
    /// A static builtin fn — with no call-semantics as it's neither a jump/message call.
    /// Non-static builtin member fn's callable through an instance (eg `address.call`) are [`CallKind::External`] as they're message-calls
    Static,
}

impl CallKind {
    pub fn is_external(self) -> bool {
        self == Self::External
    }

    pub fn is_bound(self) -> bool {
        self == Self::Bound
    }

    /// Number of implicit receiver arguments. Solidity permits at most one.
    pub fn bound_args(self) -> u8 {
        u8::from(self.is_bound())
    }
}


#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum BaseKind {
    /// `C.f` inside `C`; the name is contract-qualified but selects an
    /// implementation directly rather than using virtual dispatch.
    SelfQualified,
    /// `Base.f` where `Base` is a strict base of the lexical caller.
    Explicit,
    /// `super.f` — collected from the caller's C3 ancestors.
    Super,
}


#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum ViaTypeKind {
    Library,
    NonLibrary,
}

impl ViaTypeKind {
    pub fn from_owner(owner: DefId) -> Self {
        match owner {
            DefId::Library(_) => Self::Library,
            _ => Self::NonLibrary,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Callee {
    Fn(FunctionId),
    Getter(VarId),
    Constructor(ContractId),
    /// An allocation, as denoted by `new T[](n)`, `new bytes(n)`, or new string(n)`
    Allocation(Type),
    Builtin(BuiltinFn),
}

