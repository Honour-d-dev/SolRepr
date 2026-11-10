//! The shape of `AstIdMap`: which nodes get ids, and how members are numbered.

use tree_sitter::Node;

use crate::ast::kinds::NodeKind;
use crate::ast::{self, AstIdMap, ErasedAstId};
use crate::hir::tests::helper::{find_first_kind, make_test_db};
use crate::salsa::RootDb;

/// Every state variable's erased id, in document order.
fn var_ids(source: &str) -> Vec<ErasedAstId> {
    let (db, file) = make_test_db(source);
    let root = db.root(file);
    let map = db.ast_id_map(file);
    let mut out = Vec::new();
    collect_vars(root.node(), &map, &mut out);
    out
}

fn collect_vars(node: Node<'_>, map: &AstIdMap, out: &mut Vec<ErasedAstId>) {
    if let Some(id) = map.id_of_node::<ast::Var>(node) {
        out.push(id.erase());
    }
    for child in node.named_children(&mut node.walk()) {
        collect_vars(child, map, out);
    }
}

#[test]
fn a_sibling_container_keeps_its_member_ids_when_another_gains_a_member() {
    let before = var_ids("contract A { uint256 a1; }\ncontract B { uint256 b1; }");
    let after = var_ids("contract A { uint256 a1; uint256 a2; }\ncontract B { uint256 b1; }");

    assert_eq!(before.len(), 2);
    assert_eq!(after.len(), 3);
    assert_eq!(
        before[1], after[2],
        "contract B's member must not shift when contract A gains a member"
    );
    assert_ne!(
        before[0], before[1],
        "sibling containers must not number their members off one counter"
    );
}

#[test]
fn the_source_file_is_not_indexed() {
    // The source file is transparent, which is what keeps top-level items
    // parentless. If it took an id they would become its members and share one
    // counter with every container's members.
    let (db, file) = make_test_db("contract A { uint256 x; }");
    let root = db.root(file);
    let map = db.ast_id_map(file);

    assert!(map.id_of_node::<ast::SourceFile>(root.node()).is_none());
    assert!(
        map.id_of_node::<ast::Contract>(
            find_first_kind(root.node(), NodeKind::CONTRACT_DEFINITION).unwrap()
        )
        .is_some()
    );
}

#[test]
fn a_container_and_its_first_member_do_not_share_an_id() {
    // Both land on index 0 — the container is the first node indexed and the
    // first member of it is index 0 within it — so `parent` is the only thing
    // keeping them apart.
    let (db, file) = make_test_db("contract A { uint256 x; }");
    let root = db.root(file);
    let map = db.ast_id_map(file);

    let container = map
        .id_of_node::<ast::Contract>(find_first_kind(root.node(), NodeKind::CONTRACT_DEFINITION).unwrap())
        .unwrap()
        .erase();
    let member = var_ids("contract A { uint256 x; }")[0];

    assert_ne!(container, member);
}
