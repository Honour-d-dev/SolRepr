pub mod completion;
pub mod definition;
pub mod diagnostics;
pub mod hover;
pub mod references;
pub mod rename;
pub mod view_ast;
pub mod view_hir;

use lsp_types::Range;

use crate::ast::{EnumId, NodeRange};
use crate::hir::body_map::LocalId;
use crate::hir::item_data::{FieldId, SemanticId, VariantId};
use crate::hir::resolution::{Field, Resolution};
use crate::ir::def_map::DefId;
use crate::salsa::{Db, File, HirDb};
use crate::utilities::byte_to_position;

/// A byte range in `file`, as an LSP range. The one conversion the capabilities
/// share.
pub(crate) fn lsp_range(db: &Db, file: File, range: NodeRange) -> Range {
    let rope = db.rope(file);
    Range {
        start: byte_to_position(&rope, range.start as usize),
        end: byte_to_position(&rope, range.end as usize),
    }
}

/// The declaration a spelling denotes — the handle the lowering allocated, never
/// a source position.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Identity {
    Def(DefId),
    Local { owner: DefId, local: LocalId },
    Field { owner: DefId, field: FieldId },
    Variant { enum_: EnumId, variant: VariantId },
}

impl Identity {
    pub fn file(&self) -> File {
        match self {
            Identity::Def(def) => def.file(),
            Identity::Local { owner, .. } | Identity::Field { owner, .. } => owner.file(),
            Identity::Variant { enum_, .. } => enum_.file,
        }
    }

    pub fn def(&self) -> DefId {
        match self {
            Identity::Def(def) => *def,
            Identity::Local { owner, .. } | Identity::Field { owner, .. } => *owner,
            Identity::Variant { enum_, .. } => DefId::Enum(*enum_),
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Identity::Local { .. })
    }
}

/// The single Identity `resolution` denotes; `None` when it denotes nothing
/// or several.
fn identity_of_resolution(db: &dyn HirDb, resolution: &Resolution) -> Option<Identity> {
    match resolution {
        Resolution::Var(def) | Resolution::This(def) | Resolution::Super(def) => Some(Identity::Def(*def)),
        Resolution::Callable(callable) => callable.def().map(Identity::Def),
        Resolution::Type(ty) => ty.ty().def_id(db).map(Identity::Def),
        Resolution::Module(file) => Some(Identity::Def(DefId::File(*file))),
        Resolution::Local(local) => Some(Identity::Local {
            owner: local.owner,
            local: local.id,
        }),
        Resolution::Field(Field::Struct(owner, field)) => Some(Identity::Field {
            owner: *owner,
            field: *field,
        }),
        Resolution::Variant(variant) => Some(Identity::Variant {
            enum_: variant.enum_id,
            variant: variant.id,
        }),
        Resolution::Field(Field::Builtin(_))
        | Resolution::Callables(_)
        | Resolution::Builtin(_)
        | Resolution::Literal(_)
        | Resolution::MetaType(_) => None,
    }
}

/// The single Identity a semantic `id` denotes, read from the def's own store.
pub fn identity_of_semantic(db: &dyn HirDb, def: DefId, id: SemanticId) -> Option<Identity> {
    match id {
        SemanticId::Local(local) => Some(Identity::Local { owner: def, local }),
        SemanticId::Field(field) => match def {
            DefId::Struct(_) => Some(Identity::Field { owner: def, field }),
            _ => None,
        },
        SemanticId::Variant(variant) => match def {
            DefId::Enum(enum_) => Some(Identity::Variant { enum_, variant }),
            _ => None,
        },
        SemanticId::Expr(expr) => db
            .inference(def)
            .expr_resolution(expr)
            .and_then(|resolution| identity_of_resolution(db, resolution)),
        SemanticId::Type(ty) => db.inference(def).ty(ty).ty().def_id(db).map(Identity::Def),
    }
}

#[cfg(test)]
mod tests;
