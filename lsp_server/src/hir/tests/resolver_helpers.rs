use crate::hir::resolver::*;
use crate::hir::exprs::Name;
use crate::hir::resolution::Resolution;
use super::helper::*;
use crate::hir::types::{Ty, TyKind, def_to_type};
use crate::salsa::{File, Db};

pub(super) fn contract_type(db: &Db, file: File, name: &str) -> Ty {
    def_to_type(db, find_contract(db, file, name))
        .expect("a contract is a type")
        .ty()
}

/// Wrap a nominal type as the `Types` receiver `T` used by `T.member`.
pub(super) fn types_receiver(db: &Db, ty: Ty) -> Ty {
    let name = ty.narrow(db).unwrap();
    Ty::new(db, TyKind::Type(name))
}

pub(super) fn lookup_member(resolver: &Resolver, ty: Ty, name: &Name) -> Option<Resolution> {
    resolver.member_named(ty, name).effective_resolution()
}

pub(super) fn member_resolutions(resolver: &Resolver, ty: Ty) -> Vec<Resolution> {
    resolver
        .members_of(ty)
        .into_values()
        .flat_map(|result| result.completion_resolutions())
        .collect()
}


