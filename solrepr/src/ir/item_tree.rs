use std::ops::Index;

use triomphe::Arc;

use rustc_hash::FxHashMap;
use smol_str::SmolStr as Name;

use crate::ast::{
    self, Ast, AstId, AstIdMap, ErasedAstId, FunctionKind, HasMembers, HasName, ImportKind,
    ToAstNode,
};
use crate::salsa::{File, RootDb};


/// useful for pattern matching out of items to retain type info.
/// similar to ast::Item, but on the id level.
/// ErasedFileAstId -> FileAstId -> ItemId
/// either this or we can_cast
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemId {
    Import(AstId<ast::Import>),
    Using(AstId<ast::Using>),
    Udvt(AstId<ast::Udvt>),
    Contract(AstId<ast::Contract>),
    Interface(AstId<ast::Interface>),
    Library(AstId<ast::Library>),
    Function(AstId<ast::Function>),
    Var(AstId<ast::Var>),
    Struct(AstId<ast::Struct>),
    Enum(AstId<ast::Enum>),
    Event(AstId<ast::Event>),
    Error(AstId<ast::Error>),
    Modifier(AstId<ast::Modifier>),
}

/// Item_tree::Item, (not to be mistaken with ast::Item) but similar concept
/// Blanket type for all item_tree::node types
#[derive(Debug, PartialEq, Eq)]
pub enum Item {
    Import(Import),
    Using(Using),
    Udvt(Udvt),
    Contract(Contract),
    Interface(Interface),
    Library(Library),
    Function(Function),
    Var(Var),
    Struct(Struct),
    Enum(Enum),
    Event(Event),
    Error(Error),
    Modifier(Modifier),
}

/// ItemTree::Lowerer
pub struct Lowerer {
    ast: Arc<Ast>,
    ast_id_map: Arc<AstIdMap>,
    top_level: Vec<ItemId>,
    data: FxHashMap<ErasedAstId, Item>,
}

impl Lowerer {
    pub fn lower(db: &dyn RootDb, file: File) -> ItemTree {
        let mut lowerer = Lowerer {
            ast: db.ast(file),
            ast_id_map: db.ast_id_map(file),
            top_level: Vec::new(),
            data: FxHashMap::default(),
        };

        lowerer.lower_top();

        lowerer.finish()
    }

    fn lower_top(&mut self) {
        let root = self.ast.root();

        for node in root.node().named_children(&mut root.node().walk()) {
            match ast::Item::cast(root.upcast(node)) {
                Some(ast::Item::Import(i)) => {
                    let Some(id) = self.ast_id_map.id(&i) else {
                        continue;
                    };
                    let import = Import {
                        path: i.path().into(),
                        import_kind: i.import_kind(),
                    };
                    self.data.insert(id.erase(), Item::Import(import));
                    self.top_level.push(ItemId::Import(id));
                }
                Some(ast::Item::Udvt(u)) => {
                    let Some(id) = self.ast_id_map.id(&u) else {
                        continue;
                    };
                    let Some(underlying) = u.underlying() else {
                        continue;
                    };
                    let udvt = Udvt {
                        name: u.name(),
                        underlying,
                    };
                    self.data.insert(id.erase(), Item::Udvt(udvt));
                    self.top_level.push(ItemId::Udvt(id));
                }
                Some(ast::Item::Using(u)) => {
                    let Some(id) = self.ast_id_map.id(&u) else {
                        continue;
                    };
                    let using = Using {
                        target: u.target(),
                        sources: u.sources(),
                        is_global: u.is_global(),
                    };
                    self.data.insert(id.erase(), Item::Using(using));
                    self.top_level.push(ItemId::Using(id));
                }
                Some(ast::Item::Contract(c)) => {
                    let Some(id) = self.ast_id_map.id(&c) else {
                        continue;
                    };
                    let contract = Contract {
                        name: c.name().unwrap_or_default(),
                        members: self.lower_members(c.members()),
                    };
                    self.data.insert(id.erase(), Item::Contract(contract));
                    self.top_level.push(ItemId::Contract(id));
                }
                Some(ast::Item::Interface(i)) => {
                    let Some(id) = self.ast_id_map.id(&i) else {
                        continue;
                    };
                    let interface = Interface {
                        name: i.name().unwrap_or_default(),
                        members: self.lower_members(i.members()),
                    };
                    self.data.insert(id.erase(), Item::Interface(interface));
                    self.top_level.push(ItemId::Interface(id));
                }
                Some(ast::Item::Library(l)) => {
                    let Some(id) = self.ast_id_map.id(&l) else {
                        continue;
                    };
                    let library = Library {
                        name: l.name().unwrap_or_default(),
                        members: self.lower_members(l.members()),
                    };
                    self.data.insert(id.erase(), Item::Library(library));
                    self.top_level.push(ItemId::Library(id));
                }
                Some(ast::Item::Function(f)) => {
                    let Some(id) = self.ast_id_map.id(&f) else {
                        continue;
                    };
                    let function = Function {
                        name: function_name(&f),
                        kind: f.kind(),
                    };
                    self.top_level.push(ItemId::Function(id));
                    self.data.insert(id.erase(), Item::Function(function));
                }
                Some(ast::Item::Var(v)) => {
                    let Some(id) = self.ast_id_map.id(&v) else {
                        continue;
                    };
                    let variable = Var {
                        name: v.name().unwrap_or_default(),
                    };
                    self.top_level.push(ItemId::Var(id));
                    self.data.insert(id.erase(), Item::Var(variable));
                }
                Some(ast::Item::Struct(s)) => {
                    let Some(id) = self.ast_id_map.id(&s) else {
                        continue;
                    };
                    let struct_ = Struct {
                        name: s.name().unwrap_or_default(),
                    };
                    self.top_level.push(ItemId::Struct(id));
                    self.data.insert(id.erase(), Item::Struct(struct_));
                }
                Some(ast::Item::Enum(e)) => {
                    let Some(id) = self.ast_id_map.id(&e) else {
                        continue;
                    };
                    let enum_ = Enum {
                        name: e.name().unwrap_or_default(),
                    };
                    self.top_level.push(ItemId::Enum(id));
                    self.data.insert(id.erase(), Item::Enum(enum_));
                }
                Some(ast::Item::Event(e)) => {
                    let Some(id) = self.ast_id_map.id(&e) else {
                        continue;
                    };
                    let event = Event {
                        name: e.name().unwrap_or_default(),
                    };
                    self.top_level.push(ItemId::Event(id));
                    self.data.insert(id.erase(), Item::Event(event));
                }
                Some(ast::Item::Error(e)) => {
                    let Some(id) = self.ast_id_map.id(&e) else {
                        continue;
                    };
                    let error = Error {
                        name: e.name().unwrap_or_default(),
                    };
                    self.top_level.push(ItemId::Error(id));
                    self.data.insert(id.erase(), Item::Error(error));
                }
                Some(ast::Item::Modifier(m)) => {
                    let Some(id) = self.ast_id_map.id(&m) else {
                        continue;
                    };
                    let modifier = Modifier {
                        name: m.name().unwrap_or_default(),
                    };
                    self.top_level.push(ItemId::Modifier(id));
                    self.data.insert(id.erase(), Item::Modifier(modifier));
                }
                _ => {}
            }
        }
    }

    fn lower_members(&mut self, members: Box<[ast::Item]>) -> Box<[ItemId]> {
        let mut result = Vec::new();
        for member in members.into_iter() {
            match member {
                ast::Item::Using(u) => {
                    let Some(id) = self.ast_id_map.id(&u) else {
                        continue;
                    };
                    let using = Using {
                        target: u.target(),
                        sources: u.sources(),
                        is_global: u.is_global(),
                    };
                    result.push(ItemId::Using(id));
                    self.data.insert(id.erase(), Item::Using(using));
                }
                ast::Item::Udvt(u) => {
                    let Some(id) = self.ast_id_map.id(&u) else {
                        continue;
                    };
                    let Some(underlying) = u.underlying() else {
                        continue;
                    };
                    let udvt = Udvt {
                        name: u.name(),
                        underlying,
                    };
                    result.push(ItemId::Udvt(id));
                    self.data.insert(id.erase(), Item::Udvt(udvt));
                }
                ast::Item::Function(f) => {
                    let Some(id) = self.ast_id_map.id(&f) else {
                        continue;
                    };
                    let function = Function {
                        name: function_name(&f),
                        kind: f.kind(),
                    };
                    result.push(ItemId::Function(id));
                    self.data.insert(id.erase(), Item::Function(function));
                }
                ast::Item::Event(e) => {
                    let Some(id) = self.ast_id_map.id(&e) else {
                        continue;
                    };
                    let event = Event {
                        name: e.name().unwrap_or_default(),
                    };
                    result.push(ItemId::Event(id));
                    self.data.insert(id.erase(), Item::Event(event));
                }
                ast::Item::Struct(s) => {
                    let Some(id) = self.ast_id_map.id(&s) else {
                        continue;
                    };
                    let strukt = Struct {
                        name: s.name().unwrap_or_default(),
                    };
                    result.push(ItemId::Struct(id));
                    self.data.insert(id.erase(), Item::Struct(strukt));
                }
                ast::Item::Enum(e) => {
                    let Some(id) = self.ast_id_map.id(&e) else {
                        continue;
                    };
                    let enm = Enum {
                        name: e.name().unwrap_or_default(),
                    };
                    result.push(ItemId::Enum(id));
                    self.data.insert(id.erase(), Item::Enum(enm));
                }
                ast::Item::Error(e) => {
                    let Some(id) = self.ast_id_map.id(&e) else {
                        continue;
                    };
                    let error = Error {
                        name: e.name().unwrap_or_default(),
                    };
                    result.push(ItemId::Error(id));
                    self.data.insert(id.erase(), Item::Error(error));
                }
                ast::Item::Modifier(m) => {
                    let Some(id) = self.ast_id_map.id(&m) else {
                        continue;
                    };
                    let modifier = Modifier {
                        name: m.name().unwrap_or_default(),
                    };
                    result.push(ItemId::Modifier(id));
                    self.data.insert(id.erase(), Item::Modifier(modifier));
                }
                ast::Item::Var(v) => {
                    let Some(id) = self.ast_id_map.id(&v) else {
                        continue;
                    };
                    let variable = Var {
                        name: v.name().unwrap_or_default(),
                    };
                    result.push(ItemId::Var(id));
                    self.data.insert(id.erase(), Item::Var(variable));
                }
                _ => continue,
            }
        }
        result.into_boxed_slice()
    }

    fn finish(mut self) -> ItemTree {
        self.data.shrink_to_fit();
        ItemTree {
            top_level: self.top_level.into_boxed_slice(),
            data: self.data,
        }
    }
}

/// Special functions are unnamed in source; the def map keys them by their
/// keyword so that a second one collides as a redeclaration, not an overload.
fn function_name(f: &ast::Function) -> Name {
    match f.kind() {
        FunctionKind::Constructor => "constructor".into(),
        FunctionKind::Fallback => "fallback".into(),
        FunctionKind::Receive => "receive".into(),
        FunctionKind::Regular => f.name().unwrap_or_default(),
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ItemTree {
    // file-scope only
    pub top_level: Box<[ItemId]>,

    pub data: FxHashMap<ErasedAstId, Item>,
}

macro_rules! impl_item_tree_index {
    ($($ast_ty:ty => $variant:ident),* $(,)?) => {
        $(
            impl Index<AstId<$ast_ty>> for ItemTree {
                type Output = $variant;
                fn index(&self, index: AstId<$ast_ty>) -> &Self::Output {
                    match self.data.get(&index.erase()).unwrap() {
                        Item::$variant(it) => it,
                        _ => panic!(concat!("expected ", stringify!($variant))),
                    }
                }
            }
        )*
    };
}

impl_item_tree_index! {
    ast::Import => Import,
    ast::Using => Using,
    ast::Udvt => Udvt,
    ast::Contract => Contract,
    ast::Interface => Interface,
    ast::Library => Library,
    ast::Function => Function,
    ast::Struct => Struct,
    ast::Enum => Enum,
    ast::Event => Event,
    ast::Error => Error,
    ast::Modifier => Modifier,
    ast::Var => Var,
}
///////////////////////////////////// MIR ///////////////////////////////////////////
///                             Item_tree::Nodes                                 ///
/// ////////////////////////////////////////////////////////////////////////////////

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    pub path: Name, //tecnically not a small str but just to capitalize on the cheap clones
    pub import_kind: ImportKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Using {
    pub target: Option<Name>,
    pub sources: Box<[Name]>,
    pub is_global: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Udvt {
    pub name: Option<Name>,
    pub underlying: Name,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contract {
    pub name: Name,
    pub members: Box<[ItemId]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub name: Name,
    pub members: Box<[ItemId]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Library {
    pub name: Name,
    pub members: Box<[ItemId]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Function {
    pub name: Name,
    pub kind: FunctionKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Var {
    pub name: Name,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Struct {
    pub name: Name,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enum {
    pub name: Name,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub name: Name,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub name: Name,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Modifier {
    pub name: Name,
}
