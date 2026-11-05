use std::mem;

use la_arena::{Arena, Idx};
use tree_sitter::Node;
use triomphe::Arc;

use crate::ast::kinds::{FieldKind, NodeKind};
use crate::ast::{
    AstIdMap, AstNode, Contract, Enum, Error, Event, Function, FunctionId, FunctionKind,
    HasMembers, HasName, Import, ImportKind, Interface, Item, Library, Modifier, NodeRange, Struct,
    ToAstNode, Udvt, Using, Var,
};
use crate::hir::body_map::{ByteOffset, Local, LocalId};
use crate::hir::exprs::{Expr, ExprBuilder, ExprId, Name};
use crate::hir::syntax_store::SyntaxStore;
use crate::hir::types::{
    Location, Mutability, Primitive, RefKind, TyName, TyNameId, TypeBuilder, VariableKind,
    Visibility,
};
use crate::salsa::FileId;

pub type FieldId = Idx<Field>;
pub type VariantId = Idx<Variant>;


#[derive(PartialEq, Eq, Clone, Copy)]
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
    /// Source range of the written visibility keyword; `None` when inferred.
    pub vis_range: Option<NodeRange>,
    pub override_specifier: Option<OverrideSpecifier>,
    pub kind: VariableKind,
    pub loc: Location,
    pub range: NodeRange,
    /// Initializer expression for constant/immutable variables
    pub init: Option<ExprId>,
    pub syntax_store: SyntaxStore,
}

#[derive(PartialEq, Eq, Hash)]
pub struct Field {
    pub name: Name,
    pub ty_ref: TyNameId,
    pub range: NodeRange,
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
    pub range: NodeRange,
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
    pub vis_range: Option<NodeRange>,
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
    /// The body block's range, when the modifier has one.
    pub body_range: Option<NodeRange>,
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
    Bare {
        range: NodeRange,
    },
    List {
        paths: Box<[OverridePath]>,
        range: NodeRange,
    },
}

/// One contract named in `override(A.B, C)`. The range is kept even when the
/// path does not resolve to a type, so diagnostics can still point at it.
#[derive(Debug, PartialEq, Eq)]
pub struct OverridePath {
    pub ty_ref: Option<TyNameId>,
    pub range: NodeRange,
}

pub struct ItemBuilder {
    root: AstNode,
    file: FileId,
    ast_id_map: Arc<AstIdMap>,
    syntax_store: SyntaxStore,
}

impl ExprBuilder for ItemBuilder {
    fn root(&self) -> &AstNode {
        &self.root
    }

    fn alloc_expr_range(&mut self, expr: Expr, range: NodeRange) -> ExprId {
        let expr_id = self.syntax_store.exprs.alloc(expr);
        self.syntax_store.expr_ranges.insert(expr_id, range);
        self.syntax_store
            .range_to_semantic
            .insert(range, SemanticId::Expr(expr_id));
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
        expr_id
    }
}

impl TypeBuilder for ItemBuilder {
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

impl ItemBuilder {
    pub fn new(root: AstNode, file: FileId, ast_id_map: Arc<AstIdMap>) -> Self {
        Self {
            root,
            file,
            ast_id_map,
            syntax_store: SyntaxStore::default(),
        }
    }

    // ── Import Builder ───────────────────────────────────────────────────

    pub fn build_import(&mut self, import: &Import) -> ImportData {
        let import_kind = import.import_kind();
        let import_node = import.to_node_ref().node();
        match import_kind {
            ImportKind::Named { symbols } => {
                let mut name_cursor = import_node.walk();
                let mut alias_cursor = import_node.walk();
                let mut names = import
                    .to_node_ref()
                    .node()
                    .children_by_field_id(FieldKind::IMPORT_NAME.into(), &mut name_cursor);
                let mut aliases = import
                    .to_node_ref()
                    .node()
                    .children_by_field_id(FieldKind::ALIAS.into(), &mut alias_cursor);
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
                let node = import_node
                    .child_by_field_id(FieldKind::ALIAS.into())
                    .unwrap();
                self.alloc_expr(expr, node);
            }
            ImportKind::Full => {} //only has path
        };

        let node = import_node
            .child_by_field_id(FieldKind::SOURCE.into())
            .unwrap();
        let path = import
            .to_node_ref()
            .text_by_range(node.byte_range())
            .trim_matches(['"', '\'']);
        let expr = Expr::Path(path.into());
        self.alloc_expr(expr, node);

        ImportData {
            syntax_store: mem::take(&mut self.syntax_store),
        }
    }

    // ── Variable Builder ─────────────────────────────────────────────────

    pub fn build_var(&mut self, var: &Var) -> VarData {
        let node = var.to_node_ref().node();
        let mut name = Name::default();
        let mut range = NodeRange::from(&node); //just a placeholder
        let mut vis = Visibility::default();
        let mut vis_range = None;
        let mut override_specifier = None;
        let mut kind = VariableKind::StateVariable;
        let mut ty_ref = None;
        let mut init = None;
        for child in node.children(&mut node.walk()) {
            match child.kind_id().into() {
                NodeKind::TYPE_NAME => ty_ref = self.lower_type(child),
                NodeKind::CONSTANT => kind = VariableKind::Const,
                NodeKind::IMMUTABLE => kind = VariableKind::Immutable,
                NodeKind::VISIBILITY => {
                    vis = Visibility::parse(self.root().text_by_range(child.byte_range()).trim());
                    vis_range = Some(NodeRange::from(&child));
                }
                NodeKind::OVERRIDE_SPECIFIER => {
                    override_specifier = Some(self.lower_override_specifier(child));
                }
                NodeKind::IDENTIFIER => {
                    self.lower_expr(child);
                    name = self.root.text_by_range(child.byte_range()).trim().into();
                    range = NodeRange::from(&child);
                }
                NodeKind::EXPRESSION => {
                    init = self.lower_expr(child);
                }
                _ => {}
            }
        }
        VarData {
            name,
            ty_ref: ty_ref.unwrap(),
            vis,
            vis_range,
            override_specifier,
            kind,
            loc: if kind == VariableKind::StateVariable {
                Location::Storage(RefKind::Reference)
            } else {
                Location::Stack
            },
            range,
            init,
            syntax_store: std::mem::take(&mut self.syntax_store),
        }
    }

    fn lower_override_specifier(&mut self, node: Node) -> OverrideSpecifier {
        let range = NodeRange::from(&node);
        if !node
            .children(&mut node.walk())
            .any(|child| child.kind_id() == NodeKind::LPAREN)
        {
            return OverrideSpecifier::Bare { range };
        }
        let paths = node
            .named_children(&mut node.walk())
            .filter(|path| path.kind_id() == NodeKind::USER_DEFINED_TYPE)
            .map(|path| OverridePath {
                ty_ref: self.lower_type(path),
                range: NodeRange::from(&path),
            })
            .collect();
        OverrideSpecifier::List { paths, range }
    }

    // ── Struct Builder ───────────────────────────────────────────────────

    pub fn build_struct(&mut self, strukt: &Struct) -> StructData {
        let node = strukt.to_node_ref().node();
        let name = strukt.name().unwrap_or_default();
        let mut fields = Arena::new();

        node.child_by_field_id(FieldKind::NAME.into())
            .map(|n| self.lower_expr(n));

        if let Some(body) = node.child_by_field_id(FieldKind::BODY.into()) {
            for member in body.named_children(&mut body.walk()) {
                match member.kind_id().into() {
                    NodeKind::STRUCT_MEMBER => {
                        let Some((name, range)) =
                            member.child_by_field_id(FieldKind::NAME.into()).map(|n| {
                                (
                                    self.root().text_by_range(n.byte_range()).trim().into(),
                                    NodeRange::from(&n),
                                )
                            })
                        else {
                            continue;
                        };

                        let Some(ty_ref) = member
                            .child_by_field_id(FieldKind::TYPE.into())
                            .and_then(|ty| self.lower_type(ty))
                        else {
                            continue;
                        };

                        let field_id = fields.alloc(Field {
                            name,
                            ty_ref,
                            range: NodeRange::from(&member),
                        });

                        self.syntax_store
                            .range_to_semantic
                            .insert(NodeRange::from(&member), SemanticId::Field(field_id));
                        self.syntax_store
                            .range_to_semantic
                            .insert(range, SemanticId::Field(field_id));
                    }
                    _ => {}
                }
            }
        }

        StructData {
            name,
            fields,
            syntax_store: std::mem::take(&mut self.syntax_store),
        }
    }

    // ── Enum Builder ─────────────────────────────────────────────────────

    pub fn build_enum(&mut self, enum_: &Enum) -> EnumData {
        let node = enum_.to_node_ref().node();
        let name = enum_.name().unwrap_or_default();
        let mut variants = Arena::new();

        node.child_by_field_id(FieldKind::NAME.into())
            .map(|n| self.lower_expr(n));

        if let Some(body) = node.child_by_field_id(FieldKind::BODY.into()) {
            for value in body.named_children(&mut body.walk()) {
                if value.kind_id() == NodeKind::ENUM_VALUE {
                    let name = self.root().text_by_range(value.byte_range()).trim().into();

                    let variant_id = variants.alloc(Variant {
                        name,
                        range: NodeRange::from(&value),
                    });
                    self.syntax_store
                        .range_to_semantic
                        .insert(NodeRange::from(&value), SemanticId::Variant(variant_id));
                }
            }
        }

        EnumData {
            name,
            variants,
            syntax_store: std::mem::take(&mut self.syntax_store),
        }
    }

    // ── Event Builder ────────────────────────────────────────────────────

    pub fn build_event(mut self, event: &Event) -> EventData {
        let node = event.to_node_ref().node();
        let name = event.name().unwrap_or_default();
        let mut parameters = Arena::new();
        let mut anonymous = false;

        node.child_by_field_id(FieldKind::NAME.into())
            .map(|n| self.lower_expr(n));

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
            name,
            anonymous,
            parameters,
            syntax_store: std::mem::take(&mut self.syntax_store),
        }
    }

    // ── Error Builder ────────────────────────────────────────────────────

    pub fn build_error(mut self, error: &Error) -> ErrorData {
        let node = error.to_node_ref().node();
        let name = error.name().unwrap_or_default();
        let mut parameters = Arena::new();

        node.child_by_field_id(FieldKind::NAME.into())
            .map(|n| self.lower_expr(n));

        for child in node.named_children(&mut node.walk()) {
            match child.kind_id().into() {
                NodeKind::ERROR_PARAMETER => {
                    self.build_parameter(&mut parameters, child);
                }
                _ => {}
            }
        }

        ErrorData {
            name,
            parameters,
            syntax_store: std::mem::take(&mut self.syntax_store),
        }
    }

    // ── Modifier Builder ─────────────────────────────────────────────────
    

    pub fn build_modifier(mut self, modifier: &Modifier) -> ModifierData {
        let node = modifier.to_node_ref().node();
        let name = modifier.name().unwrap_or_default();
        let mut marked_virtual = false;
        let mut override_specifier = None;
        let mut parameters = Arena::new();

        // node.child_by_field_id(FieldKind::NAME.into()).map(|n| self.lower_expr(n));

        for child in node.named_children(&mut node.walk()) {
            match child.kind_id().into() {
                NodeKind::PARAMETER => {
                    self.build_parameter(&mut parameters, child);
                }
                NodeKind::VIRTUAL => marked_virtual = true,
                NodeKind::OVERRIDE_SPECIFIER => {
                    override_specifier = Some(self.lower_override_specifier(child));
                }
                _ => {}
            }
        }

        let body = node.child_by_field_id(FieldKind::BODY.into());
        ModifierData {
            name,
            is_virtual: marked_virtual,
            override_specifier,
            has_body: body.is_some(),
            body_range: body.map(|body| NodeRange::from(&body)),
            has_placeholder: body
                .is_some_and(|body| self.body_has_placeholder(body)),
            parameters,
            syntax_store: std::mem::take(&mut self.syntax_store),
        }
    }

    fn body_has_placeholder(&self, node: Node) -> bool {
        if node.kind_id() == NodeKind::EXPRESSION_STATEMENT {
            return node.named_children(&mut node.walk())
                .find(|child| child.kind_id() == NodeKind::EXPRESSION)
                .is_some_and(|expr| self.root().text_by_range(expr.byte_range()).trim() == "_");
        }
        node.named_children(&mut node.walk())
            .any(|child| self.body_has_placeholder(child))
    }

    // ── Function Builder ─────────────────────────────────────────────────

    pub fn build_fn(mut self, func: &Function) -> FunctionData {
        let node = func.to_node_ref().node();
        let name = func.name().unwrap_or_default();
        let mut vis = Visibility::Internal;
        let mut vis_range = None;
        let mut mutability = Mutability::NonPayable;
        let mut is_virtual = false;
        let mut override_specifier = None;
        let mut parameters = Arena::new();
        let mut args = vec![];
        let mut rets = vec![];

        // node.child_by_field_id(FieldKind::NAME.into()).map(|n| self.lower_expr(n));

        for child in node.children(&mut node.walk()) {
            match child.kind_id().into() {
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
                NodeKind::VISIBILITY | NodeKind::PUBLIC | NodeKind::INTERNAL => {
                    vis = Visibility::parse(func.to_node_ref().text_by_range(child.byte_range()).trim());
                    vis_range = Some(NodeRange::from(&child));
                }
                NodeKind::STATE_MUTABILITY => {
                    mutability =
                        Mutability::parse(func.to_node_ref().text_by_range(child.byte_range()).trim());
                }
                NodeKind::PAYABLE => mutability = Mutability::Payable, //@NOTE is payable not under mutability in tree?
                NodeKind::VIRTUAL => is_virtual = true,
                NodeKind::OVERRIDE_SPECIFIER => {
                    override_specifier = Some(self.lower_override_specifier(child));
                }
                _ => {}
            }
        }

        let args = args
            .iter()
            .copied()
            .collect::<Option<Vec<_>>>()
            .unwrap_or_default();
        let rets = rets
            .iter()
            .copied()
            .collect::<Option<Vec<_>>>()
            .unwrap_or_default();

        FunctionData {
            name,
            kind: func.kind(),
            vis,
            vis_range,
            mutability,
            is_virtual,
            override_specifier,
            has_body: node.child_by_field_id(FieldKind::BODY.into()).is_some(),
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
                NodeKind::TYPE_NAME => {
                    ty = self.lower_type(child).map(|ty_ref| (ty_ref, child))
                }
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
        let local_id = parameters.alloc(Local::new(
            name,
            VariableKind::Parameter,
            ty_ref,
            local_range,
            node.start_byte() as ByteOffset,
            None,
        ));
        self.syntax_store
            .range_to_semantic
            .insert(parameter_range, SemanticId::Local(local_id));

        if let Some(range) = name_range {
            self.syntax_store
                .range_to_semantic
                .insert(range, SemanticId::Local(local_id));
        }
        Some(local_id)
    }

    // ── Contract Builder ─────────────────────────────────────────────────

    pub fn build_contract(mut self, contract: &Contract) -> ContractData {
        let node = contract.to_node_ref().node();
        let name = contract.name().unwrap_or_default();
        let is_abstract = node
            .children(&mut node.walk())
            .any(|child| child.kind_id() == NodeKind::ABSTRACT);
        let mut bases = Vec::new();

        node.child_by_field_id(FieldKind::NAME.into())
            .map(|n| self.lower_expr(n));

        for child in node.named_children(&mut node.walk()) {
            if child.kind_id() == NodeKind::INHERITANCE_SPECIFIER
                && let Some(expr_id) = self.lower_expr(child)
            {
                bases.push(expr_id);
            }
        }

        let constructor = contract
            .members()
            .into_iter()
            .find_map(|member| match member {
                Item::Function(func) if func.kind() == FunctionKind::Constructor => {
                    self.ast_id_map.id(&func).map(|id| FunctionId {
                        file: self.file,
                        id,
                    })
                }
                _ => None,
            });

        ContractData {
            name,
            is_abstract,
            bases: bases.into_boxed_slice(),
            constructor,
            syntax_store: std::mem::take(&mut self.syntax_store),
        }
    }

    // ── Interface Builder ────────────────────────────────────────────────

    pub fn build_interface(mut self, interface: &Interface) -> InterfaceData {
        let node = interface.to_node_ref().node();
        let name = interface.name().unwrap_or_default();
        let mut bases = Vec::new();

        node.child_by_field_id(FieldKind::NAME.into())
            .map(|n| self.lower_expr(n));

        for child in node.named_children(&mut node.walk()) {
            if child.kind_id() == NodeKind::INHERITANCE_SPECIFIER
                && let Some(expr_id) = self.lower_expr(child)
            {
                bases.push(expr_id);
            }
        }

        InterfaceData {
            name,
            bases: bases.into_boxed_slice(),
            syntax_store: std::mem::take(&mut self.syntax_store),
        }
    }

    // ── Library Builder ──────────────────────────────────────────────────

    pub fn build_library(mut self, library: &Library) -> LibraryData {
        let name = library.name().unwrap_or_default();

        library
            .to_node_ref()
            .node()
            .child_by_field_id(FieldKind::NAME.into())
            .map(|n| self.lower_expr(n));

        LibraryData {
            name,
            syntax_store: std::mem::take(&mut self.syntax_store),
        }
    }

    // ── Udvt Builder ─────────────────────────────────────────────────────

    pub fn build_udvt(mut self, udvt: &Udvt) -> UdvtData {
        let name = udvt.name().unwrap_or_default();
        let underlying = Primitive::parse(&udvt.underlying().unwrap_or_default());

        for node in udvt
            .to_node_ref()
            .node()
            .named_children(&mut udvt.to_node_ref().node().walk())
        {
            if node.kind_id() == NodeKind::IDENTIFIER {
                self.lower_expr(node);
            }
            if node.kind_id() == NodeKind::PRIMITIVE_TYPE {
                self.lower_type(node);
            }
        }

        UdvtData {
            name,
            underlying,
            syntax_store: std::mem::take(&mut self.syntax_store),
        }
    }

    // ── Using Builder ────────────────────────────────────────────────────

    pub fn build_using(mut self, using: &Using) -> UsingData {
        let mut sources = vec![];
        let mut target = None;
        let node = using.to_node_ref().node();

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
            global: using.is_global(),
            syntax_store: std::mem::take(&mut self.syntax_store),
        }
    }
}

