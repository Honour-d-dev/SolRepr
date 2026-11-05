use crate::ast::kinds::NodeKind;
use crate::ast::nodes::FunctionKind;
use crate::ast::{Function, FunctionId, Var, VarId};
use crate::hir::item_data::*;
use crate::hir::tests::helper::{
    find_contract, find_first_kind, find_function, find_modifier, make_test_db,
};
use crate::hir::types::{Mutability, Visibility};
use crate::ir::def_map::DefId;
use crate::salsa::hir_db::HirDb;
use crate::salsa::root_db::RootDb;

#[test]
fn function_header_facts_preserve_authored_specifiers() {
    let source = "abstract contract C { function listed() public view virtual override(A.B, D) returns (uint); function bare() external override; function implemented() internal {} function implicit() {} }";
    let (db, file) = make_test_db(source);

    let DefId::Function(listed_id) = find_function(&db, file, "listed") else {
        unreachable!()
    };
    let listed = db.function_data(listed_id);
    assert_eq!(listed.kind, FunctionKind::Regular);
    assert!(listed.vis == Visibility::Public);
    let vis_range = db
        .item_source_map(DefId::Function(listed_id))
        .vis_range
        .expect("written visibility keeps its range");
    assert_eq!(
        &source[vis_range.start as usize..vis_range.end as usize],
        "public"
    );
    assert!(listed.mutability == Mutability::View);
    assert!(listed.is_virtual);
    assert!(!listed.has_body);
    let Some(OverrideSpecifier::List { paths }) = &listed.override_specifier else {
        panic!("expected an override list");
    };
    assert_eq!(paths.len(), 2);
    let override_ranges = db.item_source_map(DefId::Function(listed_id)).override_ranges.clone();
    assert_eq!(
        &source[override_ranges[0].start as usize..override_ranges[0].end as usize],
        "override(A.B, D)"
    );
    assert_eq!(
        &source[override_ranges[1].start as usize..override_ranges[1].end as usize],
        "A.B"
    );
    let ty_of = |path: &OverridePath| {
        let ty_ref = path.ty_ref.expect("path should lower to a type reference");
        listed.syntax_store.types[ty_ref].to_string(&listed.syntax_store.types)
    };
    assert_eq!(ty_of(&paths[0]), "A.B");
    assert_eq!(ty_of(&paths[1]), "D");

    let DefId::Function(bare_id) = find_function(&db, file, "bare") else {
        unreachable!()
    };
    let bare = db.function_data(bare_id);
    assert!(matches!(
        bare.override_specifier,
        Some(OverrideSpecifier::Bare)
    ));

    let DefId::Function(implemented_id) = find_function(&db, file, "implemented") else {
        unreachable!()
    };
    let implemented = db.function_data(implemented_id);
    assert!(implemented.has_body);
    assert!(!implemented.is_virtual);
    assert!(db.item_source_map(DefId::Function(implemented_id)).vis_range.is_some());

    let DefId::Function(implicit_id) = find_function(&db, file, "implicit") else {
        unreachable!()
    };
    let implicit = db.function_data(implicit_id);
    assert!(implicit.vis == Visibility::Internal);
    assert!(db.item_source_map(DefId::Function(implicit_id)).vis_range.is_none());
}

#[test]
fn modifier_header_facts_include_override_and_implementation() {
    let (db, file) = make_test_db(
        "abstract contract C { modifier guarded() virtual override(A.B) { _; } modifier declaration() override; }",
    );

    let DefId::Modifier(guarded_id) = find_modifier(&db, file, "guarded") else {
        unreachable!()
    };
    let guarded = db.modifier_data(guarded_id);
    assert!(guarded.is_virtual);
    assert!(guarded.has_body);
    let Some(OverrideSpecifier::List { paths, .. }) = &guarded.override_specifier else {
        panic!("expected an override list");
    };
    assert_eq!(paths.len(), 1);
    let ty_ref = paths[0]
        .ty_ref
        .expect("path should lower to a type reference");
    assert_eq!(
        guarded.syntax_store.types[ty_ref].to_string(&guarded.syntax_store.types),
        "A.B"
    );

    let DefId::Modifier(declaration_id) = find_modifier(&db, file, "declaration") else {
        unreachable!()
    };
    assert!(!db.modifier_data(declaration_id).has_body);
}

#[test]
fn state_variable_and_contract_headers_are_retained() {
    let (db, file) =
        make_test_db("abstract contract C { uint public override(A.B) value; } contract D {}");
    let root = db.ast(file).root();
    let node = find_first_kind(root.node(), NodeKind::STATE_VAR_DECLARATION)
        .expect("state variable should be parsed");
    let id = db
        .ast_id_map(file)
        .id_of_node::<Var>(node)
        .expect("state variable should have an AstId");
    let var_id = VarId { file, id };
    let value = db.var_data(var_id);
    assert!(value.vis == Visibility::Public);
    assert!(db.item_source_map(DefId::Var(var_id)).vis_range.is_some());
    let Some(OverrideSpecifier::List { paths, .. }) = &value.override_specifier else {
        panic!("expected a state-variable override list");
    };
    assert_eq!(paths.len(), 1);
    let ty_ref = paths[0]
        .ty_ref
        .expect("path should lower to a type reference");
    assert_eq!(
        value.syntax_store.types[ty_ref].to_string(&value.syntax_store.types),
        "A.B"
    );

    let DefId::Contract(contract_id) = find_contract(&db, file, "C") else {
        unreachable!()
    };
    assert!(db.contract_data(contract_id).is_abstract);
    let DefId::Contract(concrete_id) = find_contract(&db, file, "D") else {
        unreachable!()
    };
    assert!(!db.contract_data(concrete_id).is_abstract);
}

#[test]
fn constructor_header_reads_anonymous_visibility_and_mutability_tokens() {
    let (db, file) = make_test_db("contract C { constructor() public payable {} }");
    let root = db.ast(file).root();
    let node = find_first_kind(root.node(), NodeKind::CONSTRUCTOR_DEFINITION)
        .expect("constructor should be parsed");
    let id = db
        .ast_id_map(file)
        .id_of_node::<Function>(node)
        .expect("constructor should have an AstId");
    let data = db.function_data(FunctionId { file, id });
    assert_eq!(data.kind, FunctionKind::Constructor);
    assert!(data.vis == Visibility::Public);
    assert!(data.mutability == Mutability::Payable);
    assert!(db.item_source_map(DefId::Function(FunctionId { file, id })).vis_range.is_some());
    assert!(data.has_body);
}
