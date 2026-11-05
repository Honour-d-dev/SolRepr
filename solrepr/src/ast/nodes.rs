use crate::ast::ast::{AstNode, ToAstNode};
use crate::ast::kinds::{FieldKind, NodeKind};
use crate::hir::exprs::Name;
use smallvec::SmallVec;
use smol_str::SmolStr;

///  Typed AstNodes that carry type-specific data and fns

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    raw: AstNode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contract {
    raw: AstNode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    raw: AstNode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Library {
    raw: AstNode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Struct {
    raw: AstNode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enum {
    raw: AstNode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FunctionKind {
    Regular,
    Constructor,
    Fallback,
    Receive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Function {
    raw: AstNode,
}

impl Function {
    pub fn kind(&self) -> FunctionKind {
        match self.raw.node().kind_id().into() {
            NodeKind::CONSTRUCTOR_DEFINITION => FunctionKind::Constructor,
            NodeKind::FALLBACK_RECEIVE_DEFINITION => {
                let text = self.raw.text_by_range(self.raw.node().byte_range());
                if text.trim_start().starts_with("receive") {
                    FunctionKind::Receive
                } else {
                    FunctionKind::Fallback
                }
            }
            _ => FunctionKind::Regular,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Var {
    //Single wrapper for multiple var types
    raw: AstNode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    raw: AstNode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    raw: AstNode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Modifier {
    raw: AstNode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Udvt {
    raw: AstNode,
}

impl Udvt {
    pub fn underlying(&self) -> Option<Name> {
        let underlying_node = self
            .raw
            .node()
            .named_children(&mut self.raw.node().walk())
            .find(|n| n.kind_id() == NodeKind::PRIMITIVE_TYPE)?;
        Some(self.raw.text_by_range(underlying_node.byte_range()).into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Using {
    raw: AstNode,
}

impl Using {
    pub fn sources(&self) -> Box<[Name]> {
        let mut sources = vec![];
        for source in self.raw.node().children(&mut self.raw.node().walk()) {
            if source.kind_id() == NodeKind::TYPE_ALIAS || source.kind_id() == NodeKind::USING_ALIAS
            {
                sources.push(self.raw.text_by_range(source.byte_range()).into());
            }
        }
        sources.into_boxed_slice()
    }

    pub fn target(&self) -> Option<Name> {
        let target_node = self
            .raw
            .node()
            .named_children(&mut self.raw.node().walk())
            .find(|n| n.kind_id() == NodeKind::TYPE_NAME)?;
        Some(self.raw.text_by_range(target_node.byte_range()).into())
    }

    pub fn is_global(&self) -> bool {
        self.raw
            .node()
            .children(&mut self.raw.node().walk())
            .any(|n| n.kind_id() == NodeKind::GLOBAL)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    raw: AstNode,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum ImportKind {
    #[default]
    Full, //can i use full for namespace? namespace is just full with an alias
    Named {
        symbols: Vec<ImportItem>,
    },
    Namespace {
        alias: SmolStr,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportItem {
    pub name: SmolStr,
    pub alias: Option<SmolStr>,
}

impl Import {
    pub fn import_kind(&self) -> ImportKind {
        let mut symbols: Vec<ImportItem> = Vec::new();
        let mut is_alias = false;
        let mut alias = SmolStr::default();
        let mut name = SmolStr::default();
        for node in self.raw.node().children(&mut self.raw.node().walk()) {
            if node.kind_id() == NodeKind::IDENTIFIER {
                if is_alias {
                    alias = self.raw.text_by_range(node.byte_range()).into();
                    if !symbols.is_empty() {
                        symbols.last_mut().unwrap().alias = Some(alias.clone());
                    }
                    //consume alias flag
                    is_alias = false;
                } else {
                    name = self.raw.text_by_range(node.byte_range()).into();
                    symbols.push(ImportItem {
                        name: name.clone(),
                        alias: None,
                    });
                }
            }

            if node.kind_id() == NodeKind::AS {
                //prepare for incoming alias identifier
                is_alias = true;
            }
        }

        match (name, alias) {
            (name, _) if !name.is_empty() => ImportKind::Named { symbols },
            (name, alias) if name.is_empty() && alias.is_empty() => ImportKind::Full,
            (name, alias) if name.is_empty() && !alias.is_empty() => {
                ImportKind::Namespace { alias }
            }
            _ => ImportKind::Full, //never reached, match is logically exhaustive
        }
    }
    #[inline]
    pub fn path(&self) -> &str {
        self.raw
            .node()
            .child_by_field_id(FieldKind::SOURCE.into())
            .map(|p| self.raw.text_by_range(p.byte_range()))
            .unwrap_or("")
            .trim_matches(['"', '\''])
    }
}


#[derive(PartialEq, Eq)]
pub struct Path {
    pub segments: SmallVec<[Name; 2]>,
}

pub trait HasName: ToAstNode {
    fn name(&self) -> Option<SmolStr> {
        self.to_node_ref()
            .node()
            .child_by_field_id(FieldKind::NAME.into())
            .map(|n| self.to_node_ref().text_by_range(n.byte_range()).into())
    }
}


pub trait HasBases: ToAstNode {
    fn bases(&self) -> Box<[Path]> {
        //@NOTE we repeat this logic in itemdata. do we make this take a cb that returns the box inner instead?
        self.to_node_ref()
            .node()
            .named_children(&mut self.to_node_ref().node().walk())
            .filter_map(|n| {
                if n.kind_id() == NodeKind::INHERITANCE_SPECIFIER {
                    n.named_children(&mut n.walk()).find_map(|base| {
                        if base.kind_id() == NodeKind::USER_DEFINED_TYPE {
                            let segments = base
                                .named_children(&mut base.walk())
                                .map(|ident| self.to_node_ref().text_by_range(ident.byte_range()).into())
                                .collect::<SmallVec<_>>();
                            Some(Path { segments })
                        } else {
                            None
                        }
                    })
                } else {
                    None
                }
            })
            .collect::<Box<_>>()
    }
}

pub trait HasMembers: ToAstNode {
    fn members(&self) -> Box<[Item]> {
        let mut members = Vec::new();
        let Some(body) = self.to_node_ref().node().child_by_field_id(FieldKind::BODY.into()) else {
            return members.into();
        };
        for node in body.children(&mut self.to_node_ref().node().walk()) {
            match NodeKind::from(node.kind_id()) {
                NodeKind::USING_DIRECTIVE => {
                    if let Some(using) = Using::cast(self.to_node_ref().upcast(node)) {
                        members.push(Item::Using(using));
                    }
                }
                NodeKind::USER_DEFINED_TYPE_DEFINITION => {
                    if let Some(udvt) = Udvt::cast(self.to_node_ref().upcast(node)) {
                        members.push(Item::Udvt(udvt));
                    }
                }
                NodeKind::FUNCTION_DEFINITION
                | NodeKind::CONSTRUCTOR_DEFINITION
                | NodeKind::FALLBACK_RECEIVE_DEFINITION => {
                    if let Some(func) = Function::cast(self.to_node_ref().upcast(node)) {
                        members.push(Item::Function(func));
                    }
                }
                NodeKind::EVENT_DEFINITION => {
                    if let Some(event) = Event::cast(self.to_node_ref().upcast(node)) {
                        members.push(Item::Event(event));
                    }
                }
                NodeKind::STRUCT_DEFINITION => {
                    if let Some(strukt) = Struct::cast(self.to_node_ref().upcast(node)) {
                        members.push(Item::Struct(strukt));
                    }
                }
                NodeKind::ENUM_DEFINITION => {
                    if let Some(enum_) = Enum::cast(self.to_node_ref().upcast(node)) {
                        members.push(Item::Enum(enum_));
                    }
                }
                NodeKind::ERROR_DEFINITION => {
                    if let Some(error) = Error::cast(self.to_node_ref().upcast(node)) {
                        members.push(Item::Error(error));
                    }
                }
                NodeKind::MODIFIER_DEFINITION => {
                    if let Some(modifier) = Modifier::cast(self.to_node_ref().upcast(node)) {
                        members.push(Item::Modifier(modifier));
                    }
                }
                NodeKind::STATE_VAR_DECLARATION => {
                    if let Some(var) = Var::cast(self.to_node_ref().upcast(node)) {
                        members.push(Item::Var(var));
                    }
                }
                _ => {}
            }
        }
        members.into_boxed_slice()
    }
}

macro_rules! impl_ast_node {
    ($($type:ty, $($node_kind:ident)|+ $(, $trait:ty)*)+) => {
        $(
            impl ToAstNode for $type {

                #[inline]
                fn to_node(self) -> AstNode {
                    self.raw
                }

                #[inline]
                fn to_node_ref(&self) -> &AstNode {
                    &self.raw
                }

                fn cast(n: AstNode) -> Option<Self> {
                    if Self::can_cast(n.node().kind_id().into()) {
                        Some(Self{raw: n})
                    } else {
                        None
                    }
                }

                #[inline]
                fn can_cast(n: NodeKind) -> bool {
                    false $(|| n == NodeKind::$node_kind)+
                }
            }

            $(
                impl $trait for $type {}
            )*
        )*
    };
}

impl_ast_node!(
    SourceFile, SOURCE_FILE
    Import, IMPORT_DIRECTIVE
    Using, USING_DIRECTIVE
    Contract, CONTRACT_DEFINITION, HasName, HasBases, HasMembers
    Interface, INTERFACE_DEFINITION, HasName, HasBases, HasMembers
    Library, LIBRARY_DEFINITION, HasName, HasMembers
    Function, FUNCTION_DEFINITION | CONSTRUCTOR_DEFINITION | FALLBACK_RECEIVE_DEFINITION, HasName
    Modifier, MODIFIER_DEFINITION, HasName
    Struct, STRUCT_DEFINITION, HasName
    Enum, ENUM_DEFINITION, HasName
    Event, EVENT_DEFINITION, HasName
    Error, ERROR_DEFINITION, HasName
    Var, STATE_VAR_DECLARATION | CONST_VAR_DECLARATION, HasName
    Udvt, USER_DEFINED_TYPE_DEFINITION, HasName
);



/// Ast types union type so we can preserve underlying type identity
/// as opposed to direct AstNode with no underlying type info
/// so we go AstNode -> Specific Type -> Item

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    SourceFile(SourceFile),
    Using(Using),
    Udvt(Udvt),
    Contract(Contract),
    Interface(Interface),
    Library(Library),
    Struct(Struct),
    Enum(Enum),
    Function(Function),
    Event(Event),
    Error(Error),
    Modifier(Modifier),
    Import(Import),
    Var(Var),
}

impl ToAstNode for Item {
    fn to_node(self) -> AstNode {
        match self {
            Item::SourceFile(s) => s.to_node(),
            Item::Using(u) => u.to_node(),
            Item::Udvt(u) => u.to_node(),
            Item::Contract(c) => c.to_node(),
            Item::Interface(i) => i.to_node(),
            Item::Library(l) => l.to_node(),
            Item::Struct(s) => s.to_node(),
            Item::Enum(e) => e.to_node(),
            Item::Function(f) => f.to_node(),
            Item::Event(e) => e.to_node(),
            Item::Error(e) => e.to_node(),
            Item::Modifier(m) => m.to_node(),
            Item::Import(i) => i.to_node(),
            Item::Var(v) => v.to_node(),
        }
    }

    fn to_node_ref(&self) -> &AstNode {
        match self {
            Item::SourceFile(s) => s.to_node_ref(),
            Item::Using(u) => u.to_node_ref(),
            Item::Udvt(u) => u.to_node_ref(),
            Item::Contract(c) => c.to_node_ref(),
            Item::Interface(i) => i.to_node_ref(),
            Item::Library(l) => l.to_node_ref(),
            Item::Struct(s) => s.to_node_ref(),
            Item::Enum(e) => e.to_node_ref(),
            Item::Function(f) => f.to_node_ref(),
            Item::Event(e) => e.to_node_ref(),
            Item::Error(e) => e.to_node_ref(),
            Item::Modifier(m) => m.to_node_ref(),
            Item::Import(i) => i.to_node_ref(),
            Item::Var(v) => v.to_node_ref(),
        }
    }

    fn cast(node: AstNode) -> Option<Self> {
        match NodeKind::from(node.node().kind_id()) {
            NodeKind::SOURCE_FILE => Some(Self::SourceFile(SourceFile::cast(node).unwrap())),
            NodeKind::USING_DIRECTIVE => Some(Self::Using(Using::cast(node).unwrap())),
            NodeKind::USER_DEFINED_TYPE_DEFINITION => Some(Self::Udvt(Udvt::cast(node).unwrap())),
            NodeKind::CONTRACT_DEFINITION => Some(Self::Contract(Contract::cast(node).unwrap())),
            NodeKind::INTERFACE_DEFINITION => Some(Self::Interface(Interface::cast(node).unwrap())),
            NodeKind::LIBRARY_DEFINITION => Some(Self::Library(Library::cast(node).unwrap())),
            NodeKind::STRUCT_DEFINITION => Some(Self::Struct(Struct::cast(node).unwrap())),
            NodeKind::ENUM_DEFINITION => Some(Self::Enum(Enum::cast(node).unwrap())),
            NodeKind::FUNCTION_DEFINITION
            | NodeKind::CONSTRUCTOR_DEFINITION
            | NodeKind::FALLBACK_RECEIVE_DEFINITION => {
                Some(Self::Function(Function::cast(node).unwrap()))
            }
            NodeKind::EVENT_DEFINITION => Some(Self::Event(Event::cast(node).unwrap())),
            NodeKind::ERROR_DEFINITION => Some(Self::Error(Error::cast(node).unwrap())),
            NodeKind::MODIFIER_DEFINITION => Some(Self::Modifier(Modifier::cast(node).unwrap())),
            NodeKind::IMPORT_DIRECTIVE => Some(Self::Import(Import::cast(node).unwrap())),
            NodeKind::STATE_VAR_DECLARATION | NodeKind::CONST_VAR_DECLARATION => {
                Some(Self::Var(Var::cast(node).unwrap()))
            }
            _ => None,
        }
    }

    fn can_cast(n: NodeKind) -> bool {
        matches!(
            n,
            NodeKind::SOURCE_FILE
                | NodeKind::USING_DIRECTIVE
                | NodeKind::USER_DEFINED_TYPE_DEFINITION
                | NodeKind::CONTRACT_DEFINITION
                | NodeKind::INTERFACE_DEFINITION
                | NodeKind::LIBRARY_DEFINITION
                | NodeKind::STRUCT_DEFINITION
                | NodeKind::ENUM_DEFINITION
                | NodeKind::FUNCTION_DEFINITION
                | NodeKind::CONSTRUCTOR_DEFINITION
                | NodeKind::FALLBACK_RECEIVE_DEFINITION
                | NodeKind::EVENT_DEFINITION
                | NodeKind::ERROR_DEFINITION
                | NodeKind::MODIFIER_DEFINITION
                | NodeKind::IMPORT_DIRECTIVE
                | NodeKind::STATE_VAR_DECLARATION
                | NodeKind::CONST_VAR_DECLARATION
        )
    }
}
