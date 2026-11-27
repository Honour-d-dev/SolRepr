use camino::Utf8PathBuf;
use lsp_server::{Request, RequestId};
use lsp_types::request::{PrepareRenameRequest, Rename, Request as LspRequest};
use lsp_types::{Position, RenameParams, TextDocumentIdentifier, TextDocumentPositionParams};
use ropey::Rope;
use rustc_hash::FxHashSet;

use crate::capabilities::Identity;
use crate::capabilities::rename::{Target, is_identifier, prepare_rename, references, rename};
use crate::hir::body_map::BodyOwnerId;
use crate::hir::item_data::SemanticId;
use crate::hir::resolution::{Field, Resolution};
use crate::hir::tests::helper::{def_name_range, find_contract, find_function, make_test_db};
use crate::ir::def_map::DefId;
use crate::paths::NormalizedPath;
use crate::salsa::root_db::RootDb;
use crate::salsa::{Db, HirDb};
use crate::utilities::{byte_to_position, to_url};
use crate::workspace::{
    LoadedFile, Package as WsPackage, PackageId, RootWithFiles, WorkspaceWithFiles,
};

fn root() -> Utf8PathBuf {
    Utf8PathBuf::from_path_buf(std::env::temp_dir().join("rename_tests"))
        .expect("temp dir is UTF-8")
}

fn path(relative: &str) -> Utf8PathBuf {
    root().join(relative)
}

fn db_with(files: &[(&str, &str)]) -> Db {
    let root = root();
    Db::new(WorkspaceWithFiles {
        ignored_roots: Vec::new(),
        packages: vec![WsPackage {
            root: NormalizedPath::new(root.clone()),
            remappings: Vec::new(),
        }],
        roots: vec![RootWithFiles {
            path: NormalizedPath::new(root.clone()),
            package_id: PackageId(NormalizedPath::new(root.clone())),
            is_dependency: false,
            files: files
                .iter()
                .map(|(path, text)| LoadedFile {
                    path: NormalizedPath::new(root.join(path)),
                    text: Some(Rope::from_str(text)),
                })
                .collect(),
        }],
    })
}

fn defs_of(db: &Db, file: crate::salsa::File, kind: impl Fn(&DefId) -> bool) -> Vec<DefId> {
    db.file_def_map(file)
        .defs
        .keys()
        .copied()
        .filter(kind)
        .collect()
}

/// solc 0.8.x, probed with `contract C { bool <name>; }`: canonical type sizes
/// and atomic keywords are reserved; non-canonical sizes, contextual keywords,
/// and builtins are legal identifiers.
#[test]
fn identifier_validation_matches_solc() {
    const RESERVED: &[&str] = &[
        "uint256",
        "int8",
        "bytes1",
        "bytes32",
        "address",
        "bool",
        "string",
        "uint",
        "int",
        "bytes",
        "fixed128x18",
        "fixed8x0",
        "fixed",
        "ufixed",
        "hex",
        "years",
        "copyof",
        "supports",
        "typedef",
        "byte",
        "throw",
        "wei",
        "gwei",
        "payable",
        "immutable",
    ];
    const LEGAL: &[&str] = &[
        "uint7",
        "uint08",
        "uint0",
        "uint264",
        "bytes0",
        "bytes33",
        "ufixed128",
        "fixed08x18",
        "error",
        "revert",
        "global",
        "transient",
        "layout",
        "leave",
        "szabo",
        "finney",
        "keccak256",
        "require",
        "assert",
        "undefined",
        "counter",
    ];
    for &name in RESERVED {
        assert!(!is_identifier(name), "{name} is reserved in solc");
    }
    for &name in LEGAL {
        assert!(is_identifier(name), "{name} is a legal identifier in solc");
    }
}

#[test]
fn a_local_declaration_targets_its_local() {
    let (db, file) = make_test_db("contract C { function f() external { uint x = 1; x = 2; } }");
    let f = find_function(&db, file, "f");
    let DefId::Function(id) = f else {
        unreachable!()
    };
    let map = db.body_source_map(BodyOwnerId::Function(id));
    let (range, local) = map
        .range_to_semantic
        .iter()
        .find_map(|(range, id)| match id {
            SemanticId::Local(local) => Some((*range, *local)),
            _ => None,
        })
        .expect("the fixture declares a local");

    let target = Target::at(&db, f, range).expect("a local is renameable");
    assert_eq!(target.name, "x");
    assert_eq!(target.identity, Identity::Local { owner: f, local });
}

#[test]
fn a_state_variable_targets_its_declaration() {
    let (db, file) = make_test_db("contract C { uint256 state; }");
    let var = defs_of(&db, file, |def| matches!(def, DefId::Var(_)))
        .into_iter()
        .next()
        .expect("the fixture declares a state variable");
    let range = def_name_range(&db, file, var).expect("a state variable has a name");

    let target = Target::at(&db, var, range).expect("a state variable is renameable");
    assert_eq!(target.name, "state");
    assert_eq!(target.identity, Identity::Def(var));
}

#[test]
fn a_contract_targets_its_declaration() {
    let (db, file) = make_test_db("contract Widget {}");
    let widget = find_contract(&db, file, "Widget");
    let range = def_name_range(&db, file, widget).expect("a contract has a name");

    let target = Target::at(&db, widget, range).expect("a contract is renameable");
    assert_eq!(target.name, "Widget");
    assert_eq!(target.identity, Identity::Def(widget));
}

#[test]
fn a_struct_field_targets_its_field() {
    let (db, file) = make_test_db("struct S { uint256 a; } contract C {}");
    let structure = defs_of(&db, file, |def| matches!(def, DefId::Struct(_)))
        .into_iter()
        .next()
        .expect("the fixture declares a struct");
    // Both the member and its name map to the field; the name is the shorter.
    let (range, field) = db
        .source_map(structure)
        .range_to_semantic
        .iter()
        .filter_map(|(range, id)| match id {
            SemanticId::Field(field) => Some((*range, *field)),
            _ => None,
        })
        .min_by_key(|(range, _)| range.end - range.start)
        .expect("the struct declares a field");

    let target = Target::at(&db, structure, range).expect("a field is renameable");
    assert_eq!(target.name, "a");
    assert_eq!(
        target.identity,
        Identity::Field {
            owner: structure,
            field
        }
    );
}

#[test]
fn an_enum_variant_targets_its_variant() {
    let (db, file) = make_test_db("enum E { A, B } contract C {}");
    let enumeration = defs_of(&db, file, |def| matches!(def, DefId::Enum(_)))
        .into_iter()
        .next()
        .expect("the fixture declares an enum");
    let (range, variant) = db
        .source_map(enumeration)
        .range_to_semantic
        .iter()
        .find_map(|(range, id)| match id {
            SemanticId::Variant(variant) => Some((*range, *variant)),
            _ => None,
        })
        .expect("the enum declares a variant");
    let DefId::Enum(enum_) = enumeration else {
        unreachable!()
    };
    let text = db.text(file);
    let spelling = text[range.start as usize..range.end as usize].to_string();

    let target = Target::at(&db, enumeration, range).expect("a variant is renameable");
    assert_eq!(target.name, spelling);
    assert_eq!(target.identity, Identity::Variant { enum_, variant });
}

/// The cursor on one overload's declaration names that declaration. Whether an
/// edit from here covers the siblings is the scan's policy, not the target's.
#[test]
fn a_declaration_targets_one_overload_not_the_set() {
    let (db, file) =
        make_test_db("contract C { function f(uint256) external {} function f(bool) external {} }");
    let overloads = defs_of(&db, file, |def| matches!(def, DefId::Function(_)));
    assert_eq!(overloads.len(), 2);

    for overload in overloads {
        let range = def_name_range(&db, file, overload).expect("a function has a name");
        let target = Target::at(&db, overload, range).expect("an overload is renameable");
        assert_eq!(
            target.identity,
            Identity::Def(overload),
            "the cursor's own declaration only"
        );
    }
}

#[test]
fn keywords_builtins_and_paths_are_not_renameable() {
    let db = db_with(&[
        ("B.sol", "contract B {}"),
        (
            "Main.sol",
            "import \"./B.sol\";\ncontract C { function f() external { msg.sender; this; uint256 x = 1; } }",
        ),
    ]);
    let main = db.file(&path("Main.sol")).unwrap();
    let f = find_function(&db, main, "f");
    let DefId::Function(id) = f else {
        unreachable!()
    };
    let map = db.body_source_map(BodyOwnerId::Function(id));
    let inference = db.inference(f);

    let (mut builtin, mut this, mut primitive) = (false, false, false);
    for (range, semantic) in map.range_to_semantic.iter() {
        let rejected = match semantic {
            SemanticId::Expr(expr) => match inference.expr_resolution(*expr) {
                Some(Resolution::Field(Field::Builtin(_))) => {
                    builtin = true;
                    true
                }
                Some(Resolution::This(_)) => {
                    this = true;
                    true
                }
                _ => false,
            },
            SemanticId::Type(ty) if inference.ty(*ty).ty().def_id(&db).is_none() => {
                primitive = true;
                true
            }
            _ => false,
        };
        if rejected {
            assert!(
                Target::at(&db, f, *range).is_none(),
                "the spelling at {range:?} must not be renameable"
            );
        }
    }
    assert!(builtin && this && primitive, "{builtin} {this} {primitive}");

    // An import path names a file, not a declaration: the spelling is a string
    // node, so it never reaches `Target::at`.
    let import = defs_of(&db, main, |def| matches!(def, DefId::Import(_)))
        .into_iter()
        .next()
        .expect("the fixture imports");
    let path = db
        .source_map(import)
        .range_to_semantic
        .iter()
        .find_map(|(range, id)| matches!(id, SemanticId::Expr(_)).then_some(*range))
        .expect("the import lowers its path");
    let response = prepare_rename(
        &db,
        Request {
            id: RequestId::from(4),
            method: PrepareRenameRequest::METHOD.to_string(),
            params: serde_json::to_value(TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: to_url(main.path(&db)),
                },
                position: byte_to_position(&db.rope(main), path.start as usize),
            })
            .unwrap(),
        },
    )
    .unwrap();
    assert_eq!(response.result, Some(serde_json::Value::Null));
}

#[test]
fn prepare_rename_returns_the_identifier_range_and_its_spelling() {
    let (db, file) = make_test_db("contract Widget {}");
    let widget = find_contract(&db, file, "Widget");
    let range = def_name_range(&db, file, widget).expect("a contract has a name");
    let position = byte_to_position(&db.rope(file), range.start as usize);

    let response = prepare_rename(
        &db,
        Request {
            id: RequestId::from(1),
            method: PrepareRenameRequest::METHOD.to_string(),
            params: serde_json::to_value(TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: to_url(file.path(&db)),
                },
                position,
            })
            .unwrap(),
        },
    )
    .unwrap();

    let result = response.result.expect("prepareRename answers");
    assert_eq!(result["placeholder"], "Widget");
    assert_eq!(
        result["range"],
        serde_json::to_value(crate::capabilities::lsp_range(&db, file, range)).unwrap()
    );
}

#[test]
fn prepare_rename_is_null_where_nothing_is_renameable() {
    let source = "contract C { function f() external { msg.sender; } }";
    let (db, file) = make_test_db(source);
    let offset = source.find("msg").expect("the fixture reads msg");
    let position = byte_to_position(&db.rope(file), offset);

    let response = prepare_rename(
        &db,
        Request {
            id: RequestId::from(2),
            method: PrepareRenameRequest::METHOD.to_string(),
            params: serde_json::to_value(TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: to_url(file.path(&db)),
                },
                position,
            })
            .unwrap(),
        },
    )
    .unwrap();

    assert_eq!(response.result, Some(serde_json::Value::Null));
}

#[test]
fn a_local_renames_its_declaration_and_use_only() {
    let (db, file) = make_test_db(
        "contract C { function f() external { uint x = 1; x = 2; } function g() external { uint x = 3; } }",
    );
    let f = find_function(&db, file, "f");
    let DefId::Function(f_id) = f else {
        unreachable!()
    };
    let map = db.body_source_map(BodyOwnerId::Function(f_id));
    let inference = db.inference(f);
    let (decl_range, local) = map
        .range_to_semantic
        .iter()
        .find_map(|(range, id)| match id {
            SemanticId::Local(local) => Some((*range, *local)),
            _ => None,
        })
        .expect("f declares a local");
    let use_range = map
        .range_to_semantic
        .iter()
        .find_map(|(range, id)| match id {
            SemanticId::Expr(expr) => match inference.expr_resolution(*expr) {
                Some(Resolution::Local(resolved)) if resolved.id == local => Some(*range),
                _ => None,
            },
            _ => None,
        })
        .expect("f uses the local");

    let target = Target::at(&db, f, decl_range).expect("a local is renameable");
    let hits = references(&db, &target, f.file());

    assert_eq!(
        hits,
        FxHashSet::from_iter([(file, decl_range), (file, use_range)]),
        "the declaration and its use, nothing from `g`"
    );
}

#[test]
fn a_state_variable_renames_its_uses_across_files() {
    let db = db_with(&[
        ("A.sol", "contract C { uint256 public state; }"),
        (
            "B.sol",
            "import \"./A.sol\";\ncontract D { function f(C c) external { uint256 v = c.state(); } }",
        ),
    ]);
    let a = db.file(&path("A.sol")).unwrap();
    let b = db.file(&path("B.sol")).unwrap();
    let var = defs_of(&db, a, |def| matches!(def, DefId::Var(_)))
        .into_iter()
        .next()
        .expect("A declares a state variable");
    let range = def_name_range(&db, a, var).expect("the variable has a name");

    let target = Target::at(&db, var, range).expect("a state variable is renameable");
    let hits = references(&db, &target, var.file());

    assert_eq!(hits.len(), 2, "the declaration and `c.state`: {hits:?}");
    assert!(hits.iter().any(|(file, _)| *file == b), "the use in B.sol");
}

#[test]
fn an_overload_renames_itself_and_its_calls_only() {
    let (db, file) = make_test_db(
        "contract C { function f(uint256 v) public {} function f(bool b) public {} function g() external { f(1); f(true); } }",
    );
    let overloads = defs_of(&db, file, |def| match def {
        DefId::Function(id) => db.function_data(*id).name == "f",
        _ => false,
    });
    assert_eq!(overloads.len(), 2);

    for &overload in &overloads {
        let range = def_name_range(&db, file, overload).expect("an overload has a name");
        let target = Target::at(&db, overload, range).expect("an overload is renameable");
        let hits = references(&db, &target, overload.file());

        assert_eq!(hits.len(), 2, "declaration + its call: {hits:?}");
        for &sibling in overloads.iter().filter(|other| **other != overload) {
            let sibling_decl = def_name_range(&db, file, sibling).expect("the sibling has a name");
            assert!(
                !hits.contains(&(file, sibling_decl)),
                "the sibling declaration is untouched"
            );
        }
    }
}

#[test]
fn a_tied_call_site_is_left_untouched() {
    let (db, file) = make_test_db(
        "contract C { function f(uint8 a) public {} function f(uint16 b) public {} function g() external { f(1); } }",
    );
    let overloads = defs_of(&db, file, |def| match def {
        DefId::Function(id) => db.function_data(*id).name == "f",
        _ => false,
    });
    let range = def_name_range(&db, file, overloads[0]).expect("an overload has a name");
    let target = Target::at(&db, overloads[0], range).expect("an overload is renameable");

    let g = find_function(&db, file, "g");
    let DefId::Function(id) = g else {
        unreachable!()
    };
    let call = db
        .body_source_map(BodyOwnerId::Function(id))
        .range_to_semantic
        .iter()
        .find_map(|(range, _)| {
            (&db.text(file)[range.start as usize..range.end as usize] == "f").then_some(*range)
        })
        .expect("the fixture calls f");

    let hits = references(&db, &target, overloads[0].file());
    assert_eq!(
        hits,
        FxHashSet::from_iter([(file, range)]),
        "only the declaration"
    );
    assert!(
        !hits.contains(&(file, call)),
        "the tied call is left untouched"
    );
}

#[test]
fn a_contract_renames_every_spelling_across_files() {
    let db = db_with(&[
        ("Base.sol", "contract Base {}"),
        (
            "Main.sol",
            "import {Base} from \"./Base.sol\";\ncontract D is Base { function f() external { new Base(); } }",
        ),
    ]);
    let base_file = db.file(&path("Base.sol")).unwrap();
    let main = db.file(&path("Main.sol")).unwrap();
    let base = find_contract(&db, base_file, "Base");
    let range = def_name_range(&db, base_file, base).expect("the contract has a name");

    let target = Target::at(&db, base, range).expect("a contract is renameable");
    let hits = references(&db, &target, base.file());

    assert_eq!(
        hits.len(),
        4,
        "declaration, import specifier, `is Base`, `new Base()`: {hits:?}"
    );
    assert_eq!(hits.iter().filter(|(file, _)| *file == main).count(), 3);
}

#[test]
fn a_struct_field_renames_its_member_uses() {
    let db = db_with(&[
        ("S.sol", "struct S { uint256 a; }"),
        (
            "Main.sol",
            "import \"./S.sol\";\ncontract C { function f(S s) external { s.a = 1; } }",
        ),
    ]);
    let s_file = db.file(&path("S.sol")).unwrap();
    let main = db.file(&path("Main.sol")).unwrap();
    let structure = defs_of(&db, s_file, |def| matches!(def, DefId::Struct(_)))
        .into_iter()
        .next()
        .expect("S.sol declares a struct");
    let decl_range = db
        .source_map(structure)
        .range_to_semantic
        .iter()
        .filter_map(|(range, id)| matches!(id, SemanticId::Field(_)).then_some(*range))
        .min_by_key(|range| range.end - range.start)
        .expect("the struct declares a field");

    let target = Target::at(&db, structure, decl_range).expect("a field is renameable");
    let hits = references(&db, &target, structure.file());

    assert_eq!(hits.len(), 2, "the declaration and `s.a`: {hits:?}");
    assert!(hits.iter().any(|(file, _)| *file == main));
}

/// `import {A as B}` allocates one alias ident at both the specifier and the
/// alias node, so resolution alone would rewrite the `B` uses too.
#[test]
fn an_import_alias_is_not_mistaken_for_the_imported_name() {
    let db = db_with(&[
        ("A.sol", "contract A {}"),
        (
            "Main.sol",
            "import {A as B} from \"./A.sol\";\ncontract D { function f() external { new B(); } }",
        ),
    ]);
    let a_file = db.file(&path("A.sol")).unwrap();
    let a = find_contract(&db, a_file, "A");
    let range = def_name_range(&db, a_file, a).expect("the contract has a name");

    let target = Target::at(&db, a, range).expect("a contract is renameable");
    let hits = references(&db, &target, a.file());

    assert_eq!(hits.len(), 2, "the declaration and the specifier: {hits:?}");
    for (file, range) in &hits {
        let text = db.text(*file);
        assert_eq!(
            &text[range.start as usize..range.end as usize],
            "A",
            "only the `A` spelling"
        );
    }
}

#[test]
fn a_dependency_root_is_never_edited() {
    let root = Utf8PathBuf::from("/tmp/rename_tests_dependency");
    let package = PackageId(NormalizedPath::new(root.clone()));
    let db = Db::new(WorkspaceWithFiles {
        ignored_roots: Vec::new(),
        packages: vec![WsPackage {
            root: NormalizedPath::new(root.clone()),
            remappings: Vec::new(),
        }],
        roots: vec![
            RootWithFiles {
                path: NormalizedPath::new(root.join("src")),
                package_id: package.clone(),
                is_dependency: false,
                files: vec![LoadedFile {
                    path: NormalizedPath::new(root.join("src/Main.sol")),
                    text: Some(Rope::from_str("contract M {}")),
                }],
            },
            RootWithFiles {
                path: NormalizedPath::new(root.join("lib/dep")),
                package_id: package,
                is_dependency: true,
                files: vec![LoadedFile {
                    path: NormalizedPath::new(root.join("lib/dep/D.sol")),
                    text: Some(Rope::from_str("contract Dep {}")),
                }],
            },
        ],
    });
    let dep_file = db.file(&root.join("lib/dep/D.sol")).unwrap();
    let dep = find_contract(&db, dep_file, "Dep");
    let range = def_name_range(&db, dep_file, dep).expect("the contract has a name");

    assert!(
        Target::at(&db, dep, range).is_none(),
        "a dependency root is never edited"
    );
}

fn rename_request(
    db: &Db,
    file: crate::salsa::File,
    position: Position,
    new_name: &str,
) -> Request {
    Request {
        id: RequestId::from(7),
        method: Rename::METHOD.to_string(),
        params: serde_json::to_value(RenameParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: to_url(file.path(db)),
                },
                position,
            },
            new_name: new_name.to_string(),
            work_done_progress_params: Default::default(),
        })
        .unwrap(),
    }
}

#[test]
fn rename_returns_edits_for_every_spelling() {
    let (db, file) =
        make_test_db("contract C { uint256 public state; function f() external { state = 1; } }");
    let var = defs_of(&db, file, |def| matches!(def, DefId::Var(_)))
        .into_iter()
        .next()
        .expect("the fixture declares a state variable");
    let range = def_name_range(&db, file, var).expect("the variable has a name");
    let position = byte_to_position(&db.rope(file), range.start as usize);

    let response = rename(&db, rename_request(&db, file, position, "total")).unwrap();
    let result = response.result.expect("rename answers");
    let edits = result["changes"]
        .as_object()
        .expect("changes")
        .values()
        .next()
        .expect("one file")
        .as_array()
        .expect("edits");

    assert_eq!(edits.len(), 2, "declaration and use: {result}");
    assert!(edits.iter().all(|edit| edit["newText"] == "total"));
}

#[test]
fn rename_rejects_a_keyword() {
    let (db, file) = make_test_db("contract C { uint256 public state; }");
    let var = defs_of(&db, file, |def| matches!(def, DefId::Var(_)))
        .into_iter()
        .next()
        .expect("the fixture declares a state variable");
    let range = def_name_range(&db, file, var).expect("the variable has a name");
    let position = byte_to_position(&db.rope(file), range.start as usize);

    let response = rename(&db, rename_request(&db, file, position, "function")).unwrap();
    let error = response.error.expect("a keyword is rejected");
    assert!(error.message.contains("not a valid"), "{}", error.message);
}

#[test]
fn a_tied_call_site_does_not_block_a_rename() {
    let (db, file) = make_test_db(
        "contract C { function f(uint8 a) public {} function f(uint16 b) public {} function g() external { f(1); } }",
    );
    let overloads = defs_of(&db, file, |def| match def {
        DefId::Function(id) => db.function_data(*id).name == "f",
        _ => false,
    });
    let range = def_name_range(&db, file, overloads[0]).expect("an overload has a name");
    let position = byte_to_position(&db.rope(file), range.start as usize);

    let response = rename(&db, rename_request(&db, file, position, "h")).unwrap();
    assert!(
        response.error.is_none(),
        "the tied call is skipped, not fatal"
    );
    let result = response.result.expect("rename answers");
    let edits = result["changes"]
        .as_object()
        .expect("one file")
        .values()
        .next()
        .and_then(|edits| edits.as_array())
        .expect("edits");
    assert_eq!(edits.len(), 1, "only the declaration: {edits:?}");
}

/// The alias's semantic entry, found by spelling.
fn import_alias_range(
    db: &Db,
    file: crate::salsa::File,
    import: DefId,
    spelling: &str,
) -> crate::ast::NodeRange {
    db.source_map(import)
        .range_to_semantic
        .iter()
        .find_map(|(range, id)| {
            let text = &db.text(file)[range.start as usize..range.end as usize];
            matches!(id, SemanticId::Expr(_))
                .then_some(*range)
                .filter(|_| text == spelling)
        })
        .expect("the alias has a semantic entry")
}

#[test]
fn a_namespace_alias_renames_its_spelling() {
    let db = db_with(&[
        ("B.sol", "contract B {}"),
        (
            "Main.sol",
            "import \"./B.sol\" as NS;\ncontract D { function f() external { new NS.B(); } }",
        ),
    ]);
    let main = db.file(&path("Main.sol")).unwrap();
    let import = defs_of(&db, main, |def| matches!(def, DefId::Import(_)))
        .into_iter()
        .next()
        .expect("the fixture imports");
    let alias = import_alias_range(&db, main, import, "NS");

    let target = Target::at(&db, import, alias).expect("a namespace alias is renameable");
    assert_eq!(target.name, "NS");
    let hits = references(&db, &target, main);
    assert_eq!(hits.len(), 2, "alias declaration + `NS.B` use: {hits:?}");
}

#[test]
fn two_importers_aliasing_one_file_stay_independent() {
    let db = db_with(&[
        ("B.sol", "contract B {}"),
        (
            "One.sol",
            "import \"./B.sol\" as NS;\ncontract O { function f() external { new NS.B(); } }",
        ),
        (
            "Two.sol",
            "import \"./B.sol\" as NS;\ncontract T { function g() external { new NS.B(); } }",
        ),
    ]);
    let one = db.file(&path("One.sol")).unwrap();
    let two = db.file(&path("Two.sol")).unwrap();
    let import = defs_of(&db, one, |def| matches!(def, DefId::Import(_)))
        .into_iter()
        .next()
        .expect("the fixture imports");
    let alias = import_alias_range(&db, one, import, "NS");

    let target = Target::at(&db, import, alias).expect("a namespace alias is renameable");
    let hits = references(&db, &target, one);
    assert_eq!(hits.len(), 2, "only this importer's spellings: {hits:?}");
    assert!(
        !hits.iter().any(|(file, _)| *file == two),
        "the other importer's alias is a different declaration"
    );
}

#[test]
fn a_named_alias_is_file_local() {
    let db = db_with(&[
        ("A.sol", "contract A {}"),
        (
            "One.sol",
            "import {A as B} from \"./A.sol\";\ncontract O { function f() external { new B(); } }",
        ),
        (
            "Two.sol",
            "import {A as B} from \"./A.sol\";\ncontract T { function g() external { new B(); } }",
        ),
    ]);
    let one = db.file(&path("One.sol")).unwrap();
    let two = db.file(&path("Two.sol")).unwrap();
    let import = defs_of(&db, one, |def| matches!(def, DefId::Import(_)))
        .into_iter()
        .next()
        .expect("the fixture imports");
    let alias = import_alias_range(&db, one, import, "B");

    let target = Target::at(&db, import, alias).expect("an import alias is renameable");
    assert_eq!(target.name, "B");
    let hits = references(&db, &target, one);
    assert_eq!(hits.len(), 2, "alias declaration + `new B()`: {hits:?}");
    assert!(
        !hits.iter().any(|(file, _)| *file == two),
        "the other importer's alias is a different declaration"
    );
}

#[test]
fn a_dependency_declaration_is_not_renamed_from_the_workspace() {
    let root = Utf8PathBuf::from("/tmp/rename_tests_dep_alias");
    let package = PackageId(NormalizedPath::new(root.clone()));
    let db = Db::new(WorkspaceWithFiles {
        ignored_roots: Vec::new(),
        packages: vec![WsPackage {
            root: NormalizedPath::new(root.clone()),
            remappings: Vec::new(),
        }],
        roots: vec![
            RootWithFiles {
                path: NormalizedPath::new(root.join("src")),
                package_id: package.clone(),
                is_dependency: false,
                files: vec![LoadedFile {
                    path: NormalizedPath::new(root.join("src/Main.sol")),
                    text: Some(Rope::from_str(
                        "import {Dep} from \"lib/dep/D.sol\";\ncontract M { function f() external { new Dep(); } }",
                    )),
                }],
            },
            RootWithFiles {
                path: NormalizedPath::new(root.join("lib/dep")),
                package_id: package,
                is_dependency: true,
                files: vec![LoadedFile {
                    path: NormalizedPath::new(root.join("lib/dep/D.sol")),
                    text: Some(Rope::from_str("contract Dep {}")),
                }],
            },
        ],
    });
    let main = db.file(&root.join("src/Main.sol")).unwrap();
    let import = defs_of(&db, main, |def| matches!(def, DefId::Import(_)))
        .into_iter()
        .next()
        .expect("the fixture imports");
    let specifier = import_alias_range(&db, main, import, "Dep");

    // The edit would change the specifier and the uses but not the declaration,
    // which lives in a regenerated root.
    assert!(
        Target::at(&db, import, specifier).is_none(),
        "a dependency declaration must not be renamed from the workspace"
    );
}

#[test]
fn a_namespace_alias_to_a_dependency_file_is_renameable() {
    let root = Utf8PathBuf::from("/tmp/rename_tests_dep_ns");
    let package = PackageId(NormalizedPath::new(root.clone()));
    let db = Db::new(WorkspaceWithFiles {
        ignored_roots: Vec::new(),
        packages: vec![WsPackage {
            root: NormalizedPath::new(root.clone()),
            remappings: Vec::new(),
        }],
        roots: vec![
            RootWithFiles {
                path: NormalizedPath::new(root.join("src")),
                package_id: package.clone(),
                is_dependency: false,
                files: vec![LoadedFile {
                    path: NormalizedPath::new(root.join("src/Main.sol")),
                    text: Some(Rope::from_str(
                        "import \"lib/dep/D.sol\" as NS;\ncontract M { function f() external { new NS.Dep(); } }",
                    )),
                }],
            },
            RootWithFiles {
                path: NormalizedPath::new(root.join("lib/dep")),
                package_id: package,
                is_dependency: true,
                files: vec![LoadedFile {
                    path: NormalizedPath::new(root.join("lib/dep/D.sol")),
                    text: Some(Rope::from_str("contract Dep {}")),
                }],
            },
        ],
    });
    let main = db.file(&root.join("src/Main.sol")).unwrap();
    let import = defs_of(&db, main, |def| matches!(def, DefId::Import(_)))
        .into_iter()
        .next()
        .expect("the fixture imports");
    let alias = import_alias_range(&db, main, import, "NS");

    // The alias is file-local: renaming it never edits the dependency file.
    let target = Target::at(&db, import, alias).expect("a namespace alias is renameable");
    let hits = references(&db, &target, main);
    assert_eq!(hits.len(), 2, "alias declaration + `NS.Dep` use: {hits:?}");
}

#[test]
fn a_tied_call_is_not_a_rename_target() {
    let source = "contract C { function f(uint8) public {} function f(uint16) public {} function g() external { f(1); } }";
    let (db, file) = make_test_db(source);
    let g = find_function(&db, file, "g");
    let DefId::Function(id) = g else {
        unreachable!()
    };
    let map = db.body_source_map(BodyOwnerId::Function(id));
    let call = map
        .range_to_semantic
        .iter()
        .find_map(|(range, _)| {
            let text = &db.text(file)[range.start as usize..range.end as usize];
            (text == "f").then_some(*range)
        })
        .expect("the fixture calls f");

    assert!(
        Target::at(&db, g, call).is_none(),
        "a tied call denotes several declarations, and solc reports the tie"
    );
}
