use camino::Utf8PathBuf;
use lsp_server::{Request, RequestId};
use lsp_types::request::{References, Request as LspRequest};
use lsp_types::{
    PartialResultParams, Position, ReferenceContext, ReferenceParams, TextDocumentIdentifier, TextDocumentPositionParams,
    WorkDoneProgressParams,
};
use ropey::Rope;

use crate::capabilities::references::{Target, occurrences, references};
use crate::hir::tests::helper::{def_name_range, find_contract, make_test_db};
use crate::ir::def_map::DefId;
use crate::paths::NormalizedPath;
use crate::salsa::root_db::RootDb;
use crate::salsa::{Db, File, HirDb};
use crate::utilities::{byte_to_position, to_url};
use crate::workspace::{LoadedFile, Package as WsPackage, PackageId, RootWithFiles, WorkspaceWithFiles};

fn root() -> Utf8PathBuf {
    Utf8PathBuf::from_path_buf(std::env::temp_dir().join("references_tests")).expect("temp dir is UTF-8")
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

fn defs_of(db: &Db, file: File, kind: impl Fn(&DefId) -> bool) -> Vec<DefId> {
    db.file_def_map(file).defs.keys().copied().filter(kind).collect()
}

fn references_request(db: &Db, file: File, position: Position, include_declaration: bool) -> Request {
    Request {
        id: RequestId::from(11),
        method: References::METHOD.to_string(),
        params: serde_json::to_value(ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: to_url(file.path(db)),
                },
                position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: ReferenceContext { include_declaration },
        })
        .unwrap(),
    }
}

/// A dependency symbol is visible to other dependency files as well as the
/// workspace; references must walk dependency roots too.
#[test]
fn a_dependency_symbol_reports_uses_in_other_dependency_files() {
    let root = Utf8PathBuf::from_path_buf(std::env::temp_dir().join("references_tests_dep")).expect("temp dir is UTF-8");
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
                files: vec![
                    LoadedFile {
                        path: NormalizedPath::new(root.join("lib/dep/D.sol")),
                        text: Some(Rope::from_str("contract Dep {}")),
                    },
                    LoadedFile {
                        path: NormalizedPath::new(root.join("lib/dep/Other.sol")),
                        text: Some(Rope::from_str(
                            "import \"./D.sol\";\ncontract O { function g() external { new Dep(); } }",
                        )),
                    },
                ],
            },
        ],
    });
    let dep_file = db.file(&root.join("lib/dep/D.sol")).unwrap();
    let other = db.file(&root.join("lib/dep/Other.sol")).unwrap();
    let dep = find_contract(&db, dep_file, "Dep");
    let range = def_name_range(&db, dep_file, dep).expect("the contract has a name");

    let target = Target::at(&db, dep, range).expect("a dependency declaration is a reference target");
    let hits = occurrences(&db, &target, dep_file);

    assert_eq!(
        hits.len(),
        4,
        "declaration, the workspace import specifier and use, and the dependency use: {hits:?}"
    );
    assert!(hits.iter().any(|(file, _)| *file == other), "the use in Other.sol");
}

#[test]
fn references_can_omit_the_declaration() {
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
    let position = byte_to_position(&db.rope(a), range.start as usize);

    let with_declaration = references(&db, references_request(&db, a, position, true)).unwrap();
    let locations = with_declaration.result.unwrap();
    assert_eq!(locations.as_array().unwrap().len(), 2, "{locations}");

    let without_declaration = references(&db, references_request(&db, a, position, false)).unwrap();
    let locations = without_declaration.result.unwrap();
    let locations = locations.as_array().unwrap();
    assert_eq!(locations.len(), 1, "{locations:?}");
    assert_eq!(locations[0]["uri"], to_url(b.path(&db)).as_str());
}

#[test]
fn a_local_reports_only_its_owner() {
    let (db, file) = make_test_db("contract C { function f() external { uint x = 1; x = 2; } function g() external { uint x = 3; } }");
    let f = crate::hir::tests::helper::find_function(&db, file, "f");
    let DefId::Function(id) = f else { unreachable!() };
    let map = db.body_source_map(crate::hir::body_map::BodyOwnerId::Function(id));
    let (decl_range, _) = map
        .range_to_semantic
        .iter()
        .find_map(|(range, id)| match id {
            crate::hir::item_data::SemanticId::Local(local) => Some((*range, *local)),
            _ => None,
        })
        .expect("f declares a local");
    let target = Target::at(&db, f, decl_range).expect("a local is a reference target");
    let hits = occurrences(&db, &target, file);

    assert_eq!(hits.len(), 2, "the declaration and its use: {hits:?}");
    assert!(hits.iter().all(|(hit_file, _)| *hit_file == file));
}
