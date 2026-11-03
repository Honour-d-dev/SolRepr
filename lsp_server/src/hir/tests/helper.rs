//! Shared test fixtures for the inference pipeline.
//!
//! `make_test_db` builds an in-memory `SalsaDatabase` from a single Solidity
//! source string. The `find_*` helpers locate defs by AST traversal +
//! `AstIdMap`, and the `*_inference` helpers run the ID-keyed inference query
//! for the first / last / named function in the fixture.

use ropey::Rope;
use tree_sitter::Node;
use triomphe::Arc;

use crate::ast;
use crate::ast::kinds::{FieldKind, NodeKind};
use crate::ast::{AstNode, HasName, NodeRange, ToAstNode};
use crate::hir::inference::{Inference, inference};
use crate::ir::def_map::DefId;
use crate::salsa::interned_db::Id;
use crate::salsa::{File, HirDb, RootDb, Db};
use crate::workspace::{LoadedFile, Package as WsPackage, PackageId, SourceRoot, Workspace};

pub fn make_test_db(source: &str) -> (Db, File) {
    let root = camino::Utf8PathBuf::from("/tmp/next_tests");
    let workspace = Workspace {
        ignored_roots: Vec::new(),
        packages: vec![WsPackage {
            root: root.clone(),
            remappings: Vec::new(),
        }],
        source_roots: vec![SourceRoot {
            root: root.clone(),
            package_id: PackageId(0),
            is_dependency: false,
            files: vec![LoadedFile {
                path: root.join("src/Test.sol"),
                text: Rope::from_str(source),
            }],
        }],
    };
    let db = Db::new(workspace);
    let file = db.file(&root.join("src/Test.sol")).unwrap();
    (db, file)
}

pub fn find_first_kind<'a>(node: Node<'a>, kind: NodeKind) -> Option<Node<'a>> {
    if node.kind_id() == kind {
        return Some(node);
    }
    for child in node.named_children(&mut node.walk()) {
        if let Some(found) = find_first_kind(child, kind) {
            return Some(found);
        }
    }
    None
}

pub fn find_last_kind<'a>(node: Node<'a>, kind: NodeKind) -> Option<Node<'a>> {
    let mut found = (node.kind_id() == kind).then_some(node);
    for child in node.named_children(&mut node.walk()) {
        if let Some(candidate) = find_last_kind(child, kind) {
            found = Some(candidate);
        }
    }
    found
}

/// Finds the contract or interface with the given name in the fixture file.
pub fn find_contract(db: &Db, file: File, name: &str) -> DefId {
    let root = db.ast(file).root();
    let mut found = None;
    collect_named_contract(db, file, &root, root.node(), name, &mut found);
    match found.expect("fixture should contain the named contract") {
        ContractOrInterface::Contract(id) => DefId::Contract(id),
        ContractOrInterface::Interface(id) => DefId::Interface(id),
    }
}

enum ContractOrInterface {
    Contract(ast::ContractId),
    Interface(ast::InterfaceId),
}

/// Finds the function with the given name in the fixture file.
pub fn find_function(db: &Db, file: File, name: &str) -> DefId {
    let root = db.ast(file).root();
    let mut found = None;
    collect_named_function(db, file, &root, root.node(), name, &mut found);
    DefId::Function(found.expect("fixture should contain the named function"))
}

/// The AST node for the named declaration of `kind`.
fn find_named_node<'a>(
    root: &AstNode,
    node: Node<'a>,
    kind: NodeKind,
    name: &str,
) -> Option<Node<'a>> {
    if node.kind_id() == kind
        && node
            .child_by_field_id(FieldKind::NAME.into())
            .is_some_and(|n| root.text_by_range(n.byte_range()).trim() == name)
    {
        return Some(node);
    }
    node.named_children(&mut node.walk())
        .find_map(|child| find_named_node(root, child, kind, name))
}

/// Finds the error with the given name in the fixture file.
pub fn find_error(db: &Db, file: File, name: &str) -> DefId {
    let root = db.ast(file).root();
    let node = find_named_node(&root, root.node(), NodeKind::ERROR_DEFINITION, name)
        .expect("fixture should contain the named error");
    DefId::Error(ast::ErrorId {
        file,
        id: db
            .ast_id_map(file)
            .id_of_node::<ast::Error>(node)
            .expect("error node should map to an AstId"),
    })
}

/// Finds the event with the given name in the fixture file.
pub fn find_event(db: &Db, file: File, name: &str) -> DefId {
    let root = db.ast(file).root();
    let node = find_named_node(&root, root.node(), NodeKind::EVENT_DEFINITION, name)
        .expect("fixture should contain the named event");
    DefId::Event(ast::EventId {
        file,
        id: db
            .ast_id_map(file)
            .id_of_node::<ast::Event>(node)
            .expect("event node should map to an AstId"),
    })
}

/// Finds the modifier with the given name in the fixture file.
pub fn find_modifier(db: &Db, file: File, name: &str) -> DefId {
    let root = db.ast(file).root();
    let node = find_named_node(&root, root.node(), NodeKind::MODIFIER_DEFINITION, name)
        .expect("fixture should contain the named modifier");
    DefId::Modifier(ast::ModifierId {
        file,
        id: db
            .ast_id_map(file)
            .id_of_node::<ast::Modifier>(node)
            .expect("modifier node should map to an AstId"),
    })
}

/// The source range of a def's declared name (`error Bad(..)` → `Bad`).
pub fn def_name_range(db: &Db, file: File, def: DefId) -> Option<NodeRange> {
    let (_, ast_id) = def.file_and_id();
    let root = db.ast(file).root();
    let node = db.ast_id_map(file).get_node(&root, ast_id?)?;
    node.node()
        .child_by_field_id(FieldKind::NAME.into())
        .map(|name| NodeRange::from(&name))
}

fn collect_named_contract(
    db: &Db,
    file: File,
    root: &AstNode,
    node: Node<'_>,
    name: &str,
    found: &mut Option<ContractOrInterface>,
) {
    if node.kind_id() == NodeKind::CONTRACT_DEFINITION
        && let Some(contract) = ast::Contract::cast(root.upcast(node))
        && contract.name().as_deref() == Some(name)
    {
        *found = Some(ContractOrInterface::Contract(ast::ContractId {
            file,
            id: db
                .ast_id_map(file)
                .id_of_node::<ast::Contract>(node)
                .expect("contract node should map to an AstId"),
        }));
        return;
    }
    if node.kind_id() == NodeKind::INTERFACE_DEFINITION
        && let Some(interface) = ast::Interface::cast(root.upcast(node))
        && interface.name().as_deref() == Some(name)
    {
        *found = Some(ContractOrInterface::Interface(ast::InterfaceId {
            file,
            id: db
                .ast_id_map(file)
                .id_of_node::<ast::Interface>(node)
                .expect("interface node should map to an AstId"),
        }));
        return;
    }
    for child in node.named_children(&mut node.walk()) {
        collect_named_contract(db, file, root, child, name, found);
    }
}

fn collect_named_function(
    db: &Db,
    file: File,
    root: &AstNode,
    node: Node<'_>,
    name: &str,
    found: &mut Option<ast::FunctionId>,
) {
    if node.kind_id() == NodeKind::FUNCTION_DEFINITION
        && let Some(function) = ast::Function::cast(root.upcast(node))
        && function.name().as_deref() == Some(name)
    {
        *found = Some(ast::FunctionId {
            file,
            id: db
                .ast_id_map(file)
                .id_of_node::<ast::Function>(node)
                .expect("function node should map to an AstId"),
        });
        return;
    }
    for child in node.named_children(&mut node.walk()) {
        collect_named_function(db, file, root, child, name, found);
    }
}

/// Runs the new inference query for the first function in the fixture.
pub fn function_inference(db: &Db, file: File) -> Arc<Inference> {
    inference(db, Id::new(db, function_def(db, file, find_first_kind)))
}

/// Runs the new inference query for the last function in the fixture.
pub fn last_function_inference(db: &Db, file: File) -> Arc<Inference> {
    inference(db, Id::new(db, function_def(db, file, find_last_kind)))
}

/// Runs the new inference query for the named function in the fixture.
pub fn named_function_inference(db: &Db, file: File, name: &str) -> Arc<Inference> {
    inference(db, Id::new(db, find_function(db, file, name)))
}

/// Runs the new inference query for the named modifier in the fixture.
pub fn named_modifier_inference(db: &Db, file: File, name: &str) -> Arc<Inference> {
    inference(db, Id::new(db, find_modifier(db, file, name)))
}

/// Runs the inference query for the named variable declaration — state,
/// `constant`, `immutable`, and file-scope variables are `DefId::Var`s.
pub fn var_inference(db: &Db, file: File, name: &str) -> Arc<Inference> {
    let defmap = db.file_def_map(file);
    let var = defmap
        .defs
        .keys()
        .filter_map(|def| match def {
            DefId::Var(id) => Some(*id),
            _ => None,
        })
        .find(|id| db.var_data(*id).name.as_str() == name)
        .expect("fixture should declare the named variable");
    inference(db, Id::new(db, DefId::Var(var)))
}

fn function_def(
    db: &Db,
    file: File,
    find: fn(tree_sitter::Node<'_>, NodeKind) -> Option<tree_sitter::Node<'_>>,
) -> DefId {
    let root = db.ast(file).root();
    let function = find(root.node(), NodeKind::FUNCTION_DEFINITION)
        .expect("fixture should contain a function_definition node");
    let id = ast::FunctionId {
        file,
        id: db
            .ast_id_map(file)
            .id_of_node::<ast::Function>(function)
            .expect("fixture function should map to AstId<Function>"),
    };
    DefId::Function(id)
}
