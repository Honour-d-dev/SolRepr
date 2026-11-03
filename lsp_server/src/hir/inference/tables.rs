use la_arena::{Arena, ArenaMap};
use triomphe::Arc;

use crate::hir::body_map::{Local, LocalId};
use crate::hir::exprs::ExprId;
use crate::hir::item_data::{FieldId, SemanticId, SyntaxStore, VariantId};
use crate::hir::resolution::Resolution;
use crate::hir::semantic_errors::SemanticDiagnostic;
use crate::hir::types::{Mutability, Ty, TyNameId, Type};
use crate::salsa::HirDb;

/// Inference results keyed by arena index. `expr_types` is total;
/// `expr_resolutions` is sparse — only syntax that names a declaration/type.
#[derive(Clone, PartialEq, Eq)]
pub struct Inference {
    pub expr_types: ArenaMap<ExprId, Ty>,
    pub expr_resolutions: ArenaMap<ExprId, Resolution>,
    pub locals: ArenaMap<LocalId, Type>,
    pub fields: ArenaMap<FieldId, Type>,
    pub variants: ArenaMap<VariantId, Type>,
    pub types: ArenaMap<TyNameId, Type>,
    /// The least state mutability this body requires (`Pure` < `View` <
    /// `NonPayable`). A modifier declares none, so this is where its effect
    /// survives for the functions that invoke it.
    pub effect: Mutability,
    /// Semantic diagnostics produced during inference.
    pub diagnostics: Arc<[SemanticDiagnostic]>,
}

impl Inference {
    pub fn empty() -> Self {
        Self {
            expr_types: ArenaMap::new(),
            expr_resolutions: ArenaMap::new(),
            locals: ArenaMap::new(),
            fields: ArenaMap::new(),
            variants: ArenaMap::new(),
            types: ArenaMap::new(),
            effect: Mutability::Pure,
            diagnostics: Arc::from(Vec::new()),
        }
    }

    /// Recovery tables for a salsa cycle: every allocated ID gets `Invalid`.
    pub fn invalid_for(
        db: &dyn HirDb,
        store: &SyntaxStore,
        locals: Option<&Arena<Local>>,
        fields: Option<&Arena<crate::hir::item_data::Field>>,
        variants: Option<&Arena<crate::hir::item_data::Variant>>,
    ) -> Self {
        let mut tables = Self::empty();
        for (ty_ref_id, _) in store.types.iter() {
            tables.set_ty(ty_ref_id, Type::invalid(db));
        }
        for (expr_id, _) in store.exprs.iter() {
            tables.set_expr_ty(expr_id, Ty::invalid(db));
        }
        if let Some(locals) = locals {
            for (local_id, _) in locals.iter() {
                tables.set_local(local_id, Type::invalid(db));
            }
        }
        if let Some(fields) = fields {
            for (field_id, _) in fields.iter() {
                tables.set_field(field_id, Type::invalid(db));
            }
        }
        if let Some(variants) = variants {
            for (variant_id, _) in variants.iter() {
                tables.set_variant(variant_id, Type::invalid(db));
            }
        }
        tables
    }

    // ── Construction-time probes ─────────────────────────────────────────
    //
    // Inference writes results through these while it is still running, so
    // absence here means "not computed yet", not "failed".

    pub(crate) fn try_expr_ty(&self, id: ExprId) -> Option<Ty> {
        self.expr_types.get(id).copied()
    }

    pub(crate) fn try_ty(&self, id: TyNameId) -> Option<Type> {
        self.types.get(id).copied()
    }

    // ── Direct accessors by ID ────────────────────────────────────────────

    /// Sparse by design: only syntax that names a declaration/type has
    /// a resolution.
    pub fn expr_resolution(&self, id: ExprId) -> Option<&Resolution> {
        self.expr_resolutions.get(id)
    }

    pub fn expr_ty(&self, id: ExprId) -> Ty {
        self.expr_types[id]
    }

    pub fn local_ty(&self, id: LocalId) -> Type {
        self.locals[id]
    }

    pub fn field_ty(&self, id: FieldId) -> Type {
        self.fields[id]
    }

    pub fn variant_ty(&self, id: VariantId) -> Type {
        self.variants[id]
    }

    pub fn ty(&self, id: TyNameId) -> Type {
        self.types[id]
    }

    // ── SemanticId dispatch ──────────────────────────────────────────────

    pub fn sem_ty(&self, id: SemanticId) -> Ty {
        match id {
            SemanticId::Expr(e) => self.expr_ty(e),
            SemanticId::Local(l) => self.local_ty(l).ty(),
            SemanticId::Field(f) => self.field_ty(f).ty(),
            SemanticId::Variant(v) => self.variant_ty(v).ty(),
            SemanticId::Type(t) => self.ty(t).ty(),
        }
    }

    // ── Mutation helpers ─────────────────────────────────────────────────

    pub fn set_expr_ty(&mut self, id: ExprId, ty: Ty) {
        self.expr_types.insert(id, ty);
    }

    pub fn set_expr_resolution(&mut self, id: ExprId, resolution: Resolution) {
        self.expr_resolutions.insert(id, resolution);
    }

    pub fn set_local(&mut self, id: LocalId, ty: Type) {
        self.locals.insert(id, ty);
    }

    pub fn set_field(&mut self, id: FieldId, ty: Type) {
        self.fields.insert(id, ty);
    }

    pub fn set_variant(&mut self, id: VariantId, ty: Type) {
        self.variants.insert(id, ty);
    }

    pub fn set_ty(&mut self, id: TyNameId, ty: Type) {
        self.types.insert(id, ty);
    }
}
