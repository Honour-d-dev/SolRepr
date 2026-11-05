use crate::ast::kinds::NodeKind;
use crate::hir::body_map::*;
use crate::hir::exprs::Expr;
use crate::hir::types::TyName;
use crate::ast::ToAstNode;
use crate::hir::tests::helper::make_test_db;
use crate::hir::types::{Mutability, Primitive, Visibility};
use tree_sitter::Node;

use crate::{ast, hir::exprs::Literal, salsa::root_db::RootDb};

const TYPE_FIXTURE: &str = r#"
    pragma solidity ^0.8.30;

    contract A {
        function testA() external {
            uint a;
            uint[10] b;
            uint[Const] c;
            uint[1][3] d;
            ContractType e;
            Path.ContractType f;
            mapping(address => ContractType) g;
            function(uint) external view returns(int) h;
            (uint8 i, uint32 j) = (1, 2);
        }
    }
"#;

fn find_first_kind<'a>(node: Node<'a>, kind: NodeKind) -> Option<Node<'a>> {
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

fn build_maps_for_first_function(source: &str) -> (BodyMap, BodySourceMap) {
    let (db, file) = make_test_db(source);
    let ast = db.ast(file);
    let root = ast.root();
    let fn_node = find_first_kind(root.node(), NodeKind::FUNCTION_DEFINITION)
        .expect("fixture should contain a function_definition node");

    let fn_id = db
        .ast_id_map(file)
        .id_of_node::<ast::Function>(fn_node)
        .expect("fixture function should map to AstId<Function>");

    let ast_root = db.ast(file).root();
    let ast_id_map = db.ast_id_map(file);
    let owner = ast_id_map.get(&ast_root, fn_id).unwrap().to_node();

    BodyBuilder::build(ast_root, owner).expect("BodyBuilder::build should succeed for fixture")
}

fn local_type<'a>(body_map: &'a BodyMap, _source_map: &'a BodySourceMap, name: &str) -> &'a TyName {
    let scope = &body_map.scopes[body_map.root_scope];
    let local_id = *scope
        .by_name
        .get(name)
        .expect("expected local declaration to exist");

    let ty_ref_id = *body_map.local(local_id).ty_ref();
    &body_map.syntax_store.types[ty_ref_id]
}

#[test]
fn collects_tuple_declarations_into_scope() {
    let (body_map, _) = build_maps_for_first_function(TYPE_FIXTURE);
    let scope = &body_map.scopes[body_map.root_scope];

    assert!(scope.by_name.contains_key("i"));
    assert!(scope.by_name.contains_key("j"));
}

#[test]
fn lowers_mapping_function_and_nested_array_types() {
    let (body_map, source_map) = build_maps_for_first_function(TYPE_FIXTURE);

    match local_type(&body_map, &source_map, "g") {
        TyName::Mapping { key, value } => {
            assert!(matches!(
                body_map.syntax_store.types[*key],
                TyName::Primitive(Primitive::Address)
            ));

            match &body_map.syntax_store.types[*value] {
                TyName::UserDefined { base, name } => {
                    assert!(base.is_none());
                    assert_eq!(name.as_str(), "ContractType");
                }
                _ => panic!("expected mapping value user-defined type"),
            }
        }
        _ => panic!("expected `g` to lower to TyRef::Mapping"),
    }

    match local_type(&body_map, &source_map, "h") {
        TyName::Fn(fn_ty) => {
            assert!(matches!(fn_ty.visibility, Visibility::External));
            assert!(matches!(fn_ty.mutability, Mutability::View));
            assert_eq!(fn_ty.params.len(), 1);
            assert_eq!(fn_ty.returns.len(), 1);

            assert!(matches!(
                body_map.syntax_store.types[fn_ty.params[0]],
                TyName::Primitive(Primitive::Uint(256))
            ));
            assert!(matches!(
                body_map.syntax_store.types[fn_ty.returns[0]],
                TyName::Primitive(Primitive::Int(256))
            ));
        }
        _ => panic!("expected `h` to lower to TyRef::Fn"),
    }

    match local_type(&body_map, &source_map, "d") {
        TyName::Array {
            ty: outer_base,
            size: Some(outer_size),
        } => {
            assert!(matches!(
                body_map.syntax_store.exprs[*outer_size],
                Expr::Literal(Literal::Number(_))
            ));

            match &body_map.syntax_store.types[*outer_base] {
                TyName::Array {
                    ty: inner_base,
                    size: Some(inner_size),
                } => {
                    assert!(matches!(
                        body_map.syntax_store.exprs[*inner_size],
                        Expr::Literal(Literal::Number(_))
                    ));
                    assert!(matches!(
                        body_map.syntax_store.types[*inner_base],
                        TyName::Primitive(Primitive::Uint(256))
                    ));
                }
                _ => panic!("expected nested array for `d` base type"),
            }
        }
        _ => panic!("expected `d` to lower to nested TyRef::Array"),
    }
}
