use la_arena::{Arena, ArenaMap};
use rustc_hash::FxHashMap;

use crate::ast::NodeRange;
use crate::hir::body_map::{BodyOwnerId, Local, LocalId, ScopeId};
use crate::hir::diagnostics::DiagnosticAnchor;
use crate::hir::exprs::{Expr, ExprId};
use crate::hir::item_data::{Field, FieldId, SemanticId, Variant, VariantId};
use crate::hir::types::{TyName, TyNameId};
use crate::ir::def_map::DefId;
use crate::salsa::HirDb;
use crate::salsa::root_db::def_range;

/// Lowered syntax content for one def — no positions.
#[derive(PartialEq, Eq)]
pub struct SyntaxStore {
    pub types: Arena<TyName>,
    pub exprs: Arena<Expr>,
    /// The def's own declared name, when it has one.
    pub name: Option<ExprId>,
}

impl Default for SyntaxStore {
    fn default() -> Self {
        Self {
            types: Arena::new(),
            exprs: Arena::new(),
            name: None,
        }
    }
}

/// Positions for one def's lowered syntax — a leaf artifact. Inference and
/// signatures never read this; only the capability and diagnostic boundary does.
#[derive(Default, PartialEq, Eq)]
pub struct SourceMap {
    pub expr_ranges: ArenaMap<ExprId, NodeRange>,
    pub ty_ref_ranges: ArenaMap<TyNameId, NodeRange>,
    pub local_ranges: ArenaMap<LocalId, NodeRange>,
    pub field_ranges: ArenaMap<FieldId, NodeRange>,
    pub variant_ranges: ArenaMap<VariantId, NodeRange>,
    pub vis_range: Option<NodeRange>,
    pub override_ranges: Box<[NodeRange]>,
    pub range_to_semantic: FxHashMap<NodeRange, SemanticId>,
}

/// Runs `f` on the def's content store and source map; `None` for a file.
fn with_stores<R>(db: &dyn HirDb, def: DefId, f: impl FnOnce(&SyntaxStore, &SourceMap) -> R) -> Option<R> {
    Some(match def {
        DefId::Function(id) => {
            let body = db.body_map(BodyOwnerId::Function(id));
            let map = db.body_source_map(BodyOwnerId::Function(id));
            f(&body.syntax_store, &map)
        }
        DefId::Modifier(id) => {
            let body = db.body_map(BodyOwnerId::Modifier(id));
            let map = db.body_source_map(BodyOwnerId::Modifier(id));
            f(&body.syntax_store, &map)
        }
        DefId::Contract(id) => {
            let data = db.contract_data(id);
            f(&data.syntax_store, &db.source_map(def))
        }
        DefId::Interface(id) => {
            let data = db.interface_data(id);
            f(&data.syntax_store, &db.source_map(def))
        }
        DefId::Library(id) => {
            let data = db.library_data(id);
            f(&data.syntax_store, &db.source_map(def))
        }
        DefId::Struct(id) => {
            let data = db.struct_data(id);
            f(&data.syntax_store, &db.source_map(def))
        }
        DefId::Enum(id) => {
            let data = db.enum_data(id);
            f(&data.syntax_store, &db.source_map(def))
        }
        DefId::Event(id) => {
            let data = db.event_data(id);
            f(&data.syntax_store, &db.source_map(def))
        }
        DefId::Error(id) => {
            let data = db.error_data(id);
            f(&data.syntax_store, &db.source_map(def))
        }
        DefId::Var(id) => {
            let data = db.var_data(id);
            f(&data.syntax_store, &db.source_map(def))
        }
        DefId::Udvt(id) => {
            let data = db.udvt_data(id);
            f(&data.syntax_store, &db.source_map(def))
        }
        DefId::Using(id) => {
            let data = db.using_data(id);
            f(&data.syntax_store, &db.source_map(def))
        }
        DefId::Import(id) => {
            let data = db.import_data(id);
            f(&data.syntax_store, &db.source_map(def))
        }
        DefId::File(_) => return None,
    })
}

pub fn semantic_at(db: &dyn HirDb, def: DefId, range: NodeRange) -> Option<SemanticId> {
    with_stores(db, def, |_, map| map.range_to_semantic.get(&range).copied())?
}

/// Every spelled occurrence in `def`'s store; a file has none. A callback
/// because the map borrows a per-def `Arc` that must not outlive the call.
pub(crate) fn for_each_semantic(db: &dyn HirDb, def: DefId, mut f: impl FnMut(NodeRange, SemanticId)) {
    let _ = with_stores(db, def, |_, map| {
        for (&range, &id) in &map.range_to_semantic {
            f(range, id);
        }
    });
}

/// The callee an expression *denotes*: the expression itself, or — when it is a
/// call or a call-options node — the expression in its `callee` field.
///
/// `alloc_call_expr` remaps a bare-identifier callee's *own* range onto the
/// call, so reading through the call is what makes the callee reachable.
pub fn callee_at(db: &dyn HirDb, def: DefId, range: NodeRange) -> Option<ExprId> {
    with_stores(db, def, |store, map| match map.range_to_semantic.get(&range) {
        Some(SemanticId::Expr(expr)) => match &store.exprs[*expr] {
            Expr::Call { callee, .. } | Expr::CallOptions { callee, .. } => Some(*callee),
            _ => Some(*expr),
        },
        _ => None,
    })?
}

/// Resolve a diagnostic anchor to a source range.
///
/// A function or modifier owns two stores — its item data (signatures, override
/// lists) and its body — so a token may live in either; the item map is the
/// fallback when the body map misses.
pub(crate) fn anchor_range(db: &dyn HirDb, anchor: DiagnosticAnchor) -> Option<NodeRange> {
    let def = anchor.def_id();
    let Some(id) = anchor.token_id() else {
        return def_range(db, def);
    };

    let lookup = |map: &SourceMap| match id {
        SemanticId::Expr(expr) => map.expr_ranges.get(expr).copied(),
        SemanticId::Type(ty_ref) => map.ty_ref_ranges.get(ty_ref).copied(),
        SemanticId::Local(local) => map.local_ranges.get(local).copied(),
        SemanticId::Field(field) => map.field_ranges.get(field).copied(),
        SemanticId::Variant(variant) => map.variant_ranges.get(variant).copied(),
    };
    with_stores(db, def, |_, map| lookup(map))
        .flatten()
        .or_else(|| lookup(&db.source_map(def)))
}

#[derive(Clone, Copy)]
pub struct SemanticStore<'a> {
    pub store: &'a SyntaxStore,
    pub locals: Option<&'a Arena<Local>>,
    pub fields: Option<&'a Arena<Field>>,
    pub variants: Option<&'a Arena<Variant>>,
    pub expr_scopes: Option<&'a ArenaMap<ExprId, ScopeId>>,
    pub expr_orders: Option<&'a ArenaMap<ExprId, u32>>,
}

impl<'a> SemanticStore<'a> {
    pub fn new(store: &'a SyntaxStore) -> Self {
        Self {
            store,
            locals: None,
            fields: None,
            variants: None,
            expr_scopes: None,
            expr_orders: None,
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

    pub fn with_expr_scopes(mut self, expr_scopes: &'a ArenaMap<ExprId, ScopeId>) -> Self {
        self.expr_scopes = Some(expr_scopes);
        self
    }

    pub fn with_expr_orders(mut self, expr_orders: &'a ArenaMap<ExprId, u32>) -> Self {
        self.expr_orders = Some(expr_orders);
        self
    }
}

/// Runs `f` with the def's inputs; `None` for a file. A callback because the
/// inputs borrow a per-def `Arc` and must not outlive this call.
pub(crate) fn with_semantic_store<R>(db: &dyn HirDb, def: DefId, f: impl FnOnce(SemanticStore<'_>) -> R) -> Option<R> {
    Some(match def {
        DefId::Function(id) => {
            let body = db.body_map(BodyOwnerId::Function(id));
            f(SemanticStore::new(&body.syntax_store)
                .with_locals(&body.locals)
                .with_expr_scopes(&body.expr_scopes)
                .with_expr_orders(&body.expr_orders))
        }
        DefId::Modifier(id) => {
            let body = db.body_map(BodyOwnerId::Modifier(id));
            f(SemanticStore::new(&body.syntax_store)
                .with_locals(&body.locals)
                .with_expr_scopes(&body.expr_scopes)
                .with_expr_orders(&body.expr_orders))
        }
        DefId::Event(id) => {
            let data = db.event_data(id);
            f(SemanticStore::new(&data.syntax_store).with_locals(&data.parameters))
        }
        DefId::Error(id) => {
            let data = db.error_data(id);
            f(SemanticStore::new(&data.syntax_store).with_locals(&data.parameters))
        }
        DefId::Struct(id) => {
            let data = db.struct_data(id);
            f(SemanticStore::new(&data.syntax_store).with_fields(&data.fields))
        }
        DefId::Enum(id) => {
            let data = db.enum_data(id);
            f(SemanticStore::new(&data.syntax_store).with_variants(&data.variants))
        }
        DefId::Import(id) => f(SemanticStore::new(&db.import_data(id).syntax_store)),
        DefId::Udvt(id) => f(SemanticStore::new(&db.udvt_data(id).syntax_store)),
        DefId::Using(id) => f(SemanticStore::new(&db.using_data(id).syntax_store)),
        DefId::Contract(id) => f(SemanticStore::new(&db.contract_data(id).syntax_store)),
        DefId::Interface(id) => f(SemanticStore::new(&db.interface_data(id).syntax_store)),
        DefId::Library(id) => f(SemanticStore::new(&db.library_data(id).syntax_store)),
        DefId::Var(id) => f(SemanticStore::new(&db.var_data(id).syntax_store)),
        DefId::File(_) => return None,
    })
}
