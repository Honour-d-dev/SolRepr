use crate::ast::kinds::{FieldKind, NodeKind};
use crate::ast::{AstNode, FunctionId, ModifierId, NodeRange};
use crate::hir::exprs::{Expr, ExprBuilder, ExprId, Name};
use crate::hir::item_data::{SemanticId, SyntaxStore};
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
    range: NodeRange,
    pub(crate) by_name: FxHashMap<Name, LocalId>,
}

impl Scope {
    pub fn get(&self, name: &Name) -> Option<&LocalId> {
        self.by_name.get(name)
    }

    pub fn parent(&self) -> Option<ScopeId> {
        self.parent
    }

    pub fn range(&self) -> NodeRange {
        self.range
    }
}

pub type LocalId = Idx<Local>;

#[derive(PartialEq, Eq)]
pub struct Local {
    name: Name,
    kind: VariableKind,
    ty_ref: TyNameId,
    range: NodeRange,
    offset: ByteOffset, //from where this decl becomes visible
    init: Option<ExprId>,
}

#[derive(PartialEq, Eq)]
pub struct BodyMap {
    pub root_scope: ScopeId,
    pub scopes: Arena<Scope>,
    pub locals: Arena<Local>,
    pub syntax_store: SyntaxStore,
}

#[derive(PartialEq, Eq)]
pub struct BodySourceMap {
    pub expr_scopes: ArenaMap<ExprId, ScopeId>,
}

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
        range: NodeRange,
        offset: ByteOffset,
        init: Option<ExprId>,
    ) -> Self {
        Self {
            name,
            kind,
            ty_ref,
            range,
            offset,
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

    pub fn range(&self) -> NodeRange {
        self.range
    }

    pub fn offset(&self) -> ByteOffset {
        self.offset
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
    expr_scopes: ArenaMap<ExprId, ScopeId>,
}

impl BodyBuilder {
    pub fn build(ast_root: AstNode, owner: AstNode) -> Option<(BodyMap, BodySourceMap)> {
        let mut scopes = Arena::new();
        let root_scope = scopes.alloc(Scope {
            parent: None,
            //we use "fn range" instead of the intuitive "body range" because parameters are visible from decl.
            //if root scope starts at body then parameter offset < body offset(ie outside the fn scope)
            //resolution fails @BodyMap::scope_at(offset)
            range: NodeRange::from(&owner.node()),
            by_name: FxHashMap::default(),
        });

        let mut builder = BodyBuilder {
            current_scope: root_scope,
            ast_root,
            root_scope,
            scopes,
            locals: Arena::new(),
            syntax_store: SyntaxStore::default(),
            expr_scopes: ArenaMap::new(),
        };

        builder.collect_parameters(owner.node());

        if let Some(body) = owner.node().child_by_field_id(FieldKind::BODY.into()) {
            //Interface fns dont have bodies
            builder.walk_block(body);
        }

        Some(builder.finish())
    }

    fn push_scope(&mut self, range: NodeRange) {
        self.current_scope = self.alloc_scope(range);
    }

    fn pop_scope(&mut self) {
        self.current_scope = self.scopes[self.current_scope].parent.unwrap();
    }

    fn walk_block(&mut self, block: Node<'_>) {
        for child in block.named_children(&mut block.walk()) {
            match child.kind_id().into() {
                NodeKind::VAR_DECLARATION_STATEMENT => self.collect_declaration(child),
                // `try` statements expose bare parameters as direct descendants.
                NodeKind::PARAMETER => self.add_local(
                    child,
                    VariableKind::Parameter,
                    child.start_byte() as u32,
                    None,
                ),
                NodeKind::STATEMENT
                | NodeKind::IF_STATEMENT
                | NodeKind::WHILE_STATEMENT
                | NodeKind::DO_WHILE_STATEMENT => self.walk_block(child),
                NodeKind::BLOCK_STATEMENT | NodeKind::FOR_STATEMENT | NodeKind::TRY_STATEMENT => {
                    self.push_scope(NodeRange::from(&child));
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
                    self.add_local(
                        child,
                        VariableKind::LocalVariable,
                        child.range().end_byte as ByteOffset,
                        init,
                    );
                }
                NodeKind::VAR_DECLARATION_TUPLE => {
                    // A tuple initializer destructures; without that support the
                    // components must not each be checked against the whole value.
                    for child in child.named_children(&mut child.walk()) {
                        self.add_local(
                            child,
                            VariableKind::LocalVariable,
                            child.range().end_byte as ByteOffset,
                            None,
                        );
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
                    self.lower_expr(child);
                }
                NodeKind::PARAMETER => {
                    self.add_local(
                        child,
                        VariableKind::Parameter,
                        child.range().start_byte as ByteOffset,
                        None,
                    );
                }
                NodeKind::MODIFIER_INVOCATION => {
                    self.lower_expr(child);
                }
                NodeKind::RETURN_DEFINITION => self.collect_parameters(child),
                _ => {}
            }
        }
    }

    fn add_local(
        &mut self,
        node: Node,
        kind: VariableKind,
        offset: ByteOffset,
        init: Option<ExprId>,
    ) {
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

        let local_id = self.locals.alloc(Local {
            name: name.clone(),
            kind,
            ty_ref,
            range,
            offset,
            init,
        });

        self.scopes[self.current_scope]
            .by_name
            .insert(name, local_id);
        self.syntax_store
            .range_to_semantic
            .insert(range, SemanticId::Local(local_id));
    }

    fn alloc_scope(&mut self, range: NodeRange) -> ScopeId {
        self.scopes.alloc(Scope {
            parent: Some(self.current_scope),
            range,
            by_name: FxHashMap::default(),
        })
    }

    fn finish(self) -> (BodyMap, BodySourceMap) {
        let BodyBuilder {
            root_scope,
            mut scopes,
            locals: mut decls,
            mut syntax_store,
            mut expr_scopes,
            ..
        } = self;
        scopes.shrink_to_fit();
        decls.shrink_to_fit();
        syntax_store.exprs.shrink_to_fit();
        syntax_store.types.shrink_to_fit();
        syntax_store.range_to_semantic.shrink_to_fit();
        expr_scopes.shrink_to_fit();
        (
            BodyMap {
                root_scope,
                scopes,
                locals: decls,
                syntax_store,
            },
            BodySourceMap { expr_scopes },
        )
    }
}

impl ExprBuilder for BodyBuilder {
    fn root(&self) -> &AstNode {
        &self.ast_root
    }

    fn alloc_expr_range(&mut self, expr: Expr, range: NodeRange) -> ExprId {
        let expr_id = self.syntax_store.exprs.alloc(expr);
        self.syntax_store.expr_ranges.insert(expr_id, range);
        self.syntax_store
            .range_to_semantic
            .insert(range, SemanticId::Expr(expr_id));
        self.expr_scopes.insert(expr_id, self.current_scope);
        expr_id
    }

    fn alloc_member_expr(&mut self, member: Expr, range: NodeRange, prop: NodeRange) -> ExprId {
        let mem_id = self.alloc_expr_range(member, range);
        self.syntax_store
            .range_to_semantic
            .insert(prop, SemanticId::Expr(mem_id));
        mem_id
    }

    fn alloc_call_expr(&mut self, call: Expr, node: Node, ident: Option<NodeRange>) -> ExprId {
        let call_id = self.alloc_expr(call, node);
        if let Some(ident) = ident {
            self.syntax_store
                .range_to_semantic
                .insert(ident, SemanticId::Expr(call_id));
        };
        call_id
    }

    fn alloc_wrapper_expr(&mut self, expr: Expr, node: Node) -> ExprId {
        let expr_id = self.syntax_store.exprs.alloc(expr);
        self.syntax_store
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
        self.syntax_store.ty_ref_ranges.insert(ty_ref_id, range);
        self.syntax_store
            .range_to_semantic
            .insert(range, SemanticId::Type(ty_ref_id));
        ty_ref_id
    }
}

