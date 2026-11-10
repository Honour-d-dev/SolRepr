use crate::ast;
use crate::hir::body_map::BodyOwnerId;
use crate::ir::def_map::DefId;

// Interned variants of the ast ids. At the salsa boundaries since non-interned values are not allowed
#[salsa::interned]
#[derive(Debug)]
pub struct Import<'db> {
    pub id: ast::ImportId,
}

#[salsa::interned]
#[derive(Debug)]
pub struct Contract<'db> {
    pub id: ast::ContractId,
}

#[salsa::interned]
#[derive(Debug)]
pub struct Interface<'db> {
    pub id: ast::InterfaceId,
}

#[salsa::interned]
#[derive(Debug)]
pub struct Library<'db> {
    pub id: ast::LibraryId,
}

#[salsa::interned]
#[derive(Debug)]
pub struct Function<'db> {
    pub id: ast::FunctionId,
}

#[salsa::interned]
#[derive(Debug)]
pub struct Modifier<'db> {
    pub id: ast::ModifierId,
}

#[salsa::interned]
#[derive(Debug)]
pub struct Struct<'db> {
    pub id: ast::StructId,
}

#[salsa::interned]
#[derive(Debug)]
pub struct Event<'db> {
    pub id: ast::EventId,
}

#[salsa::interned]
#[derive(Debug)]
pub struct Enum<'db> {
    pub id: ast::EnumId,
}

#[salsa::interned]
#[derive(Debug)]
pub struct Error<'db> {
    pub id: ast::ErrorId,
}

#[salsa::interned]
#[derive(Debug)]
pub struct Var<'db> {
    pub id: ast::VarId,
}

#[salsa::interned]
#[derive(Debug)]
pub struct Udvt<'db> {
    pub id: ast::UdvtId,
}

#[salsa::interned]
#[derive(Debug)]
pub struct Using<'db> {
    pub id: ast::UsingId,
}

#[derive(PartialEq, Eq, Clone, Hash, Debug)]
pub enum DefWithBasesId {
    Contract(ast::ContractId),
    Interface(ast::InterfaceId),
}

#[salsa::interned]
#[derive(Debug)]
pub struct DefWithBases<'db> {
    pub id: DefWithBasesId,
}

#[salsa::interned]
#[derive(Debug)]
pub struct BodyOwner<'db> {
    pub id: BodyOwnerId,
}


/// Interned Variant of the DefId.
#[salsa::interned]
#[derive(Debug)]
pub struct Id {
    pub id: DefId,
}

