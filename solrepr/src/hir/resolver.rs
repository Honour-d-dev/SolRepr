use std::cell::RefCell;

use triomphe::Arc;

use crate::ast::NodeRange;
use crate::hir::access::{AccessChecker, AccessDenial};
use crate::hir::body_map::{BodyMap, BodyOwnerId, ScopeId};
use crate::hir::builtins::BuiltinDb;
use crate::hir::exprs::{ExprId, Name};
use crate::hir::member_lookup::MemberLookupResult;
use crate::hir::resolution::{Callable, Local, Resolution};
use crate::hir::semantic_errors::{DiagnosticAnchor, DiagnosticSink, SemanticDiagnostic};
use crate::hir::types::{Primitive, Ty, Type};
use crate::ir::def_map::{DefId, DefMap, DefNamespace, ScopeData};
use crate::salsa::{File, HirDb};
use crate::salsa::root_db::def_name_and_range;

/// The outcome of a bare-name lookup walk.
enum NameOutcome {
    Found(Resolution),
    Conflict(Vec<DefId>),
    Missed(Vec<AccessDenial>),
}

pub struct Resolver<'db> {
    pub(super) db: &'db dyn HirDb,
    defmap: Arc<DefMap>,
    container: DefId,
    this: DefId,
    body: Option<Arc<BodyMap>>,
    pub(super) access: AccessChecker<'db>,
    /// Declaration-phase resolvers run before `members` exists, so
    /// member names resolve through the declaring scopes and bases only.
    declaration_only: bool,
    diagnostics: RefCell<DiagnosticSink>,
}

impl<'db> Resolver<'db> {
    pub fn build(db: &'db dyn HirDb, this: DefId) -> Resolver<'db> {
        let body = match this {
            DefId::Function(id) => Some(db.body_map(BodyOwnerId::Function(id))),
            DefId::Modifier(id) => Some(db.body_map(BodyOwnerId::Modifier(id))),
            _ => None,
        };
        Self::new(db, this, body, false)
    }

    /// A resolver for the declaration phase, which must not query
    /// `member_map` or the body map of its container.
    pub fn for_declaration(db: &'db dyn HirDb, this: DefId) -> Resolver<'db> {
        Self::new(db, this, None, true)
    }

    fn new(
        db: &'db dyn HirDb,
        this: DefId,
        body: Option<Arc<BodyMap>>,
        declaration_only: bool,
    ) -> Resolver<'db> {
        let defmap = db.file_def_map(this.file());
        let data = defmap.defs.get(&this).unwrap();
        let container = defmap.scopes[data.child_scope.unwrap_or(data.scope)].owner;

        Resolver {
            db,
            defmap,
            container,
            body,
            this,
            access: AccessChecker::new(db, container),
            declaration_only,
            diagnostics: RefCell::new(DiagnosticSink::default()),
        }
    }

    /// Finish the diagnostics accumulated by reporting lookups in this
    /// resolver. Inference calls this once after its body analysis completes.
    pub fn finish_diagnostics(&self) -> Arc<[SemanticDiagnostic]> {
        std::mem::take(&mut *self.diagnostics.borrow_mut()).finish()
    }

    fn record_lookup_failure(&self, diagnostic: SemanticDiagnostic) {
        self.diagnostics.borrow_mut().push(diagnostic);
    }

    pub fn builtins(&self) -> BuiltinDb<'db> {//FIXME: this is only used by completion but completion can just build/instantiate a db
        BuiltinDb::new(self.db)
    }

    pub fn this(&self) -> &DefId {
        &self.this
    }

    /// The scope owner `this`'s contents resolve against — `this` itself for containers 
    pub fn container(&self) -> &DefId {
        &self.container
    }

    pub fn db(&self) -> &dyn HirDb {
        self.db
    }

    pub fn body(&self) -> Option<&BodyMap> {
        self.body.as_deref()
    }

    /// In some cases we can't make assumptions on the defmap since resolution can jump between defmaps
    /// Used When resulution may not be in the current defmap
    pub fn def_map(&self, def: &DefId) -> Arc<DefMap> {
        self.db.file_def_map(def.file())
    }

    ////////////////////////////////////////////////////////////////////////////////////////////////////////
    //                                   MARK: NAME RESOLUTION                                            ///
    /////////////////////////////////////////////////////////////////////////////////////////////////////////

    /// Resolution without reporting or the self-name check — the test-facing
    /// convenience over [`Self::resolve_name_in`].
    #[cfg(test)]
    pub fn resolve_name(
        &self,
        name: &Name,
        local_scope: Option<ScopeId>,
        order: u32,
    ) -> Option<Resolution> {
        self.resolve_name_in(name, local_scope, order, None)
    }

    /// Name lookup at a source expression: `order` is the expression's
    /// allocation order, `self_range` the occurrence used for the self-name
    /// check.
    pub fn resolve_name_in(
        &self,
        name: &Name,
        local_scope: Option<ScopeId>,
        order: u32,
        self_range: Option<NodeRange>,
    ) -> Option<Resolution> {
        match self.resolve_name_inner(name, local_scope, order, self_range) {
            NameOutcome::Found(resolution) => Some(resolution),
            NameOutcome::Conflict(_) | NameOutcome::Missed(_) => None,
        }
    }

    /// Name lookup at a source expression, recording lookup failures.
    pub fn resolve_name_at(
        &self,
        name: &Name,
        local_scope: Option<ScopeId>,
        order: u32,
        expr: ExprId,
        range: NodeRange,
    ) -> Option<Resolution> {
        match self.resolve_name_inner(name, local_scope, order, Some(range)) {
            NameOutcome::Found(resolution) => Some(resolution),
            NameOutcome::Conflict(declarations) => {
                if !declarations.is_empty() {
                    self.record_lookup_failure(SemanticDiagnostic::AmbiguousMember {
                        expr,
                        name: name.clone(),
                        declarations,
                    });
                }
                None
            }
            NameOutcome::Missed(denials) => {
                self.record_lookup_failure(SemanticDiagnostic::UnresolvedName {
                    expr,
                    name: name.clone(),
                    denials,
                });
                None
            }
        }
    }

    fn resolve_name_inner(
        &self,
        name: &Name,
        local_scope: Option<ScopeId>,
        order: u32,
        self_range: Option<NodeRange>,
    ) -> NameOutcome {
        self
            .resolve_primitive(name)
            .or_else(|| self.resolve_local(name, local_scope, order))
            .or_else(|| self.resolve_self(name, self_range))
            .map(NameOutcome::Found)
            .or_else(|| Some(self.resolve_def(name))).map(|o| match o {
                NameOutcome::Missed(denials) => self
                .resolve_builtin(name)
                .map_or(NameOutcome::Missed(denials), NameOutcome::Found),
            outcome => outcome,
            }).expect("should always have be some")
    }

    fn resolve_primitive(&self, name: &Name) -> Option<Resolution> {
        let primitive = Primitive::parse(name.as_str());
        match primitive {
            Primitive::Undefined => None,
            _ => Some(Resolution::Type(Type::primitive(self.db, primitive))),
        }
    }

    fn resolve_builtin(&self, name: &Name) -> Option<Resolution> {
        self.builtins().resolve_global(name).map(Resolution::from)
    }

    fn resolve_local(
        &self,
        name: &Name,
        local_scope: Option<ScopeId>,
        order: u32,
    ) -> Option<Resolution> {
        let body = self.body()?;
        let mut scope_id = local_scope?;
        loop {
            let scope = &body.scopes[scope_id];
            if let Some(local_id) = scope.get(name) {
                let local = &body.locals[*local_id];
                if local.decl_order() <= order {
                    return Some(Resolution::Local(Local::new(self.this, *local_id)));
                }
            }
            scope_id = scope.parent()?;
        }
    }

    /// Attempts to resolve a name reference to the current def itself.
    ///
    /// This handles the case where we're resolving the def's own name at its
    /// definition site (e.g. the `foo` in `function foo() external {}`).
    /// The occurrence-range check keeps a call to an overloaded function from
    /// inside its own body from being mistaken for a self-reference.
    fn resolve_self(&self, name: &Name, self_range: Option<NodeRange>) -> Option<Resolution> {
        let (self_name, range) = def_name_and_range(self.db, self.this)?;
        if name == &self_name
            && self_range.is_some_and(|occurrence| {
                occurrence.start >= range.start && occurrence.end <= range.end
            })
        {
            Some(self.defs_resolution(
                DefNamespace::from(&self.this),
                std::slice::from_ref(&self.this),
            ))
        } else {
            None
        }
    }

    /// Walks the lexical scopes outward from the container. Contract-ish scopes
    /// may end the walk with `Conflict` (a conflicted slot shadows outer names)
    /// or leave `Missed` denials behind for an outer hit to discard.
    fn resolve_def(&self, name: &Name) -> NameOutcome {
        let Some(data) = self.defmap.defs.get(&self.container) else {
            return NameOutcome::Missed(Vec::new());
        };
        let start = data.child_scope.unwrap_or(data.scope);
        let mut scope = &self.defmap.scopes[start];
        let mut denials = Vec::new();

        loop {
            match scope.owner {
                id @ (DefId::Contract(_) | DefId::Interface(_) | DefId::Library(_)) => {
                    if !scope.by_name.contains_key(name) {
                        match name.as_str() {
                            "this" => return NameOutcome::Found(Resolution::This(id)),
                            "super" if matches!(id, DefId::Contract(_)|DefId::Interface(_)) => return NameOutcome::Found(Resolution::Super(id)),
                            _ => {}
                        }
                    }
                    match self.scoped_member(id, name) {
                        NameOutcome::Missed(found) => denials.extend(found),
                        outcome => return outcome,
                    }
                }
                DefId::File(file) => {
                    if let Some(member) = scope.by_name.get(name) {
                        return NameOutcome::Found(self.def_resolution(member));
                    }
                    if let Some(resolution) = self.lookup_imports(file, name) {
                        return NameOutcome::Found(resolution);
                    }
                }
                _ => {}
            }
            match scope.parent {
                Some(parent) => scope = &self.defmap.scopes[parent],
                None => return NameOutcome::Missed(denials),
            }
        }
    }

    /// Direct member lookup on a contract-ish scope owner: the effective-member
    /// fold, or the declaring scopes when this resolver is in declaration
    /// context.
    fn scoped_member(&self, owner: DefId, name: &Name) -> NameOutcome {
        let lookup = if self.declaration_only {
            self.declaration_direct_member_named(owner, name)
        } else {
            self.direct_member_named(owner, name)
        };
        if !lookup.is_hit() {
            return NameOutcome::Missed(lookup.denials);
        }
        match lookup.effective_resolution() {
            Some(resolution) => NameOutcome::Found(resolution),
            // Conflicted declarations are never a source resolution, but the
            // slot still shadows outer scopes.
            None => NameOutcome::Conflict(lookup.ambiguous),
        }
    }

    fn defs_resolution(&self, namespace: DefNamespace, defs: &[DefId]) -> Resolution {
        match namespace {
            DefNamespace::Types => Resolution::from_type(self.db, defs[0]),
            DefNamespace::Variables => Resolution::Var(defs[0]),
            _ => match defs {
                [def] => Resolution::Callable(Callable::new_direct(*def)),
                _ => Resolution::Callables(
                    defs.iter().map(|def| Callable::new_direct(*def)).collect(),
                ),
            },
        }
    }

    /// Strictly type path resolution: `A.B.C`.
    /// Returns `None` if any segment is not a type.
    pub fn resolve_path(&self, path: &[Name]) -> Option<Resolution> {
        let NameOutcome::Found(mut resolution) = self.resolve_def(path.first()?) else {
            return None;
        };
        for name in path.iter().skip(1) {
            let Resolution::Type(ty) = resolution else {
                return None;
            };
            resolution = self.lookup_type(&ty.def_id(self.db)?, name)?;
        }
        Some(resolution)
    }

    /// A segment in a Solidity type path can name nested types only. Unlike
    /// expression member access, inherited type declarations remain in scope.
    /// Look up a nested type name in a type def without assuming the def belongs
    /// to the current context defmap.
    pub fn lookup_type(&self, def: &DefId, name: &Name) -> Option<Resolution> {
        let defmap = self.def_map(def);
        let data = defmap.defs.get(def)?;
        let scope = &defmap.scopes[data.child_scope?];

        if let Some(member) = scope.by_name.get(name)
            && member.namespace == DefNamespace::Types
        {
            return Some(Resolution::from_type(self.db, member.defs[0]));
        }

        match def {
            DefId::Contract(_) | DefId::Interface(_) => {
                for base in self.db.bases(*def).iter().skip(1) {
                    let base_map = self.def_map(base);
                    let Some(scope) = base_map
                        .defs
                        .get(base)
                        .and_then(|data| data.child_scope)
                    else {
                        continue;
                    };
                    if let Some(member) = base_map.scopes[scope]
                        .by_name
                        .get(name)
                        .filter(|member| member.namespace == DefNamespace::Types)
                    {
                        return Some(Resolution::from_type(self.db, member.defs[0]));
                    }
                }
                None
            }
            DefId::File(file) => self
                .lookup_file(*file, name) 
                .filter(|r| matches!(r, Resolution::Type(_))),
            _ => None,
        }
    }

    pub(super) fn file_scopes(
        &self,
        file: File,
    ) -> impl Iterator<Item = (Arc<DefMap>, crate::ir::def_map::ScopeId)> + '_ {
        let defmap = self.def_map(&DefId::File(file));
        let file_data = defmap.files.get(&file).cloned();
        file_data.into_iter().flat_map(move |file_data| {
            let own = std::iter::once((defmap.clone(), file_data.scope));
            let imported = file_data.imported_scopes.into_iter().map({
                let defmap = defmap.clone();
                move |entry| {
                    let map = if entry.root == defmap.root {
                        defmap.clone()
                    } else {
                        self.db.root_def_map(entry.root)
                    };
                    (map, entry.scope)
                }
            });
            own.chain(imported)
        })
    }

    fn lookup_file(&self, file: File, name: &Name) -> Option<Resolution> {
        self.file_scopes(file).find_map(|(defmap, scope)| {
            defmap.scopes[scope]
                .by_name
                .get(name)
                .map(|member| self.def_resolution(member))
        })
    }

    fn lookup_imports(&self, file: File, name: &Name) -> Option<Resolution> {
        self.file_scopes(file).skip(1).find_map(|(defmap, scope)| {
            defmap.scopes[scope]
                .by_name
                .get(name)
                .map(|member| self.def_resolution(member))
        })
    }

    fn def_resolution(&self, def: &ScopeData) -> Resolution {
        self.defs_resolution(def.namespace.clone(), &def.defs)
    }

    ////////////////////////////////////////////////////////////////////////////////////////////////////////
    //                                   MARK: MEMBER RESOLUTION                                          ///
    /////////////////////////////////////////////////////////////////////////////////////////////////////////

    /// Member lookup at a source expression — only `name`'s members are
    /// collected. A total miss or ambiguity is retained by the resolver and
    /// merged into inference diagnostics at body finalization.
    pub fn lookup_member_at(
        &self,
        ty: Ty,
        name: &Name,
        expr: ExprId,
    ) -> Option<Resolution> {
        let lookup = if self.declaration_only {
            self.declaration_member_named(ty, name)
        } else {
            self.member_named(ty, name)
        };
        self.finish_member_lookup(lookup, ty, name, expr)
    }

    fn finish_member_lookup(
        &self,
        lookup: MemberLookupResult,
        ty: Ty,
        name: &Name,
        expr: ExprId,
    ) -> Option<Resolution> {
        if !lookup.is_hit() {
            self.record_lookup_failure(SemanticDiagnostic::UnresolvedMember {
                expr,
                receiver: ty,
                member: name.clone(),
                denials: lookup.denials,
            });
            return None;
        }
        if lookup.is_ambiguous_value() {
            self.record_lookup_failure(SemanticDiagnostic::InferenceFailure {
                anchor: DiagnosticAnchor::Expr(expr),
                message: "member access is ambiguous".to_string(),
            });
            return None;
        }
        let Some(resolution) = lookup.effective_resolution() else {
            let declarations = lookup.ambiguous;
            if !declarations.is_empty() {
                self.record_lookup_failure(SemanticDiagnostic::AmbiguousMember {
                    expr,
                    name: name.clone(),
                    declarations,
                });
            }
            return None;
        };
        Some(resolution)
    }
}

