use crate::capabilities::hover::*;
use crate::hir::body_map::BodyOwnerId;
use crate::hir::tests::helper::make_test_db;
use crate::ir::def_map::DefId;
use crate::salsa::root_db::RootDb;
use crate::salsa::HirDb;

#[test]
fn struct_hover_formats_one_field_per_line() {
    let (db, file) = make_test_db(
        "struct Position { uint256 amount; address owner; } contract C {}",
    );
    let id = db
        .file_def_map(file)
        .defs
        .keys()
        .find_map(|def| match def {
            DefId::Struct(id) if db.struct_data(*id).name == "Position" => Some(*id),
            _ => None,
        })
        .expect("fixture declares Position");

    let hover = Hover { db: &db }.hover_struct(&id).unwrap();
    assert!(
        hover.contains("struct Position {\n    uint256 amount;\n    address owner;\n}"),
        "{hover}"
    );
}

#[test]
fn local_hover_drops_stack_and_notes_pointer_kinds() {
    let (db, file) = make_test_db(
        "struct S { uint256 a; } contract C { function f(S storage sp, function(uint256) internal fp) internal { uint256 x; } }",
    );
    let fn_id = db
        .file_def_map(file)
        .defs
        .keys()
        .find_map(|def| match def {
            DefId::Function(id) if db.function_data(*id).name == "f" => Some(*id),
            _ => None,
        })
        .expect("fixture declares f");
    let hover = Hover { db: &db };
    let fn_data = db.function_data(fn_id);
    let params = fn_data
        .parameters
        .iter()
        .map(|(_, local)| hover.format_local(local, &fn_data.syntax_store))
        .collect::<Vec<_>>();
    assert!(
        params[0].contains("### Parameter")
            && params[0].contains("S storage sp")
            && params[0].contains("*storage pointer*"),
        "{}",
        params[0]
    );
    assert!(
        params[1].contains("### Parameter")
            && params[1].contains("function(uint256) internal fp")
            && params[1].contains("*function pointer*"),
        "{}",
        params[1]
    );

    let body = db.body_map(BodyOwnerId::Function(fn_id));
    let local = body
        .locals
        .iter()
        .find(|(_, local)| local.name().as_str() == "x")
        .map(|(_, local)| local)
        .expect("fixture declares x");
    let hover_x = hover.format_local(local, &body.syntax_store);
    assert!(
        hover_x.contains("### Local Variable")
            && hover_x.contains("uint256 x")
            && !hover_x.contains("stack")
            && !hover_x.contains('*'),
        "{hover_x}"
    );
}

#[test]
fn enum_hover_formats_one_variant_per_line() {
    let (db, file) = make_test_db("enum Status { Pending, Active, Closed } contract C {}");
    let id = db
        .file_def_map(file)
        .defs
        .keys()
        .find_map(|def| match def {
            DefId::Enum(id) if db.enum_data(*id).name == "Status" => Some(*id),
            _ => None,
        })
        .expect("fixture declares Status");

    let hover = Hover { db: &db }.hover_enum(&id).unwrap();
    assert!(
        hover.contains("enum Status {\n    Pending,\n    Active,\n    Closed\n}"),
        "{hover}"
    );
}

#[test]
fn contract_hover_formats_bare_and_argumented_bases() {
    let (db, file) = make_test_db(
        "contract A {} contract B { constructor(uint256) {} } contract C is A, B(1) {}",
    );
    let id = db
        .file_def_map(file)
        .defs
        .keys()
        .find_map(|def| match def {
            DefId::Contract(id) if db.contract_data(*id).name == "C" => Some(*id),
            _ => None,
        })
        .expect("fixture declares C");

    let hover = Hover { db: &db }.hover_contract(&id).unwrap();
    assert!(hover.contains("contract C is A, B"), "{hover}");
}
