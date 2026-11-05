//! Namespace-aware declaration access for names and members.
//!
//! The resolver supplies the lexical caller, declaring owner, access path and
//! declaration group. This module decides both whether a declaration belongs
//! to that lookup namespace and whether its visibility permits access. A denied
//! declaration is invisible and does not shadow later candidates.

use crate::hir::types::{BaseKind, CallKind, VariableKind, ViaTypeKind, Visibility};
use crate::ir::def_map::{DefId, DefNamespace};
use crate::salsa::HirDb;

/// A declaration was reached but rejected by the visibility check.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AccessDenial {
    pub def: DefId,
    pub vis: Visibility,
}

/// The result of applying namespace membership and visibility together.
pub enum MemberScan {
    /// No declaration belongs to the requested member set.
    Skip,
    Denied(Vec<AccessDenial>),
    Hit { defs: Vec<DefId> },
}

/**
 * AccessChecker validates member access e.g In this soldidity code:
  ```solidity
   contract B { uint internal f; }
   contract C is B {}
  
   contract D {
       function g(C c) { 
           c.f;
       }
   }
  ```
 * For the `c.f` member access:
 * Container = D; owner = B
 * c.f is denied as `f` is not visible in `D` even though it is in `C`
 */
pub struct AccessChecker<'db> {
    db: &'db dyn HirDb,
    container: DefId,
}

impl<'db> AccessChecker<'db> {
    pub fn new(db: &'db dyn HirDb, container: DefId) -> Self {
        Self { db, container }
    }

    pub fn caller(&self) -> DefId {
        self.container
    }

    /// Whether `owner` is a base of the container — or the container itself.
    fn is_container_or_base(&self, owner: DefId) -> bool {
        self.container == owner || self.db.bases(self.container).contains(&owner)
    }

    /// Access kind when indexing members through `owner`'s type name.
    pub fn type_access_kind(&self, owner: DefId) -> CallKind {
        match owner {
            DefId::Library(_) => CallKind::ViaType(ViaTypeKind::Library),
            DefId::Contract(_) | DefId::Interface(_) if owner == self.container => {
                CallKind::Base(BaseKind::SelfQualified)
            }
            DefId::Contract(_) | DefId::Interface(_) if self.is_container_or_base(owner) => {
                CallKind::Base(BaseKind::Explicit)
            }
            _ => CallKind::ViaType(ViaTypeKind::NonLibrary),
        }
    }

    /// Applies membership [`Self::belongs_to_set`] and visibility [`Self::visible_in_set`] in one pass.
    /// Inaccessible declarations are returned as explanations only if no accessible overload exists.
    pub fn scan(&self, path: CallKind, owner: DefId, namespace: &DefNamespace, defs: &[DefId]) -> MemberScan {
        let mut accessible = Vec::with_capacity(defs.len());
        let mut denials = Vec::new();
        for &def in defs {
            if self.belongs_to_set(path, namespace, def) {
                match self.visible_in_set(path, owner, def) {
                    Some(denial) => denials.push(denial),
                    None => accessible.push(def),
                }
            }
        }

        match (accessible.is_empty(), denials.is_empty()) {
            (false, _) => MemberScan::Hit { defs: accessible },
            (_, false) => MemberScan::Denied(denials),
            _ => MemberScan::Skip,//both empty
        }
    }
    /// Membership check
    fn belongs_to_set(&self, path: CallKind, namespace: &DefNamespace, def: DefId) -> bool {
        match path {
            CallKind::Direct => true,
            CallKind::External => match (namespace, def) {
                (DefNamespace::Functions, DefId::Function(_)) => true,
                (DefNamespace::Variables, DefId::Var(_)) => true,
                _ => false,
            },
            CallKind::ViaType(_) | CallKind::Base(BaseKind::SelfQualified | BaseKind::Explicit) => {
                match namespace {
                    DefNamespace::Types
                    | DefNamespace::Functions
                    | DefNamespace::Modifiers
                    | DefNamespace::Events
                    | DefNamespace::Errors => true,
                    DefNamespace::Variables => {
                        matches!(
                            def,
                            DefId::Var(id) if self.db.var_data(id).kind == VariableKind::Const
                        )
                    }
                }
            }
            CallKind::Base(BaseKind::Super) => {
                matches!((namespace, def), (DefNamespace::Functions, DefId::Function(_)))
            }
            CallKind::Bound | CallKind::Static => false,
        }
    }

    /// Visibility check
    fn visible_in_set(&self, path: CallKind, owner: DefId, def: DefId) -> Option<AccessDenial> {
        let vis = self.def_visibility(def)?;
        if !matches!(owner, DefId::Contract(_) | DefId::Interface(_) | DefId::Library(_)) {
            return None;
        }

        let is_base_of_container = self.is_container_or_base(owner);
        let inaccessible = match path {
            CallKind::External => !matches!(
                (def, vis),
                (
                    DefId::Function(_),
                    Visibility::Public | Visibility::External
                ) | (DefId::Var(_), Visibility::Public)
            ),
            CallKind::ViaType(_) | CallKind::Base(BaseKind::SelfQualified | BaseKind::Explicit) => {
                match (def, vis) {
                    // Constants through a type name follow internal scoping, even
                    // when declared public. Private members never project here.
                    (DefId::Var(_), Visibility::Private) => true,
                    // `internal` members of a library inline into every caller
                    (_, Visibility::Internal) => {
                        !matches!(owner, DefId::Library(_)) && !is_base_of_container
                    }
                    (DefId::Var(_), _) => !is_base_of_container,
                    (_, Visibility::Private) => true,
                    (_, Visibility::Public | Visibility::External) => false,
                }
            }
            CallKind::Direct => match vis {
                Visibility::Private => self.container != owner,
                Visibility::Internal => !is_base_of_container,
                Visibility::Public => false,
                Visibility::External => true,
            },
            CallKind::Base(BaseKind::Super) => match vis {
                Visibility::Private | Visibility::External => true,
                Visibility::Internal | Visibility::Public => false,
            },
            CallKind::Bound | CallKind::Static => false,
        };

        inaccessible.then_some(AccessDenial { def, vis })
    }

    
    fn def_visibility(&self, def: DefId) -> Option<Visibility> {
        match def {
            DefId::Function(id) => Some(self.db.function_data(id).vis),
            DefId::Var(id) => Some(self.db.var_data(id).vis),
            _ => None,
        }
    }
}
