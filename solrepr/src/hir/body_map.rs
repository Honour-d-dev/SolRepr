use crate::ast::kinds::{FieldKind, NodeKind};
use crate::ast::{AstNode, FunctionId, ModifierId, NodeRange};
use crate::hir::exprs::{Expr, ExprBuilder, ExprId, Name};
use crate::hir::item_data::SemanticId;
use crate::hir::syntax_store::{SourceMap, SyntaxStore};
use crate::hir::types::{Location, RefKind, TyName, TyNameId, TypeBuilder, VariableKind};
use la_arena::{Arena, ArenaMap, Idx};
use rustc_hash::FxHashMap;
use tree_sitter::Node;

pub type ByteOffset = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BodyOwnerId {
    Function(FunctionId),
    Modifier(ModifierId),
}

pub type ScopeId = Idx<Scope>;

#[derive(PartialEq, Eq)]
pub struct Scope {
    parent: Option<ScopeId>,
    pub(crate) by_name: FxHashMap<Name, LocalId>,
}

impl Scope {
    pub fn get(&self, name: &Name) -> Option<&LocalId> {
        self.by_name.get(name)
    }

    pub fn parent(&self) -> Option<ScopeId> {
        self.parent
    }

}

pub type LocalId = Idx<Local>;

#[derive(PartialEq, Eq)]
pub struct Local {
    name: Name,
    kind: VariableKind,
    ty_ref: TyNameId,
    /// Allocation order in the body — replaces byte offsets for the
    /// declaration-order visibility check.
    decl_order: u32,
    init: Option<ExprId>,
}

#[derive(PartialEq, Eq)]
pub struct BodyMap {
    pub root_scope: ScopeId,
    pub scopes: Arena<Scope>,
    pub locals: Arena<Local>,
    pub syntax_store: SyntaxStore,
    /// The scope each expression belongs to — content, not a source map.
    pub expr_scopes: ArenaMap<ExprId, ScopeId>,
    /// Allocation order of each expression, for declaration-order visibility.
    pub expr_orders: ArenaMap<ExprId, u32>,
}

/// The positional sibling of `BodyMap`.
pub type BodySourceMap = SourceMap;

impl BodyMap {
    pub fn local(&self, decl: LocalId) -> &Local {
        &self.locals[decl]
    }

    pub fn root_scope(&self) -> ScopeId {
        self.root_scope
    }

    pub fn scope(&self, scope: ScopeId) -> &Scope {
        &self.scopes[scope]
    }
}

impl Local {
    pub fn new(
        name: Name,
        kind: VariableKind,
        ty_ref: TyNameId,
        decl_order: u32,
        init: Option<ExprId>,
    ) -> Self {
        Self {
            name,
            kind,
            ty_ref,
            decl_order,
            init,
        }
    }

    pub fn name(&self) -> &Name {
        &self.name
    }
    pub fn kind(&self) -> VariableKind {
        self.kind
    }

    pub fn ty_ref(&self) -> &TyNameId {
        &self.ty_ref
    }

    /// The data location written on this declaration's type name — `None`
    /// when the `ty_ref` carries no `Located` annotation.
    pub fn location(&self, types: &Arena<TyName>) -> Option<Location> {
        match &types[self.ty_ref] {
            TyName::Located { loc, .. } => Some(*loc),
            _ => None,
        }
    }

    pub fn decl_order(&self) -> u32 {
        self.decl_order
    }

    pub fn init(&self) -> Option<ExprId> {
        self.init
    }
}

pub struct BodyBuilder {
    current_scope: ScopeId,
    ast_root: AstNode,
    root_scope: ScopeId,
    scopes: Arena<Scope>,
    locals: Arena<Local>,
    syntax_store: SyntaxStore,
    source_map: SourceMap,
    expr_scopes: ArenaMap<ExprId, ScopeId>,
    expr_orders: ArenaMap<ExprId, u32>,
    order: u32,
}

impl BodyBuilder {
    pub fn build(ast_root: AstNode, owner: AstNode) -> Option<(BodyMap, BodySourceMap)> {
        let mut scopes = Arena::new();
        let root_scope = scopes.alloc(Scope {
            parent: None,
            by_name: FxHashMap::default(),
        });

        let mut builder = BodyBuilder {
            current_scope: root_scope,
            ast_root,
            root_scope,
            scopes,
            locals: Arena::new(),
            syntax_store: SyntaxStore::default(),
            source_map: SourceMap::default(),
            expr_scopes: ArenaMap::new(),
            expr_orders: ArenaMap::new(),
            order: 1,
        };

        builder.collect_parameters(owner.node());

        if let Some(body) = owner.node().child_by_field_id(FieldKind::BODY.into()) {
            //Interface fns dont have bodies
            builder.walk_block(body);
        }

        Some(builder.finish())
    }

    /// Monotonic source order shared by locals and expressions.
    fn next_order(&mut self) -> u32 {
        let order = self.order;
        self.order += 1;
        order
    }

    fn push_scope(&mut self) {
        self.current_scope = self.alloc_scope();
    }

    fn pop_scope(&mut self) {
        self.current_scope = self.scopes[self.current_scope].parent.unwrap();
    }

    fn walk_block(&mut self, block: Node<'_>) {
        for child in block.named_children(&mut block.walk()) {
            match child.kind_id().into() {
                NodeKind::VAR_DECLARATION_STATEMENT => self.collect_declaration(child),
                // `try` statements expose bare parameters as direct descendants.
                NodeKind::PARAMETER => {
                    self.add_local(child, VariableKind::Parameter, None)
                }
                NodeKind::STATEMENT
                | NodeKind::IF_STATEMENT
                | NodeKind::WHILE_STATEMENT
                | NodeKind::DO_WHILE_STATEMENT => self.walk_block(child),
                NodeKind::BLOCK_STATEMENT | NodeKind::FOR_STATEMENT | NodeKind::TRY_STATEMENT => {
                    self.push_scope();
                    self.walk_block(child);
                    self.pop_scope();
                }
                NodeKind::EXPRESSION | NodeKind::EXPRESSION_STATEMENT => {
                    self.lower_expr(child);
                }
                NodeKind::EMIT_STATEMENT
                | NodeKind::REVERT_STATEMENT
                | NodeKind::RETURN_STATEMENT => {
                    self.lower_expr(child);
                }
                _ => {
                    // Recover expressions nested in error nodes.
                    self.walk_block(child);
                }
            }
        }
    }

    fn collect_declaration(&mut self, node: Node<'_>) {
        // The initializer is a sibling of the declaration, not a child of it.
        let init = node
            .child_by_field_id(FieldKind::VALUE.into())
            .and_then(|value| self.lower_expr(value));
        for child in node.named_children(&mut node.walk()) {
            match child.kind_id().into() {
                NodeKind::VAR_DECLARATION => {
                    self.add_local(child, VariableKind::LocalVariable, init);
                }
                NodeKind::VAR_DECLARATION_TUPLE => {
                    // A tuple initializer destructures; without that support the
                    // components must not each be checked against the whole value.
                    for child in child.named_children(&mut child.walk()) {
                        self.add_local(child, VariableKind::LocalVariable, None);
                    }
                }
                _ => {}
            }
        }
    }

    fn collect_parameters(&mut self, owner_node: Node<'_>) {
        for child in owner_node.named_children(&mut owner_node.walk()) {
            match child.kind_id().into() {
                NodeKind::IDENTIFIER => {
                    // The owner's own name at its declaration — an id, not a
                    // range, so the resolver never reads the source map.
                    if let Some(expr_id) = self.lower_expr(child) {
                        self.syntax_store.own_name_expr = Some(expr_id);
                    }
                }
                NodeKind::PARAMETER => {
                    self.add_local(child, VariableKind::Parameter, None);
                }
                NodeKind::MODIFIER_INVOCATION => {
                    self.lower_expr(child);
                }
                NodeKind::RETURN_DEFINITION => self.collect_parameters(child),
                _ => {}
            }
        }
    }

    fn add_local(&mut self, node: Node, kind: VariableKind, init: Option<ExprId>) {
        let mut name: Name = "".into();
        let mut ty = None;
        let mut location = None;
        let mut range = None;
        for child in node.children(&mut node.walk()) {
            match child.kind_id().into() {
                NodeKind::IDENTIFIER => {
                    name = self
                        .ast_root
                        .text_by_range(child.byte_range())
                        .trim()
                        .into();
                    range = Some(NodeRange::from(&child));
                }
                NodeKind::TYPE_NAME => {
                    ty = self.lower_type(child).map(|ty_ref| (ty_ref, child))
                }
                NodeKind::MEMORY => location = Some(Location::Memory),
                NodeKind::STORAGE => location = Some(Location::Storage(RefKind::Pointer)),
                NodeKind::CALLDATA => location = Some(Location::Calldata),
                _ => {}
            }
        }

        if name.is_empty() || ty.is_none() {
            return;
        }
        let (ty_ref, ty_node) = ty.unwrap();
        let range = range.unwrap();
        let ty_ref = match location {
            Some(loc) => self.alloc_type(TyName::Located { ty: ty_ref, loc }, ty_node),
            None => ty_ref,
        };

        let decl_order = self.next_order();
        let local_id = self.locals.alloc(Local {
            name: name.clone(),
            kind,
            ty_ref,
            decl_order,
            init,
        });

        self.scopes[self.current_scope]
            .by_name
            .insert(name, local_id);
        self.source_map.local_ranges.insert(local_id, range);
        self.source_map
            .range_to_semantic
            .insert(range, SemanticId::Local(local_id));
    }

    fn alloc_scope(&mut self) -> ScopeId {
        self.scopes.alloc(Scope {
            parent: Some(self.current_scope),
            by_name: FxHashMap::default(),
        })
    }

    fn finish(self) -> (BodyMap, BodySourceMap) {
        let BodyBuilder {
            root_scope,
            mut scopes,
            locals: mut decls,
            mut syntax_store,
            mut source_map,
            mut expr_scopes,
            mut expr_orders,
            ..
        } = self;
        scopes.shrink_to_fit();
        decls.shrink_to_fit();
        syntax_store.exprs.shrink_to_fit();
        syntax_store.types.shrink_to_fit();
        source_map.range_to_semantic.shrink_to_fit();
        expr_scopes.shrink_to_fit();
        expr_orders.shrink_to_fit();
        (
            BodyMap {
                root_scope,
                scopes,
                locals: decls,
                syntax_store,
                expr_scopes,
                expr_orders,
            },
            source_map,
        )
    }
}

impl ExprBuilder for BodyBuilder {
    fn root(&self) -> &AstNode {
        &self.ast_root
    }

    fn alloc_expr_range(&mut self, expr: Expr, range: NodeRange) -> ExprId {
        let expr_id = self.syntax_store.exprs.alloc(expr);
        let order = self.next_order();
        self.expr_orders.insert(expr_id, order);
        self.source_map.expr_ranges.insert(expr_id, range);
        self.source_map
            .range_to_semantic
            .insert(range, SemanticId::Expr(expr_id));
        self.expr_scopes.insert(expr_id, self.current_scope);
        expr_id
    }

    fn alloc_member_expr(&mut self, member: Expr, range: NodeRange, prop: NodeRange) -> ExprId {
        let mem_id = self.alloc_expr_range(member, range);
        self.source_map
            .range_to_semantic
            .insert(prop, SemanticId::Expr(mem_id));
        mem_id
    }

    fn alloc_call_expr(&mut self, call: Expr, node: Node, ident: Option<NodeRange>) -> ExprId {
        let call_id = self.alloc_expr(call, node);
        if let Some(ident) = ident {
            self.source_map
                .range_to_semantic
                .insert(ident, SemanticId::Expr(call_id));
        };
        call_id
    }

    fn alloc_wrapper_expr(&mut self, expr: Expr, node: Node) -> ExprId {
        let expr_id = self.syntax_store.exprs.alloc(expr);
        let order = self.next_order();
        self.expr_orders.insert(expr_id, order);
        self.source_map
            .expr_ranges
            .insert(expr_id, NodeRange::from(&node));
        self.expr_scopes.insert(expr_id, self.current_scope);
        expr_id
    }
}

impl TypeBuilder for BodyBuilder {
    fn alloc_type(&mut self, ty: TyName, node: Node) -> TyNameId {
        let ty_ref_id = self.syntax_store.types.alloc(ty);
        let range = NodeRange::from(&node);
        self.source_map.ty_ref_ranges.insert(ty_ref_id, range);
        self.source_map
            .range_to_semantic
            .insert(range, SemanticId::Type(ty_ref_id));
        ty_ref_id
    }
}

