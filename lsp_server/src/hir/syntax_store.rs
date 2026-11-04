use la_arena::{Arena, ArenaMap};
use rustc_hash::FxHashMap;

use crate::ast::NodeRange;
use crate::hir::body_map::{BodyOwnerId, BodySourceMap, Local};
use crate::hir::exprs::{Expr, ExprId};
use crate::hir::item_data::{Field, SemanticId, Variant};
use crate::hir::types::{TyName, TyNameId};
use crate::ir::def_map::DefId;
use crate::salsa::HirDb;

#[derive(PartialEq, Eq)]
pub struct SyntaxStore {
    pub types: Arena<TyName>,
    pub exprs: Arena<Expr>,
    pub expr_ranges: ArenaMap<ExprId, NodeRange>,
    pub ty_ref_ranges: ArenaMap<TyNameId, NodeRange>,
    pub range_to_semantic: FxHashMap<NodeRange, SemanticId>,
}

impl Default for SyntaxStore {
    fn default() -> Self {
        Self {
            types: Arena::new(),
            exprs: Arena::new(),
            expr_ranges: ArenaMap::new(),
            ty_ref_ranges: ArenaMap::new(),
            range_to_semantic: FxHashMap::default(),
        }
    }
}


/// Runs `f` on the owner's `SyntaxStore`; `None` when the owner has none.
/// A callback because body and item stores live behind a per-owner `Arc`.
fn with_store<R>(
    db: &dyn HirDb,
    owner: DefId,
    f: impl FnOnce(&SyntaxStore) -> R,
) -> Option<R> {
    Some(match owner {
        DefId::Function(id) => f(&db.body_map(BodyOwnerId::Function(id)).syntax_store),
        DefId::Modifier(id) => f(&db.body_map(BodyOwnerId::Modifier(id)).syntax_store),
        DefId::Contract(id) => f(&db.contract_data(id).syntax_store),
        DefId::Interface(id) => f(&db.interface_data(id).syntax_store),
        DefId::Library(id) => f(&db.library_data(id).syntax_store),
        DefId::Struct(id) => f(&db.struct_data(id).syntax_store),
        DefId::Enum(id) => f(&db.enum_data(id).syntax_store),
        DefId::Event(id) => f(&db.event_data(id).syntax_store),
        DefId::Error(id) => f(&db.error_data(id).syntax_store),
        DefId::Var(id) => f(&db.var_data(id).syntax_store),
        DefId::Udvt(id) => f(&db.udvt_data(id).syntax_store),
        DefId::Using(id) => f(&db.using_data(id).syntax_store),
        DefId::Import(id) => f(&db.import_data(id).syntax_store),
        DefId::File(_) => return None,
    })
}

pub fn semantic_at(db: &dyn HirDb, owner: DefId, range: NodeRange) -> Option<SemanticId> {
    with_store(db, owner, |store| store.range_to_semantic.get(&range).copied())?
}

/// The callee an expression *denotes*: the expression itself, or — when it is a
/// call or a call-options node — the expression in its `callee` field.
///
/// `alloc_call_expr` remaps a bare-identifier callee's *own* range onto the
/// call, so reading through the call is what makes the callee reachable.
pub fn callee_at(db: &dyn HirDb, owner: DefId, range: NodeRange) -> Option<ExprId> {
    with_store(db, owner, |store| match store.range_to_semantic.get(&range) {
        Some(SemanticId::Expr(expr)) => match &store.exprs[*expr] {
            Expr::Call { callee, .. } | Expr::CallOptions { callee, .. } => Some(*callee),
            _ => Some(*expr),
        },
        _ => None,
    })?
}


#[derive(Clone, Copy)]
pub struct OwnerStore<'a> {
    pub store: &'a SyntaxStore,
    pub locals: Option<&'a Arena<Local>>,
    pub fields: Option<&'a Arena<Field>>,
    pub variants: Option<&'a Arena<Variant>>,
    pub source_map: Option<&'a BodySourceMap>,
}

impl<'a> OwnerStore<'a> {
    pub fn new(store: &'a SyntaxStore) -> Self {
        Self {
            store,
            locals: None,
            fields: None,
            variants: None,
            source_map: None,
        }
    }

    pub fn with_locals(mut self, locals: &'a Arena<Local>) -> Self {
        self.locals = Some(locals);
        self
    }

    pub fn with_fields(mut self, fields: &'a Arena<Field>) -> Self {
        self.fields = Some(fields);
        self
    }

    pub fn with_variants(mut self, variants: &'a Arena<Variant>) -> Self {
        self.variants = Some(variants);
        self
    }

    pub fn with_source_map(mut self, source_map: &'a BodySourceMap) -> Self {
        self.source_map = Some(source_map);
        self
    }
}

/// Runs `f` with the owner's inputs; `None` for a file. A callback because the
/// inputs borrow a per-owner `Arc` and must not outlive this call.
pub(crate) fn with_owner_store<R>(
    db: &dyn HirDb,
    owner: DefId,
    f: impl FnOnce(OwnerStore<'_>) -> R,
) -> Option<R> {
    Some(match owner {
        DefId::Function(id) => {
            let (body, source_map) = db.body_and_source_map(BodyOwnerId::Function(id));
            f(OwnerStore::new(&body.syntax_store)
                .with_locals(&body.locals)
                .with_source_map(&source_map))
        }
        DefId::Modifier(id) => {
            let (body, source_map) = db.body_and_source_map(BodyOwnerId::Modifier(id));
            f(OwnerStore::new(&body.syntax_store)
                .with_locals(&body.locals)
                .with_source_map(&source_map))
        }
        DefId::Event(id) => {
            let data = db.event_data(id);
            f(OwnerStore::new(&data.syntax_store).with_locals(&data.parameters))
        }
        DefId::Error(id) => {
            let data = db.error_data(id);
            f(OwnerStore::new(&data.syntax_store).with_locals(&data.parameters))
        }
        DefId::Struct(id) => {
            let data = db.struct_data(id);
            f(OwnerStore::new(&data.syntax_store).with_fields(&data.fields))
        }
        DefId::Enum(id) => {
            let data = db.enum_data(id);
            f(OwnerStore::new(&data.syntax_store).with_variants(&data.variants))
        }
        DefId::Import(id) => f(OwnerStore::new(&db.import_data(id).syntax_store)),
        DefId::Udvt(id) => f(OwnerStore::new(&db.udvt_data(id).syntax_store)),
        DefId::Using(id) => f(OwnerStore::new(&db.using_data(id).syntax_store)),
        DefId::Contract(id) => f(OwnerStore::new(&db.contract_data(id).syntax_store)),
        DefId::Interface(id) => f(OwnerStore::new(&db.interface_data(id).syntax_store)),
        DefId::Library(id) => f(OwnerStore::new(&db.library_data(id).syntax_store)),
        DefId::Var(id) => f(OwnerStore::new(&db.var_data(id).syntax_store)),
        DefId::File(_) => return None,
    })
}
