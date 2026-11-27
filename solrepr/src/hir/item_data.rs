use std::mem;

use la_arena::{Arena, Idx};
use tree_sitter::Node;
use triomphe::Arc;

use crate::ast::kinds::{FieldKind, NodeKind};
use crate::ast::{
    AstId, AstIdMap, AstNode, Contract, Enum, Error, Event, Function, FunctionId, FunctionKind, Import, ImportKind, Interface, Library,
    Modifier, NodeRange, Struct, ToAstNode, Udvt, Using, Var,
};
use crate::hir::body_map::{Local, LocalId};
use crate::hir::exprs::{Expr, ExprBuilder, ExprId, Name};
use crate::hir::syntax_store::{SourceMap, SyntaxStore};
use crate::hir::types::{Location, Mutability, Primitive, RefKind, TyName, TyNameId, TypeBuilder, VariableKind, Visibility};
use crate::ir::item_tree::{ItemId, ItemTree};
use crate::salsa::FileId;

pub type FieldId = Idx<Field>;
pub type VariantId = Idx<Variant>;

#[derive(PartialEq, Eq, Clone, Copy, Debug, Hash)]
pub enum SemanticId {
    //Local semantics
    Local(LocalId),
    Field(FieldId),
    Variant(VariantId),
    // Global semantics
    Expr(ExprId),
    Type(TyNameId),
}

#[derive(Default, PartialEq, Eq)]
pub struct ImportData {
    pub syntax_store: SyntaxStore,
}

#[derive(PartialEq, Eq)]
pub struct VarData {
    pub name: Name,
    pub ty_ref: TyNameId,
    /// Effective visibility: the written keyword, or the default when omitted.
    pub vis: Visibility,
    pub override_specifier: Option<OverrideSpecifier>,
    pub kind: VariableKind,
    pub loc: Location,
    /// Initializer expression for constant/immutable variables
    pub init: Option<ExprId>,
    pub syntax_store: SyntaxStore,
}

#[derive(PartialEq, Eq, Hash)]
pub struct Field {
    pub name: Name,
    pub ty_ref: TyNameId,
}

#[derive(PartialEq, Eq)]
pub struct StructData {
    pub name: Name,
    pub fields: Arena<Field>,
    pub syntax_store: SyntaxStore,
}

#[derive(PartialEq, Eq, Hash)]
pub struct Variant {
    pub name: Name,
}

#[derive(PartialEq, Eq)]
pub struct EnumData {
    pub name: Name,
    pub variants: Arena<Variant>,
    pub syntax_store: SyntaxStore,
}

#[derive(PartialEq, Eq)]
pub struct FunctionData {
    pub name: Name,
    pub kind: FunctionKind,
    pub vis: Visibility,
    pub mutability: Mutability,
    pub is_virtual: bool,
    pub override_specifier: Option<OverrideSpecifier>,
    pub has_body: bool,
    pub parameters: Arena<Local>,
    pub args: Vec<LocalId>,
    pub rets: Vec<LocalId>,
    pub syntax_store: SyntaxStore,
}

#[derive(PartialEq, Eq)]
pub struct EventData {
    pub name: Name,
    pub anonymous: bool,
    pub parameters: Arena<Local>,
    pub syntax_store: SyntaxStore,
}

#[derive(PartialEq, Eq)]
pub struct ErrorData {
    pub name: Name,
    pub parameters: Arena<Local>,
    pub syntax_store: SyntaxStore,
}

#[derive(PartialEq, Eq)]
pub struct ModifierData {
    pub name: Name,
    pub is_virtual: bool,
    pub override_specifier: Option<OverrideSpecifier>,
    pub has_body: bool,
    /// Whether the body contains the `_` placeholder statement.
    pub has_placeholder: bool,
    pub parameters: Arena<Local>,
    pub syntax_store: SyntaxStore,
}

#[derive(PartialEq, Eq)]
pub struct ContractData {
    pub name: Name,
    pub is_abstract: bool,
    pub bases: Box<[ExprId]>,
    pub constructor: Option<FunctionId>,
    pub syntax_store: SyntaxStore,
}

#[derive(PartialEq, Eq)]
pub struct InterfaceData {
    pub name: Name,
    pub bases: Box<[ExprId]>,
    pub syntax_store: SyntaxStore,
}

#[derive(PartialEq, Eq)]
pub struct LibraryData {
    pub name: Name,
    pub syntax_store: SyntaxStore,
}

#[derive(PartialEq, Eq)]
pub struct UdvtData {
    pub name: Name,
    pub underlying: Primitive,
    pub syntax_store: SyntaxStore,
}

#[derive(PartialEq, Eq)]
pub struct UsingData {
    pub target: Option<TyNameId>,
    pub sources: Box<[ExprId]>,
    pub global: bool,
    pub syntax_store: SyntaxStore,
}

#[derive(Debug, PartialEq, Eq)]
pub enum OverrideSpecifier {
    Bare,
    List { paths: Box<[OverridePath]> },
}

/// One contract named in `override(A.B, C)`.
#[derive(Debug, PartialEq, Eq)]
pub struct OverridePath {
    pub ty_ref: Option<TyNameId>,
}

pub struct ItemBuilder {
    root: AstNode,
    file: FileId,
    ast_id_map: Arc<AstIdMap>,
    syntax_store: SyntaxStore,
    source_map: SourceMap,
}

impl ExprBuilder for ItemBuilder {
    fn root(&self) -> &AstNode {
        &self.root
    }

    fn alloc_expr_range(&mut self, expr: Expr, range: NodeRange) -> ExprId {
        let expr_id = self.syntax_store.exprs.alloc(expr);
        self.source_map.expr_ranges.insert(expr_id, range);
        self.source_map.range_to_semantic.insert(range, SemanticId::Expr(expr_id));
        expr_id
    }

    fn alloc_member_expr(&mut self, member: Expr, range: NodeRange, prop: NodeRange) -> ExprId {
        let mem_id = self.alloc_expr_range(member, range);
        self.source_map.range_to_semantic.insert(prop, SemanticId::Expr(mem_id));
        mem_id
    }

    fn alloc_call_expr(&mut self, call: Expr, node: Node, ident: Option<NodeRange>) -> ExprId {
        let call_id = self.alloc_expr(call, node);
        if let Some(ident) = ident {
            self.source_map.range_to_semantic.insert(ident, SemanticId::Expr(call_id));
        };
        call_id
    }

    fn alloc_wrapper_expr(&mut self, expr: Expr, node: Node) -> ExprId {
        let expr_id = self.syntax_store.exprs.alloc(expr);
        self.source_map.expr_ranges.insert(expr_id, NodeRange::from(&node));
        expr_id
    }
}

impl TypeBuilder for ItemBuilder {
    fn alloc_type(&mut self, ty: TyName, node: Node) -> TyNameId {
        let ty_ref_id = self.syntax_store.types.alloc(ty);
        let range = NodeRange::from(&node);
        self.source_map.ty_ref_ranges.insert(ty_ref_id, range);
        self.source_map.range_to_semantic.insert(range, SemanticId::Type(ty_ref_id));
        ty_ref_id
    }
}

impl ItemBuilder {
    pub fn new(root: AstNode, file: FileId, ast_id_map: Arc<AstIdMap>) -> Self {
        Self {
            root,
            file,
            ast_id_map,
            syntax_store: SyntaxStore::default(),
            source_map: SourceMap::default(),
        }
    }

    /// The positions collected while building this item.
    pub fn take_source_map(&mut self) -> SourceMap {
        mem::take(&mut self.source_map)
    }

    /// The node `id` names. A builder takes the declaration's id and derives
    /// both views from it — the syntax to lower here, the digested facts from
    /// the item tree — so the two can never name different declarations.
    ///
    /// Returns the node by value: it owns its tree handle, so the caller keeps
    /// it across `&mut self` calls.
    fn ast_node<T: ToAstNode>(&self, id: AstId<T>) -> AstNode {
        self.ast_id_map
            .get_node(&self.root, id.erase())
            .expect("an AstId resolves in its own file")
    }

    // ── Import Builder ───────────────────────────────────────────────────

    pub fn build_import(&mut self, id: AstId<Import>, items: &ItemTree) -> ImportData {
        let import_kind = items[id].import_kind.clone();
        let ast = self.ast_node(id);
        let import_node = ast.node();
        match import_kind {
            ImportKind::Named { symbols } => {
                let mut name_cursor = import_node.walk();
                let mut alias_cursor = import_node.walk();
                let mut names = import_node.children_by_field_id(FieldKind::IMPORT_NAME.into(), &mut name_cursor);
                let mut aliases = import_node.children_by_field_id(FieldKind::ALIAS.into(), &mut alias_cursor);
                for symbol in symbols {
                    let expr = if let Some(alias) = symbol.alias {
                        let alias_node = aliases.next().unwrap();
                        let expr = Expr::Ident(alias);
                        self.alloc_expr(expr.clone(), alias_node);
                        expr
                    } else {
                        Expr::Ident(symbol.name)
                    };
                    let name_node = names.next().unwrap();
                    self.alloc_expr(expr, name_node);
                }
            }
            ImportKind::Namespace { alias } => {
                let expr = Expr::Ident(alias);
                let node = import_node.child_by_field_id(FieldKind::ALIAS.into()).unwrap();
                self.alloc_expr(expr, node);
            }
            ImportKind::Full => {} //only has path
        };

        let node = import_node.child_by_field_id(FieldKind::SOURCE.into()).unwrap();
        let path = self.root().text_by_range(node.byte_range()).trim_matches(['"', '\'']);
        let expr = Expr::Path(path.into());
        self.alloc_expr(expr, node);

        ImportData {
            syntax_store: mem::take(&mut self.syntax_store),
        }
    }

    // ── Variable Builder ─────────────────────────────────────────────────

    pub fn build_var(&mut self, id: AstId<Var>, items: &ItemTree) -> VarData {
        let ast = self.ast_node(id);
        let node = ast.node();
        let item = &items[id];
        let mut override_specifier = None;
        let mut ty_ref = None;
        let mut init = None;
        for child in node.children(&mut node.walk()) {
            match child.kind_id().into() {
                NodeKind::TYPE_NAME => ty_ref = self.lower_type(child),
                // The kind and visibility are the item's facts; the walk keeps
                // the position of the visibility token.
                NodeKind::VISIBILITY => {
                    self.source_map.vis_range = Some(NodeRange::from(&child));
                }
                NodeKind::OVERRIDE_SPECIFIER => {
                    override_specifier = Some(self.lower_override_specifier(child));
                }
                NodeKind::IDENTIFIER => {
                    if let Some(expr_id) = self.lower_expr(child) {
                        self.syntax_store.name = Some(expr_id);
                    }
                }
                NodeKind::EXPRESSION => {
                    init = self.lower_expr(child);
                }
                _ => {}
            }
        }
        VarData {
            name: item.name.clone(),
            ty_ref: ty_ref.unwrap(),
            vis: item.vis.unwrap_or_default(),
            override_specifier,
            kind: item.kind,
            loc: if item.kind == VariableKind::StateVariable {
                Location::Storage(RefKind::Reference)
            } else {
                Location::Stack
            },
            init,
            syntax_store: std::mem::take(&mut self.syntax_store),
        }
    }

    fn lower_override_specifier(&mut self, node: Node) -> OverrideSpecifier {
        if !node.children(&mut node.walk()).any(|child| child.kind_id() == NodeKind::LPAREN) {
            return OverrideSpecifier::Bare;
        }
        let paths = node
            .named_children(&mut node.walk())
            .filter(|path| path.kind_id() == NodeKind::USER_DEFINED_TYPE)
            .map(|path| OverridePath {
                ty_ref: self.lower_type(path),
            })
            .collect();
        OverrideSpecifier::List { paths }
    }

    // ── Struct Builder ───────────────────────────────────────────────────

    pub fn build_struct(&mut self, id: AstId<Struct>, items: &ItemTree) -> StructData {
        let ast = self.ast_node(id);
        let node = ast.node();
        let mut fields = Arena::new();

        self.syntax_store.name = node.child_by_field_id(FieldKind::NAME.into()).and_then(|n| self.lower_expr(n));

        if let Some(body) = node.child_by_field_id(FieldKind::BODY.into()) {
            for member in body.named_children(&mut body.walk()) {
                if let NodeKind::STRUCT_MEMBER = member.kind_id().into() {
                    let Some((name, range)) = member
                        .child_by_field_id(FieldKind::NAME.into())
                        .map(|n| (self.root().text_by_range(n.byte_range()).trim().into(), NodeRange::from(&n)))
                    else {
                        continue;
                    };

                    let Some(ty_ref) = member.child_by_field_id(FieldKind::TYPE.into()).and_then(|ty| self.lower_type(ty)) else {
                        continue;
                    };

                    let field_id = fields.alloc(Field { name, ty_ref });

                    self.source_map.field_ranges.insert(field_id, NodeRange::from(&member));
                    self.source_map
                        .range_to_semantic
                        .insert(NodeRange::from(&member), SemanticId::Field(field_id));
                    self.source_map.range_to_semantic.insert(range, SemanticId::Field(field_id));
                }
            }
        }

        StructData {
            name: items[id].name.clone(),
            fields,
            syntax_store: std::mem::take(&mut self.syntax_store),
        }
    }

    // ── Enum Builder ─────────────────────────────────────────────────────

    pub fn build_enum(&mut self, id: AstId<Enum>, items: &ItemTree) -> EnumData {
        let ast = self.ast_node(id);
        let node = ast.node();
        let mut variants = Arena::new();

        self.syntax_store.name = node.child_by_field_id(FieldKind::NAME.into()).and_then(|n| self.lower_expr(n));

        if let Some(body) = node.child_by_field_id(FieldKind::BODY.into()) {
            for value in body.named_children(&mut body.walk()) {
                if value.kind_id() == NodeKind::ENUM_VALUE {
                    let name = self.root().text_by_range(value.byte_range()).trim().into();

                    let variant_id = variants.alloc(Variant { name });
                    self.source_map.variant_ranges.insert(variant_id, NodeRange::from(&value));
                    self.source_map
                        .range_to_semantic
                        .insert(NodeRange::from(&value), SemanticId::Variant(variant_id));
                }
            }
        }

        EnumData {
            name: items[id].name.clone(),
            variants,
            syntax_store: std::mem::take(&mut self.syntax_store),
        }
    }

    // ── Event Builder ────────────────────────────────────────────────────

    pub fn build_event(&mut self, id: AstId<Event>, items: &ItemTree) -> EventData {
        let ast = self.ast_node(id);
        let node = ast.node();
        let mut parameters = Arena::new();
        let mut anonymous = false;

        if let Some(expr_id) = node.child_by_field_id(FieldKind::NAME.into()).and_then(|n| self.lower_expr(n)) {
            self.syntax_store.name = Some(expr_id);
        }

        for child in node.children(&mut node.walk()) {
            match child.kind_id().into() {
                NodeKind::EVENT_PARAMETER => {
                    self.build_parameter(&mut parameters, child);
                }
                _ => {
                    // `anonymous` is an unnamed token, matched by kind text.
                    if child.kind() == "anonymous" {
                        anonymous = true;
                    }
                }
            }
        }

        EventData {
            name: items[id].name.clone(),
            anonymous,
            parameters,
            syntax_store: std::mem::take(&mut self.syntax_store),
        }
    }

    // ── Error Builder ────────────────────────────────────────────────────

    pub fn build_error(&mut self, id: AstId<Error>, items: &ItemTree) -> ErrorData {
        let ast = self.ast_node(id);
        let node = ast.node();
        let mut parameters = Arena::new();

        if let Some(expr_id) = node.child_by_field_id(FieldKind::NAME.into()).and_then(|n| self.lower_expr(n)) {
            self.syntax_store.name = Some(expr_id);
        }

        for child in node.named_children(&mut node.walk()) {
            if let NodeKind::ERROR_PARAMETER = child.kind_id().into() {
                self.build_parameter(&mut parameters, child);
            }
        }

        ErrorData {
            name: items[id].name.clone(),
            parameters,
            syntax_store: std::mem::take(&mut self.syntax_store),
        }
    }

    // ── Modifier Builder ─────────────────────────────────────────────────

    pub fn build_modifier(&mut self, id: AstId<Modifier>, items: &ItemTree) -> ModifierData {
        let ast = self.ast_node(id);
        let node = ast.node();
        let item = &items[id];
        let mut override_specifier = None;
        let mut parameters = Arena::new();

        for child in node.named_children(&mut node.walk()) {
            match child.kind_id().into() {
                NodeKind::IDENTIFIER => {
                    if let Some(expr_id) = self.lower_expr(child) {
                        self.syntax_store.name = Some(expr_id);
                    }
                }
                NodeKind::PARAMETER => {
                    self.build_parameter(&mut parameters, child);
                }
                NodeKind::OVERRIDE_SPECIFIER => {
                    override_specifier = Some(self.lower_override_specifier(child));
                }
                _ => {}
            }
        }

        let body = node.child_by_field_id(FieldKind::BODY.into());
        ModifierData {
            name: item.name.clone(),
            is_virtual: item.is_virtual,
            override_specifier,
            has_body: item.has_body,
            has_placeholder: body.is_some_and(|body| self.body_has_placeholder(body)),
            parameters,
            syntax_store: std::mem::take(&mut self.syntax_store),
        }
    }

    fn body_has_placeholder(&self, node: Node) -> bool {
        if node.kind_id() == NodeKind::EXPRESSION_STATEMENT {
            return node
                .named_children(&mut node.walk())
                .find(|child| child.kind_id() == NodeKind::EXPRESSION)
                .is_some_and(|expr| self.root().text_by_range(expr.byte_range()).trim() == "_");
        }
        node.named_children(&mut node.walk()).any(|child| self.body_has_placeholder(child))
    }

    // ── Function Builder ─────────────────────────────────────────────────

    pub fn build_fn(&mut self, id: AstId<Function>, items: &ItemTree) -> FunctionData {
        let ast = self.ast_node(id);
        let node = ast.node();
        let item = &items[id];
        let mut override_specifier = None;
        let mut parameters = Arena::new();
        let mut args = vec![];
        let mut rets = vec![];

        for child in node.children(&mut node.walk()) {
            match child.kind_id().into() {
                NodeKind::IDENTIFIER => {
                    if let Some(expr_id) = self.lower_expr(child) {
                        self.syntax_store.name = Some(expr_id);
                    }
                }
                NodeKind::PARAMETER => {
                    args.push(self.build_parameter(&mut parameters, child));
                }
                NodeKind::RETURN_DEFINITION => {
                    for ret_child in child.named_children(&mut child.walk()) {
                        if ret_child.kind_id() == NodeKind::PARAMETER {
                            rets.push(self.build_parameter(&mut parameters, ret_child));
                        }
                    }
                }
                // The visibility itself is the item's fact; the walk keeps the
                // position of the token.
                NodeKind::VISIBILITY | NodeKind::PUBLIC | NodeKind::INTERNAL => {
                    self.source_map.vis_range = Some(NodeRange::from(&child));
                }
                NodeKind::OVERRIDE_SPECIFIER => {
                    override_specifier = Some(self.lower_override_specifier(child));
                }
                NodeKind::MODIFIER_INVOCATION => {
                    self.lower_expr(child);
                }
                _ => {}
            }
        }

        let args = args.iter().copied().collect::<Option<Vec<_>>>().unwrap_or_default();
        let rets = rets.iter().copied().collect::<Option<Vec<_>>>().unwrap_or_default();

        FunctionData {
            name: item.name.clone(),
            kind: item.kind,
            vis: item.vis.unwrap_or_default(),
            mutability: item.mutability.unwrap_or_default(),
            is_virtual: item.is_virtual,
            override_specifier,
            has_body: item.has_body,
            parameters,
            args,
            rets,
            syntax_store: std::mem::take(&mut self.syntax_store),
        }
    }

    fn build_parameter(&mut self, parameters: &mut Arena<Local>, node: Node) -> Option<LocalId> {
        let mut name: Name = "".into();
        let mut ty = None;
        let mut location = None;
        let mut name_range = None;

        for child in node.children(&mut node.walk()) {
            match child.kind_id().into() {
                NodeKind::IDENTIFIER => {
                    name = self.root().text_by_range(child.byte_range()).trim().into();
                    name_range = Some(NodeRange::from(&child));
                }
                NodeKind::TYPE_NAME => ty = self.lower_type(child).map(|ty_ref| (ty_ref, child)),
                NodeKind::MEMORY => location = Some(Location::Memory),
                NodeKind::STORAGE => location = Some(Location::Storage(RefKind::Pointer)),
                NodeKind::CALLDATA => location = Some(Location::Calldata),
                _ => {}
            }
        }

        let (ty_ref, ty_node) = ty?;
        let ty_ref = match location {
            Some(loc) => self.alloc_type(TyName::Located { ty: ty_ref, loc }, ty_node),
            None => ty_ref,
        };
        let parameter_range = NodeRange::from(&node);
        let local_range = name_range.unwrap_or(parameter_range);
        let decl_order = parameters.len() as u32;
        let local_id = parameters.alloc(Local::new(name, VariableKind::Parameter, ty_ref, decl_order, None));
        self.source_map.local_ranges.insert(local_id, local_range);

        // Only the written name denotes the local. An unnamed parameter's node
        // range is its type's range, and claiming it would hide the type.
        if let Some(range) = name_range {
            self.source_map.range_to_semantic.insert(range, SemanticId::Local(local_id));
        }
        Some(local_id)
    }

    // ── Contract Builder ─────────────────────────────────────────────────

    pub fn build_contract(&mut self, id: AstId<Contract>, items: &ItemTree) -> ContractData {
        let ast = self.ast_node(id);
        let node = ast.node();
        let is_abstract = node.children(&mut node.walk()).any(|child| child.kind_id() == NodeKind::ABSTRACT);
        let mut bases = Vec::new();

        self.syntax_store.name = node.child_by_field_id(FieldKind::NAME.into()).and_then(|n| self.lower_expr(n));

        for child in node.named_children(&mut node.walk()) {
            if child.kind_id() == NodeKind::INHERITANCE_SPECIFIER
                && let Some(expr_id) = self.lower_expr(child)
            {
                bases.push(expr_id);
            }
        }

        // The item tree already walked the members, so the constructor is a
        // lookup rather than a second walk.
        let constructor = items[id].members.iter().find_map(|member| {
            let ItemId::Function(id) = member else {
                return None;
            };
            match items[*id].kind {
                FunctionKind::Constructor => Some(FunctionId { file: self.file, id: *id }),
                _ => None,
            }
        });

        ContractData {
            name: items[id].name.clone(),
            is_abstract,
            bases: bases.into_boxed_slice(),
            constructor,
            syntax_store: std::mem::take(&mut self.syntax_store),
        }
    }

    // ── Interface Builder ────────────────────────────────────────────────

    pub fn build_interface(&mut self, id: AstId<Interface>, items: &ItemTree) -> InterfaceData {
        let ast = self.ast_node(id);
        let node = ast.node();
        let mut bases = Vec::new();

        self.syntax_store.name = node.child_by_field_id(FieldKind::NAME.into()).and_then(|n| self.lower_expr(n));

        for child in node.named_children(&mut node.walk()) {
            if child.kind_id() == NodeKind::INHERITANCE_SPECIFIER
                && let Some(expr_id) = self.lower_expr(child)
            {
                bases.push(expr_id);
            }
        }

        InterfaceData {
            name: items[id].name.clone(),
            bases: bases.into_boxed_slice(),
            syntax_store: std::mem::take(&mut self.syntax_store),
        }
    }

    // ── Library Builder ──────────────────────────────────────────────────

    pub fn build_library(&mut self, id: AstId<Library>, items: &ItemTree) -> LibraryData {
        let ast = self.ast_node(id);
        self.syntax_store.name = ast
            .node()
            .child_by_field_id(FieldKind::NAME.into())
            .and_then(|n| self.lower_expr(n));

        LibraryData {
            name: items[id].name.clone(),
            syntax_store: std::mem::take(&mut self.syntax_store),
        }
    }

    // ── Udvt Builder ─────────────────────────────────────────────────────

    pub fn build_udvt(&mut self, id: AstId<Udvt>, items: &ItemTree) -> UdvtData {
        let item = &items[id];
        let underlying = Primitive::parse(&item.underlying);
        let ast = self.ast_node(id);
        let node = ast.node();

        for node in node.named_children(&mut node.walk()) {
            if node.kind_id() == NodeKind::IDENTIFIER {
                self.syntax_store.name = self.lower_expr(node);
            }
            if node.kind_id() == NodeKind::PRIMITIVE_TYPE {
                self.lower_type(node);
            }
        }

        UdvtData {
            name: item.name.clone().unwrap_or_default(),
            underlying,
            syntax_store: std::mem::take(&mut self.syntax_store),
        }
    }

    // ── Using Builder ────────────────────────────────────────────────────

    pub fn build_using(&mut self, id: AstId<Using>, items: &ItemTree) -> UsingData {
        let mut sources = vec![];
        let mut target = None;
        let ast = self.ast_node(id);
        let node = ast.node();

        for child in node.named_children(&mut node.walk()) {
            match child.kind_id().into() {
                NodeKind::TYPE_ALIAS | NodeKind::USING_ALIAS => {
                    let Some(source) = self.lower_expr(child) else {
                        continue;
                    };
                    sources.push(source);
                }
                NodeKind::TYPE_NAME => {
                    target = self.lower_type(child);
                }
                _ => {}
            }
        }
        UsingData {
            target,
            sources: sources.into_boxed_slice(),
            global: items[id].is_global,
            syntax_store: std::mem::take(&mut self.syntax_store),
        }
    }
}
