use la_arena::{Arena, ArenaMap};
use rustc_hash::FxHashMap;

use crate::ast::NodeRange;
use crate::hir::body_map::{BodyOwnerId, Local, LocalId, ScopeId};
use crate::hir::exprs::{Expr, ExprId};
use crate::hir::item_data::{Field, FieldId, SemanticId, Variant, VariantId};
use crate::hir::diagnostics::DiagnosticAnchor;
use crate::hir::types::{TyName, TyNameId};
use crate::ir::def_map::DefId;
use crate::salsa::HirDb;
use crate::salsa::root_db::def_range;

/// Lowered syntax content for one owner — no positions.
#[derive(PartialEq, Eq)]
pub struct SyntaxStore {
    pub types: Arena<TyName>,
    pub exprs: Arena<Expr>,
    /// The owner's own name occurrence at its declaration — an id, not a range.
    pub own_name_expr: Option<ExprId>,
}

impl Default for SyntaxStore {
    fn default() -> Self {
        Self {
            types: Arena::new(),
            exprs: Arena::new(),
            own_name_expr: None,
        }
    }
}

/// Positions for one owner's lowered syntax — a leaf artifact. Inference and
/// signatures never read this; only the capability and diagnostic boundary does.
#[derive(Default, PartialEq, Eq)]
pub struct SourceMap {
    pub expr_ranges: ArenaMap<ExprId, NodeRange>,
    pub ty_ref_ranges: ArenaMap<TyNameId, NodeRange>,
    pub local_ranges: ArenaMap<LocalId, NodeRange>,
    pub field_ranges: ArenaMap<FieldId, NodeRange>,
    pub variant_ranges: ArenaMap<VariantId, NodeRange>,
    pub vis_range: Option<NodeRange>,
    pub body_range: Option<NodeRange>,
    pub override_ranges: Box<[NodeRange]>,
    pub range_to_semantic: FxHashMap<NodeRange, SemanticId>,
}

/// Runs `f` on the owner's content store and source map; `None` for a file.
fn with_owner<R>(
    db: &dyn HirDb,
    owner: DefId,
    f: impl FnOnce(&SyntaxStore, &SourceMap) -> R,
) -> Option<R> {
    Some(match owner {
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
            f(&data.syntax_store, &db.source_map(owner))
        }
        DefId::Interface(id) => {
            let data = db.interface_data(id);
            f(&data.syntax_store, &db.source_map(owner))
        }
        DefId::Library(id) => {
            let data = db.library_data(id);
            f(&data.syntax_store, &db.source_map(owner))
        }
        DefId::Struct(id) => {
            let data = db.struct_data(id);
            f(&data.syntax_store, &db.source_map(owner))
        }
        DefId::Enum(id) => {
            let data = db.enum_data(id);
            f(&data.syntax_store, &db.source_map(owner))
        }
        DefId::Event(id) => {
            let data = db.event_data(id);
            f(&data.syntax_store, &db.source_map(owner))
        }
        DefId::Error(id) => {
            let data = db.error_data(id);
            f(&data.syntax_store, &db.source_map(owner))
        }
        DefId::Var(id) => {
            let data = db.var_data(id);
            f(&data.syntax_store, &db.source_map(owner))
        }
        DefId::Udvt(id) => {
            let data = db.udvt_data(id);
            f(&data.syntax_store, &db.source_map(owner))
        }
        DefId::Using(id) => {
            let data = db.using_data(id);
            f(&data.syntax_store, &db.source_map(owner))
        }
        DefId::Import(id) => {
            let data = db.import_data(id);
            f(&data.syntax_store, &db.source_map(owner))
        }
        DefId::File(_) => return None,
    })
}

pub fn semantic_at(db: &dyn HirDb, owner: DefId, range: NodeRange) -> Option<SemanticId> {
    with_owner(db, owner, |_, map| map.range_to_semantic.get(&range).copied())?
}

/// The callee an expression *denotes*: the expression itself, or — when it is a
/// call or a call-options node — the expression in its `callee` field.
///
/// `alloc_call_expr` remaps a bare-identifier callee's *own* range onto the
/// call, so reading through the call is what makes the callee reachable.
pub fn callee_at(db: &dyn HirDb, owner: DefId, range: NodeRange) -> Option<ExprId> {
    with_owner(db, owner, |store, map| {
        match map.range_to_semantic.get(&range) {
            Some(SemanticId::Expr(expr)) => match &store.exprs[*expr] {
                Expr::Call { callee, .. } | Expr::CallOptions { callee, .. } => Some(*callee),
                _ => Some(*expr),
            },
            _ => None,
        }
    })?
}

/// Resolve a diagnostic anchor to a source range in `owner`'s store.
pub(crate) fn anchor_range(
    db: &dyn HirDb,
    owner: DefId,
    anchor: DiagnosticAnchor,
) -> Option<NodeRange> {
    match anchor {
        DiagnosticAnchor::Expr(expr) => {
            with_owner(db, owner, |_, map| map.expr_ranges.get(expr).copied())?
        }
        DiagnosticAnchor::TyRef(ty_ref) => {
            with_owner(db, owner, |_, map| map.ty_ref_ranges.get(ty_ref).copied())?
        }
        DiagnosticAnchor::Local(local) => {
            with_owner(db, owner, |_, map| map.local_ranges.get(local).copied())?
        }
        DiagnosticAnchor::Def(def) => def_range(db, def),
        DiagnosticAnchor::Body(def) => match def {
            DefId::Modifier(_) => db.source_map(def).body_range,
            _ => None,
        },
    }
}

#[derive(Clone, Copy)]
pub struct OwnerStore<'a> {
    pub store: &'a SyntaxStore,
    pub locals: Option<&'a Arena<Local>>,
    pub fields: Option<&'a Arena<Field>>,
    pub variants: Option<&'a Arena<Variant>>,
    pub expr_scopes: Option<&'a ArenaMap<ExprId, ScopeId>>,
    pub expr_orders: Option<&'a ArenaMap<ExprId, u32>>,
}

impl<'a> OwnerStore<'a> {
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

/// Runs `f` with the owner's inputs; `None` for a file. A callback because the
/// inputs borrow a per-owner `Arc` and must not outlive this call.
pub(crate) fn with_owner_store<R>(
    db: &dyn HirDb,
    owner: DefId,
    f: impl FnOnce(OwnerStore<'_>) -> R,
) -> Option<R> {
    Some(match owner {
        DefId::Function(id) => {
            let body = db.body_map(BodyOwnerId::Function(id));
            f(OwnerStore::new(&body.syntax_store)
                .with_locals(&body.locals)
                .with_expr_scopes(&body.expr_scopes)
                .with_expr_orders(&body.expr_orders))
        }
        DefId::Modifier(id) => {
            let body = db.body_map(BodyOwnerId::Modifier(id));
            f(OwnerStore::new(&body.syntax_store)
                .with_locals(&body.locals)
                .with_expr_scopes(&body.expr_scopes)
                .with_expr_orders(&body.expr_orders))
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
